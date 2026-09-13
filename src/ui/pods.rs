//! One tab's panes: the search row, the pods table, and the details pane.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use ratatui::layout::{Constraint, Layout};

use super::details::{field, quiet, refused, render_pane, section, subtitle};
use super::table::{Cell, TableSpec, render_list_table, table_geometry};
use super::textpane::render_text_pane;
use super::theme::theme;
use super::widgets::{PODS_PLACEHOLDER, Pane, render_panes, render_scrollbar};
use crate::app::scope::{SCHEMA, ScopeScreen};
use crate::app::screen::{Button, Target};
use crate::app::shell::{Focus, Shell};
use crate::columns::{ColumnConfig, ColumnId, TableLayout};
use crate::config::Tab;
use crate::kube::Pod;
use crate::search::Query;
use crate::store::ScopeData;
use crate::timestamp::{Timestamp, age};

/// Draws the tab: one row for the search box, then the table and the details
/// pane, laid out to fit.
pub fn render(
    frame: &mut Frame,
    shell: &mut Shell,
    screen: &mut ScopeScreen,
    tab: &Tab,
    data: &ScopeData,
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
            Pane::Table => render_table(frame, shell, screen, data, rect),
            Pane::Details => render_details(frame, shell, screen, tab, data, rect),
        },
    );
}

