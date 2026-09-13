//! The application: which tab is open, what the keys do before a screen sees
//! them, and how a frame is put together.

pub mod cursor;
pub mod keys;
pub mod scope;
pub mod screen;
pub mod shell;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::widgets::{Paragraph, Wrap};

use scope::ScopeScreen;
use screen::{AppAction, Target};
use shell::{Focus, Shell};

use crate::columns::{ColumnId, TableLayout};
use crate::config::Tab;
use crate::session::{Session, SessionColumn};
use crate::text_input::TextInput;
use crate::ui;
use crate::ui::widgets::TabLabel;

pub struct App {
    pub shell: Shell,
    /// The tabs, in `config.toml`'s order. One screen each.
    pub tabs: Vec<Tab>,
    pub tab: usize,
    pub screens: Vec<ScopeScreen>,
}

impl App {
    #[must_use]
    pub fn new(tabs: Vec<Tab>) -> Self {
        let screens = tabs
            .iter()
            .map(|tab| {
                let mut screen = ScopeScreen::default();
                // A tab over every namespace says which each pod is in.
                if tab.scope.namespace.is_none() {
                    screen.layout.set_visible(ColumnId::Namespace, true);
                }
                screen
            })
            .collect();
        Self {
            shell: Shell::default(),
            tabs,
            tab: 0,
            screens,
        }
    }

    /// The open tab's screen, if there is a tab at all.
    fn screen(&mut self) -> Option<&mut ScopeScreen> {
        self.screens.get_mut(self.tab)
    }

