//! One tab: the list over one namespace of one cluster, and the state that is
//! this tab's alone — its cursor, its search, its sort.

use std::cmp::Ordering;

use super::cursor::{ListCursor, ScrollState};
use super::screen::{AppAction, Target};
use super::shell::{Focus, Shell};
use crate::columns::{ColumnId, POD_COLUMNS, TableLayout};
use crate::filter::{self, Query};
use crate::kube::{Pod, PodKey};
use crate::store::ScopeData;
use crate::text_input::TextInput;

/// The `key:` filters the pods list knows. Everything else typed is a word.
pub const SCHEMA: &[&str] = &["name", "ns", "status", "owner", "app", "node"];

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
        }
    }
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

    /// One key the shell did not take: movement and sorting.
    pub fn handle_key(&mut self, shell: &mut Shell, key: crossterm::event::KeyEvent) -> AppAction {
        use crossterm::event::KeyCode;
        let count = self.visible.len();
        // `j` and `k` scroll the details pane when that is what has focus.
        if shell.focus == Focus::Details {
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
                    self.details_scroll
                        .scroll_by(-i32::try_from(self.details_scroll.page_step()).unwrap_or(1));
                }
                _ => {}
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
        if target == Some(Target::Details) {
            self.details_scroll.scroll_by(delta);
            return;
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
        if shell.focus == Focus::Search {
            return "Esc/Enter keep the filter  Esc again clears it  Ctrl-U empties the box"
                .to_owned();
        }
        "↑↓/jk move  [ ] tabs  / search  S sort  r refresh  ? help".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kube::tests::{crashing, pod};

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
}
