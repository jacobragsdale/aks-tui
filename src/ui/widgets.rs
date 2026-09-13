//! The pieces of the frame that are not a table: the tab bar, the search
//! row, the status bar, the scrollbar, and the help modal.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use super::theme::theme;
use crate::app::keys;
use crate::app::screen::Target;
use crate::app::shell::{Focus, Level, Panes, Shell};
use crate::text_input::{TextInput, field_window};

/// The frames of the spinner that turns while a read runs.
const SPINNER: [char; 4] = ['◐', '◓', '◑', '◒'];

/// What the search row says before anything is typed.
pub const PODS_PLACEHOLDER: &str =
    "Type / to search pods, or status:crash owner:orders-api app: node:";

/// The `key:value` filters the search box takes, for the help. The README is
/// otherwise the only place the grammar is written down.
const FILTERS: &[(&str, &str)] = &[(
    "Filters",
    "name: ns: status: owner: app: node: — every word and filter must match",
)];

/// Which pane a frame is asking a screen to draw, at the width it has.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Pane {
    Table,
    Details,
}

/// One tab as the bar draws it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TabLabel {
    pub label: String,
    /// What the bar falls back to when it is narrow.
    pub short: String,
    pub badge: Option<String>,
}

/// A tab's body: the search row, then the table and the details pane laid
/// out to fit — side by side, stacked, or one at a time with `Tab` saying
/// which. The screen draws each pane it is asked for; this decides where.
pub fn render_panes(
    frame: &mut Frame,
    shell: &mut Shell,
    area: Rect,
    input: &TextInput,
    placeholder: &str,
    mut draw: impl FnMut(&mut Frame, &mut Shell, Pane, Rect),
) {
    let [search, body] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(3)])
        .areas(area);
    let focused = shell.focus == Focus::Search;
    render_search_row(frame, shell, search, input, focused, placeholder);

    match Shell::panes(area.width) {
        Panes::SideBySide => {
            let [table, details] = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
                .areas(body);
            draw(frame, shell, Pane::Table, table);
            draw(frame, shell, Pane::Details, details);
        }
        Panes::Stacked => {
            let [table, details] = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
                .areas(body);
            draw(frame, shell, Pane::Table, table);
            draw(frame, shell, Pane::Details, details);
        }
        Panes::One if shell.focus == Focus::Details => draw(frame, shell, Pane::Details, body),
        Panes::One => draw(frame, shell, Pane::Table, body),
    }
}

/// Which spinner frame this instant shows. Driven by the clock rather than by
/// a counter, so a slow frame does not make the spinner stutter.
#[must_use]
pub fn spinner_frame(millis: u128) -> char {
    SPINNER[(millis / 120) as usize % SPINNER.len()]
}

/// The tab bar: one tab per scope, numbered for the first nine, with a badge
/// where a tab has something to say. Names shorten before any is dropped.
pub fn render_tab_bar(
    frame: &mut Frame,
    shell: &mut Shell,
    area: Rect,
    active: usize,
    tabs: &[TabLabel],
) {
    let palette = theme();
    let full_width: usize = tabs
        .iter()
        .map(|tab| {
            tab.label.chars().count() + 4 + tab.badge.as_ref().map_or(0, |b| b.chars().count() + 1)
        })
        .sum();
    let short = full_width + 2 > usize::from(area.width);
    let mut spans = Vec::new();
    let mut column = area.x;
    for (index, tab) in tabs.iter().enumerate() {
        let name = if short { &tab.short } else { &tab.label };
        let label = if index < 9 {
            format!(" {} {name}", index + 1)
        } else {
            format!(" {name}")
        };
        let width = u16::try_from(label.chars().count()).unwrap_or(u16::MAX);
        let style = if index == active {
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.muted)
        };
        spans.push(Span::styled(label, style));
        let mut hit_width = width;
        if let Some(badge) = &tab.badge {
            let badge = format!(" {badge}");
            hit_width += u16::try_from(badge.chars().count()).unwrap_or(0);
            spans.push(Span::styled(badge, Style::default().fg(palette.error)));
        }
        if column < area.right() {
            shell.region(
                Rect::new(column, area.y, hit_width.min(area.right() - column), 1),
                Target::Tab(index),
            );
        }
        column += hit_width;
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);

    // The `?` sits at the right end of the same row.
    if area.width > 2 {
        let help = Rect::new(area.right() - 2, area.y, 1, 1);
        frame.render_widget(
            Paragraph::new(Span::styled("?", Style::default().fg(palette.muted))),
            help,
        );
        shell.region(help, Target::Help);
    }
}

