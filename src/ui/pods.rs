//! One tab's panes: the search row, the pods table, and the details pane.

use ratatui::Frame;
use ratatui::layout::Rect;

use super::details::{quiet, render_pane};
use super::table::{Cell, TableSpec, render_list_table, table_geometry};
use super::widgets::{PODS_PLACEHOLDER, Pane, render_panes, render_scrollbar};
use crate::app::scope::ScopeScreen;
use crate::app::screen::Target;
use crate::app::shell::{Focus, Shell};
use crate::columns::TableLayout;
use crate::config::Tab;

/// Draws the tab: one row for the search box, then the table and the details
/// pane, laid out to fit.
pub fn render(
    frame: &mut Frame,
    shell: &mut Shell,
    screen: &mut ScopeScreen,
    tab: &Tab,
    area: Rect,
) {
    let input = screen.input.clone();
    render_panes(
        frame,
        shell,
        area,
        &input,
        PODS_PLACEHOLDER,
        |frame, shell, pane, rect| match pane {
            Pane::Table => render_table(frame, shell, screen, tab, rect),
            Pane::Details => render_details(frame, shell, screen, tab, rect),
        },
    );
}

/// The table itself. The screen has already decided which rows are shown and
/// in what order; this turns the window of them that fits into cells.
pub fn render_table(
    frame: &mut Frame,
    shell: &mut Shell,
    screen: &mut ScopeScreen,
    _tab: &Tab,
    area: Rect,
) {
    let geometry = table_geometry(area);
    let available = TableLayout::available_width(geometry.inner.width);
    screen.note_width(available);
    let columns = screen.layout.visible_columns(available);

    let total = screen.visible().len();
    let window = geometry.window(&mut screen.cursor, total);
    let first = window.start;
    let shown: Vec<Vec<Cell>> = Vec::new();

    let mut spec = TableSpec {
        title: " Pods ".to_owned(),
        status: screen.status(total),
        focused: shell.focus == Focus::Table,
        columns: &columns,
        sorted: Some((screen.sort, if screen.descending { "↓" } else { "↑" })),
        rows: &shown,
        total,
        cursor: &mut screen.cursor,
        hovered: None,
    };
    let hits = render_list_table(frame, area, &mut spec);

    for (index, rect) in hits.rows {
        shell.region(rect, Target::Row(index));
    }
    for (column, rect) in hits.headers {
        shell.region(rect, Target::Header(column));
    }
    render_scrollbar(
        frame,
        Rect::new(
            geometry.inner.right().saturating_sub(1),
            geometry.body.y,
            1,
            geometry.body.height,
        ),
        first,
        geometry.visible_rows,
        total,
    );
}

/// The details pane for the row under the cursor.
pub fn render_details(
    frame: &mut Frame,
    shell: &mut Shell,
    screen: &mut ScopeScreen,
    tab: &Tab,
    area: Rect,
) {
    let focused = shell.focus == Focus::Details;
    let lines = vec![quiet(format!("Reading {}…", tab.scope.describe()))];
    render_pane(
        frame,
        shell,
        area,
        focused,
        &mut screen.details_scroll,
        lines,
    );
}
