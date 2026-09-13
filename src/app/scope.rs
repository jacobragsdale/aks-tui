//! One tab: the list over one namespace of one cluster, and the state that is
//! this tab's alone — its cursor, its search, its sort, and the text pane
//! under the details that shows a log, a describe or a YAML.

use std::cmp::Ordering;
use std::collections::HashMap;

use super::cursor::{ListCursor, ScrollState};
use super::screen::{AppAction, Target};
use super::shell::{Focus, Shell};
use crate::columns::{ColumnId, POD_COLUMNS, TableLayout};
use crate::config::Tab;
use crate::filter::{self, Query};
use crate::kube::{LogFollow, ObjectRef, Pod, PodKey, Replicas, Request, TextKind};
use crate::store::ScopeData;
use crate::text_input::TextInput;

/// The `key:` filters the pods list knows. Everything else typed is a word.
pub const SCHEMA: &[&str] = &["name", "ns", "status", "owner", "app", "node"];

/// How many log lines the pane keeps. Past this the oldest go, and the first
/// line says how many.
pub const LOG_LINE_CAP: usize = 20_000;

/// What a row looks like to the search: every cell a person might type part
/// of, joined once per read rather than per keystroke.
#[must_use]
pub fn haystack(pod: &Pod) -> String {
    let mut text = String::with_capacity(96);
    text.push_str(&pod.key.name);
    text.push(' ');
    text.push_str(&pod.key.namespace);
    text.push(' ');
    text.push_str(&pod.status);
    text.push(' ');
    text.push_str(pod.owner_name());
    text.push(' ');
    text.push_str(&pod.node);
    for container in &pod.containers {
        text.push(' ');
        text.push_str(&container.image);
    }
    text
}

/// Whether one pod answers every `key:value` in the query. The words are
/// [`crate::search`]'s job; this is only the fields.
#[must_use]
pub fn passes(pod: &Pod, query: &Query) -> bool {
    query.fields.iter().all(|(key, value)| match key.as_str() {
        "name" => filter::contains(&pod.key.name, value),
        "ns" => filter::contains(&pod.key.namespace, value),
        "status" => filter::contains(&pod.status, value),
        "owner" => filter::contains(pod.owner_name(), value),
        "app" => pod.app().is_some_and(|app| filter::contains(app, value)),
        "node" => filter::contains(&pod.node, value),
        _ => true,
    })
}

/// Orders two pods by one column, and by name whenever the column cannot
/// tell them apart: a list re-read every few seconds must not shuffle rows
/// that are equal.
fn compare(left: &Pod, right: &Pod, by: ColumnId, descending: bool) -> Ordering {
    let flip = |ordering: Ordering| {
        if descending {
            ordering.reverse()
        } else {
            ordering
        }
    };
    let text = |left: &str, right: &str| flip(cmp_ignore_ascii_case(left, right));
    let ordering = match by {
        ColumnId::Name => Ordering::Equal,
        ColumnId::Namespace => text(&left.key.namespace, &right.key.namespace),
        // How much of a pod is up first, then how big it is: `0/1` before
        // `1/2` before `2/2`.
        ColumnId::Ready => flip(left.ready.cmp(&right.ready)),
        ColumnId::Status => text(&left.status, &right.status),
        ColumnId::Restarts => flip(left.restarts.cmp(&right.restarts)),
        // Ascending age is the newest pod first; one with no timestamp has no
        // age and sorts last whichever way the column is turned.
        ColumnId::Age => match (left.created, right.created) {
            (Some(left), Some(right)) => flip(right.cmp(&left)),
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (None, None) => Ordering::Equal,
        },
        ColumnId::Node => text(&left.node, &right.node),
        ColumnId::Ip => text(&left.ip, &right.ip),
        ColumnId::Owner => text(left.owner_name(), right.owner_name()),
        ColumnId::Image => text(
            left.containers.first().map_or("", |c| c.image.as_str()),
            right.containers.first().map_or("", |c| c.image.as_str()),
        ),
    };
    ordering.then_with(|| text(&left.key.name, &right.key.name))
}

/// Two names, compared without regard to ASCII case and without allocating.
fn cmp_ignore_ascii_case(left: &str, right: &str) -> Ordering {
    left.bytes()
        .map(|byte| byte.to_ascii_lowercase())
        .cmp(right.bytes().map(|byte| byte.to_ascii_lowercase()))
}

/// What a confirmation is about to do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Verb {
    /// Delete the pod and let its owner put a new one up.
    Restart(PodKey),
    /// `kubectl rollout restart` of the owner.
    Rollout(ObjectRef),
}

impl Verb {
    /// The modal's yes button.
    #[must_use]
    pub const fn button(&self) -> &'static str {
        match self {
            Self::Restart(_) => "Restart",
            Self::Rollout(_) => "Rollout restart",
        }
    }

    /// What the key line under the buttons says.
    #[must_use]
    pub const fn hint(&self) -> &'static str {
        match self {
            Self::Restart(_) => "x again to restart it",
            Self::Rollout(_) => "X again to restart the rollout",
        }
    }
}

/// A question on top of the table, taking every key until it is answered.
#[derive(Debug)]
pub enum Modal {
    Confirm {
        title: String,
        body: Vec<String>,
        verb: Verb,
    },
    Scale {
        object: ObjectRef,
        input: TextInput,
        current: Option<Replicas>,
    },
}

/// The owner kinds `kubectl scale` takes.
const SCALABLE: &[&str] = &["deployment", "statefulset", "replicaset"];
/// The owner kinds `kubectl rollout restart` takes.
const ROLLABLE: &[&str] = &["deployment", "statefulset", "daemonset"];

/// What the text pane under the pod's details is showing.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PaneText {
    /// The pod's log, tailed.
    #[default]
    Log,
    Describe,
    Yaml,
}