/// The one-line search field: the `/` glyph, the text, and a `×` to clear it.
pub fn render_search_row(
    frame: &mut Frame,
    shell: &mut Shell,
    area: Rect,
    input: &TextInput,
    focused: bool,
    placeholder: &str,
) {
    let palette = theme();
    // `/ ` on the left, ` × ` on the right when there is anything to clear.
    let clearable = !input.is_empty();
    let right = if clearable { 3 } else { 0 };
    let [glyph, field, clear] = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(right),
        ])
        .areas(area);

    frame.render_widget(
        Paragraph::new(Span::styled(
            "/ ",
            Style::default().fg(if focused {
                palette.accent
            } else {
                palette.muted
            }),
        )),
        glyph,
    );

    if input.is_empty() && !focused {
        frame.render_widget(
            Paragraph::new(Span::styled(
                placeholder.to_owned(),
                Style::default().fg(palette.muted),
            )),
            field,
        );
    } else {
        let (first, caret) = field_window(input.text(), input.cursor(), field.width);
        let shown: String = input.text().chars().skip(first).collect();
        frame.render_widget(
            Paragraph::new(Span::styled(shown, Style::default().fg(palette.text))),
            field,
        );
        if focused {
            frame.set_cursor_position((field.x + caret, field.y));
        }
    }
    shell.region(field, Target::SearchField);

    if clearable {
        frame.render_widget(
            Paragraph::new(Span::styled(" × ", Style::default().fg(palette.muted))),
            clear,
        );
        shell.region(clear, Target::ClearSearch);
    }
}

/// The bottom row: what just happened, or what the keys do, and on the right
/// what the open tab holds.
pub fn render_status_bar(
    frame: &mut Frame,
    shell: &mut Shell,
    area: Rect,
    hint: &str,
    right: &str,
    right_style: Style,
) {
    let palette = theme();
    let (left, style) = match shell.notification() {
        Some((said, Level::Error)) => (said.to_owned(), Style::default().fg(palette.error)),
        Some((said, Level::Info)) => (said.to_owned(), Style::default().fg(palette.success)),
        None => (hint.to_owned(), Style::default().fg(palette.muted)),
    };
    let right_width = u16::try_from(right.chars().count()).unwrap_or(0);

    // The right-hand end is the one that cannot be guessed from the keys, so
    // it keeps its room and the hint is cut to what is left. Two spaces of
    // gap, so the two never read as one sentence.
    let left = if right_width + 3 >= area.width {
        // No room for both: what is happening beats what the keys do.
        String::new()
    } else {
        truncate(&left, usize::from(area.width - right_width - 3))
    };

    frame.render_widget(Paragraph::new(Span::styled(left, style)), area);
    if right_width < area.width {
        frame.render_widget(
            Paragraph::new(Span::styled(right.to_owned(), right_style)).alignment(Alignment::Right),
            Rect::new(area.x, area.y, area.width.saturating_sub(1), 1),
        );
    }
}

/// `text` in at most `room` cells, with an ellipsis where something was cut.
fn truncate(text: &str, room: usize) -> String {
    if text.chars().count() <= room {
        return text.to_owned();
    }
    if room <= 1 {
        return String::new();
    }
    text.chars().take(room - 1).chain(['…']).collect()
}

/// A centred box for a modal, with the screen behind it washed out where the
/// palette says to.
pub fn render_modal_frame(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    width: u16,
    height: u16,
) -> Rect {
    let palette = theme();
    if palette.dim_behind_modals {
        dim_behind(frame, area);
    }
    let width = width.min(area.width.saturating_sub(2)).max(1);
    let height = height.min(area.height.saturating_sub(2)).max(1);
    let modal = Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, modal);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(palette.border_type)
        .border_style(Style::default().fg(palette.border_focused))
        .title(Line::from(format!(" {title} ")).style(Style::default().fg(palette.accent)));
    let inner = block.inner(modal);
    frame.render_widget(block, modal);
    inner
}

/// Washes out what is behind a modal, so the modal is obviously the thing
/// taking keys.
pub fn dim_behind(frame: &mut Frame, area: Rect) {
    let buffer = frame.buffer_mut();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            buffer[(x, y)].set_style(Style::default().add_modifier(Modifier::DIM));
        }
    }
}

