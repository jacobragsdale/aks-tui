//! The application: which tab is open, what the keys do before a screen sees
//! them, and how a frame is put together.

pub mod cursor;
pub mod keys;
pub mod list;
pub mod scope;
pub mod screen;
pub mod shell;

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::widgets::{Paragraph, Wrap};

use scope::ScopeScreen;
use screen::{AppAction, Button, Target};
use shell::{Focus, Shell};

use crate::columns::{ColumnId, TableLayout};
use crate::config::Tab;
use crate::kube::{Event, Kind, LogFollow, Request, TextKind};
use crate::session::{Session, SessionColumn};
use crate::store::{Applied, ScopeData, Store};
use crate::text_input::TextInput;
use crate::timestamp::Timestamp;
use crate::ui;
use crate::ui::widgets::{TabLabel, spinner_frame};

/// How long the cursor has to sit on a pod before its owner is asked about.
/// Holding `j` down across forty pods must not be forty requests.
pub const REST: Duration = Duration::from_millis(150);

pub struct App {
    pub shell: Shell,
    /// The tabs, in `config.toml`'s order. One screen and one store slot
    /// each.
    pub tabs: Vec<Tab>,
    pub tab: usize,
    pub screens: Vec<ScopeScreen>,
    pub store: Store,
    /// Whether a read has landed since the cache was last written.
    pub cache_dirty: bool,
    /// What the worker was last told to follow, so the tick only speaks
    /// when that changes.
    following: Option<LogFollow>,
    /// Where the cursor is and when it got there, for the rest interval that
    /// gates the owner read.
    rested: Option<(usize, usize, Instant)>,
}

impl App {
    #[must_use]
    pub fn new(tabs: Vec<Tab>, store: Store) -> Self {
        let screens = tabs
            .iter()
            .map(|tab| ScopeScreen::new(tab.scope.namespace.is_none()))
            .collect();
        Self {
            shell: Shell::default(),
            tabs,
            tab: 0,
            screens,
            store,
            cache_dirty: false,
            following: None,
            rested: None,
        }
    }

    /// The open tab, its screen, its data and the shell, borrowed apart.
    fn parts(&mut self) -> Option<(&Tab, &mut ScopeScreen, &ScopeData, &mut Shell)> {
        let tab = self.tabs.get(self.tab)?;
        let screen = self.screens.get_mut(self.tab)?;
        let data = self.store.scopes.get(self.tab)?;
        Some((tab, screen, data, &mut self.shell))
    }

    /// The kind the open tab shows.
    #[must_use]
    pub fn kind(&self) -> Kind {
        self.screens
            .get(self.tab)
            .map_or(Kind::Pods, |screen| screen.kind)
    }

    /// One key. The global keys are matched here first; everything else goes
    /// to the tab, except while a box has focus, which takes everything but
    /// `Esc`, `Enter` and `Tab`.
    pub fn handle_key(&mut self, key: KeyEvent) -> AppAction {
        if key.kind != KeyEventKind::Press {
            return AppAction::None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return AppAction::Quit;
        }
        if self.shell.help_open {
            // The help takes every key: the one thing it can do is close.
            self.shell.help_open = false;
            return AppAction::None;
        }
        if self
            .screens
            .get(self.tab)
            .is_some_and(|screen| screen.modal.is_some())
        {
            // The modal takes every key: its own answer it, any other closes
            // it and is not otherwise acted on.
            let tab = self.tab;
            let Some(screen) = self.screens.get_mut(tab) else {
                return AppAction::None;
            };
            return screen
                .modal_key(&mut self.shell, tab, key)
                .map_or(AppAction::None, AppAction::Send);
        }
        if let Some(at) = self.shell.kind_menu {
            // The menu takes the keys that walk it; any other closes it.
            let count = Kind::ALL.len();
            return match key.code {
                KeyCode::Char('j') | KeyCode::Down => {
                    self.shell.kind_menu = Some((at + 1) % count);
                    AppAction::None
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.shell.kind_menu = Some((at + count - 1) % count);
                    AppAction::None
                }
                KeyCode::Enter => {
                    self.shell.kind_menu = None;
                    self.set_kind(Kind::ALL[at])
                }
                _ => {
                    self.shell.kind_menu = None;
                    AppAction::None
                }
            };
        }
        if self.shell.focus == Focus::Search {
            return self.key_in_search(key);
        }
        if self.shell.focus == Focus::PaneSearch {
            return self.key_in_pane_search(key);
        }
        let kind = self.kind();
        let pane_open = self
            .screens
            .get(self.tab)
            .is_some_and(|screen| screen.pane_open);
        match key.code {
            KeyCode::Char(number @ '1'..='9') => {
                let index = usize::from(u8::try_from(number).unwrap_or(b'1') - b'1');
                self.switch_to(index)
            }
            KeyCode::Char('[') | KeyCode::Left => self.step_tab(-1),
            KeyCode::Char(']') | KeyCode::Right => self.step_tab(1),
            KeyCode::Char('?') => {
                self.shell.help_open = true;
                AppAction::None
            }
            KeyCode::Char('q') => AppAction::Quit,
            // Esc takes things off in the order they were put on: the pane's
            // filter, the query, then the pane itself.
            KeyCode::Esc => {
                self.escape();
                AppAction::None
            }
            KeyCode::Char('r') => self.refresh(),
            KeyCode::Char('/') if pane_open && self.shell.focus == Focus::Details => {
                self.shell.focus = Focus::PaneSearch;
                AppAction::None
            }
            KeyCode::Char('/') => {
                self.shell.focus = Focus::Search;
                AppAction::None
            }
            KeyCode::Tab => {
                self.shell.toggle_focus();
                AppAction::None
            }
            // The kinds. `e` on a pod is that pod's events.
            KeyCode::Char('p') => self.set_kind(Kind::Pods),
            KeyCode::Char('e') if kind == Kind::Pods => {
                let tab = self.tab;
                if let Some((_, screen, data, _)) = self.parts() {
                    screen.events_for_selected_pod(data);
                }
                self.shell.focus = Focus::Table;
                AppAction::Send(Request::Showing(tab, Kind::Events))
            }
            KeyCode::Char('e') => self.set_kind(Kind::Events),
            KeyCode::Char('m') => self.set_kind(Kind::ConfigMaps),
            KeyCode::Char('s') => self.set_kind(Kind::Secrets),
            KeyCode::Enter => match kind {
                Kind::Pods => self.button(Button::Logs),
                Kind::Events => self.button(Button::Pod),
                Kind::ConfigMaps | Kind::Secrets => self.button(Button::Value),
            },
            KeyCode::Char('l') if kind == Kind::Pods => self.button(Button::Logs),
            KeyCode::Char('d') => self.button(Button::Describe),
            KeyCode::Char('v') => match kind {
                Kind::Pods | Kind::Events => self.button(Button::Yaml),
                Kind::ConfigMaps | Kind::Secrets => self.button(Button::Value),
            },
            KeyCode::Char('b') if kind == Kind::Pods => self.button(Button::Bash),
            KeyCode::Char('x') if kind == Kind::Pods => self.button(Button::Restart),
            KeyCode::Char('X') if kind == Kind::Pods => {
                if let Some((_, screen, data, shell)) = self.parts() {
                    screen.rollout_prompt(shell, data);
                }
                AppAction::None
            }
            KeyCode::Char('=') if kind == Kind::Pods => self.button(Button::Scale),
            KeyCode::Char('b' | 'x' | 'X' | '=' | 'l') => {
                self.shell.set_status("That is a pod's key: p for the pods");
                AppAction::None
            }
            KeyCode::Char('P') if kind == Kind::Pods => {
                if let Some((_, screen, _, shell)) = self.parts() {
                    screen.toggle_previous(shell);
                }
                AppAction::None
            }
            KeyCode::Char('C') if kind == Kind::Pods => {
                if let Some((_, screen, data, shell)) = self.parts() {
                    screen.next_container(shell, data);
                }
                AppAction::None
            }
            KeyCode::Char('z') => {
                if let Some((_, screen, _, _)) = self.parts() {
                    screen.toggle_zoom();
                }
                AppAction::None
            }
            KeyCode::Char('y') => match kind {
                Kind::ConfigMaps | Kind::Secrets => self.button(Button::Copy),
                _ => self
                    .parts()
                    .and_then(|(_, screen, data, _)| screen.selected_name(data))
                    .map_or(AppAction::None, |name| AppAction::Copy {
                        label: format!("Copied {name}"),
                        text: name,
                    }),
            },
            KeyCode::Char('Y') => self
                .parts()
                .and_then(|(tab, screen, data, _)| screen.kubectl_line(tab, data))
                .map_or(AppAction::None, |line| AppAction::Copy {
                    label: format!("Copied `{line}`"),
                    text: line,
                }),
            _ => self.screen_key(key),
        }
    }

