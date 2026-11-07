//! What the SQL shell draws: entries printed into scrollback, the banner,
//! and the live region (spinner, input between rules, mode line).

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::code_ui::items::Item;
use crate::sql::results;
use crate::sql::state::{Entry, ShellState};
use crate::tui::theme;

/// Input lines shown before the box scrolls.
const MAX_INPUT_ROWS: usize = 10;

pub fn entry_lines(entry: &Entry, width: usize) -> Vec<Line<'static>> {
    let mut lines = match entry {
        Entry::Statement(sql) => {
            let mut rows = Item::User(sql.clone()).render(width);
            rows.pop();
            rows
        }
        Entry::Rows {
            page,
            elapsed,
            expanded,
        } => {
            let mut rows = results::render(page, width, *expanded);
            rows.push(results::summary(page, *elapsed));
            rows
        }
        Entry::Done { message, elapsed } => {
            let mut text = message.clone();
            if let Some(elapsed) = elapsed {
                text.push_str(&format!(" · {:.2}s", elapsed.as_secs_f64()));
            }
            vec![Line::from(vec![
                Span::styled("✓ ", Style::default().fg(theme::OK)),
                Span::raw(text),
            ])]
        }
        Entry::Error { message, statement } => {
            let red = Style::default().fg(theme::ERROR);
            let mut rows: Vec<Line<'static>> =
                crate::tui::chat::wrap(message, width.saturating_sub(2))
                    .into_iter()
                    .enumerate()
                    .map(|(i, piece)| {
                        let lead = if i == 0 { "✗ " } else { "  " };
                        Line::from(vec![Span::styled(lead, red), Span::styled(piece, red)])
                    })
                    .collect();
            if let Some(sql) = statement {
                let first = sql.lines().next().unwrap_or_default();
                let more = if sql.lines().count() > 1 { " …" } else { "" };
                rows.push(Line::from(Span::styled(
                    format!("  in: {first}{more}"),
                    theme::muted(),
                )));
            }
            rows
        }
        Entry::Notice(text) => vec![Line::from(Span::styled(text.clone(), theme::muted()))],
        Entry::Info(rows) => rows
            .iter()
            .map(|r| Line::from(Span::styled(format!("  {r}"), theme::muted())))
            .collect(),
    };
    lines.push(Line::from(""));
    lines
}

/// The first characters of a session id, enough to recognise it.
pub fn short_id(session: &str) -> String {
    session.chars().take(8).collect()
}

/// The rounded welcome box: warehouse, session, compute, and a hint.
pub fn banner(
    warehouse: &str,
    session: &str,
    compute: &str,
    reused: bool,
    width: usize,
) -> Vec<Line<'static>> {
    let inner = width.saturating_sub(4).clamp(20, 76);
    let short = short_id(session);
    let how = if reused { "reused" } else { "started" };
    let body = vec![
        Line::from(vec![
            Span::styled("✻ ", theme::brand()),
            Span::styled("gz sql", theme::title()),
            Span::styled(format!("  ·  {warehouse}"), theme::muted()),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            format!("session {short}… ({how}) · compute {compute}"),
            theme::muted(),
        )),
        Line::from(Span::styled(
            "statements end with ;  ·  \\help for commands  ·  ctrl+d quits",
            theme::muted(),
        )),
    ];
    let border = theme::border(true);
    let mut out = vec![Line::from(Span::styled(
        format!("╭{}╮", "─".repeat(inner + 2)),
        border,
    ))];
    for line in body {
        let pad = inner.saturating_sub(line.width());
        let mut spans = vec![Span::styled("│ ", border)];
        spans.extend(line.spans);
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(" │", border));
        out.push(Line::from(spans));
    }
    out.push(Line::from(Span::styled(
        format!("╰{}╯", "─".repeat(inner + 2)),
        border,
    )));
    out.push(Line::from(""));
    out
}

fn input_rows(state: &ShellState) -> usize {
    state.input.lines().len().clamp(1, MAX_INPUT_ROWS)
}

/// Rows the live region needs: spinner, rules, input, mode line.
pub fn live_height(state: &ShellState, max: u16) -> u16 {
    let spinner = usize::from(state.is_busy());
    let rows = spinner + 2 + input_rows(state) + 1;
    (rows as u16).min(max.max(4))
}