/// The help: every key, the filters the search box takes, then whatever is
/// wrong.
pub fn render_help(frame: &mut Frame, shell: &mut Shell, area: Rect, problems: &[String]) {
    const WIDTH: u16 = 74;
    let palette = theme();
    let entry = |keys: &str, does: &str| {
        Line::from(vec![
            Span::styled(format!("{keys:<12}"), Style::default().fg(palette.accent)),
            Span::styled(does.to_owned(), Style::default().fg(palette.body)),
        ])
    };
    let mut lines: Vec<Line> = keys::KEYS
        .iter()
        .map(|key| entry(key.keys, key.does))
        .collect();
    lines.push(Line::from(""));
    lines.extend(FILTERS.iter().map(|(label, grammar)| entry(label, grammar)));
    if !problems.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Problems",
            Style::default()
                .fg(palette.header)
                .add_modifier(Modifier::BOLD),
        )));
        for problem in problems {
            lines.push(Line::from(Span::styled(
                problem.clone(),
                Style::default().fg(palette.error),
            )));
        }
    }
    // Wrapped, and sized to what the wrapping makes of it: a refusal names
    // the fix in its second half, which a cut line lost.
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let height = u16::try_from(paragraph.line_count(WIDTH - 2) + 2).unwrap_or(u16::MAX);
    let inner = render_modal_frame(frame, area, "Keys", WIDTH, height);
    shell.region(area, Target::Help);
    frame.render_widget(paragraph, inner);
}

