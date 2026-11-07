use ratatui::{
    layout::{Alignment, Constraint, Layout, Margin, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Cell, Clear, Padding, Paragraph, Row, Scrollbar,
        ScrollbarOrientation, ScrollbarState, Table, TableState, Wrap,
    },
    Frame,
};

use crate::tui::app::{App, Screen, Tone};
use crate::tui::model::Grid;
use crate::tui::theme;

/// Below this width the module sidebar folds into the header.
const SIDEBAR_MIN_WIDTH: u16 = 90;
const SIDEBAR_WIDTH: u16 = 22;
const COLUMN_SPACING: u16 = 2;

pub fn render(app: &mut App, frame: &mut Frame) {
    let area = frame.area();
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(5),
        Constraint::Length(1),
    ])
    .areas(area);
    render_header(app, frame, header);
    if area.width >= SIDEBAR_MIN_WIDTH {
        let [side, main] =
            Layout::horizontal([Constraint::Length(SIDEBAR_WIDTH), Constraint::Min(20)])
                .areas(body);
        render_sidebar(app, frame, side);
        render_main(app, frame, main);
    } else {
        render_main(app, frame, body);
    }
    render_footer(app, frame, footer);
    if let Some(modal) = app.modal.as_mut() {
        render_modal(frame, area, modal);
    }
    if app.help {
        render_help(frame, area);
    }
}

fn spinner(app: &App) -> &'static str {
    theme::SPINNER[(app.tick as usize) % theme::SPINNER.len()]
}

fn render_header(app: &App, frame: &mut Frame, area: Rect) {
    let mut left = vec![Span::styled(" ◆ groundzero ", theme::brand())];
    if let Some(url) = app.resolved.web_url() {
        left.push(Span::styled(
            url.trim_start_matches("https://").to_string(),
            theme::muted(),
        ));
    }
    if area.width < SIDEBAR_MIN_WIDTH {
        left.push(Span::raw("  "));
        left.push(Span::styled(
            format!("{} {}", app.screen.icon(), app.screen.title()),
            theme::title(),
        ));
    }
    let (dot, who) = if app.signed_in() {
        let name = app
            .resolved
            .profile
            .username
            .clone()
            .unwrap_or_else(|| "signed in".to_string());
        (Span::styled("● ", Style::default().fg(theme::OK)), name)
    } else {
        (
            Span::styled("● ", Style::default().fg(theme::ERROR)),
            "not signed in".to_string(),
        )
    };
    let busy = if app.is_busy() { spinner(app) } else { " " };
    let right = Line::from(vec![
        dot,
        Span::styled(who, theme::muted()),
        Span::raw("  "),
        Span::styled(busy, Style::default().fg(theme::ACCENT)),
        Span::raw(" "),
    ]);
    frame.render_widget(Paragraph::new(Line::from(left)), area);
    frame.render_widget(Paragraph::new(right).alignment(Alignment::Right), area);
}