    /// While the box has focus every key is a character, except the three
    /// that leave it.
    fn key_in_search(&mut self, key: KeyEvent) -> AppAction {
        match key.code {
            KeyCode::Enter | KeyCode::Esc | KeyCode::Tab => {
                self.shell.focus = Focus::Table;
            }
            _ => {
                if let Some(input) = self.input() {
                    input.handle_key(key);
                }
            }
        }
        AppAction::None
    }

    /// The pane's filter takes every key but the three that leave it.
    fn key_in_pane_search(&mut self, key: KeyEvent) -> AppAction {
        match key.code {
            KeyCode::Enter | KeyCode::Esc | KeyCode::Tab => {
                self.shell.focus = Focus::Details;
            }
            _ => {
                if let Some(screen) = self.screens.get_mut(self.tab) {
                    screen.pane_filter.handle_key(key);
                    screen.scroll_pane(0);
                }
            }
        }
        AppAction::None
    }

    /// `Esc` out of the table: the pane's filter goes first, then the query,
    /// then the pane itself.
    fn escape(&mut self) {
        let Some(screen) = self.screens.get_mut(self.tab) else {
            return;
        };
        if screen.pane_open && !screen.pane_filter.is_empty() {
            screen.pane_filter.clear();
        } else if !screen.list().input.is_empty() {
            screen.list_mut().input.clear();
        } else if screen.pane_open {
            screen.close_pane();
            self.shell.focus = Focus::Table;
        }
    }

    /// One toolbar button, whether clicked or pressed as its key.
    fn button(&mut self, button: Button) -> AppAction {
        let tab = self.tab;
        let Some((tab_ref, screen, data, shell)) = self.parts() else {
            return AppAction::None;
        };
        match button {
            Button::Logs => {
                let open = screen.toggle_log();
                shell.focus = if open { Focus::Details } else { Focus::Table };
                AppAction::None
            }
            Button::Describe => {
                let request = screen.show_text(tab, TextKind::Describe, data);
                shell.focus = Focus::Details;
                request.map_or(AppAction::None, AppAction::Send)
            }
            Button::Yaml => {
                let request = screen.show_text(tab, TextKind::Yaml, data);
                shell.focus = Focus::Details;
                request.map_or(AppAction::None, AppAction::Send)
            }
            Button::Bash => screen.bash_target(tab_ref, data).unwrap_or_else(|| {
                shell.set_error("No pod is selected");
                AppAction::None
            }),
            Button::Restart => {
                screen.restart_prompt(shell, data);
                AppAction::None
            }
            Button::Scale => screen
                .scale_prompt(shell, tab, data)
                .map_or(AppAction::None, AppAction::Send),
            Button::Pod => {
                screen.jump_to_object(shell, data);
                shell.focus = Focus::Table;
                if screen.kind == Kind::Pods {
                    AppAction::Send(Request::Showing(tab, Kind::Pods))
                } else {
                    AppAction::None
                }
            }
            Button::Value => {
                let request = screen.show_value(tab, data);
                shell.focus = Focus::Details;
                request.map_or(AppAction::None, AppAction::Send)
            }
            Button::Copy => screen.copy_value(shell, tab, data),
        }
    }

    /// A paste, which bracketed paste hands over whole. It goes into
    /// whichever box has focus, and nowhere otherwise.
    pub fn handle_paste(&mut self, text: &str) -> AppAction {
        match self.shell.focus {
            Focus::Search => {
                if let Some(input) = self.input() {
                    input.paste(text);
                }
            }
            Focus::PaneSearch => {
                if let Some(screen) = self.screens.get_mut(self.tab) {
                    screen.pane_filter.paste(text);
                }
            }
            _ => {}
        }
        AppAction::None
    }

    /// The search box of whichever list is showing.
    fn input(&mut self) -> Option<&mut TextInput> {
        self.screens
            .get_mut(self.tab)
            .map(|screen| &mut screen.list_mut().input)
    }

    /// The `×` on the search row: the filter goes, and the table comes back
    /// whole.
    fn clear_query(&mut self) {
        if let Some(input) = self.input() {
            input.clear();
        }
    }

    fn screen_key(&mut self, key: KeyEvent) -> AppAction {
        let Some((_, screen, data, shell)) = self.parts() else {
            return AppAction::None;
        };
        screen.handle_key(shell, data, key)
    }

    /// `r`: this tab's scope, read again now, and what the pane said about
    /// its rows forgotten.
    fn refresh(&mut self) -> AppAction {
        let Some(tab) = self.tabs.get(self.tab) else {
            return AppAction::None;
        };
        self.shell
            .set_status(format!("Reading {}…", tab.scope.describe()));
        if let Some(screen) = self.screens.get_mut(self.tab) {
            screen.on_refresh();
        }
        AppAction::Send(Request::Refresh(self.tab))
    }

    /// Another tab: the worker is told, so it is read at once and kept
    /// fresh while it shows.
    fn switch_to(&mut self, tab: usize) -> AppAction {
        if self.tab == tab || tab >= self.tabs.len() {
            return AppAction::None;
        }
        self.tab = tab;
        self.shell.focus = Focus::Table;
        AppAction::Send(Request::Showing(tab, self.kind()))
    }

    /// `[` and `]`: the previous and the next tab, round the ends.
    fn step_tab(&mut self, by: isize) -> AppAction {
        let count = self.tabs.len();
        if count == 0 {
            return AppAction::None;
        }
        let next = (self.tab as isize + by).rem_euclid(count as isize) as usize;
        self.switch_to(next)
    }

    /// Another kind on this tab: the worker reads it at once and keeps it
    /// fresh while it shows.
    fn set_kind(&mut self, kind: Kind) -> AppAction {
        let tab = self.tab;
        let Some(screen) = self.screens.get_mut(tab) else {
            return AppAction::None;
        };
        if screen.kind == kind {
            return AppAction::None;
        }
        screen.set_kind(kind);
        self.shell.focus = Focus::Table;
        AppAction::Send(Request::Showing(tab, kind))
    }