pub struct ScopeScreen {
    pub cursor: ListCursor,
    pub layout: TableLayout,
    pub input: TextInput,
    pub sort: ColumnId,
    pub descending: bool,
    /// Which rows of the tab's list are shown, in the order they are shown.
    visible: Vec<usize>,
    /// Every row, in sort order. A query filters this rather than the list,
    /// so the shown rows come out sorted without being sorted again.
    sorted: Vec<usize>,
    /// One searchable string per row, built when the rows change rather
    /// than when the query does.
    haystacks: Vec<String>,
    /// What `sorted` was last built from.
    ordered_for: Option<(ColumnId, bool, usize)>,
    /// What `visible` was last built from, so a redraw that changed nothing
    /// does not rebuild it.
    built_for: Option<(String, ColumnId, bool, usize)>,
    /// The width the columns were last solved at, which is what `S` walks.
    available: u16,
    /// How far down the details pane is scrolled.
    pub details_scroll: ScrollState,

    // ── The text pane ──────────────────────────────────────────────────
    /// Whether the text pane is open under the details at all.
    pub pane_open: bool,
    pub pane: PaneText,
    pub pane_scroll: ScrollState,
    /// Whether the pane has the whole details area to itself.
    pub pane_zoom: bool,
    /// `/` inside the pane: only lines containing every word are shown.
    pub pane_filter: TextInput,
    /// Whether the log pane is pinned to the tail.
    log_follow: bool,
    /// Whether the stream has ended: the pod went, or `kubectl` refused.
    log_finished: bool,
    /// What the pane is tailing now, which is also what the worker is told
    /// to follow. The lines held are this target's and nobody else's.
    log_target: Option<LogFollow>,
    log_lines: Vec<String>,
    /// How many lines have gone off the top of the buffer, for the line that
    /// says so.
    log_skipped: usize,
    /// The container chosen with `C`, for the pod the pane is on, and whether
    /// `P` has asked for the run before the last restart.
    container: Option<String>,
    previous: bool,
    /// What describe and `get -o yaml` said this run, by object, and the one
    /// that is out and not yet back.
    texts: HashMap<(TextKind, ObjectRef), Result<Vec<String>, String>>,
    pending: Option<(TextKind, ObjectRef)>,

    // ── Actions ────────────────────────────────────────────────────────
    /// The question on top of the table, while one is asked.
    pub modal: Option<Modal>,
    /// What each scalable owner said about its replicas this run, and the
    /// one asked and not yet answered.
    owners: HashMap<ObjectRef, Result<Replicas, String>>,
    owner_pending: Option<ObjectRef>,
}

impl Default for ScopeScreen {
    fn default() -> Self {
        Self {
            cursor: ListCursor::default(),
            layout: TableLayout::new(POD_COLUMNS),
            input: TextInput::default(),
            sort: ColumnId::Name,
            descending: false,
            visible: Vec::new(),
            sorted: Vec::new(),
            haystacks: Vec::new(),
            ordered_for: None,
            built_for: None,
            available: 0,
            details_scroll: ScrollState::default(),
            pane_open: false,
            pane: PaneText::Log,
            pane_scroll: ScrollState::default(),
            pane_zoom: false,
            pane_filter: TextInput::default(),
            log_follow: true,
            log_finished: false,
            log_target: None,
            log_lines: Vec::new(),
            log_skipped: 0,
            container: None,
            previous: false,
            texts: HashMap::new(),
            pending: None,
            modal: None,
            owners: HashMap::new(),
            owner_pending: None,
        }
    }
}

/// The pod's owner as an object `kubectl` can be asked about:
/// `deployment/orders-api`.
#[must_use]
pub fn owner_object(pod: &Pod) -> Option<ObjectRef> {
    let (kind, name) = pod.owner.as_ref()?;
    Some(ObjectRef {
        kind: kind.to_ascii_lowercase(),
        namespace: pod.key.namespace.clone(),
        name: name.clone(),
    })
}

impl ScopeScreen {
    /// The rows on screen, as indices into the tab's list.
    #[must_use]
    pub fn visible(&self) -> &[usize] {
        &self.visible
    }