fn render_sidebar(app: &App, frame: &mut Frame, area: Rect) {
    let lines: Vec<Line> = Screen::ALL
        .iter()
        .enumerate()
        .map(|(i, screen)| {
            let active = *screen == app.screen;
            let marker = if active { "▌" } else { " " };
            let style = if active {
                Style::default()
                    .fg(theme::ACCENT)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Line::from(vec![
                Span::styled(marker, Style::default().fg(theme::ACCENT)),
                Span::styled(format!("{} ", i + 1), theme::muted()),
                Span::styled(format!("{} {}", screen.icon(), screen.title()), style),
            ])
        })
        .collect();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::border(false))
        .title(Span::styled(" Modules ", theme::muted()))
        .padding(Padding::vertical(1));
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_main(app: &mut App, frame: &mut Frame, area: Rect) {
    let screen = app.screen;
    let state = app.module();
    let crumbs = state.breadcrumb(screen);
    let mut title = vec![Span::raw(" ")];
    for (i, part) in crumbs.iter().enumerate() {
        if i > 0 {
            title.push(Span::styled(" › ", theme::muted()));
        }
        let style = if i + 1 == crumbs.len() {
            theme::title()
        } else {
            theme::muted()
        };
        title.push(Span::styled(part.clone(), style));
    }
    title.push(Span::raw(" "));
    let grid = state.grid();
    let count = if grid.is_empty() {
        String::new()
    } else if grid.filter.is_empty() {
        format!(" {} rows ", grid.rows.len())
    } else {
        format!(" {}/{} rows ", grid.visible_len(), grid.rows.len())
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::border(true))
        .title(Line::from(title))
        .title(Line::from(Span::styled(count, theme::muted())).alignment(Alignment::Right));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let kinds = screen.kinds();
    let show_filter = app.editing_filter || !grid.filter.is_empty();
    let [tabs, filter, content] = Layout::vertical([
        Constraint::Length(if kinds.len() > 1 { 2 } else { 0 }),
        Constraint::Length(u16::from(show_filter)),
        Constraint::Min(1),
    ])
    .areas(inner);
    if kinds.len() > 1 {
        render_kind_tabs(app, frame, tabs);
    }
    if show_filter {
        render_filter_bar(app, frame, filter);
    }
    render_content(app, frame, content);
}

fn render_kind_tabs(app: &App, frame: &mut Frame, area: Rect) {
    let selected = app.module().kind;
    let mut spans = vec![Span::raw(" ")];
    for (i, kind) in app.screen.kinds().iter().enumerate() {
        let style = if i == selected {
            theme::active_tab()
        } else {
            theme::muted()
        };
        spans.push(Span::styled(format!(" {} ", kind.label), style));
        spans.push(Span::raw(" "));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_filter_bar(app: &App, frame: &mut Frame, area: Rect) {
    let grid = app.module().grid();
    let cursor = if app.editing_filter { "▏" } else { "" };
    let line = Line::from(vec![
        Span::styled(" / ", theme::key_hint()),
        Span::raw(grid.filter.clone()),
        Span::styled(cursor, Style::default().fg(theme::ACCENT)),
        Span::styled(
            format!("   {} match(es)", grid.visible_len()),
            theme::muted(),
        ),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn render_content(app: &mut App, frame: &mut Frame, area: Rect) {
    let spin = spinner(app);
    let label = app.screen.kinds()[app.module().kind].label.to_lowercase();
    let state = app.module();
    if let Some(err) = &state.error {
        let lines = vec![
            Line::from(Span::styled(
                "Couldn't load this view",
                Style::default()
                    .fg(theme::ERROR)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(err.clone()),
            Line::from(""),
            Line::from(vec![
                Span::styled("r", theme::key_hint()),
                Span::styled(" retry   ", theme::muted()),
                Span::styled("⌫", theme::key_hint()),
                Span::styled(" back", theme::muted()),
            ]),
        ];
        render_centered(frame, area, lines);
        return;
    }
    if state.grid().is_empty() {
        let text = if state.is_loading() {
            Line::from(vec![
                Span::styled(format!("{spin} "), Style::default().fg(theme::ACCENT)),
                Span::styled(format!("Loading {label}…"), theme::muted()),
            ])
        } else {
            Line::from(Span::styled(format!("No {label} yet"), theme::muted()))
        };
        render_centered(frame, area, vec![text]);
        return;
    }
    if state.grid().visible_len() == 0 {
        let text = Line::from(Span::styled(
            "Nothing matches the filter · Esc clears it",
            theme::muted(),
        ));
        render_centered(frame, area, vec![text]);
        return;
    }
    // Header row takes one line; the rest are data rows.
    let viewport = area.height.saturating_sub(1).max(1) as usize;
    app.grid_viewport = viewport;
    let grid = app.module_mut().grid_mut();
    grid.follow_selection(viewport);
    render_grid(grid, frame, area, viewport);
}

fn render_centered(frame: &mut Frame, area: Rect, lines: Vec<Line>) {
    let height = lines.len() as u16;
    let top = area.y + area.height.saturating_sub(height) / 2;
    let rect = Rect::new(area.x, top, area.width, height.min(area.height));
    frame.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
        rect,
    );
}

fn render_grid(grid: &Grid, frame: &mut Frame, area: Rect, viewport: usize) {
    let table_area = area.inner(Margin {
        horizontal: 1,
        vertical: 0,
    });
    let first_col = grid.col_offset.min(grid.cols.len().saturating_sub(1));
    let mut shown = 0;
    let mut used: u16 = 2;
    for width in &grid.widths[first_col..] {
        if shown > 0 && used + width + COLUMN_SPACING > table_area.width {
            break;
        }
        used += width + COLUMN_SPACING;
        shown += 1;
    }
    let cols = first_col..first_col + shown.max(1);

    let header = Row::new(
        grid.cols[cols.clone()]
            .iter()
            .map(|c| Cell::from(c.clone()).style(theme::header_cell())),
    );
    let start = grid.row_offset;
    let end = (start + viewport).min(grid.matches.len());
    let rows: Vec<Row> = grid.matches[start..end]
        .iter()
        .map(|&row| {
            Row::new(cols.clone().map(|c| {
                let width = grid.widths[c] as usize;
                Cell::from(fit(&grid.rows[row][c], width))
            }))
        })
        .collect();
    let widths: Vec<Constraint> = grid.widths[cols.clone()]
        .iter()
        .map(|w| Constraint::Length(*w))
        .collect();
    let mut state = TableState::new().with_selected(Some(grid.selected - start));
    let table = Table::new(rows, widths)
        .header(header)
        .column_spacing(COLUMN_SPACING)
        .highlight_symbol("▸ ")
        .row_highlight_style(theme::selected_row());
    frame.render_stateful_widget(table, table_area, &mut state);

    if grid.matches.len() > viewport {
        let mut bar = ScrollbarState::new(grid.matches.len()).position(grid.selected);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .thumb_style(Style::default().fg(theme::ACCENT))
                .track_style(theme::muted()),
            area.inner(Margin {
                horizontal: 0,
                vertical: 1,
            }),
            &mut bar,
        );
    }
    let hidden_left = cols.start;
    let hidden_right = grid.cols.len() - cols.end;
    if hidden_left + hidden_right > 0 {
        let hint = format!(
            "{}{} columns · [ ] scroll ",
            if hidden_left > 0 {
                format!("◀ {hidden_left}  ")
            } else {
                String::new()
            },
            if hidden_right > 0 {
                format!("{hidden_right} more ▶")
            } else {
                String::new()
            },
        );
        let y = area.y + area.height.saturating_sub(1);
        let rect = Rect::new(area.x, y, area.width.saturating_sub(1), 1);
        if grid.matches.len() < viewport {
            frame.render_widget(
                Paragraph::new(Span::styled(hint, theme::muted())).alignment(Alignment::Right),
                rect,
            );
        }
    }
}

/// Truncates to `width` display chars with an ellipsis; newlines flattened.
fn fit(text: &str, width: usize) -> String {
    let flat = text.replace(['\n', '\r', '\t'], " ");
    if flat.chars().count() <= width {
        return flat;
    }
    let kept: String = flat.chars().take(width.saturating_sub(1)).collect();
    format!("{kept}…")
}

fn render_footer(app: &App, frame: &mut Frame, area: Rect) {
    let (icon, color) = match app.status.tone {
        Tone::Info => ("›", theme::ACCENT),
        Tone::Ok => ("✓", theme::OK),
        Tone::Error => ("✗", theme::ERROR),
    };
    let status = Line::from(vec![
        Span::styled(format!(" {icon} "), Style::default().fg(color)),
        Span::styled(app.status.text.clone(), Style::default().fg(color)),
    ]);
    let hints: &[(&str, &str)] = if app.editing_filter {
        &[("⏎", "keep"), ("esc", "clear")]
    } else if app.modal.is_some() {
        &[("↑↓", "scroll"), ("esc", "close")]
    } else {
        &[
            ("1-8", "module"),
            ("tab", "resource"),
            ("⏎", "open"),
            ("/", "filter"),
            ("r", "reload"),
            ("?", "help"),
            ("q", "quit"),
        ]
    };
    let mut spans = Vec::new();
    for (key, label) in hints {
        spans.push(Span::styled(*key, theme::key_hint()));
        spans.push(Span::styled(format!(" {label}  "), theme::muted()));
    }
    frame.render_widget(Paragraph::new(status), area);
    frame.render_widget(
        Paragraph::new(Line::from(spans)).alignment(Alignment::Right),
        area,
    );
}

fn centered(area: Rect, width_pct: u16, height_pct: u16) -> Rect {
    let width = (area.width * width_pct / 100).max(30).min(area.width);
    let height = (area.height * height_pct / 100).max(8).min(area.height);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

fn render_modal(frame: &mut Frame, area: Rect, modal: &mut crate::tui::app::Modal) {
    let rect = centered(area, 80, 85);
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::border(true))
        .title(Span::styled(format!(" {} ", modal.title), theme::title()))
        .title(
            Line::from(Span::styled(
                format!(
                    " {}/{} ",
                    (modal.scroll + 1).min(modal.lines.len()),
                    modal.lines.len()
                ),
                theme::muted(),
            ))
            .alignment(Alignment::Right),
        )
        .padding(Padding::horizontal(1));
    let inner = block.inner(rect);
    modal.viewport = inner.height as usize;
    let lines: Vec<Line> = modal
        .lines
        .iter()
        .skip(modal.scroll)
        .take(modal.viewport)
        .map(|l| highlight_json(l))
        .collect();
    frame.render_widget(Paragraph::new(lines).block(block), rect);
    if modal.lines.len() > modal.viewport {
        let mut bar = ScrollbarState::new(modal.lines.len().saturating_sub(modal.viewport))
            .position(modal.scroll);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .thumb_style(Style::default().fg(theme::ACCENT))
                .track_style(theme::muted()),
            rect.inner(Margin {
                horizontal: 0,
                vertical: 1,
            }),
            &mut bar,
        );
    }
}

/// Colors one line of pretty-printed JSON: keys, strings, numbers, literals.
fn highlight_json(line: &str) -> Line<'static> {
    let indent_len = line.len() - line.trim_start().len();
    let (indent, rest) = line.split_at(indent_len);
    let mut spans = vec![Span::raw(indent.to_string())];
    let (key, value) = match split_key(rest) {
        Some((key, value)) => (Some(key), value),
        None => (None, rest),
    };
    if let Some(key) = key {
        spans.push(Span::styled(key.to_string(), theme::json_key()));
        spans.push(Span::styled(": ", theme::muted()));
    }
    let trimmed = value.trim_end_matches(',');
    let comma = &value[trimmed.len()..];
    let style = match trimmed.chars().next() {
        Some('"') => theme::json_string(),
        Some(c) if c == '-' || c.is_ascii_digit() => theme::json_number(),
        Some('t' | 'f' | 'n') => theme::json_literal(),
        _ => theme::muted(),
    };
    spans.push(Span::styled(trimmed.to_string(), style));
    spans.push(Span::styled(comma.to_string(), theme::muted()));
    Line::from(spans)
}

/// `"key": value` → (`"key"`, `value`), honoring escaped quotes in the key.
fn split_key(text: &str) -> Option<(&str, &str)> {
    if !text.starts_with('"') {
        return None;
    }
    let bytes = text.as_bytes();
    let mut i = 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => break,
            _ => i += 1,
        }
    }
    let after = text.get(i + 1..)?;
    let value = after.strip_prefix(": ")?;
    Some((&text[..=i], value))
}

fn render_help(frame: &mut Frame, area: Rect) {
    let rect = centered(area, 56, 70);
    frame.render_widget(Clear, rect);
    let sections: [(&str, &[(&str, &str)]); 3] = [
        (
            "Navigate",
            &[
                ("1 – 8", "switch module"),
                ("tab ← →", "switch resource"),
                ("↑ ↓  j k", "move selection"),
                ("PgUp PgDn", "page"),
                ("g  G", "first / last row"),
                ("[  ]", "scroll columns"),
            ],
        ),
        (
            "Act",
            &[
                ("⏎", "open / drill in"),
                ("⌫  esc", "go back"),
                ("/", "filter rows (live)"),
                ("r", "reload"),
            ],
        ),
        ("App", &[("?", "toggle help"), ("q  ctrl-c", "quit")]),
    ];
    let mut lines = Vec::new();
    for (title, keys) in sections {
        lines.push(Line::from(Span::styled(title, theme::brand())));
        for (key, what) in keys {
            lines.push(Line::from(vec![
                Span::styled(format!("  {key:<12}"), theme::key_hint()),
                Span::raw(*what),
            ]));
        }
        lines.push(Line::from(""));
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::border(true))
        .title(Span::styled(" Keys ", theme::title()))
        .title_bottom(
            Line::from(Span::styled(" esc to close ", theme::muted())).alignment(Alignment::Right),
        )
        .padding(Padding::new(2, 2, 1, 0));
    frame.render_widget(Paragraph::new(lines).block(block), rect);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(line: &Line) -> Vec<String> {
        line.spans.iter().map(|s| s.content.to_string()).collect()
    }

    #[test]
    fn splits_json_keys_and_values() {
        let line = highlight_json(r#"  "name": "sales","#);
        assert_eq!(texts(&line), vec!["  ", "\"name\"", ": ", "\"sales\"", ","]);
        let line = highlight_json(r#"  "a\"b": 3"#);
        assert_eq!(texts(&line)[1], r#""a\"b""#);
        let line = highlight_json("  }");
        assert_eq!(texts(&line), vec!["  ", "}", ""]);
    }

    #[test]
    fn fits_cells_to_width() {
        assert_eq!(fit("abc", 5), "abc");
        assert_eq!(fit("abcdef", 4), "abc…");
        assert_eq!(fit("a\nb", 5), "a b");
    }
}