fn mode_line(state: &ShellState) -> Line<'static> {
    let mut spans = vec![Span::raw("  ")];
    if state.exit_armed() {
        spans.push(Span::styled(
            "Press Ctrl+C again to exit",
            Style::default().fg(theme::WARN),
        ));
        return Line::from(spans);
    }
    spans.push(Span::styled(state.warehouse.clone(), theme::brand()));
    // Unqualified names resolve in `default`; the provider refuses USE.
    spans.push(Span::styled(" · default", theme::muted()));
    let state_style = if state.session_state == "Running" {
        Style::default().fg(theme::OK)
    } else {
        Style::default().fg(theme::WARN)
    };
    spans.push(Span::styled(" · ", theme::muted()));
    spans.push(Span::styled(state.session_state.clone(), state_style));
    let mut flags = Vec::new();
    if state.expanded {
        flags.push("\\x on");
    }
    if !state.timing {
        flags.push("timing off");
    }
    flags.push("\\help");
    spans.push(Span::styled(
        format!("  ·  {}", flags.join(" · ")),
        theme::muted(),
    ));
    Line::from(spans)
}

pub fn draw_live(frame: &mut Frame, state: &ShellState) {
    let area = frame.area();
    let width = area.width as usize;
    let mut y = area.y;
    let line = |frame: &mut Frame, content: Line<'static>, y: &mut u16| {
        if *y < area.bottom() {
            frame.render_widget(
                Paragraph::new(content),
                Rect::new(area.x, *y, area.width, 1),
            );
            *y += 1;
        }
    };
    if let Some((label, since)) = &state.running {
        let spark = theme::SPINNER[(state.tick as usize) % theme::SPINNER.len()];
        line(
            frame,
            Line::from(vec![
                Span::styled(format!("{spark} {label}… "), theme::brand()),
                Span::styled(
                    format!("({}s · esc to cancel)", since.elapsed().as_secs()),
                    theme::muted(),
                ),
            ]),
            &mut y,
        );
    }
    let rule = || Line::from(Span::styled("─".repeat(width), theme::muted()));
    line(frame, rule(), &mut y);
    let rows = (input_rows(state) as u16).min(area.bottom().saturating_sub(y).saturating_sub(2));
    if rows > 0 {
        let prefix: Vec<Line> = (0..rows)
            .map(|i| {
                Line::from(Span::styled(
                    if i == 0 { "› " } else { "  " },
                    theme::brand(),
                ))
            })
            .collect();
        frame.render_widget(Paragraph::new(prefix), Rect::new(area.x, y, 2, rows));
        frame.render_widget(
            &state.input,
            Rect::new(area.x + 2, y, area.width.saturating_sub(2), rows),
        );
        y += rows;
    }
    line(frame, rule(), &mut y);
    line(frame, mode_line(state), &mut y);
}

/// Lines as plain text, for piped (non-terminal) output.
pub fn plain(lines: &[Line]) -> String {
    let mut out = String::new();
    for line in lines {
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        out.push_str(text.trim_end());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::history::History;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn screen(state: &mut ShellState, width: u16) -> String {
        let height = live_height(state, 20);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| draw_live(f, state)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn idle_shows_input_between_rules_and_mode_line() {
        let mut state = ShellState::new("GZWarehousePROD".into(), History::load(None));
        let out = screen(&mut state, 80);
        assert!(out.contains("› "), "{out}");
        assert!(out.contains("GZWarehousePROD · default · Running"), "{out}");
        for width in [30, 80, 140] {
            screen(&mut state, width);
        }
    }

    #[test]
    fn busy_shows_a_spinner() {
        let mut state = ShellState::new("wh".into(), History::load(None));
        state.submit("select 1;");
        let out = screen(&mut state, 80);
        assert!(out.contains("Running… (0s · esc to cancel)"), "{out}");
    }

    #[test]
    fn errors_name_the_failing_statement() {
        let lines = entry_lines(
            &Entry::Error {
                message: "Table not found".into(),
                statement: Some("select *\nfrom nope".into()),
            },
            60,
        );
        let text = plain(&lines);
        assert!(text.contains("✗ Table not found"));
        assert!(text.contains("in: select * …"));
    }
}