/// The table itself. The screen has already decided which rows are shown and
/// in what order; this turns the window of them that fits into cells.
pub fn render_table(
    frame: &mut Frame,
    shell: &mut Shell,
    screen: &mut ScopeScreen,
    data: &ScopeData,
    area: Rect,
) {
    let geometry = table_geometry(area);
    let available = TableLayout::available_width(geometry.inner.width);
    screen.note_width(available);
    let columns = screen.layout.visible_columns(available);

    let now = Timestamp::now();
    let mut highlighter =
        Query::new(&crate::filter::Query::parse(screen.input.text(), SCHEMA).words);

    // `window` is what records the viewport on the cursor, so a page and an
    // End know how far to move.
    let total = screen.visible().len();
    let window = geometry.window(&mut screen.cursor, total);
    let first = window.start;
    let shown: Vec<Vec<Cell>> = screen.visible()[window]
        .iter()
        .map(|at| row_cells(&data.pods[*at], &columns, &mut highlighter, now))
        .collect();

    let mut spec = TableSpec {
        title: " Pods ".to_owned(),
        status: screen.status(data),
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

/// The colour of a pod's glyph and status word, and of its whole row when it
/// is in trouble or finished: what its state reads as, at a glance.
#[must_use]
pub fn pod_style(pod: &Pod) -> Style {
    let palette = theme();
    let colour = match pod.glyph() {
        "\u{25cf}" => palette.success,
        "\u{25d0}" => palette.warning,
        "\u{2717}" => palette.error,
        _ => palette.muted,
    };
    Style::default().fg(colour)
}

/// The colour the row's other cells take: the error colour on a pod in
/// trouble, muted on one that has finished, and nothing otherwise.
fn row_style(pod: &Pod) -> Style {
    let palette = theme();
    if pod.is_unhealthy() {
        Style::default().fg(palette.error)
    } else if matches!(pod.glyph(), "\u{2713}" | "\u{25cb}") {
        Style::default().fg(palette.muted)
    } else {
        Style::default()
    }
}

/// One row's cells, in the order the visible columns are in.
fn row_cells(
    pod: &Pod,
    columns: &[ColumnConfig],
    highlighter: &mut Query,
    now: Timestamp,
) -> Vec<Cell> {
    let base = row_style(pod);
    columns
        .iter()
        .map(|column| match column.id {
            ColumnId::Name => {
                Cell::styled(pod.key.name.clone(), base).matched(highlighter.indices(&pod.key.name))
            }
            ColumnId::Namespace => Cell::styled(pod.key.namespace.clone(), base)
                .matched(highlighter.indices(&pod.key.namespace)),
            ColumnId::Ready => Cell::styled(pod.ready_label(), base),
            ColumnId::Status => {
                Cell::styled(format!("{} {}", pod.glyph(), pod.status), pod_style(pod)).matched(
                    highlighter
                        .indices(&pod.status)
                        .into_iter()
                        .map(|index| index + 2)
                        .collect(),
                )
            }
            ColumnId::Restarts => Cell::styled(pod.restarts.to_string(), base),
            ColumnId::Age => Cell::styled(age(pod.created, now), base),
            ColumnId::Node => {
                Cell::styled(pod.node.clone(), base).matched(highlighter.indices(&pod.node))
            }
            ColumnId::Ip => Cell::styled(pod.ip.clone(), base),
            ColumnId::Owner => Cell::styled(pod.owner_name().to_owned(), base)
                .matched(highlighter.indices(pod.owner_name())),
            ColumnId::Image => {
                let image = pod.containers.first().map_or("", |c| c.image.as_str());
                Cell::styled(image.to_owned(), base).matched(highlighter.indices(image))
            }
        })
        .collect()
}

/// The details pane for the pod under the cursor.
pub fn render_details(
    frame: &mut Frame,
    shell: &mut Shell,
    screen: &mut ScopeScreen,
    tab: &Tab,
    data: &ScopeData,
    area: Rect,
) {
    // With the text pane open the details keep the top and the keys go to
    // the pane; `z` gives the pane the whole area.
    if screen.pane_open && screen.pane_zoom {
        render_text_pane(frame, shell, screen, data, area);
        return;
    }
    let focused = shell.focus == Focus::Details && !screen.pane_open;
    let width = super::details::pane_width(area);
    let selected = screen.selected(data).cloned();
    let lines = match &selected {
        Some(pod) => detail_lines(pod, screen.owner_of(pod), data, width, Timestamp::now()),
        None => nothing_selected(tab, data),
    };
    if !screen.pane_open {
        render_pane(
            frame,
            shell,
            area,
            focused,
            &mut screen.details_scroll,
            lines,
        );
        register_toolbar(shell, area, screen, selected.is_some());
        return;
    }
    // The details take what they need up to just under half; the pane
    // takes the rest and never less than a few lines.
    let wanted = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .saturating_add(2);
    let top = wanted.min(area.height * 45 / 100).max(5.min(area.height));
    let [details, pane] =
        Layout::vertical([Constraint::Length(top), Constraint::Min(4)]).areas(area);
    render_pane(
        frame,
        shell,
        details,
        focused,
        &mut screen.details_scroll,
        lines,
    );
    register_toolbar(shell, details, screen, selected.is_some());
    render_text_pane(frame, shell, screen, data, pane);
}

/// The toolbar's buttons, as regions on the pane's first line: each stands
/// for the key it names. Only while the first line is the one on screen and
/// the whole row fits, so a region never sits over a wrapped word.
fn register_toolbar(shell: &mut Shell, area: Rect, screen: &ScopeScreen, have_pod: bool) {
    if !have_pod || screen.details_scroll.offset != 0 || area.height < 3 {
        return;
    }
    let inner = Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        1,
    );
    if usize::from(inner.width) < toolbar_width() {
        return;
    }
    let mut column = inner.x;
    for button in Button::ALL {
        let width = u16::try_from(button.label().chars().count() + 2).unwrap_or(0);
        shell.region(Rect::new(column, inner.y, width, 1), Target::Button(button));
        column = column.saturating_add(width + 1);
    }
}

/// `[Logs] [Bash] [Restart] [Scale] [Describe] [YAML]`, with a space after
/// each.
fn toolbar_width() -> usize {
    Button::ALL
        .iter()
        .map(|button| button.label().chars().count() + 3)
        .sum::<usize>()
        - 1
}

fn toolbar_line() -> Line<'static> {
    let palette = theme();
    let mut spans = Vec::new();
    for (at, button) in Button::ALL.iter().enumerate() {
        if at > 0 {
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled("[", Style::default().fg(palette.muted)));
        spans.push(Span::styled(
            button.label(),
            Style::default()
                .fg(palette.link)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled("]", Style::default().fg(palette.muted)));
    }
    Line::from(spans)
}

/// Everything the pane says about one pod, top to bottom: the toolbar first,
/// so its buttons are always where the regions say they are.
fn detail_lines(
    pod: &Pod,
    owner: Option<&crate::kube::Replicas>,
    data: &ScopeData,
    width: u16,
    now: Timestamp,
) -> Vec<Line<'static>> {
    let palette = theme();
    let mut lines = vec![toolbar_line()];
    lines.push(Line::from(vec![
        Span::styled(format!("{} ", pod.glyph()), pod_style(pod)),
        Span::styled(
            pod.key.name.clone(),
            Style::default()
                .fg(palette.text)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(pod.status.clone(), pod_style(pod)),
    ]));
    let created = age(pod.created, now);
    let owner_said = owner.map_or_else(
        || pod.owner_label(),
        |replicas| format!("{} · {}", pod.owner_label(), replicas.label()),
    );
    lines.push(subtitle(&[
        &format!("{}/{}", pod.key.cluster, pod.key.namespace),
        &owner_said,
        &created,
    ]));
    lines.push(Line::from(""));
    lines.push(field("Ready", pod.ready_label()));
    lines.push(field("Restarts", pod.restarts.to_string()));
    lines.push(field("Node", dash_if_empty(&pod.node)));
    lines.push(field("IP", dash_if_empty(&pod.ip)));
    lines.push(field(
        "Created",
        pod.created.map_or_else(
            || "\u{2014}".to_owned(),
            |stamp| format!("{} · {created}", stamp.calendar_date()),
        ),
    ));
    if !pod.labels.is_empty() {
        let mut spans = vec![Span::styled(
            format!("{:<width$}", "Labels", width = super::details::LABEL),
            Style::default().fg(palette.muted),
        )];
        for (at, (key, value)) in pod.labels.iter().enumerate() {
            if at > 0 {
                spans.push(Span::raw("  "));
            }
            spans.push(super::details::chip(key, value));
        }
        lines.push(Line::from(spans));
    }

    lines.push(Line::from(""));
    lines.push(section("Containers", width));
    for container in &pod.containers {
        let (mark, colour) = if container.ready {
            ("\u{2713}", palette.success)
        } else {
            ("\u{2717}", palette.error)
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{mark} "), Style::default().fg(colour)),
            Span::styled(container.name.clone(), Style::default().fg(palette.text)),
            Span::styled(
                format!("  {}  \u{21bb}{}", container.state, container.restarts),
                Style::default().fg(if container.ready {
                    palette.muted
                } else {
                    palette.error
                }),
            ),
        ]));
        lines.push(Line::from(Span::styled(
            format!("  {}", container.image),
            Style::default().fg(palette.muted),
        )));
        if let Some((reason, code)) = &container.last_termination {
            lines.push(Line::from(Span::styled(
                format!("  last exit: {reason} ({code})"),
                Style::default().fg(palette.warning),
            )));
        }
    }
    if let Some(message) = &data.error {
        lines.push(Line::from(""));
        lines.push(section("Problem", width));
        lines.push(refused(format!("last read failed: {message}")));
        lines.push(quiet("these rows are from the read before it"));
    }
    lines
}

