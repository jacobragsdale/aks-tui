//! One tab: the list over one namespace of one cluster, and the state that is
//! this tab's alone — its cursor, its search, its sort.

use super::cursor::{ListCursor, ScrollState};
use super::screen::{AppAction, Target};
use super::shell::{Focus, Shell};
use crate::columns::{ColumnId, POD_COLUMNS, TableLayout};
use crate::text_input::TextInput;

/// The `key:` filters the pods list knows. Everything else typed is a word.
pub const SCHEMA: &[&str] = &["name", "ns", "status", "owner", "app", "node"];

pub struct ScopeScreen {
    pub cursor: ListCursor,
    pub layout: TableLayout,
    pub input: TextInput,
    pub sort: ColumnId,
    pub descending: bool,
    /// Which rows of the tab's list are shown, in the order they are shown.
    visible: Vec<usize>,
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
    pub fn status(&self, total: usize) -> String {
        let arrow = if self.descending { "↓" } else { "↑" };
        if self.visible.len() == total {
            format!("{total} · {} {arrow}", self.sort.label())
        } else {
            format!(
                "{}/{total} · {} {arrow}",
                self.visible.len(),
                self.sort.label()
            )
        }
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