    /// The pod under the cursor.
    #[must_use]
    pub fn selected<'a>(&self, data: &'a ScopeData) -> Option<&'a Pod> {
        data.pods.get(*self.visible.get(self.cursor.index)?)
    }

    /// Rebuilds the shown rows when the query, the sort or the rows have
    /// moved. Cheap to call every frame: it compares first, and a keystroke
    /// only ever re-runs the filter.
    pub fn refilter(&mut self, data: &ScopeData) {
        let order_key = (self.sort, self.descending, data.pods.len());
        if self.ordered_for != Some(order_key) {
            self.ordered_for = Some(order_key);
            self.built_for = None;
            self.reorder(data);
        }
        let key = (
            self.input.text().to_owned(),
            self.sort,
            self.descending,
            data.pods.len(),
        );
        if self.built_for.as_ref() == Some(&key) {
            return;
        }
        self.built_for = Some(key);

        let parsed = Query::parse(self.input.text(), SCHEMA);
        let mut words = crate::search::Query::new(&parsed.words);
        self.visible = self
            .sorted
            .iter()
            .copied()
            .filter(|at| passes(&data.pods[*at], &parsed) && words.matches(&self.haystacks[*at]))
            .collect();
        self.cursor.clamp(self.visible.len());
    }

    /// Every row, in sort order, and the searchable text of each. Run when
    /// the rows or the sort change, not once a keystroke of the query.
    fn reorder(&mut self, data: &ScopeData) {
        if self.haystacks.len() != data.pods.len() {
            self.haystacks = data.pods.iter().map(haystack).collect();
        }
        self.sorted = (0..data.pods.len()).collect();
        let (by, descending) = (self.sort, self.descending);
        self.sorted
            .sort_by(|a, b| compare(&data.pods[*a], &data.pods[*b], by, descending));
    }

    /// Forces the next `refilter` to do the work, after the rows underneath
    /// have moved.
    pub fn invalidate(&mut self) {
        self.built_for = None;
        self.ordered_for = None;
        self.haystacks.clear();
    }

    /// After a read: back onto the same pod if it is still shown, wherever
    /// it now sorts.
    pub fn keep_cursor(&mut self, data: &ScopeData, was: Option<PodKey>) {
        self.refilter(data);
        let Some(key) = was else {
            self.cursor.clamp(self.visible.len());
            return;
        };
        match self.visible.iter().position(|at| data.pods[*at].key == key) {
            Some(at) => self.cursor.focus(at),
            None => self.cursor.clamp(self.visible.len()),
        }
    }

    /// What the cursor is on, by identity rather than by position.
    #[must_use]
    pub fn cursor_identity(&self, data: &ScopeData) -> Option<PodKey> {
        self.selected(data).map(|pod| pod.key.clone())
    }

    /// `S`: the next column on screen.
    pub fn next_sort(&mut self) {
        let columns: Vec<ColumnId> = self
            .layout
            .visible_columns(self.available)
            .into_iter()
            .map(|column| column.id)
            .collect();
        if columns.is_empty() {
            return;
        }
        let at = columns.iter().position(|held| *held == self.sort);
        self.sort = columns[at.map_or(0, |at| (at + 1) % columns.len())];
        self.descending = false;
    }

    /// A header click: the same column cycles ascending, descending, then
    /// back to the default; a different column starts ascending.
    pub fn sort_by(&mut self, column: ColumnId) {
        if self.sort == column {
            if self.descending {
                self.sort = ColumnId::Name;
                self.descending = false;
            } else {
                self.descending = true;
            }
        } else {
            self.sort = column;
            self.descending = false;
        }
    }

    /// What the bottom border says: how many rows of how many, and the sort.
    #[must_use]
    pub fn status(&self, data: &ScopeData) -> String {
        let total = data.pods.len();
        let arrow = if self.descending { "↓" } else { "↑" };
        let shown = if self.visible.len() == total {
            format!("{total}")
        } else {
            format!("{}/{total}", self.visible.len())
        };
        format!("{shown} · {} {arrow}", self.sort.label())
    }

    /// `✗ N` while N pods are in trouble.
    #[must_use]
    pub fn badge(data: &ScopeData) -> Option<String> {
        let count = data.unhealthy();
        (count > 0).then(|| format!("\u{2717} {count}"))
    }

    /// Remembers the width the columns were solved at, so `S` walks the
    /// columns that are actually on screen.
    pub fn note_width(&mut self, available: u16) {
        self.available = available;
    }

    // ── The text pane ──────────────────────────────────────────────────

    /// What the log pane should be following: the pod under the cursor, the
    /// container chosen if the pane is still on the pod it was chosen for,
    /// and whether the run before the last restart was asked for. `None`
    /// while the pane is closed or showing something else. The app diffs
    /// this against what the worker was last told.
    #[must_use]
    pub fn log_target(&self, scope: usize, data: &ScopeData) -> Option<LogFollow> {
        if !self.pane_open || self.pane != PaneText::Log {
            return None;
        }
        let pod = self.selected(data)?;
        let same_pod = self
            .log_target
            .as_ref()
            .is_some_and(|held| held.key == pod.key);
        let container = self
            .container
            .as_ref()
            .filter(|_| same_pod)
            .filter(|name| pod.containers.iter().any(|held| held.name == **name))
            .cloned();
        Some(LogFollow {
            scope,
            key: pod.key.clone(),
            container,
            previous: self.previous,
        })
    }

    /// Settles the pane on a new stream. A different target is a different
    /// stream: the lines held were the last one's, and the pane goes back to
    /// the tail of the new one.
    pub fn begin_follow(&mut self, target: Option<LogFollow>) {
        self.container = target.as_ref().and_then(|held| held.container.clone());
        self.log_target = target;
        self.log_lines.clear();
        self.log_skipped = 0;
        self.log_finished = false;
        self.log_follow = true;
        self.pane_scroll = ScrollState::default();
    }

    /// What the pane is on now, which is what the lines held belong to.
    #[must_use]
    pub fn following(&self) -> Option<&LogFollow> {
        self.log_target.as_ref()
    }

    /// Folds lines onto the end of the log. Lines for a stream the pane has
    /// already left are dropped rather than mixed into the one it is on.
    pub fn append_log(&mut self, target: &LogFollow, lines: Vec<String>, finished: bool) {
        if Some(target) != self.log_target.as_ref() {
            return;
        }
        self.log_lines.extend(lines);
        if self.log_lines.len() > LOG_LINE_CAP {
            // One more than the overflow, because the line saying what went
            // takes a place of its own — and when there already is one, it
            // is the first line to go and its count carries on.
            let overflow = self.log_lines.len() - LOG_LINE_CAP + 1;
            let dropped = overflow - usize::from(self.log_skipped > 0);
            self.log_lines.drain(..overflow);
            self.log_skipped += dropped;
            self.log_lines.insert(
                0,
                format!("\u{2026} {} earlier lines skipped", self.log_skipped),
            );
        }
        self.log_finished = finished;
    }

    #[must_use]
    pub fn log_lines(&self) -> &[String] {
        &self.log_lines
    }

    /// Whether the log pane is pinned to the tail, which is what `End` puts
    /// it back to and scrolling up takes it out of.
    #[must_use]
    pub const fn log_following(&self) -> bool {
        self.log_follow
    }

    /// Whether the stream the pane is on has ended.
    #[must_use]
    pub const fn log_ended(&self) -> bool {
        self.log_finished
    }

    #[must_use]
    pub const fn previous(&self) -> bool {
        self.previous
    }

    /// What describe or yaml said about one object, if it has come back.
    #[must_use]
    pub fn text(&self, kind: TextKind, object: &ObjectRef) -> Option<&Result<Vec<String>, String>> {
        self.texts.get(&(kind, object.clone()))
    }

    /// Whether a describe or yaml for this object is out and not yet back.
    #[must_use]
    pub fn text_pending(&self, kind: TextKind, object: &ObjectRef) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|(held, held_object)| *held == kind && held_object == object)
    }

    /// One text has come back. Kept whichever pod the cursor is on now: the
    /// cursor coming back to that pod shows it without asking again.
    pub fn set_text(
        &mut self,
        kind: TextKind,
        object: ObjectRef,
        text: Result<Vec<String>, String>,
    ) {
        if self.pending.as_ref() == Some(&(kind, object.clone())) {
            self.pending = None;
        }
        self.texts.insert((kind, object), text);
    }

    /// The object the pane is about: the pod under the cursor.
    #[must_use]
    pub fn pane_object(&self, data: &ScopeData) -> Option<ObjectRef> {
        self.selected(data).map(|pod| ObjectRef::pod(&pod.key))
    }

    /// `Enter` or `l`: the log pane, open with the pod's log; again, closed.
    /// Says whether the pane is open afterwards.
    pub fn toggle_log(&mut self) -> bool {
        if self.pane_open && self.pane == PaneText::Log {
            self.close_pane();
            false
        } else {
            self.open_pane(PaneText::Log);
            true
        }
    }

    /// `d` or `v`: the pane on that text, and the request that fetches it
    /// when nothing has yet, for this object.
    pub fn show_text(&mut self, scope: usize, kind: TextKind, data: &ScopeData) -> Option<Request> {
        let object = self.pane_object(data)?;
        self.open_pane(match kind {
            TextKind::Describe => PaneText::Describe,
            TextKind::Yaml => PaneText::Yaml,
        });
        if self.texts.contains_key(&(kind, object.clone())) {
            return None;
        }
        self.pending = Some((kind, object.clone()));
        Some(match kind {
            TextKind::Describe => Request::Describe { scope, object },
            TextKind::Yaml => Request::Yaml { scope, object },
        })
    }

    fn open_pane(&mut self, pane: PaneText) {
        self.pane_open = true;
        if self.pane != pane {
            self.pane_scroll.scroll_to(0);
        }
        self.pane = pane;
        if pane == PaneText::Log {
            self.log_follow = true;
        }
    }

    /// `Esc` with the pane showing: closed, and the follow goes with it on
    /// the next tick.
    pub fn close_pane(&mut self) {
        self.pane_open = false;
        self.pane_zoom = false;
        self.pane_filter.clear();
    }

    /// `z`: the text pane alone in the details area, and back.
    pub fn toggle_zoom(&mut self) {
        if self.pane_open {
            self.pane_zoom = !self.pane_zoom;
        }
    }

    /// `C`: the log moves to the pod's next container, round to the first
    /// again. A pod with one container says so rather than doing nothing.
    pub fn next_container(&mut self, shell: &mut Shell, data: &ScopeData) {
        let Some(pod) = self.selected(data) else {
            return;
        };
        let names: Vec<&str> = pod
            .containers
            .iter()
            .map(|container| container.name.as_str())
            .collect();
        if names.len() < 2 {
            shell.set_status(format!("{} has one container", pod.key.name));
            return;
        }
        let current = self
            .container
            .as_ref()
            .and_then(|held| names.iter().position(|name| *name == held.as_str()))
            .unwrap_or(0);
        let next = names[(current + 1) % names.len()].to_owned();
        shell.set_status(format!("Following {next}"));
        self.container = Some(next);
        // The choice is for the pod the pane is on; the follow sync reads it
        // from here.
        if self
            .log_target
            .as_ref()
            .is_none_or(|held| held.key != pod.key)
        {
            self.log_target = Some(LogFollow {
                scope: 0,
                key: pod.key.clone(),
                container: None,
                previous: self.previous,
            });
        }
        self.open_pane(PaneText::Log);
    }

    /// `P`: the `-p` on the log the pane follows, on or off. The run before
    /// the last restart is where a crash loop says why.
    pub fn toggle_previous(&mut self, shell: &mut Shell) {
        self.previous = !self.previous;
        shell.set_status(if self.previous {
            "Following the log from before the last restart"
        } else {
            "Following the running log"
        });
        self.open_pane(PaneText::Log);
    }

    /// `r`: what describe, yaml and the owners said is stale; the pod lists
    /// re-read on their own.
    pub fn on_refresh(&mut self) {
        self.texts.clear();
        self.pending = None;
        self.owners.clear();
        self.owner_pending = None;
    }

    // ── Actions ────────────────────────────────────────────────────────

    /// The replica counts of the pod's owner, once they have come back.
    #[must_use]
    pub fn owner_of(&self, pod: &Pod) -> Option<&Replicas> {
        owner_object(pod)
            .and_then(|object| self.owners.get(&object))
            .and_then(|held| held.as_ref().ok())
    }

    /// The owner read the cursor has settled on, when its counts are not on
    /// file and it is a kind that has any.
    pub fn owner_request(&mut self, scope: usize, data: &ScopeData) -> Option<Request> {
        let object = self.selected(data).and_then(owner_object)?;
        if !SCALABLE.contains(&object.kind.as_str())
            || self.owners.contains_key(&object)
            || self.owner_pending.as_ref() == Some(&object)
        {
            return None;
        }
        self.owner_pending = Some(object.clone());
        Some(Request::Owner { scope, object })
    }

    pub fn set_owner(&mut self, object: ObjectRef, replicas: Result<Replicas, String>) {
        if self.owner_pending.as_ref() == Some(&object) {
            self.owner_pending = None;
        }
        // The scale modal, if it is open on this owner, learns the count too.
        if let Some(Modal::Scale {
            object: held,
            input,
            current,
        }) = &mut self.modal
            && *held == object
            && let Ok(replicas) = &replicas
        {
            if input.is_empty() {
                input.set_text(replicas.desired.to_string());
                input.move_end();
            }
            *current = Some(*replicas);
        }
        self.owners.insert(object, replicas);
    }

    /// `x`: asks, rather than deleting. A pod nothing put there is refused
    /// outright — deleting it would take it away for good rather than
    /// restart it.
    pub fn restart_prompt(&mut self, shell: &mut Shell, data: &ScopeData) {
        let Some(pod) = self.selected(data) else {
            shell.set_error("No pod is selected");
            return;
        };
        let Some((kind, owner)) = &pod.owner else {
            shell.set_error(format!(
                "{} has no controller to put it back; deleting it would not restart it",
                pod.key.name
            ));
            return;
        };
        self.modal = Some(Modal::Confirm {
            title: "Restart pod".to_owned(),
            body: vec![
                format!("Restart {}?", pod.key.name),
                String::new(),
                format!("Deletes the pod; {kind} {owner} replaces it."),
            ],
            verb: Verb::Restart(pod.key.clone()),
        });
    }

    /// `X`: a rollout restart of the owner, which replaces every pod of it
    /// one at a time. Refused for an owner that has no rollout.
    pub fn rollout_prompt(&mut self, shell: &mut Shell, data: &ScopeData) {
        let Some(pod) = self.selected(data) else {
            shell.set_error("No pod is selected");
            return;
        };
        let Some(object) = owner_object(pod).filter(|o| ROLLABLE.contains(&o.kind.as_str())) else {
            shell.set_error(format!(
                "{} is not under a deployment, statefulset or daemonset; nothing to roll",
                pod.key.name
            ));
            return;
        };
        self.modal = Some(Modal::Confirm {
            title: "Rollout restart".to_owned(),
            body: vec![
                format!("Restart the rollout of {}?", object.slash()),
                String::new(),
                "Every pod of it is replaced, one at a time.".to_owned(),
            ],
            verb: Verb::Rollout(object),
        });
    }

    /// `=`: the scale modal on the owner, with the current count filled in
    /// once it is known. The request reads it when it is not on file.
    pub fn scale_prompt(
        &mut self,
        shell: &mut Shell,
        scope: usize,
        data: &ScopeData,
    ) -> Option<Request> {
        let Some(pod) = self.selected(data) else {
            shell.set_error("No pod is selected");
            return None;
        };
        let Some(object) = owner_object(pod).filter(|o| SCALABLE.contains(&o.kind.as_str())) else {
            shell.set_error(format!(
                "{} is not under a deployment, statefulset or replicaset; nothing to scale",
                pod.key.name
            ));
            return None;
        };
        let current = self
            .owners
            .get(&object)
            .and_then(|held| held.as_ref().ok().copied());
        let mut input =
            TextInput::new(current.map_or_else(String::new, |held| held.desired.to_string()));
        input.move_end();
        self.modal = Some(Modal::Scale {
            object: object.clone(),
            input,
            current,
        });
        // Not on file: ask, and the answer fills the box when it lands.
        if current.is_some() || self.owner_pending.as_ref() == Some(&object) {
            return None;
        }
        self.owner_pending = Some(object.clone());
        Some(Request::Owner { scope, object })
    }

    /// A key while a modal is open. Answers the request when the modal was
    /// answered yes; closes it on `Esc` and, for a confirmation, on any key
    /// that is not its own.
    pub fn modal_key(
        &mut self,
        shell: &mut Shell,
        scope: usize,
        key: crossterm::event::KeyEvent,
    ) -> Option<Request> {
        use crossterm::event::KeyCode;
        match &mut self.modal {
            None => None,
            Some(Modal::Confirm { verb, .. }) => {
                let yes = matches!(
                    (&*verb, key.code),
                    (_, KeyCode::Enter)
                        | (Verb::Restart(_), KeyCode::Char('x'))
                        | (Verb::Rollout(_), KeyCode::Char('X'))
                );
                if yes {
                    self.confirm(shell, scope)
                } else {
                    self.dismiss();
                    None
                }
            }
            Some(Modal::Scale { input, .. }) => match key.code {
                KeyCode::Enter => self.confirm(shell, scope),
                KeyCode::Esc => {
                    self.dismiss();
                    None
                }
                _ => {
                    input.handle_key(key);
                    None
                }
            },
        }
    }

    /// The modal's yes, however it was given: the one place a change is
    /// sent from.
    pub fn confirm(&mut self, shell: &mut Shell, scope: usize) -> Option<Request> {
        match self.modal.take()? {
            Modal::Confirm { verb, .. } => Some(match verb {
                Verb::Restart(key) => {
                    shell.set_status(format!("Restarting {}\u{2026}", key.name));
                    Request::Delete { scope, key }
                }
                Verb::Rollout(object) => {
                    shell.set_status(format!(
                        "Restarting the rollout of {}\u{2026}",
                        object.slash()
                    ));
                    Request::RolloutRestart { scope, object }
                }
            }),
            Modal::Scale {
                object,
                input,
                current,
            } => match input.text().trim().parse::<u32>() {
                Ok(replicas) => {
                    shell.set_status(format!("Scaling {} to {replicas}\u{2026}", object.slash()));
                    Some(Request::Scale {
                        scope,
                        object,
                        replicas,
                    })
                }
                Err(_) => {
                    shell.set_error("Replicas must be a whole number");
                    self.modal = Some(Modal::Scale {
                        object,
                        input,
                        current,
                    });
                    None
                }
            },
        }
    }

    pub fn dismiss(&mut self) {
        self.modal = None;
    }

    /// What the delete said. A refusal is the user's to see; a delete that
    /// went through is news, because the pod it names is on its way out and
    /// another on its way in.
    pub fn deleted(
        &mut self,
        shell: &mut Shell,
        data: &ScopeData,
        key: &PodKey,
        error: Option<String>,
    ) {
        match error {
            Some(message) => {
                shell.set_error(format!("Could not restart {}: {message}", key.name));
            }
            None => {
                let owner = data
                    .pods
                    .iter()
                    .find(|pod| pod.key == *key)
                    .and_then(|pod| pod.owner.as_ref())
                    .map(|(kind, name)| format!("; {kind} {name} is putting a new one up"))
                    .unwrap_or_default();
                shell.set_status(format!("Deleted {}{owner}", key.name));
            }
        }
    }

    /// What a rollout restart or a scale said.
    pub fn acted(
        &mut self,
        shell: &mut Shell,
        verb: &str,
        object: &ObjectRef,
        error: Option<String>,
    ) {
        match error {
            Some(message) => {
                shell.set_error(format!("Could not {verb} {}: {message}", object.slash()));
            }
            None => shell.set_status(format!("{} {verb} sent", object.slash())),
        }
    }

    /// What `b` runs a shell in: the pod under the cursor, on the container
    /// the log follows when it follows one.
    #[must_use]
    pub fn bash_target(&self, tab: &Tab, data: &ScopeData) -> Option<AppAction> {
        let pod = self.selected(data)?;
        Some(AppAction::Exec {
            context: tab.scope.context.clone(),
            namespace: pod.key.namespace.clone(),
            pod: pod.key.name.clone(),
            container: self
                .log_target
                .as_ref()
                .filter(|held| held.key == pod.key)
                .and_then(|held| held.container.clone()),
        })
    }

    /// The `kubectl` line that does by hand what the pane shows: what `Y`
    /// copies.
    #[must_use]
    pub fn kubectl_line(&self, tab: &Tab, data: &ScopeData) -> Option<String> {
        let pod = self.selected(data)?;
        let prefix = format!(
            "kubectl --context {} -n {}",
            tab.scope.context, pod.key.namespace
        );
        Some(match (self.pane_open, self.pane) {
            (true, PaneText::Describe) => format!("{prefix} describe pod {}", pod.key.name),
            (true, PaneText::Yaml) => format!("{prefix} get pod {} -o yaml", pod.key.name),
            _ => {
                let mut line = format!("{prefix} logs -f {}", pod.key.name);
                if let Some(container) = self
                    .log_target
                    .as_ref()
                    .filter(|held| held.key == pod.key)
                    .and_then(|held| held.container.as_deref())
                {
                    line.push_str(&format!(" -c {container}"));
                }
                if self.previous {
                    line.push_str(" -p");
                }
                line
            }
        })
    }

    // ── Keys ───────────────────────────────────────────────────────────

    /// One key the shell did not take: movement and sorting in the table;
    /// scrolling in whichever pane has the details focus.
    pub fn handle_key(&mut self, shell: &mut Shell, key: crossterm::event::KeyEvent) -> AppAction {
        use crossterm::event::KeyCode;
        let count = self.visible.len();
        if shell.focus == Focus::Details {
            if self.pane_open {
                self.pane_key(key.code);
            } else {
                match key.code {
                    KeyCode::Char('j') | KeyCode::Down => {
                        self.details_scroll.scroll_by(1);
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        self.details_scroll.scroll_by(-1);
                    }
                    KeyCode::PageDown => {
                        self.details_scroll
                            .scroll_by(i32::try_from(self.details_scroll.page_step()).unwrap_or(1));
                    }
                    KeyCode::PageUp => {
                        self.details_scroll.scroll_by(
                            -i32::try_from(self.details_scroll.page_step()).unwrap_or(1),
                        );
                    }
                    _ => {}
                }
            }
            return AppAction::None;
        }
        let before = self.cursor.index;
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.cursor.move_by(1, count),
            KeyCode::Char('k') | KeyCode::Up => self.cursor.move_by(-1, count),
            KeyCode::PageDown => self.cursor.page(1, count),
            KeyCode::PageUp => self.cursor.page(-1, count),
            KeyCode::Home => self.cursor.focus(0),
            KeyCode::End => self.cursor.move_by(isize::MAX, count),
            KeyCode::Char('S') => self.next_sort(),
            _ => {}
        }
        if self.cursor.index != before {
            self.details_scroll.scroll_to(0);
        }
        AppAction::None
    }

    /// The text pane's keys: scrolling, and following again with `End`.
    /// Scrolling up leaves follow mode; scrolling down to the tail resumes
    /// it.
    fn pane_key(&mut self, code: crossterm::event::KeyCode) {
        use crossterm::event::KeyCode;
        let page = i32::try_from(self.pane_scroll.page_step()).unwrap_or(1);
        match code {
            KeyCode::Char('j') | KeyCode::Down => self.scroll_pane(1),
            KeyCode::Char('k') | KeyCode::Up => self.scroll_pane(-1),
            KeyCode::PageDown => self.scroll_pane(page),
            KeyCode::PageUp => self.scroll_pane(-page),
            KeyCode::Home => self.scroll_pane(i32::MIN / 2),
            KeyCode::End => {
                self.pane_scroll.scroll_to(usize::MAX / 2);
                self.log_follow = true;
            }
            _ => {}
        }
    }

    /// Scrolls the text pane by hand, wherever the scroll came from, and
    /// keeps the follow flag honest: off when the tail goes out of view, on
    /// when it comes back.
    pub fn scroll_pane(&mut self, delta: i32) {
        self.pane_scroll.scroll_by(delta);
        self.log_follow = self.pane_scroll.offset >= self.pane_scroll.max_offset();
    }

    /// A click on a row moves the cursor there; on a header, sorts by it.
    pub fn handle_click(&mut self, _shell: &mut Shell, target: Target) -> AppAction {
        match target {
            Target::Row(index) => {
                let before = self.cursor.index;
                self.cursor
                    .focus(index.min(self.visible.len().saturating_sub(1)));
                if self.cursor.index != before {
                    self.details_scroll.scroll_to(0);
                }
            }
            Target::Header(column) => self.sort_by(column),
            _ => {}
        }
        AppAction::None
    }

    pub fn handle_wheel(&mut self, _shell: &mut Shell, target: Option<Target>, delta: i32) {
        match target {
            Some(Target::Details) => {
                self.details_scroll.scroll_by(delta);
                return;
            }
            Some(Target::TextPane) => {
                self.scroll_pane(delta);
                return;
            }
            _ => {}
        }
        let before = self.cursor.index;
        let count = self.visible.len();
        self.cursor
            .scroll
            .set_viewport(self.cursor.scroll.viewport, count);
        self.cursor.scroll.scroll_by(delta);
        // The cursor follows the viewport rather than being left behind it,
        // so what a key acts on is always something on screen.
        let last = (self.cursor.scroll.offset + self.cursor.scroll.viewport.saturating_sub(1))
            .min(count.saturating_sub(1));
        let first = self.cursor.scroll.offset.min(last);
        self.cursor.index = self.cursor.index.clamp(first, last);
        if self.cursor.index != before {
            self.details_scroll.scroll_to(0);
        }
    }

    #[must_use]
    pub fn footer_hint(&self, shell: &Shell) -> String {
        match shell.focus {
            Focus::Search => {
                "Esc/Enter keep the filter  Esc again clears it  Ctrl-U empties the box".to_owned()
            }
            Focus::PaneSearch => "Esc/Enter keep the filter  Ctrl-U empties it".to_owned(),
            Focus::Details if self.pane_open => {
                "j/k scroll  End follow  / filter  z zoom  P previous  C container  Tab table  Esc close"
                    .to_owned()
            }
            _ => "↑↓/jk move  Enter logs  b bash  x restart  = scale  d describe  / search  ? help"
                .to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kube::tests::{crashing, key, pod};

    fn data() -> ScopeData {
        let mut old = pod("qa", "dev", "billing-worker-1a2b3c-old01", "Completed");
        old.created = crate::timestamp::Timestamp::parse("2026-01-01T00:00:00Z");
        old.owner = Some(("Job".to_owned(), "billing-worker".to_owned()));
        old.ready = (0, 1);
        ScopeData {
            pods: vec![
                pod("qa", "dev", "orders-api-7d9f5b-k9x2p", "Running"),
                crashing("qa", "dev", "orders-api-7d9f5b-abc12"),
                old,
            ],
            ..ScopeData::default()
        }
    }

    fn names(screen: &ScopeScreen, data: &ScopeData) -> Vec<String> {
        screen
            .visible()
            .iter()
            .map(|at| data.pods[*at].key.name.clone())
            .collect()
    }

    fn tab() -> Tab {
        crate::config::parse(crate::config::tests::TWO_CLUSTERS)
            .unwrap()
            .tabs()
            .remove(0)
    }

    #[test]
    fn the_table_opens_by_name_and_a_query_narrows_it_by_word_and_by_field() {
        let data = data();
        let mut screen = ScopeScreen::default();
        screen.refilter(&data);
        assert_eq!(
            names(&screen, &data),
            [
                "billing-worker-1a2b3c-old01",
                "orders-api-7d9f5b-abc12",
                "orders-api-7d9f5b-k9x2p"
            ]
        );
        assert_eq!(ScopeScreen::badge(&data).as_deref(), Some("\u{2717} 1"));

        screen.input.set_text("status:crash");
        screen.refilter(&data);
        assert_eq!(names(&screen, &data), ["orders-api-7d9f5b-abc12"]);

        screen.input.set_text("orders-api:1.2.3");
        screen.refilter(&data);
        assert_eq!(
            names(&screen, &data).len(),
            3,
            "an image reference is a word and matches every pod running the image"
        );

        screen.input.set_text("owner:billing");
        screen.refilter(&data);
        assert_eq!(names(&screen, &data), ["billing-worker-1a2b3c-old01"]);

        screen.input.set_text("app:orders-api k9x");
        screen.refilter(&data);
        assert_eq!(names(&screen, &data), ["orders-api-7d9f5b-k9x2p"]);

        screen.input.set_text("nothing-like-this");
        screen.refilter(&data);
        assert!(names(&screen, &data).is_empty());
        assert_eq!(screen.status(&data), "0/3 · Name ↑");
    }

    #[test]
    fn a_header_click_sorts_by_the_column_and_age_puts_the_newest_first() {
        let data = data();
        let mut screen = ScopeScreen::default();
        screen.sort_by(ColumnId::Restarts);
        screen.refilter(&data);
        assert_eq!(names(&screen, &data)[0], "billing-worker-1a2b3c-old01");
        screen.sort_by(ColumnId::Restarts);
        screen.refilter(&data);
        assert_eq!(
            names(&screen, &data)[0],
            "orders-api-7d9f5b-abc12",
            "the same header again turns it round"
        );
        screen.sort_by(ColumnId::Restarts);
        assert_eq!(
            screen.sort,
            ColumnId::Name,
            "and a third time is the default"
        );

        screen.sort_by(ColumnId::Age);
        screen.refilter(&data);
        assert_eq!(
            names(&screen, &data).last().map(String::as_str),
            Some("billing-worker-1a2b3c-old01"),
            "the oldest last"
        );
        screen.sort_by(ColumnId::Ready);
        screen.refilter(&data);
        assert_eq!(
            names(&screen, &data)[2],
            "orders-api-7d9f5b-k9x2p",
            "1/1 after the 0/1s: {:?}",
            names(&screen, &data)
        );
    }

    #[test]
    fn a_re_read_leaves_the_cursor_on_the_pod_it_was_on_wherever_it_now_sorts() {
        let mut data = data();
        let mut screen = ScopeScreen::default();
        screen.refilter(&data);
        screen.cursor.focus(2);
        let was = screen.cursor_identity(&data);
        assert_eq!(
            was.as_ref().map(|key| key.name.as_str()),
            Some("orders-api-7d9f5b-k9x2p")
        );

        data.pods
            .insert(0, pod("qa", "dev", "orders-api-7d9f5b-aaa01", "Running"));
        data.pods.remove(3);
        screen.invalidate();
        screen.keep_cursor(&data, was);
        assert_eq!(
            screen.selected(&data).map(|pod| pod.key.name.as_str()),
            Some("orders-api-7d9f5b-k9x2p")
        );
        assert_eq!(screen.cursor.index, 2);

        // A read that takes the pod away pulls the cursor back onto the list.
        let was = screen.cursor_identity(&data);
        data.pods.clear();
        screen.invalidate();
        screen.keep_cursor(&data, was);
        assert_eq!(screen.cursor.index, 0);
        assert!(screen.selected(&data).is_none());
    }

    #[test]
    fn the_log_pane_follows_the_pod_under_the_cursor_once_it_is_open_and_nothing_before() {
        let data = data();
        let mut screen = ScopeScreen::default();
        screen.refilter(&data);
        assert_eq!(
            screen.log_target(0, &data),
            None,
            "closed: nothing followed"
        );

        assert!(screen.toggle_log());
        let target = screen
            .log_target(0, &data)
            .expect("the pod under the cursor");
        assert_eq!(target.key.name, "billing-worker-1a2b3c-old01");
        assert_eq!(target.scope, 0);
        assert_eq!(target.container, None);
        screen.begin_follow(Some(target.clone()));

        screen.append_log(&target, vec!["starting".to_owned()], false);
        screen.append_log(&target, vec!["listening".to_owned()], false);
        assert_eq!(screen.log_lines(), ["starting", "listening"]);
        assert!(screen.log_following());
        assert!(!screen.log_ended());
        // Another stream's lines, still in flight when the pane moved on.
        let stale = LogFollow {
            key: key("prod", "prod", "other"),
            ..target.clone()
        };
        screen.append_log(&stale, vec!["not mine".to_owned()], false);
        assert_eq!(screen.log_lines(), ["starting", "listening"]);
        screen.append_log(&target, Vec::new(), true);
        assert!(screen.log_ended(), "the stream said it was over");

        // The cursor moves: a different target, and the lines were the last
        // pod's.
        screen.cursor.focus(1);
        let next = screen.log_target(0, &data).unwrap();
        assert_ne!(next.key, target.key);
        screen.begin_follow(Some(next));
        assert!(screen.log_lines().is_empty());
        assert!(!screen.log_ended());

        assert!(!screen.toggle_log(), "again closes it");
        assert_eq!(screen.log_target(0, &data), None);
    }

    #[test]
    fn a_log_past_the_cap_keeps_the_tail_and_says_how_much_it_dropped() {
        let data = data();
        let mut screen = ScopeScreen::default();
        screen.refilter(&data);
        screen.toggle_log();
        let target = screen.log_target(0, &data).unwrap();
        screen.begin_follow(Some(target.clone()));
        let lines: Vec<String> = (1..=LOG_LINE_CAP + 10)
            .map(|line| format!("line {line}"))
            .collect();
        screen.append_log(&target, lines, false);
        let held = screen.log_lines();
        assert_eq!(held.len(), LOG_LINE_CAP, "the cap holds");
        assert!(held[0].contains("earlier lines skipped"), "{}", held[0]);
        assert_eq!(held.last().map(String::as_str), Some("line 20010"));
    }

    #[test]
    fn c_moves_to_the_next_container_of_this_pod_only_and_p_asks_for_the_last_run() {
        let mut data = data();
        data.pods[0].containers.push(crate::kube::Container {
            name: "istio-proxy".to_owned(),
            image: "docker.io/istio/proxyv2:1.20".to_owned(),
            ready: true,
            restarts: 0,
            state: "Running".to_owned(),
            last_termination: None,
        });
        let mut screen = ScopeScreen::default();
        let mut shell = Shell::default();
        screen.refilter(&data);
        // The sidecar pod sorts last.
        screen.cursor.focus(2);
        screen.next_container(&mut shell, &data);
        assert!(screen.pane_open, "C opens the log");
        let target = screen.log_target(0, &data).unwrap();
        assert_eq!(target.container.as_deref(), Some("istio-proxy"));
        screen.begin_follow(Some(target));
        screen.next_container(&mut shell, &data);
        assert_eq!(
            screen.log_target(0, &data).unwrap().container.as_deref(),
            Some("api"),
            "round to the first again"
        );
        assert_eq!(
            screen.kubectl_line(&tab(), &data).as_deref(),
            Some("kubectl --context aks-qa -n dev logs -f orders-api-7d9f5b-k9x2p -c istio-proxy"),
            "the line copies what the worker was last told, not the choice in flight"
        );

        // Another pod: the choice does not carry over.
        screen.cursor.focus(1);
        let next = screen.log_target(0, &data).unwrap();
        assert_eq!(next.container, None);
        screen.begin_follow(Some(next));
        screen.next_container(&mut shell, &data);
        assert_eq!(
            shell.notification().map(|(said, _)| said),
            Some("orders-api-7d9f5b-abc12 has one container")
        );

        screen.toggle_previous(&mut shell);
        assert!(screen.log_target(0, &data).unwrap().previous);
        assert!(screen.previous());
        assert!(
            screen
                .kubectl_line(&tab(), &data)
                .unwrap()
                .ends_with("logs -f orders-api-7d9f5b-abc12 -p")
        );
    }

    #[test]
    fn d_and_v_fetch_a_text_once_per_object_and_the_pane_shows_it_for_that_object() {
        let data = data();
        let mut screen = ScopeScreen::default();
        screen.refilter(&data);
        let object = screen.pane_object(&data).unwrap();
        assert_eq!(object.slash(), "pod/billing-worker-1a2b3c-old01");

        let request = screen.show_text(0, TextKind::Describe, &data);
        assert_eq!(
            request,
            Some(Request::Describe {
                scope: 0,
                object: object.clone()
            })
        );
        assert!(screen.pane_open);
        assert_eq!(screen.pane, PaneText::Describe);
        assert!(screen.text_pending(TextKind::Describe, &object));
        assert_eq!(screen.log_target(0, &data), None, "nothing is followed");
        assert_eq!(
            screen.show_text(0, TextKind::Describe, &data),
            Some(Request::Describe {
                scope: 0,
                object: object.clone()
            }),
            "asked again while it is out: the worker answers both, harmlessly"
        );

        screen.set_text(
            TextKind::Describe,
            object.clone(),
            Ok(vec!["Name: x".to_owned()]),
        );
        assert!(!screen.text_pending(TextKind::Describe, &object));
        assert_eq!(
            screen.text(TextKind::Describe, &object),
            Some(&Ok(vec!["Name: x".to_owned()]))
        );
        assert_eq!(
            screen.show_text(0, TextKind::Describe, &data),
            None,
            "already on file"
        );
        assert_eq!(
            screen.show_text(0, TextKind::Yaml, &data),
            Some(Request::Yaml { scope: 0, object }),
        );
        assert_eq!(screen.pane, PaneText::Yaml);
        assert_eq!(
            screen.kubectl_line(&tab(), &data).as_deref(),
            Some("kubectl --context aks-qa -n dev get pod billing-worker-1a2b3c-old01 -o yaml")
        );
        screen.on_refresh();
        assert!(
            screen
                .text(TextKind::Describe, &screen.pane_object(&data).unwrap())
                .is_none()
        );
    }

    #[test]
    fn scrolling_up_leaves_follow_mode_and_coming_back_to_the_tail_resumes_it() {
        let data = data();
        let mut screen = ScopeScreen::default();
        let mut shell = Shell::default();
        screen.refilter(&data);
        screen.toggle_log();
        let target = screen.log_target(0, &data).unwrap();
        screen.begin_follow(Some(target.clone()));
        screen.append_log(
            &target,
            (0..50).map(|line| format!("line {line}")).collect(),
            false,
        );
        screen.pane_scroll.set_viewport(10, 50);
        screen.pane_scroll.scroll_to(40);
        shell.focus = Focus::Details;
        screen.handle_key(
            &mut shell,
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('k'),
                crossterm::event::KeyModifiers::NONE,
            ),
        );
        assert!(!screen.log_following());
        assert_eq!(screen.pane_scroll.offset, 39);
        screen.handle_key(
            &mut shell,
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('j'),
                crossterm::event::KeyModifiers::NONE,
            ),
        );
        assert!(screen.log_following(), "back at the tail");
        screen.scroll_pane(-20);
        assert!(!screen.log_following());
        screen.handle_key(
            &mut shell,
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::End,
                crossterm::event::KeyModifiers::NONE,
            ),
        );
        assert!(screen.log_following());
        screen.toggle_zoom();
        assert!(screen.pane_zoom);
        screen.close_pane();
        assert!(!screen.pane_zoom && !screen.pane_open);
    }
}