    /// One key. The global keys are matched here first; everything else goes
    /// to the tab, except while the search box has focus, which takes
    /// everything but `Esc`, `Enter` and `Tab`.
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
        if self.shell.focus == Focus::Search {
            return self.key_in_search(key);
        }
        match key.code {
            KeyCode::Char(number @ '1'..='9') => {
                let index = usize::from(u8::try_from(number).unwrap_or(b'1') - b'1');
                if index < self.tabs.len() {
                    self.switch_to(index);
                }
                AppAction::None
            }
            KeyCode::Char('[') | KeyCode::Left => {
                self.step_tab(-1);
                AppAction::None
            }
            KeyCode::Char(']') | KeyCode::Right => {
                self.step_tab(1);
                AppAction::None
            }
            KeyCode::Char('?') => {
                self.shell.help_open = true;
                AppAction::None
            }
            KeyCode::Char('q') => AppAction::Quit,
            // Esc out of the table clears the query rather than quitting:
            // Esc left the box keeping the filter, and this is the second
            // press that takes it off.
            KeyCode::Esc => {
                self.clear_query();
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

    /// A paste, which bracketed paste hands over whole. It goes into the
    /// search box when that is what has focus, and nowhere otherwise.
    pub fn handle_paste(&mut self, text: &str) -> AppAction {
        if self.shell.focus == Focus::Search
            && let Some(input) = self.input()
        {
            input.paste(text);
        }
        AppAction::None
    }

    /// The search box of whichever tab is showing.
    fn input(&mut self) -> Option<&mut TextInput> {
        self.screen().map(|screen| &mut screen.input)
    }

    /// `Esc` out of the table: the filter goes, and the table comes back
    /// whole.
    fn clear_query(&mut self) {
        if let Some(input) = self.input() {
            input.clear();
        }
    }

    fn screen_key(&mut self, key: KeyEvent) -> AppAction {
        let Some(screen) = self.screens.get_mut(self.tab) else {
            return AppAction::None;
        };
        screen.handle_key(&mut self.shell, key)
    }

    fn switch_to(&mut self, tab: usize) {
        if self.tab != tab && tab < self.tabs.len() {
            self.tab = tab;
            self.shell.focus = Focus::Table;
        }
    }

    /// `[` and `]`: the previous and the next tab, round the ends.
    fn step_tab(&mut self, by: isize) {
        let count = self.tabs.len();
        if count == 0 {
            return;
        }
        let next = (self.tab as isize + by).rem_euclid(count as isize) as usize;
        self.switch_to(next);
    }

    /// A click, resolved against what was drawn last frame. A click lands
    /// focus where it lands: on a row, the table; on the pane, the pane.
    pub fn handle_mouse(&mut self, event: MouseEvent) -> AppAction {
        let target = self.shell.hit(event.column, event.row).cloned();
        match event.kind {
            MouseEventKind::Down(_) => match target {
                Some(Target::Tab(tab)) => {
                    self.switch_to(tab);
                    AppAction::None
                }
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
                Some(Target::Details) => {
                    self.shell.focus = Focus::Details;
                    AppAction::None
                }
                Some(target) => {
                    self.shell.focus = Focus::Table;
                    let Some(screen) = self.screens.get_mut(self.tab) else {
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
                if let Some(screen) = self.screens.get_mut(self.tab) {
                    screen.handle_wheel(&mut self.shell, target, delta);
                }
                AppAction::None
            }
            _ => AppAction::None,
        }
    }

    // ── The session ────────────────────────────────────────────────────

    /// The layout as it stands, for the file. Never the query, never the
    /// cursor, and nothing a cluster answered.
    #[must_use]
    pub fn session(&self) -> Session {
        let mut session = Session {
            tab: self.tabs.get(self.tab).map(|tab| tab.label.clone()),
            ..Session::default()
        };
        for (tab, screen) in self.tabs.iter().zip(&self.screens) {
            let held = session.tab(&tab.label);
            held.sort = Some(sort_of(screen.sort, screen.descending));
            held.columns = columns_of(&screen.layout);
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
            if let Some((column, way)) = read_sort(held.sort.as_ref()) {
                screen.sort = column;
                screen.descending = way;
            }
            apply_columns(&mut screen.layout, &held.columns);
        }
    }

    /// What the status bar says on the left when nothing has just happened.
    #[must_use]
    pub fn footer_hint(&self) -> String {
        self.screens
            .get(self.tab)
            .map_or_else(String::new, |screen| screen.footer_hint(&self.shell))
    }

    /// What the status bar says on the right: what the open tab holds.
    fn store_state(&self) -> (String, Style) {
        let palette = ui::theme::theme();
        match self.tabs.get(self.tab) {
            Some(tab) => (
                format!("● {}", tab.scope.describe()),
                Style::default().fg(palette.muted),
            ),
            None => (
                "no clusters configured".to_owned(),
                Style::default().fg(palette.error),
            ),
        }
    }

    /// One frame.
    pub fn render(&mut self, frame: &mut Frame, _millis: u128) {
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
            .map(|tab| TabLabel {
                label: tab.label.clone(),
                short: tab.scope.short_label().to_owned(),
                badge: None,
            })
            .collect();
        ui::widgets::render_tab_bar(frame, &mut self.shell, tabs, self.tab, &labels);
        self.render_body(frame, body);
        let hint = self.footer_hint();
        let (right, right_style) = self.store_state();
        ui::widgets::render_status_bar(frame, &mut self.shell, status, &hint, &right, right_style);
        if self.shell.help_open {
            ui::widgets::render_help(frame, &mut self.shell, area, &[]);
        }
    }

    fn render_body(&mut self, frame: &mut Frame, area: Rect) {
        let Some((tab, screen)) = self.tabs.get(self.tab).zip(self.screens.get_mut(self.tab))
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
        ui::pods::render(frame, &mut self.shell, screen, tab, area);
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

    pub(crate) fn two_clusters() -> App {
        App::new(config::parse(config::tests::TWO_CLUSTERS).unwrap().tabs())
    }

    fn press(app: &mut App, code: KeyCode) -> AppAction {
        app.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn a_tab_is_reached_by_number_by_bracket_and_by_click() {
        let mut app = two_clusters();
        assert_eq!(app.tab, 0);
        press(&mut app, KeyCode::Char('3'));
        assert_eq!(app.tabs[app.tab].label, "qa/uat");
        press(&mut app, KeyCode::Char('9'));
        assert_eq!(app.tab, 2, "a number with no tab does nothing");
        press(&mut app, KeyCode::Char(']'));
        press(&mut app, KeyCode::Char(']'));
        assert_eq!(app.tab, 0, "round the end");
        press(&mut app, KeyCode::Char('['));
        assert_eq!(app.tabs[app.tab].label, "prod");
        press(&mut app, KeyCode::Left);
        assert_eq!(app.tab, 2);

        app.shell.begin_frame();
        app.shell.region(Rect::new(0, 0, 8, 1), Target::Tab(1));
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 2,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.tab, 1);
    }

    #[test]
    fn each_tab_keeps_its_own_search_box() {
        let mut app = two_clusters();
        app.shell.focus = Focus::Search;
        app.handle_paste("orders");
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.screens[0].input.text(), "orders");
        press(&mut app, KeyCode::Char('2'));
        assert!(
            app.screens[1].input.is_empty(),
            "the other tab's box is its own"
        );
        press(&mut app, KeyCode::Char('1'));
        assert_eq!(
            app.screens[0].input.text(),
            "orders",
            "and comes back as it was"
        );
        press(&mut app, KeyCode::Esc);
        assert!(
            app.screens[0].input.is_empty(),
            "Esc out of the table clears it"
        );
    }

    #[test]
    fn a_layout_survives_a_round_trip_through_the_file_by_the_tabs_label() {
        let mut app = two_clusters();
        app.tab = 3;
        app.screens[3].sort = ColumnId::Age;
        app.screens[3].descending = true;
        app.screens[3].layout.columns[0].width = 20;
        app.screens[0].layout.set_visible(ColumnId::Node, true);

        let session = app.session();
        let mut fresh = two_clusters();
        fresh.restore(&session);

        assert_eq!(fresh.tabs[fresh.tab].label, "prod");
        assert_eq!(fresh.screens[3].sort, ColumnId::Age);
        assert!(fresh.screens[3].descending);
        assert_eq!(fresh.screens[3].layout.columns[0].width, 20);
        assert!(
            fresh.screens[0]
                .layout
                .columns
                .iter()
                .any(|c| c.id == ColumnId::Node && c.visible)
        );
        assert!(
            !fresh.screens[1]
                .layout
                .columns
                .iter()
                .any(|c| c.id == ColumnId::Node && c.visible)
        );

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
        held.sort = Some(("from_the_future".into(), "asc".into()));
        held.columns = vec![SessionColumn {
            key: "from_the_future".into(),
            width: Some(9),
            visible: Some(false),
        }];
        let mut app = two_clusters();
        let before = app.screens[0].layout.clone();
        app.restore(&session);
        assert_eq!(app.tab, 0, "an unknown tab is the first one");
        assert_eq!(app.screens[0].sort, ColumnId::Name);
        assert_eq!(app.screens[0].layout, before);
    }

    #[test]
    fn a_tab_over_every_namespace_shows_which_one_a_pod_is_in() {
        let app = App::new(
            config::parse("[[clusters]]\nname = \"lab\"\n")
                .unwrap()
                .tabs(),
        );
        let namespace = app.screens[0]
            .layout
            .columns
            .iter()
            .find(|column| column.id == ColumnId::Namespace)
            .unwrap();
        assert!(namespace.visible);
        let app = two_clusters();
        assert!(
            !app.screens[0]
                .layout
                .columns
                .iter()
                .any(|c| c.id == ColumnId::Namespace && c.visible)
        );
    }

    #[test]
    fn no_clusters_at_all_is_a_message_not_a_panic() {
        let mut app = App::new(Vec::new());
        press(&mut app, KeyCode::Char('1'));
        press(&mut app, KeyCode::Char(']'));
        press(&mut app, KeyCode::Char('j'));
        assert!(app.session().tab.is_none());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 20)).unwrap();
        terminal.draw(|frame| app.render(frame, 0)).unwrap();
        let drawn = crate::ui::screen_text(terminal.backend().buffer());
        assert!(drawn.contains("No clusters configured"), "{drawn}");
    }

    #[test]
    fn a_frame_names_every_tab_and_the_open_tabs_scope() {
        let mut app = two_clusters();
        app.tab = 1;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 20)).unwrap();
        terminal.draw(|frame| app.render(frame, 0)).unwrap();
        let drawn = crate::ui::screen_text(terminal.backend().buffer());
        assert!(drawn.contains("1 qa/dev"), "{drawn}");
        assert!(drawn.contains("4 prod"), "{drawn}");
        assert!(drawn.contains("Pods"), "{drawn}");
        assert!(drawn.contains("● qa/qa"), "{drawn}");
        assert!(drawn.contains("Reading qa/qa"), "{drawn}");
    }
}