    /// A click, resolved against what was drawn last frame. A click lands
    /// focus where it lands: on a row, the table; on the pane, the pane.
    pub fn handle_mouse(&mut self, event: MouseEvent) -> AppAction {
        let target = self.shell.hit(event.column, event.row).cloned();
        let tab = self.tab;
        if self
            .screens
            .get(tab)
            .is_some_and(|screen| screen.modal.is_some())
        {
            // The open modal takes the pointer: its yes answers it, its body
            // does nothing, anywhere else closes it. The wheel is ignored.
            if event.kind != MouseEventKind::Down(crossterm::event::MouseButton::Left) {
                return AppAction::None;
            }
            let Some(screen) = self.screens.get_mut(tab) else {
                return AppAction::None;
            };
            return match target {
                Some(Target::Confirm) => screen
                    .confirm(&mut self.shell, tab)
                    .map_or(AppAction::None, AppAction::Send),
                Some(Target::Modal) => AppAction::None,
                _ => {
                    screen.dismiss();
                    AppAction::None
                }
            };
        }
        if let MouseEventKind::Down(_) = event.kind
            && self.shell.kind_menu.take().is_some()
        {
            // The open menu takes the click: one of its lines chooses,
            // anywhere else closes it and does nothing more.
            if let Some(Target::KindOption(kind)) = target {
                return self.set_kind(kind);
            }
            return AppAction::None;
        }
        match event.kind {
            MouseEventKind::Down(_) => match target {
                Some(Target::Tab(tab)) => self.switch_to(tab),
                Some(Target::KindPill) => {
                    let kind = self.kind();
                    self.shell.kind_menu = Kind::ALL.iter().position(|held| *held == kind);
                    AppAction::None
                }
                Some(Target::KindOption(kind)) => self.set_kind(kind),
                Some(Target::Button(button)) => self.button(button),
                Some(Target::Help) => {
                    self.shell.help_open = !self.shell.help_open;
                    AppAction::None
                }
                Some(Target::SearchField) => {
                    self.shell.focus = Focus::Search;
                    AppAction::None
                }
                Some(Target::ClearSearch) => {
                    self.clear_query();
                    AppAction::None
                }
                Some(Target::Details | Target::TextPane) => {
                    self.shell.focus = Focus::Details;
                    AppAction::None
                }
                Some(target) => {
                    self.shell.focus = Focus::Table;
                    let Some(screen) = self.screens.get_mut(tab) else {
                        return AppAction::None;
                    };
                    screen.handle_click(&mut self.shell, target)
                }
                None => AppAction::None,
            },
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let delta = if event.kind == MouseEventKind::ScrollUp {
                    -3
                } else {
                    3
                };
                if let Some(screen) = self.screens.get_mut(tab) {
                    screen.handle_wheel(&mut self.shell, target, delta);
                }
                AppAction::None
            }
            _ => AppAction::None,
        }
    }

    /// A worker event, and whatever it asks the run loop to do next. A read
    /// that landed re-sorts its list under a cursor that stays on its own
    /// row; a read that failed is said once in the status bar when it is the
    /// open tab's and the kind on screen; a secret's value goes to the one
    /// screen that asked, which shows it or copies it and keeps nothing
    /// either way.
    pub fn apply(&mut self, event: Event) -> AppAction {
        // The pane's and the modal's own events go to the tab that asked and
        // touch no rows.
        match event {
            Event::LogLines {
                target,
                lines,
                finished,
            } => {
                if let Some(screen) = self.screens.get_mut(target.scope) {
                    screen.append_log(&target, lines, finished);
                }
                return AppAction::None;
            }
            Event::Text {
                scope,
                kind,
                object,
                text,
            } => {
                if let Some(screen) = self.screens.get_mut(scope) {
                    screen.set_text(kind, object, text);
                }
                return AppAction::None;
            }
            Event::Deleted { scope, key, error } => {
                if let Some((screen, data)) = self
                    .screens
                    .get_mut(scope)
                    .zip(self.store.scopes.get(scope))
                {
                    screen.deleted(&mut self.shell, data, &key, error);
                }
                return AppAction::None;
            }
            Event::Acted {
                scope,
                verb,
                object,
                error,
            } => {
                if let Some(screen) = self.screens.get_mut(scope) {
                    screen.acted(&mut self.shell, verb, &object, error);
                }
                return AppAction::None;
            }
            Event::Owner {
                scope,
                object,
                replicas,
            } => {
                if let Some(screen) = self.screens.get_mut(scope) {
                    screen.set_owner(object, replicas);
                }
                return AppAction::None;
            }
            Event::SecretValue {
                scope,
                object,
                key,
                copy,
                value,
            } => {
                if let Some((screen, data)) = self
                    .screens
                    .get_mut(scope)
                    .zip(self.store.scopes.get(scope))
                {
                    return screen.on_secret_value(&mut self.shell, data, object, key, copy, value);
                }
                return AppAction::None;
            }
            _ => {}
        }
        // What the tab held before the event: the cursor's row, by identity,
        // and the message the last failure left. Both are read against the
        // rows as they were, which a read is about to replace.
        let landing = match &event {
            Event::Pods { scope, .. } => Some((*scope, Kind::Pods)),
            Event::Events { scope, .. } => Some((*scope, Kind::Events)),
            Event::ConfigMaps { scope, .. } => Some((*scope, Kind::ConfigMaps)),
            Event::Secrets { scope, .. } => Some((*scope, Kind::Secrets)),
            _ => None,
        };
        let (was_error, was_cursor) = landing
            .and_then(|(scope, kind)| {
                let data = self.store.scope(scope)?;
                let screen = self.screens.get(scope)?;
                Some((
                    data.listing(kind).error.cloned(),
                    screen.cursor_identity(kind, data),
                ))
            })
            .unwrap_or((None, None));
        match self.store.apply(event) {
            Applied::Rows(index, kind) => {
                if kind == Kind::Pods {
                    self.cache_dirty = true;
                }
                if let Some((screen, data)) = self
                    .screens
                    .get_mut(index)
                    .zip(self.store.scopes.get(index))
                {
                    screen.rows_changed(kind, data, was_cursor);
                }
            }
            Applied::Failed(index, kind) => {
                if index == self.tab
                    && self.kind() == kind
                    && let Some(tab) = self.tabs.get(index)
                    && let Some(message) = self.store.scopes[index].listing(kind).error.cloned()
                    && was_error.as_deref() != Some(message.as_str())
                {
                    self.shell.set_error(format!(
                        "{} {}: {message}",
                        tab.scope.describe(),
                        kind.noun()
                    ));
                }
            }
            Applied::Status | Applied::Nothing => {}
        }
        AppAction::None
    }

    /// One turn of the clock: whatever the pane should be following now, if
    /// that has changed since the worker was last told; the owner of a pod
    /// the cursor has settled on; and a revealed value that has run out.
    /// Called after every frame, once the rows the cursor counts over are
    /// settled.
    pub fn tick(&mut self, now: Instant) -> Vec<Request> {
        let tab = self.tab;
        let mut requests = Vec::new();
        if let Some(screen) = self.screens.get_mut(tab) {
            screen.tick_reveal(now);
        }
        let desired = self
            .screens
            .get(tab)
            .zip(self.store.scopes.get(tab))
            .and_then(|(screen, data)| screen.log_target(tab, data));
        if desired != self.following {
            self.following.clone_from(&desired);
            if let Some(screen) = self.screens.get_mut(tab) {
                screen.begin_follow(desired.clone());
            }
            requests.push(desired.map_or(Request::Unfollow, Request::Follow));
        }
        let here = (
            tab,
            self.screens
                .get(tab)
                .map_or(0, |screen| screen.list().cursor.index),
        );
        match self.rested {
            Some((t, c, since)) if (t, c) == here => {
                if now.saturating_duration_since(since) >= REST
                    && let Some((screen, data)) =
                        self.screens.get_mut(tab).zip(self.store.scopes.get(tab))
                    && let Some(request) = screen.owner_request(tab, data)
                {
                    requests.push(request);
                }
            }
            _ => self.rested = Some((here.0, here.1, now)),
        }
        requests
    }

    /// Whether the cursor has landed somewhere in the last [`REST`], so the
    /// loop comes back in time to ask about it.
    #[must_use]
    pub fn is_resting(&self) -> bool {
        self.rested
            .is_some_and(|(_, _, since)| since.elapsed() < REST)
    }

    /// How long the run loop may sleep: a tenth of a second while a read is
    /// in flight, so its answer is painted the moment it lands; a second
    /// while a value is counting down; the rest interval while the cursor has
    /// just landed somewhere; and whatever the caller wanted otherwise.
    #[must_use]
    pub fn poll_for(&self, settled: Duration) -> Duration {
        if self.store.reading() {
            return Duration::from_millis(100);
        }
        if self
            .screens
            .get(self.tab)
            .is_some_and(ScopeScreen::is_ticking)
        {
            return Duration::from_secs(1);
        }
        if self.is_resting() {
            return REST;
        }
        settled
    }

    // ── The session ────────────────────────────────────────────────────

    /// The layout as it stands, for the file: the tab, each tab's kind, and
    /// its pods table's sort and columns. Never the query, never the cursor,
    /// and nothing a cluster answered.
    #[must_use]
    pub fn session(&self) -> Session {
        let mut session = Session {
            tab: self.tabs.get(self.tab).map(|tab| tab.label.clone()),
            ..Session::default()
        };
        for (tab, screen) in self.tabs.iter().zip(&self.screens) {
            let held = session.tab(&tab.label);
            held.kind = Some(screen.kind.session_key().to_owned());
            let pods = screen.list_of(Kind::Pods);
            held.sort = Some(sort_of(pods.sort, pods.descending));
            held.columns = columns_of(&pods.layout);
        }
        session
    }

    /// Puts a session back, before the first frame. Anything this build does
    /// not recognise is left where it is.
    pub fn restore(&mut self, session: &Session) {
        if let Some(tab) = session
            .tab
            .as_deref()
            .and_then(|name| self.tabs.iter().position(|held| held.label == name))
        {
            self.tab = tab;
        }
        for (tab, screen) in self.tabs.iter().zip(&mut self.screens) {
            let Some(held) = session.tabs.get(&tab.label) else {
                continue;
            };
            if let Some(kind) = held.kind.as_deref().and_then(Kind::from_session_key) {
                screen.set_kind(kind);
            }
            let pods = screen.list_of_mut(Kind::Pods);
            if let Some((column, way)) = read_sort(held.sort.as_ref()) {
                pods.sort = column;
                pods.descending = way;
            }
            apply_columns(&mut pods.layout, &held.columns);
        }
    }

    /// What the status bar says on the left when nothing has just happened.
    #[must_use]
    pub fn footer_hint(&self) -> String {
        self.screens
            .get(self.tab)
            .map_or_else(String::new, |screen| screen.footer_hint(&self.shell))
    }

    /// What the status bar says on the right: what the open tab's kind is
    /// doing, or what is wrong with it, or what it holds and how old that is.
    fn store_state(&self, millis: u128) -> (String, Style) {
        let palette = ui::theme::theme();
        let Some((tab, data)) = self.tabs.get(self.tab).zip(self.store.scope(self.tab)) else {
            return (
                "no clusters configured".to_owned(),
                Style::default().fg(palette.error),
            );
        };
        let kind = self.kind();
        let listing = data.listing(kind);
        let scope = tab.scope.describe();
        let noun = kind.noun();
        if data.reading && listing.reads == 0 && listing.count == 0 {
            return (
                format!("{} reading {scope} {noun}…", spinner_frame(millis)),
                Style::default().fg(palette.info),
            );
        }
        if let Some(message) = listing.error {
            return (
                format!("! {scope} {noun}: {message}"),
                Style::default().fg(palette.error),
            );
        }
        let age = listing.read_at.map_or_else(
            || "never read".to_owned(),
            |read_at| match read_at.relative_age(Timestamp::now()).as_str() {
                "now" => "just now".to_owned(),
                age => format!("{age} ago"),
            },
        );
        let spinner = if data.reading {
            format!("{} ", spinner_frame(millis))
        } else {
            "● ".to_owned()
        };
        (
            format!("{spinner}{} {noun} · {age}", listing.count),
            Style::default().fg(palette.muted),
        )
    }

    /// One frame.
    pub fn render(&mut self, frame: &mut Frame, millis: u128) {
        let area = frame.area();
        if area.width < ui::MIN_WIDTH || area.height < ui::MIN_HEIGHT {
            ui::render_too_small(frame, area);
            return;
        }
        self.shell.begin_frame();
        let [tabs, body, status] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(3),
                Constraint::Length(1),
            ])
            .areas(area);

        let labels: Vec<TabLabel> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| TabLabel {
                label: tab.label.clone(),
                short: tab.scope.short_label().to_owned(),
                badge: self.store.scope(index).and_then(ScopeScreen::badge),
            })
            .collect();
        let kind = self.kind();
        ui::widgets::render_tab_bar(frame, &mut self.shell, tabs, self.tab, &labels, kind);
        self.render_body(frame, body);
        let hint = self.footer_hint();
        let (right, right_style) = self.store_state(millis);
        ui::widgets::render_status_bar(frame, &mut self.shell, status, &hint, &right, right_style);
        if let Some(highlighted) = self.shell.kind_menu
            && let Some(anchor) = self.shell.find(&Target::KindPill)
        {
            ui::widgets::render_kind_menu(frame, &mut self.shell, anchor, kind, highlighted);
        }
        if let Some(modal) = self
            .screens
            .get(self.tab)
            .and_then(|screen| screen.modal.as_ref())
        {
            ui::modal::render_modal(frame, &mut self.shell, modal, area);
        }
        if self.shell.help_open {
            let problems = self.store.problems(&self.tabs);
            ui::widgets::render_help(frame, &mut self.shell, area, &problems);
        }
    }

    fn render_body(&mut self, frame: &mut Frame, area: Rect) {
        let Some(((tab, screen), data)) = self
            .tabs
            .get(self.tab)
            .zip(self.screens.get_mut(self.tab))
            .zip(self.store.scopes.get(self.tab))
        else {
            frame.render_widget(
                Paragraph::new(
                    "No clusters configured.\n\nAdd a [[clusters]] table to \
                     ~/.config/aks-tui/config.toml naming a kubeconfig context and its \
                     namespaces, then start again.",
                )
                .alignment(Alignment::Center)
                .style(Style::default().fg(ui::theme::theme().muted))
                .wrap(Wrap { trim: true }),
                area,
            );
            return;
        };
        screen.refilter(data);
        ui::scope::render(frame, &mut self.shell, screen, tab, data, area);
    }
}