/// A one-character scrollbar down the right edge of a pane, drawn only when
/// there is more content than viewport.
pub fn render_scrollbar(
    frame: &mut Frame,
    area: Rect,
    offset: usize,
    viewport: usize,
    content: usize,
) {
    if content <= viewport || area.height == 0 || area.width == 0 {
        return;
    }
    let palette = theme();
    let track = area.height as usize;
    let thumb = ((track * viewport) / content).clamp(1, track);
    let travel = track.saturating_sub(thumb);
    let furthest = content - viewport;
    let at = if travel == 0 || furthest == 0 {
        0
    } else {
        (offset * travel + furthest / 2) / furthest
    };
    let buffer = frame.buffer_mut();
    for row in 0..track {
        let inside = row >= at && row < at + thumb;
        buffer[(area.x, area.y + u16::try_from(row).unwrap_or(0))]
            .set_symbol(if inside { "█" } else { "│" })
            .set_style(Style::default().fg(palette.scrollbar));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::screen_text;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn screen(width: u16, height: u16, draw: impl FnOnce(&mut Frame, &mut Shell)) -> String {
        let mut shell = Shell::default();
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                shell.begin_frame();
                draw(frame, &mut shell);
            })
            .unwrap();
        screen_text(terminal.backend().buffer())
    }

    fn tabs() -> Vec<TabLabel> {
        [
            ("qa/dev", "dev"),
            ("qa/qa", "qa"),
            ("qa/uat", "uat"),
            ("prod", "prod"),
        ]
        .into_iter()
        .map(|(label, short)| TabLabel {
            label: label.to_owned(),
            short: short.to_owned(),
            badge: None,
        })
        .collect()
    }

    #[test]
    fn the_tab_bar_numbers_every_tab_and_paints_a_badge() {
        let mut tabs = tabs();
        tabs[0].badge = Some("✗ 3".into());
        let drawn = screen(80, 1, |frame, shell| {
            render_tab_bar(frame, shell, Rect::new(0, 0, 80, 1), 0, &tabs);
        });
        assert!(drawn.contains("1 qa/dev ✗ 3"), "{drawn}");
        assert!(drawn.contains("2 qa/qa"), "{drawn}");
        assert!(drawn.contains("4 prod"), "{drawn}");
        assert!(drawn.trim_end().ends_with('?'), "{drawn}");
    }

    #[test]
    fn a_narrow_tab_bar_shortens_the_names_rather_than_dropping_one() {
        let drawn = screen(30, 1, |frame, shell| {
            render_tab_bar(frame, shell, Rect::new(0, 0, 30, 1), 0, &tabs());
        });
        assert!(drawn.contains("1 dev"), "{drawn}");
        assert!(drawn.contains("4 prod"), "{drawn}");
        assert!(!drawn.contains("qa/dev"), "{drawn}");
    }

    #[test]
    fn a_click_on_a_tab_lands_on_that_tab() {
        let mut shell = Shell::default();
        let mut terminal = Terminal::new(TestBackend::new(80, 1)).unwrap();
        terminal
            .draw(|frame| {
                shell.begin_frame();
                render_tab_bar(frame, &mut shell, Rect::new(0, 0, 80, 1), 0, &tabs());
            })
            .unwrap();
        assert_eq!(shell.hit(3, 0), Some(&Target::Tab(0)));
        assert_eq!(shell.hit(12, 0), Some(&Target::Tab(1)));
        assert_eq!(shell.hit(78, 0), Some(&Target::Help));
    }

    #[test]
    fn the_search_row_shows_the_placeholder_until_something_is_typed() {
        let drawn = screen(80, 1, |frame, shell| {
            render_search_row(
                frame,
                shell,
                Rect::new(0, 0, 80, 1),
                &TextInput::default(),
                false,
                PODS_PLACEHOLDER,
            );
        });
        assert!(drawn.starts_with("/ Type / to search"), "{drawn}");
        assert!(!drawn.contains('×'), "nothing to clear yet: {drawn}");

        let drawn = screen(80, 1, |frame, shell| {
            render_search_row(
                frame,
                shell,
                Rect::new(0, 0, 80, 1),
                &TextInput::new("orders"),
                true,
                PODS_PLACEHOLDER,
            );
        });
        assert!(drawn.contains("orders"), "{drawn}");
        assert!(drawn.contains('×'), "{drawn}");
    }

    #[test]
    fn the_status_bar_says_the_hint_then_the_notification_then_the_error() {
        let drawn = screen(100, 1, |frame, shell| {
            render_status_bar(
                frame,
                shell,
                Rect::new(0, 0, 100, 1),
                "↑↓ move",
                "● 41 pods · 2s ago",
                Style::default(),
            );
        });
        assert!(drawn.contains("↑↓ move"), "{drawn}");
        assert!(drawn.contains("41 pods"), "{drawn}");

        let mut shell = Shell::default();
        shell.set_status("Copied orders-api-7d9f5b-abc12");
        let mut terminal = Terminal::new(TestBackend::new(100, 1)).unwrap();
        terminal
            .draw(|frame| {
                shell.begin_frame();
                render_status_bar(
                    frame,
                    &mut shell,
                    Rect::new(0, 0, 100, 1),
                    "↑↓ move",
                    "● 41 pods",
                    Style::default(),
                );
            })
            .unwrap();
        let drawn = screen_text(terminal.backend().buffer());
        assert!(drawn.contains("Copied orders-api"), "{drawn}");
        assert!(!drawn.contains("↑↓ move"), "the notification wins: {drawn}");
    }

    #[test]
    fn the_two_halves_of_the_status_bar_never_run_into_each_other() {
        for width in [40, 60, 80, 100, 120] {
            let drawn = screen(width, 1, |frame, shell| {
                render_status_bar(
                    frame,
                    shell,
                    Rect::new(0, 0, width, 1),
                    "↑↓/jk move  / search  S sort  r refresh  ? help",
                    "! qa/dev: Unable to connect",
                    Style::default(),
                );
            });
            assert!(
                drawn.contains("Unable to connect"),
                "what is wrong survives every width: {width} {drawn}"
            );
            assert!(
                !drawn.contains("sor!") && !drawn.contains("movе!"),
                "the hint is cut rather than written over: {width} {drawn}"
            );
        }
    }

    #[test]
    fn a_hint_is_cut_with_an_ellipsis_rather_than_in_the_middle_of_nothing() {
        assert_eq!(truncate("↑↓/jk move", 20), "↑↓/jk move");
        assert_eq!(truncate("↑↓/jk move", 6), "↑↓/jk…");
        assert_eq!(truncate("↑↓/jk move", 1), "");
        assert_eq!(truncate("", 0), "");
    }

    #[test]
    fn the_help_lists_the_keys_the_filters_and_the_problems_under_them() {
        let drawn = screen(100, 30, |frame, shell| {
            render_help(
                frame,
                shell,
                Rect::new(0, 0, 100, 30),
                &["qa/dev: Unable to connect to the server".to_owned()],
            );
        });
        assert!(drawn.contains("Keys"), "{drawn}");
        assert!(drawn.contains("read this tab again"), "{drawn}");
        assert!(drawn.contains("Filters"), "{drawn}");
        assert!(drawn.contains("owner:"), "{drawn}");
        assert!(drawn.contains("Problems"), "{drawn}");
        assert!(drawn.contains("Unable to connect"), "{drawn}");
    }

    #[test]
    fn the_scrollbar_appears_only_when_there_is_more_than_fits() {
        let drawn = screen(3, 10, |frame, _| {
            render_scrollbar(frame, Rect::new(2, 0, 1, 10), 0, 10, 10);
        });
        assert!(!drawn.contains('█') && !drawn.contains('│'), "{drawn}");

        let drawn = screen(3, 10, |frame, _| {
            render_scrollbar(frame, Rect::new(2, 0, 1, 10), 0, 10, 40);
        });
        assert!(drawn.contains('█'), "{drawn}");
        assert_eq!(drawn.lines().next().unwrap().trim_start(), "█", "{drawn}");
        assert_eq!(drawn.lines().last().unwrap().trim_start(), "│", "{drawn}");
    }

    #[test]
    fn the_spinner_turns_with_the_clock() {
        assert_eq!(spinner_frame(0), SPINNER[0]);
        assert_eq!(spinner_frame(120), SPINNER[1]);
        assert_eq!(spinner_frame(480), SPINNER[0], "and comes round again");
    }
}