/// What the pane says with no pod under the cursor: nothing has come back
/// yet, what went wrong, or nothing matches.
fn nothing_selected(tab: &Tab, data: &ScopeData) -> Vec<Line<'static>> {
    let scope = tab.scope.describe();
    if data.pods.is_empty() {
        return match &data.error {
            Some(message) => vec![refused(format!("{scope}: {message}"))],
            None if data.reads == 0 => vec![quiet(format!("Reading {scope}…"))],
            None => vec![quiet(format!("No pods in {scope}"))],
        };
    }
    vec![quiet("No pods match")]
}

fn dash_if_empty(value: &str) -> String {
    if value.is_empty() {
        "\u{2014}".to_owned()
    } else {
        value.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use crate::kube::tests::{crashing, pod};
    use crate::ui::screen_text;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn tab() -> Tab {
        config::parse(config::tests::TWO_CLUSTERS)
            .unwrap()
            .tabs()
            .remove(0)
    }

    fn data() -> ScopeData {
        ScopeData {
            pods: vec![
                pod("qa", "dev", "orders-api-7d9f5b-k9x2p", "Running"),
                crashing("qa", "dev", "orders-worker-5c4d3e-q8zt"),
            ],
            reads: 1,
            ..ScopeData::default()
        }
    }

    fn draw(
        width: u16,
        height: u16,
        screen: &mut ScopeScreen,
        data: &ScopeData,
    ) -> (String, Shell) {
        let mut shell = Shell::default();
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                shell.begin_frame();
                screen.refilter(data);
                render(frame, &mut shell, screen, &tab(), data, frame.area());
            })
            .unwrap();
        (screen_text(terminal.backend().buffer()), shell)
    }

    #[test]
    fn the_table_paints_a_pod_in_trouble_in_the_error_colour_from_end_to_end() {
        let data = data();
        let mut screen = ScopeScreen::default();
        let mut shell = Shell::default();
        let mut terminal = Terminal::new(TestBackend::new(120, 16)).unwrap();
        terminal
            .draw(|frame| {
                shell.begin_frame();
                screen.refilter(&data);
                render(frame, &mut shell, &mut screen, &tab(), &data, frame.area());
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let drawn = screen_text(&buffer);
        assert!(drawn.contains("Ready"), "{drawn}");
        assert!(drawn.contains("\u{2717} CrashLoopBackOff"), "{drawn}");
        assert!(drawn.contains("\u{25cf} Running"), "{drawn}");
        assert!(drawn.contains("2 · Name ↑"), "{drawn}");

        let row = (0..buffer.area.height)
            .find(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, *y)].symbol())
                    .collect::<String>()
                    .contains("orders-worker")
            })
            .expect("the crashing pod's row");
        let painted: Vec<_> = (4..12).map(|x| buffer[(x, row)].fg).collect();
        assert!(
            painted.iter().all(|colour| *colour == theme().error),
            "the whole row reads as trouble: {painted:?}"
        );
    }

    #[test]
    fn the_details_pane_names_the_pod_its_owner_and_its_containers() {
        let data = data();
        let mut screen = ScopeScreen::default();
        let (drawn, _) = draw(120, 20, &mut screen, &data);
        assert!(
            drawn.contains("orders-api-7d9f5b-k9x2p  Running"),
            "{drawn}"
        );
        assert!(drawn.contains("qa/dev · Deployment/orders-api"), "{drawn}");
        assert!(drawn.contains("Node          aks-nodepool1-0"), "{drawn}");
        assert!(drawn.contains("── Containers"), "{drawn}");
        assert!(drawn.contains("✓ api  Running  ↻0"), "{drawn}");
        assert!(
            drawn.contains("myacr.azurecr.io/team/orders-api:1.2.3"),
            "{drawn}"
        );
        assert!(drawn.contains("app=orders-api"), "{drawn}");

        screen.cursor.focus(1);
        let (drawn, _) = draw(120, 20, &mut screen, &data);
        assert!(drawn.contains("✗ api  CrashLoopBackOff  ↻9"), "{drawn}");
        assert!(drawn.contains("last exit: Error (1)"), "{drawn}");
    }

    #[test]
    fn a_query_narrows_the_table_and_the_border_counts_what_is_left() {
        let data = data();
        let mut screen = ScopeScreen::default();
        screen.input.set_text("worker");
        let (drawn, _) = draw(120, 14, &mut screen, &data);
        assert!(drawn.contains("1/2 · Name ↑"), "{drawn}");
        assert!(drawn.contains("orders-worker"), "{drawn}");
        assert!(!drawn.contains("orders-api-7d9f5b-k9x2p"), "{drawn}");
        screen.input.set_text("nothing");
        let (drawn, _) = draw(120, 14, &mut screen, &data);
        assert!(drawn.contains("No pods match"), "{drawn}");
    }

    #[test]
    fn the_pane_says_what_it_is_waiting_for_what_went_wrong_and_when_there_is_nothing() {
        let mut screen = ScopeScreen::default();
        let (drawn, _) = draw(120, 14, &mut screen, &ScopeData::default());
        assert!(drawn.contains("Reading qa/dev…"), "{drawn}");

        let failed = ScopeData {
            error: Some("Unable to connect to the server".into()),
            reads: 1,
            ..ScopeData::default()
        };
        let (drawn, _) = draw(120, 14, &mut screen, &failed);
        assert!(drawn.contains("qa/dev: Unable to connect"), "{drawn}");

        let empty = ScopeData {
            reads: 1,
            ..ScopeData::default()
        };
        let (drawn, _) = draw(120, 14, &mut screen, &empty);
        assert!(drawn.contains("No pods in qa/dev"), "{drawn}");

        // Rows from before a failure stand, and the pane says so under them.
        let stale = ScopeData {
            error: Some("Unable to connect to the server".into()),
            ..data()
        };
        let (drawn, _) = draw(120, 24, &mut screen, &stale);
        assert!(drawn.contains("orders-api-7d9f5b-k9x2p"), "{drawn}");
        assert!(
            drawn.contains("last read failed: Unable to connect"),
            "{drawn}"
        );
    }

    #[test]
    fn a_click_lands_on_the_row_and_the_header_under_it() {
        let data = data();
        let mut screen = ScopeScreen::default();
        let (_, shell) = draw(120, 14, &mut screen, &data);
        let row = (0..14)
            .find_map(|y| match shell.hit(4, y) {
                Some(Target::Row(index)) => Some((y, *index)),
                _ => None,
            })
            .expect("a row region");
        assert_eq!(row.1, 0);
        let header = (0..14)
            .find_map(|y| match shell.hit(4, y) {
                Some(Target::Header(column)) => Some(*column),
                _ => None,
            })
            .expect("a header region");
        assert_eq!(header, ColumnId::Name);
    }

    #[test]
    fn under_seventy_columns_the_details_pane_waits_for_tab() {
        let data = data();
        let mut screen = ScopeScreen::default();
        let (drawn, _) = draw(60, 16, &mut screen, &data);
        assert!(
            !drawn.contains("Details"),
            "the table gets the room: {drawn}"
        );
        assert!(drawn.contains("orders-api"), "{drawn}");
    }
}