fn sort_of(column: ColumnId, descending: bool) -> (String, String) {
    (
        column.key().to_owned(),
        if descending { "desc" } else { "asc" }.to_owned(),
    )
}

/// A sort out of the file, if this build knows the column it names.
fn read_sort(sort: Option<&(String, String)>) -> Option<(ColumnId, bool)> {
    let (key, way) = sort?;
    Some((ColumnId::from_key(key)?, way.eq_ignore_ascii_case("desc")))
}

fn columns_of(layout: &TableLayout) -> Vec<SessionColumn> {
    layout
        .columns
        .iter()
        .map(|column| SessionColumn {
            key: column.id.key().to_owned(),
            width: Some(column.width),
            visible: Some(column.visible),
        })
        .collect()
}

/// Widths and visibility out of the file. A key this build does not know is
/// skipped; a column the file does not mention keeps its default.
fn apply_columns(layout: &mut TableLayout, held: &[SessionColumn]) {
    for stored in held {
        let Some(id) = ColumnId::from_key(&stored.key) else {
            continue;
        };
        let Some(column) = layout.columns.iter_mut().find(|column| column.id == id) else {
            continue;
        };
        if let Some(width) = stored.width {
            column.width = width;
        }
        if let Some(visible) = stored.visible {
            column.visible = visible;
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::config;
    use crate::kube::tests::{crashing, pod};
    use crate::kube::{ConfigMap, K8sEvent, ObjectRef, Secret, SecretMeta};

    pub(crate) fn two_clusters() -> App {
        let tabs = config::parse(config::tests::TWO_CLUSTERS).unwrap().tabs();
        let store = Store::new(tabs.len());
        App::new(tabs, store)
    }

    /// The two clusters with pods read into qa/dev and prod, and qa/dev's
    /// events, configmaps and secrets besides.
    pub(crate) fn stocked() -> App {
        let mut app = two_clusters();
        app.apply(Event::Pods {
            scope: 0,
            pods: Ok(vec![
                pod("qa", "dev", "orders-api-7d9f5b-abc12", "Running"),
                crashing("qa", "dev", "orders-api-7d9f5b-def34"),
            ]),
        });
        app.apply(Event::Pods {
            scope: 3,
            pods: Ok(vec![pod(
                "prod",
                "prod",
                "orders-api-9a1c2d-ghi56",
                "Running",
            )]),
        });
        app.apply(Event::Events {
            scope: 0,
            events: Ok(vec![
                K8sEvent::from_json(&serde_json::json!({
                    "metadata": {"name": "w1", "namespace": "dev"},
                    "lastTimestamp": "2026-09-12T12:00:00Z", "type": "Warning", "reason": "BackOff", "count": 9,
                    "involvedObject": {"kind": "Pod", "name": "orders-api-7d9f5b-def34", "namespace": "dev"},
                    "message": "Back-off restarting failed container"
                }))
                .unwrap(),
                K8sEvent::from_json(&serde_json::json!({
                    "metadata": {"name": "n1", "namespace": "dev"},
                    "lastTimestamp": "2026-09-12T11:00:00Z", "type": "Normal", "reason": "Pulled",
                    "involvedObject": {"kind": "Pod", "name": "orders-api-7d9f5b-abc12", "namespace": "dev"},
                    "message": "Pulled image"
                }))
                .unwrap(),
            ]),
        });
        app.apply(Event::ConfigMaps {
            scope: 0,
            configmaps: Ok(vec![
                ConfigMap::from_json(&serde_json::json!({
                    "metadata": {"name": "orders-config", "namespace": "dev"},
                    "data": {"LOG_LEVEL": "info"}
                }))
                .unwrap(),
            ]),
        });
        app.apply(Event::Secrets {
            scope: 0,
            secrets: Ok(vec![
                SecretMeta::from_json(&serde_json::json!({
                    "metadata": {"name": "db", "namespace": "dev"}, "type": "Opaque",
                    "data": {"password": "aHVudGVyMg=="}
                }))
                .unwrap(),
                SecretMeta::from_json(&serde_json::json!({
                    "metadata": {"name": "tls", "namespace": "dev"}, "type": "kubernetes.io/tls",
                    "data": {"tls.crt": "LS0t", "tls.key": "LS0t"}
                }))
                .unwrap(),
            ]),
        });
        app
    }

    fn press(app: &mut App, code: KeyCode) -> AppAction {
        app.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    /// What the tick says about the follow, the owner reads left aside.
    fn follow_tick(app: &mut App) -> Option<Request> {
        app.tick(Instant::now())
            .into_iter()
            .find(|request| matches!(request, Request::Follow(_) | Request::Unfollow))
    }

    fn draw(app: &mut App) -> String {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| app.render(frame, 0)).unwrap();
        crate::ui::screen_text(terminal.backend().buffer())
    }

    fn click(app: &mut App, column: u16, row: u16) -> AppAction {
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn selected_pod_name(app: &App) -> Option<String> {
        app.screens[app.tab]
            .selected_pod(&app.store.scopes[app.tab])
            .map(|pod| pod.key.name.clone())
    }

    #[test]
    fn a_tab_is_reached_by_number_by_bracket_and_by_click_and_the_worker_is_told() {
        let mut app = two_clusters();
        assert_eq!(app.tab, 0);
        assert_eq!(
            press(&mut app, KeyCode::Char('3')),
            AppAction::Send(Request::Showing(2, Kind::Pods))
        );
        assert_eq!(app.tabs[app.tab].label, "qa/uat");
        assert_eq!(press(&mut app, KeyCode::Char('9')), AppAction::None);
        assert_eq!(app.tab, 2, "a number with no tab does nothing");
        press(&mut app, KeyCode::Char(']'));
        press(&mut app, KeyCode::Char(']'));
        assert_eq!(app.tab, 0, "round the end");
        press(&mut app, KeyCode::Char('['));
        assert_eq!(app.tabs[app.tab].label, "prod");
        press(&mut app, KeyCode::Left);
        assert_eq!(app.tab, 2);
        assert_eq!(
            press(&mut app, KeyCode::Char('3')),
            AppAction::None,
            "the same tab is not a switch"
        );

        app.shell.begin_frame();
        app.shell.region(Rect::new(0, 0, 8, 1), Target::Tab(1));
        assert_eq!(
            click(&mut app, 2, 0),
            AppAction::Send(Request::Showing(1, Kind::Pods))
        );
        assert_eq!(app.tab, 1);
    }

    #[test]
    fn the_kinds_are_reached_by_key_and_by_the_pill_and_each_tab_remembers_its_own() {
        let mut app = stocked();
        assert_eq!(
            press(&mut app, KeyCode::Char('m')),
            AppAction::Send(Request::Showing(0, Kind::ConfigMaps))
        );
        assert_eq!(app.kind(), Kind::ConfigMaps);
        let drawn = draw(&mut app);
        assert!(drawn.contains("ConfigMaps ▾"), "{drawn}");
        assert!(drawn.contains("orders-config"), "{drawn}");
        assert!(drawn.contains("● 1 configmaps · just now"), "{drawn}");
        assert_eq!(
            press(&mut app, KeyCode::Char('m')),
            AppAction::None,
            "already there"
        );

        // Another tab keeps pods; back here keeps configmaps.
        assert_eq!(
            press(&mut app, KeyCode::Char('4')),
            AppAction::Send(Request::Showing(3, Kind::Pods))
        );
        assert_eq!(
            press(&mut app, KeyCode::Char('1')),
            AppAction::Send(Request::Showing(0, Kind::ConfigMaps))
        );

        // The pill opens a menu; j, Enter chooses; a click on a line chooses.
        // The pill moves with its label, so it is found again after each draw.
        let pill = |app: &mut App| {
            draw(app);
            app.shell.find(&Target::KindPill).expect("the pill")
        };
        let at = pill(&mut app);
        click(&mut app, at.x + 1, at.y);
        assert_eq!(app.shell.kind_menu, Some(2), "open, on ConfigMaps");
        let drawn = draw(&mut app);
        assert!(drawn.contains("\u{2713} ConfigMaps"), "{drawn}");
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(
            press(&mut app, KeyCode::Enter),
            AppAction::Send(Request::Showing(0, Kind::Secrets))
        );
        assert_eq!(app.shell.kind_menu, None);
        assert_eq!(app.kind(), Kind::Secrets);
        let at = pill(&mut app);
        click(&mut app, at.x + 1, at.y);
        draw(&mut app);
        let events = app
            .shell
            .find(&Target::KindOption(Kind::Events))
            .expect("a line for events");
        assert_eq!(
            click(&mut app, events.x + 2, events.y),
            AppAction::Send(Request::Showing(0, Kind::Events))
        );
        // Any other key closes the menu and is not otherwise acted on.
        let at = pill(&mut app);
        click(&mut app, at.x + 1, at.y);
        assert!(app.shell.kind_menu.is_some());
        press(&mut app, KeyCode::Char('q'));
        assert_eq!(app.shell.kind_menu, None);
        assert_eq!(app.kind(), Kind::Events);

        // A pod's key on another kind says so.
        press(&mut app, KeyCode::Char('x'));
        assert!(
            app.shell
                .notification()
                .is_some_and(|(said, _)| said.contains("pod's key"))
        );
        assert_eq!(
            press(&mut app, KeyCode::Char('s')),
            AppAction::Send(Request::Showing(0, Kind::Secrets))
        );
        assert_eq!(
            press(&mut app, KeyCode::Char('p')),
            AppAction::Send(Request::Showing(0, Kind::Pods))
        );
    }

    #[test]
    fn e_on_a_pod_shows_its_events_and_enter_on_one_goes_back_to_the_pod() {
        let mut app = stocked();
        app.screens[0].refilter(&app.store.scopes[0]);
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(
            selected_pod_name(&app).as_deref(),
            Some("orders-api-7d9f5b-def34")
        );
        assert_eq!(
            press(&mut app, KeyCode::Char('e')),
            AppAction::Send(Request::Showing(0, Kind::Events))
        );
        assert_eq!(app.kind(), Kind::Events);
        let drawn = draw(&mut app);
        assert!(drawn.contains("BackOff"), "{drawn}");
        assert!(!drawn.contains("Pulled"), "narrowed to the pod: {drawn}");
        assert!(drawn.contains("[Pod] [Describe] [YAML]"), "{drawn}");

        press(&mut app, KeyCode::Esc);
        app.screens[0].refilter(&app.store.scopes[0]);
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(
            press(&mut app, KeyCode::Enter),
            AppAction::Send(Request::Showing(0, Kind::Pods))
        );
        assert_eq!(app.kind(), Kind::Pods);
        assert_eq!(
            selected_pod_name(&app).as_deref(),
            Some("orders-api-7d9f5b-abc12")
        );

        // d on an event describes what the event is about.
        press(&mut app, KeyCode::Char('e'));
        press(&mut app, KeyCode::Esc);
        app.screens[0].refilter(&app.store.scopes[0]);
        let action = press(&mut app, KeyCode::Char('d'));
        assert!(
            matches!(
                action,
                AppAction::Send(Request::Describe { ref object, .. }) if object.slash() == "pod/orders-api-7d9f5b-def34"
            ),
            "{action:?}"
        );
    }

    #[test]
    fn a_secrets_value_is_read_on_v_shown_for_sixty_seconds_and_y_copies_it_unseen() {
        let mut app = stocked();
        press(&mut app, KeyCode::Char('s'));
        app.screens[0].refilter(&app.store.scopes[0]);
        let drawn = draw(&mut app);
        assert!(drawn.contains("› password  7 bytes"), "{drawn}");
        assert!(!drawn.contains("hunter2"), "{drawn}");
        let object = ObjectRef {
            kind: "secret".to_owned(),
            namespace: "dev".to_owned(),
            name: "db".to_owned(),
        };
        assert_eq!(
            press(&mut app, KeyCode::Char('v')),
            AppAction::Send(Request::SecretValue {
                scope: 0,
                object: object.clone(),
                key: "password".to_owned(),
                copy: false,
            })
        );
        assert_eq!(
            app.poll_for(Duration::from_secs(9)),
            Duration::from_secs(1),
            "counting"
        );
        let drawn = draw(&mut app);
        assert!(drawn.contains("Value · db · password"), "{drawn}");
        assert!(drawn.contains("Reading…"), "{drawn}");
        let action = app.apply(Event::SecretValue {
            scope: 0,
            object: object.clone(),
            key: "password".to_owned(),
            copy: false,
            value: Ok(Secret::new("hunter2")),
        });
        assert_eq!(action, AppAction::None);
        let drawn = draw(&mut app);
        assert!(drawn.contains("hunter2"), "{drawn}");
        assert!(drawn.contains("clears in"), "{drawn}");

        // Moving the cursor hides it; y reads it for the clipboard alone.
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.shell.focus, Focus::Table);
        press(&mut app, KeyCode::Char('j'));
        let drawn = draw(&mut app);
        assert!(!drawn.contains("hunter2"), "{drawn}");
        assert!(drawn.contains("tls.crt"), "{drawn}");
        press(&mut app, KeyCode::Char('k'));
        assert_eq!(
            press(&mut app, KeyCode::Char('y')),
            AppAction::Send(Request::SecretValue {
                scope: 0,
                object: object.clone(),
                key: "password".to_owned(),
                copy: true,
            })
        );
        let action = app.apply(Event::SecretValue {
            scope: 0,
            object,
            key: "password".to_owned(),
            copy: true,
            value: Ok(Secret::new("hunter2")),
        });
        assert_eq!(
            action,
            AppAction::Copy {
                text: "hunter2".to_owned(),
                label: "Copied password of db".to_owned()
            }
        );
        let written = serde_json::to_string(&app.store.snapshot(&app.tabs)).unwrap();
        assert!(
            !written.contains("hunter2") && !written.contains("password"),
            "{written}"
        );
        assert!(!format!("{:?}", app.screens[0].modal).contains("hunter2"));
    }

    #[test]
    fn a_configmaps_value_shows_on_enter_and_y_copies_it() {
        let mut app = stocked();
        press(&mut app, KeyCode::Char('m'));
        app.screens[0].refilter(&app.store.scopes[0]);
        assert_eq!(press(&mut app, KeyCode::Enter), AppAction::None);
        let drawn = draw(&mut app);
        assert!(
            drawn.contains("Value · orders-config · LOG_LEVEL"),
            "{drawn}"
        );
        assert!(drawn.contains("info"), "{drawn}");
        assert_eq!(
            press(&mut app, KeyCode::Char('y')),
            AppAction::Copy {
                text: "info".to_owned(),
                label: "Copied LOG_LEVEL of orders-config".to_owned()
            }
        );
        assert!(matches!(
            press(&mut app, KeyCode::Char('d')),
            AppAction::Send(Request::Describe { .. })
        ));
    }

    #[test]
    fn r_reads_the_open_tab_again_and_says_so() {
        let mut app = two_clusters();
        press(&mut app, KeyCode::Char('4'));
        assert_eq!(
            press(&mut app, KeyCode::Char('r')),
            AppAction::Send(Request::Refresh(3))
        );
        assert_eq!(
            app.shell.notification().map(|(said, _)| said),
            Some("Reading prod/prod…")
        );
    }

    #[test]
    fn each_tab_and_kind_keeps_its_own_search_box() {
        let mut app = two_clusters();
        app.shell.focus = Focus::Search;
        app.handle_paste("orders");
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.screens[0].list().input.text(), "orders");
        press(&mut app, KeyCode::Char('2'));
        assert!(
            app.screens[1].list().input.is_empty(),
            "the other tab's box is its own"
        );
        press(&mut app, KeyCode::Char('1'));
        assert_eq!(
            app.screens[0].list().input.text(),
            "orders",
            "and comes back as it was"
        );
        press(&mut app, KeyCode::Char('e'));
        assert!(
            app.screens[0].list().input.is_empty(),
            "the events' box is its own"
        );
        press(&mut app, KeyCode::Char('p'));
        press(&mut app, KeyCode::Esc);
        assert!(
            app.screens[0].list().input.is_empty(),
            "Esc out of the table clears it"
        );
    }

    #[test]
    fn a_read_lands_on_its_own_tab_keeps_the_cursor_and_marks_the_cache_dirty() {
        let mut app = stocked();
        assert!(app.cache_dirty);
        app.screens[0].refilter(&app.store.scopes[0]);
        app.screens[0].list_mut().cursor.focus(1);
        let chosen = app.screens[0]
            .cursor_identity(Kind::Pods, &app.store.scopes[0])
            .unwrap();
        // A pod that sorts ahead of both pushes the chosen row down a line.
        app.apply(Event::Pods {
            scope: 0,
            pods: Ok(vec![
                crashing("qa", "dev", "orders-api-7d9f5b-def34"),
                pod("qa", "dev", "orders-api-7d9f5b-abc12", "Running"),
                pod("qa", "dev", "orders-api-7d9f5b-aaa01", "Running"),
            ]),
        });
        assert_eq!(app.store.scopes[0].pods.rows.len(), 3);
        assert_eq!(app.store.scopes[3].pods.rows.len(), 1, "prod is untouched");
        assert_eq!(app.screens[0].list().cursor.index, 2, "the row moved down");
        assert_eq!(
            app.screens[0].cursor_identity(Kind::Pods, &app.store.scopes[0]),
            Some(chosen)
        );
    }

    #[test]
    fn a_failed_read_is_said_once_for_the_open_tab_and_kind_and_its_rows_stand() {
        let mut app = stocked();
        app.apply(Event::Pods {
            scope: 0,
            pods: Err("Unable to connect to the server".into()),
        });
        assert_eq!(
            app.shell.notification().map(|(said, _)| said),
            Some("qa/dev pods: Unable to connect to the server")
        );
        assert_eq!(
            app.store.scopes[0].pods.rows.len(),
            2,
            "yesterday's rows stand"
        );

        let mut app = stocked();
        app.apply(Event::Pods {
            scope: 3,
            pods: Err("Unable to connect to the server".into()),
        });
        assert!(
            app.shell.notification().is_none(),
            "a hidden tab's trouble is on its badge and in ?, not the status bar"
        );
        app.tab = 3;
        app.apply(Event::Pods {
            scope: 3,
            pods: Err("Unable to connect to the server".into()),
        });
        assert!(
            app.shell.notification().is_none(),
            "the same refusal is not said twice"
        );
        app.apply(Event::Pods {
            scope: 3,
            pods: Err("context \"aks-prod\" does not exist".into()),
        });
        assert!(app.shell.notification().is_some(), "a different one is");
        app.apply(Event::Secrets {
            scope: 3,
            secrets: Err("secrets is forbidden".into()),
        });
        let drawn = draw(&mut app);
        assert!(
            !drawn.contains("! prod/prod secrets"),
            "not the kind on screen: {drawn}"
        );
        press(&mut app, KeyCode::Char('s'));
        let drawn = draw(&mut app);
        assert!(
            drawn.contains("! prod/prod secrets: secrets is forbidden"),
            "{drawn}"
        );
    }

    #[test]
    fn the_frame_wears_the_badge_the_counts_and_the_problems() {
        let mut app = stocked();
        let drawn = draw(&mut app);
        assert!(drawn.contains("1 qa/dev \u{2717} 1"), "{drawn}");
        assert!(drawn.contains("4 prod"), "{drawn}");
        assert!(drawn.contains("orders-api-7d9f5b-def34"), "{drawn}");
        assert!(drawn.contains("● 2 pods · just now"), "{drawn}");

        app.apply(Event::Pods {
            scope: 0,
            pods: Err("Unable to connect to the server".into()),
        });
        let drawn = draw(&mut app);
        assert!(
            drawn.contains("! qa/dev pods: Unable to connect"),
            "{drawn}"
        );
        app.shell.help_open = true;
        let drawn = draw(&mut app);
        assert!(drawn.contains("Problems"), "{drawn}");
        assert!(
            drawn.contains("qa/dev pods: Unable to connect to the server"),
            "{drawn}"
        );

        let mut app = two_clusters();
        app.apply(Event::Reading(0));
        assert_eq!(
            app.poll_for(Duration::from_secs(1)),
            Duration::from_millis(100)
        );
        let drawn = draw(&mut app);
        assert!(drawn.contains("reading qa/dev pods…"), "{drawn}");
    }

    #[test]
    fn enter_opens_the_log_and_the_tick_tells_the_worker_what_to_follow() {
        let mut app = stocked();
        assert_eq!(
            follow_tick(&mut app),
            None,
            "nothing followed with the pane closed"
        );
        assert_eq!(press(&mut app, KeyCode::Enter), AppAction::None);
        assert_eq!(app.shell.focus, Focus::Details, "the pane takes the keys");
        let request = follow_tick(&mut app).expect("a follow");
        let Request::Follow(target) = request else {
            panic!("expected a follow, got {request:?}");
        };
        assert_eq!(target.scope, 0);
        assert_eq!(target.key.name, "orders-api-7d9f5b-abc12");
        assert_eq!(follow_tick(&mut app), None, "said once");

        app.apply(Event::LogLines {
            target: target.clone(),
            lines: vec!["2026-09-12T12:00:00Z INFO up".to_owned()],
            finished: false,
        });
        assert_eq!(app.screens[0].log_lines().len(), 1);
        let drawn = draw(&mut app);
        assert!(
            drawn.contains("Log · following · orders-api-7d9f5b-abc12"),
            "{drawn}"
        );
        assert!(drawn.contains("12:00:00 INFO up"), "{drawn}");

        // Back to the table and down a row: the follow moves with the cursor.
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Char('j'));
        app.screens[0].refilter(&app.store.scopes[0]);
        let Some(Request::Follow(next)) = follow_tick(&mut app) else {
            panic!("the next pod's log");
        };
        assert_eq!(next.key.name, "orders-api-7d9f5b-def34");
        assert!(
            app.screens[0].log_lines().is_empty(),
            "the lines were the last pod's"
        );

        // Another kind, another tab: nothing on this one is followed any more.
        press(&mut app, KeyCode::Char('e'));
        assert_eq!(follow_tick(&mut app), Some(Request::Unfollow));
        press(&mut app, KeyCode::Char('p'));
        press(&mut app, KeyCode::Enter);
        assert!(matches!(follow_tick(&mut app), Some(Request::Follow(_))));
        press(&mut app, KeyCode::Char('4'));
        assert_eq!(follow_tick(&mut app), Some(Request::Unfollow));
        press(&mut app, KeyCode::Char('1'));
        assert!(
            matches!(follow_tick(&mut app), Some(Request::Follow(_))),
            "and back again"
        );

        // Esc closes the pane; the tick says so.
        press(&mut app, KeyCode::Esc);
        assert!(!app.screens[0].pane_open);
        assert_eq!(app.shell.focus, Focus::Table);
        assert_eq!(follow_tick(&mut app), Some(Request::Unfollow));
    }

    #[test]
    fn d_asks_for_a_describe_once_and_the_pane_shows_it_when_it_lands() {
        let mut app = stocked();
        let AppAction::Send(Request::Describe { scope, object }) =
            press(&mut app, KeyCode::Char('d'))
        else {
            panic!("a describe request");
        };
        assert_eq!(scope, 0);
        assert_eq!(object.slash(), "pod/orders-api-7d9f5b-abc12");
        assert_eq!(follow_tick(&mut app), None, "a describe follows nothing");
        let drawn = draw(&mut app);
        assert!(
            drawn.contains("Describe · orders-api-7d9f5b-abc12"),
            "{drawn}"
        );
        assert!(drawn.contains("Describe…"), "{drawn}");

        app.apply(Event::Text {
            scope: 0,
            kind: TextKind::Describe,
            object: object.clone(),
            text: Ok(vec![
                "Name:  orders-api-7d9f5b-abc12".to_owned(),
                "Node:  aks-np1".to_owned(),
            ]),
        });
        let drawn = draw(&mut app);
        assert!(drawn.contains("Node:  aks-np1"), "{drawn}");
        assert_eq!(
            press(&mut app, KeyCode::Char('d')),
            AppAction::None,
            "on file now"
        );

        // The filter inside the pane.
        press(&mut app, KeyCode::Char('/'));
        assert_eq!(app.shell.focus, Focus::PaneSearch);
        app.handle_paste("Node");
        press(&mut app, KeyCode::Enter);
        let drawn = draw(&mut app);
        assert!(drawn.contains("/ Node · 1/2"), "{drawn}");
        assert!(!drawn.contains("Name:  orders"), "{drawn}");
        press(&mut app, KeyCode::Esc);
        assert!(
            app.screens[0].pane_filter.is_empty(),
            "Esc takes the filter off first"
        );
        assert!(app.screens[0].pane_open);

        // Y copies the line for what the pane shows; y the name.
        assert_eq!(
            press(&mut app, KeyCode::Char('Y')),
            AppAction::Copy {
                text: "kubectl --context aks-qa -n dev describe pod/orders-api-7d9f5b-abc12"
                    .to_owned(),
                label:
                    "Copied `kubectl --context aks-qa -n dev describe pod/orders-api-7d9f5b-abc12`"
                        .to_owned(),
            }
        );
        assert!(matches!(
            press(&mut app, KeyCode::Char('y')),
            AppAction::Copy { text, .. } if text == "orders-api-7d9f5b-abc12"
        ));

        // v: the YAML, asked for; r forgets both.
        assert!(matches!(
            press(&mut app, KeyCode::Char('v')),
            AppAction::Send(Request::Yaml { .. })
        ));
        press(&mut app, KeyCode::Char('r'));
        assert!(matches!(
            press(&mut app, KeyCode::Char('d')),
            AppAction::Send(Request::Describe { .. })
        ));
    }

    #[test]
    fn x_asks_first_and_the_second_x_is_the_one_delete_while_a_bare_pod_is_refused() {
        let mut app = stocked();
        assert_eq!(press(&mut app, KeyCode::Char('x')), AppAction::None);
        assert!(app.screens[0].modal.is_some(), "asked, not deleted");
        let drawn = draw(&mut app);
        assert!(
            drawn.contains("Restart orders-api-7d9f5b-abc12?"),
            "{drawn}"
        );
        assert!(
            drawn.contains("Deployment orders-api replaces it"),
            "{drawn}"
        );
        assert!(drawn.contains("x again to restart it"), "{drawn}");
        // Any other key closes it and is not otherwise acted on.
        assert_eq!(press(&mut app, KeyCode::Char('j')), AppAction::None);
        assert!(app.screens[0].modal.is_none());
        assert_eq!(
            app.screens[0].list().cursor.index,
            0,
            "j did not move the cursor"
        );

        press(&mut app, KeyCode::Char('x'));
        let action = press(&mut app, KeyCode::Char('x'));
        let AppAction::Send(Request::Delete { scope: 0, key }) = action else {
            panic!("the delete, got {action:?}");
        };
        assert_eq!(key.name, "orders-api-7d9f5b-abc12");
        assert!(app.screens[0].modal.is_none());
        app.apply(Event::Deleted {
            scope: 0,
            key,
            error: None,
        });
        assert_eq!(
            app.shell.notification().map(|(said, _)| said),
            Some("Deleted orders-api-7d9f5b-abc12; Deployment orders-api is putting a new one up")
        );

        // The modal takes the pointer: a click on its yes answers, a click
        // anywhere else closes it, and nothing underneath is reached.
        press(&mut app, KeyCode::Char('x'));
        draw(&mut app);
        let yes = app.shell.find(&Target::Confirm).expect("a yes button");
        assert!(matches!(
            click(&mut app, yes.x, yes.y),
            AppAction::Send(Request::Delete { .. })
        ));
        press(&mut app, KeyCode::Char('x'));
        draw(&mut app);
        assert_eq!(
            click(&mut app, 2, 4),
            AppAction::None,
            "a row under the modal"
        );
        assert!(app.screens[0].modal.is_none(), "closed");
        assert_eq!(
            app.screens[0].list().cursor.index,
            0,
            "and the row was not taken"
        );

        // A pod nothing put there is refused outright.
        let mut bare = pod("qa", "dev", "debug-shell", "Running");
        bare.owner = None;
        app.apply(Event::Pods {
            scope: 0,
            pods: Ok(vec![bare]),
        });
        app.screens[0].refilter(&app.store.scopes[0]);
        press(&mut app, KeyCode::Char('x'));
        assert!(app.screens[0].modal.is_none());
        assert!(
            app.shell
                .notification()
                .is_some_and(|(said, _)| said.contains("no controller"))
        );
        press(&mut app, KeyCode::Char('X'));
        assert!(app.screens[0].modal.is_none());
        assert!(
            app.shell
                .notification()
                .is_some_and(|(said, _)| said.contains("nothing to roll"))
        );
    }

    #[test]
    fn the_owner_is_read_once_the_cursor_rests_and_equals_scales_it() {
        let mut app = stocked();
        app.screens[0].refilter(&app.store.scopes[0]);
        let now = Instant::now();
        assert!(app.tick(now).is_empty(), "the rest has just started");
        assert!(app.is_resting());
        let requests = app.tick(now + REST);
        let deployment = ObjectRef {
            kind: "deployment".to_owned(),
            namespace: "dev".to_owned(),
            name: "orders-api".to_owned(),
        };
        assert_eq!(
            requests,
            vec![Request::Owner {
                scope: 0,
                object: deployment.clone()
            }]
        );
        assert!(app.tick(now + REST * 2).is_empty(), "asked once");
        app.apply(Event::Owner {
            scope: 0,
            object: deployment.clone(),
            replicas: Ok(crate::kube::Replicas {
                desired: 3,
                ready: 3,
            }),
        });
        let drawn = draw(&mut app);
        assert!(
            drawn.contains("Deployment/orders-api · 3/3 ready"),
            "{drawn}"
        );

        // = opens the scale modal filled with the count; Enter sends it.
        assert_eq!(
            press(&mut app, KeyCode::Char('=')),
            AppAction::None,
            "on file: nothing to ask"
        );
        let drawn = draw(&mut app);
        assert!(drawn.contains("Replicas  [ 3"), "{drawn}");
        assert!(drawn.contains("now 3 desired · 3 ready"), "{drawn}");
        press(&mut app, KeyCode::Backspace);
        press(&mut app, KeyCode::Char('5'));
        let action = press(&mut app, KeyCode::Enter);
        assert_eq!(
            action,
            AppAction::Send(Request::Scale {
                scope: 0,
                object: deployment.clone(),
                replicas: 5
            })
        );
        app.apply(Event::Acted {
            scope: 0,
            verb: "scale",
            object: deployment.clone(),
            error: None,
        });
        assert_eq!(
            app.shell.notification().map(|(said, _)| said),
            Some("deployment/orders-api scale sent")
        );

        // Not a number: the modal stays and says so.
        press(&mut app, KeyCode::Char('='));
        press(&mut app, KeyCode::Backspace);
        press(&mut app, KeyCode::Char('x'));
        assert_eq!(press(&mut app, KeyCode::Enter), AppAction::None);
        assert!(app.screens[0].modal.is_some());
        assert!(
            app.shell
                .notification()
                .is_some_and(|(said, _)| said.contains("whole number"))
        );
        press(&mut app, KeyCode::Esc);
        assert!(app.screens[0].modal.is_none());

        // X: the rollout, confirmed with X.
        press(&mut app, KeyCode::Char('X'));
        assert_eq!(
            press(&mut app, KeyCode::Char('X')),
            AppAction::Send(Request::RolloutRestart {
                scope: 0,
                object: deployment
            })
        );

        // A pod on a StatefulSet: scalable too; on a Job: not.
        let mut redis = pod("qa", "dev", "redis-0", "Running");
        redis.owner = Some(("StatefulSet".to_owned(), "redis".to_owned()));
        let mut job = pod("qa", "dev", "report-x1", "Completed");
        job.owner = Some(("Job".to_owned(), "report".to_owned()));
        app.apply(Event::Pods {
            scope: 0,
            pods: Ok(vec![job, redis]),
        });
        app.screens[0].refilter(&app.store.scopes[0]);
        // By name: redis-0 first, report-x1 second.
        assert!(
            matches!(
                press(&mut app, KeyCode::Char('=')),
                AppAction::Send(Request::Owner { .. })
            ),
            "not on file yet: asked, and the box fills when it lands"
        );
        assert!(app.screens[0].modal.is_some());
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('='));
        assert!(app.screens[0].modal.is_none());
        assert!(
            app.shell
                .notification()
                .is_some_and(|(said, _)| said.contains("nothing to scale"))
        );
    }

    #[test]
    fn b_hands_the_terminal_to_kubectl_exec_on_the_pod_and_its_followed_container() {
        let mut app = stocked();
        assert_eq!(
            press(&mut app, KeyCode::Char('b')),
            AppAction::Exec {
                context: "aks-qa".to_owned(),
                namespace: "dev".to_owned(),
                pod: "orders-api-7d9f5b-abc12".to_owned(),
                container: None,
            }
        );
        // The toolbar button is the same key.
        draw(&mut app);
        let bash = app
            .shell
            .find(&Target::Button(Button::Bash))
            .expect("a Bash button");
        assert!(matches!(
            click(&mut app, bash.x + 1, bash.y),
            AppAction::Exec { .. }
        ));
        let logs = app
            .shell
            .find(&Target::Button(Button::Logs))
            .expect("a Logs button");
        click(&mut app, logs.x + 1, logs.y);
        assert!(app.screens[0].pane_open, "the Logs button opens the pane");
    }

    #[test]
    fn a_layout_survives_a_round_trip_through_the_file_by_the_tabs_label() {
        let mut app = two_clusters();
        app.tab = 3;
        app.screens[3].set_kind(Kind::Events);
        app.screens[3].list_of_mut(Kind::Pods).sort = ColumnId::Age;
        app.screens[3].list_of_mut(Kind::Pods).descending = true;
        app.screens[3].list_of_mut(Kind::Pods).layout.columns[0].width = 20;
        app.screens[0]
            .list_of_mut(Kind::Pods)
            .layout
            .set_visible(ColumnId::Node, true);

        let session = app.session();
        let mut fresh = two_clusters();
        fresh.restore(&session);

        assert_eq!(fresh.tabs[fresh.tab].label, "prod");
        assert_eq!(fresh.screens[3].kind, Kind::Events);
        assert_eq!(fresh.screens[3].list_of(Kind::Pods).sort, ColumnId::Age);
        assert!(fresh.screens[3].list_of(Kind::Pods).descending);
        assert_eq!(
            fresh.screens[3].list_of(Kind::Pods).layout.columns[0].width,
            20
        );
        let node_shown = |app: &App, tab: usize| {
            app.screens[tab]
                .list_of(Kind::Pods)
                .layout
                .columns
                .iter()
                .any(|column| column.id == ColumnId::Node && column.visible)
        };
        assert!(node_shown(&fresh, 0));
        assert!(!node_shown(&fresh, 1));

        let written = serde_json::to_string(&session).unwrap();
        assert!(!written.contains("cursor"), "{written}");
        assert!(!written.contains("query"), "{written}");
    }

    #[test]
    fn a_tab_or_column_this_build_does_not_know_is_skipped_rather_than_fatal() {
        let mut session = Session {
            tab: Some("staging/blue".into()),
            ..Session::default()
        };
        let held = session.tab("qa/dev");
        held.kind = Some("nodes".into());
        held.sort = Some(("from_the_future".into(), "asc".into()));
        held.columns = vec![SessionColumn {
            key: "from_the_future".into(),
            width: Some(9),
            visible: Some(false),
        }];
        let mut app = two_clusters();
        let before = app.screens[0].list_of(Kind::Pods).layout.clone();
        app.restore(&session);
        assert_eq!(app.tab, 0, "an unknown tab is the first one");
        assert_eq!(app.screens[0].kind, Kind::Pods, "an unknown kind is pods");
        assert_eq!(app.screens[0].list_of(Kind::Pods).sort, ColumnId::Name);
        assert_eq!(app.screens[0].list_of(Kind::Pods).layout, before);
    }

    #[test]
    fn a_tab_over_every_namespace_shows_which_one_a_row_is_in() {
        let tabs = config::parse("[[clusters]]\nname = \"lab\"\n")
            .unwrap()
            .tabs();
        let app = App::new(tabs, Store::new(1));
        for kind in Kind::ALL {
            assert!(
                app.screens[0]
                    .list_of(kind)
                    .layout
                    .columns
                    .iter()
                    .any(|column| column.id == ColumnId::Namespace && column.visible),
                "{kind:?}"
            );
        }
        let app = two_clusters();
        assert!(
            !app.screens[0]
                .list_of(Kind::Pods)
                .layout
                .columns
                .iter()
                .any(|column| column.id == ColumnId::Namespace && column.visible)
        );
    }

    #[test]
    fn no_clusters_at_all_is_a_message_not_a_panic() {
        let mut app = App::new(Vec::new(), Store::new(0));
        press(&mut app, KeyCode::Char('1'));
        press(&mut app, KeyCode::Char(']'));
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('e'));
        assert_eq!(press(&mut app, KeyCode::Char('r')), AppAction::None);
        assert!(app.session().tab.is_none());
        let drawn = draw(&mut app);
        assert!(drawn.contains("No clusters configured"), "{drawn}");
    }
}
