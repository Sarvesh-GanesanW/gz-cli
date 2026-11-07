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

use crate::tui::app::{App, Focus, Hits, Modal, SidebarRow, Tone};
use crate::tui::chat::{self, ChatFocus, Role};
use crate::tui::markdown;
use crate::tui::model::Grid;
use crate::tui::nav::MODULES;
use crate::tui::theme;

/// Below this width the sidebar is hidden; Tab still reaches it via the palette.
const SIDEBAR_MIN_WIDTH: u16 = 70;
const SIDEBAR_WIDTH: u16 = 26;
/// Below this width, chatting folds the sidebar away so the transcript gets
/// the room; Tab brings it back.
const CHAT_WITH_SIDEBAR_MIN_WIDTH: u16 = 150;

fn sidebar_visible(app: &App, width: u16) -> bool {
    if width < SIDEBAR_MIN_WIDTH {
        return false;
    }
    let chatting = app.is_chat() && app.focus == Focus::Content;
    !(chatting && width < CHAT_WITH_SIDEBAR_MIN_WIDTH)
}
const COLUMN_SPACING: u16 = 2;
const CHAT_LIST_WIDTH: u16 = 30;
const CHAT_LIST_MIN_WIDTH: u16 = 66;
const COMPOSER_MAX_LINES: u16 = 6;
const PALETTE_ROWS: usize = 10;

pub fn render(app: &mut App, frame: &mut Frame) {
    app.hits = Hits::default();
    let area = frame.area();
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(5),
        Constraint::Length(1),
    ])
    .areas(area);
    render_header(app, frame, header);
    if sidebar_visible(app, area.width) {
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
    if app.palette.is_some() {
        render_palette(app, frame, area);
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

fn render_sidebar(app: &mut App, frame: &mut Frame, area: Rect) {
    let focused = app.focus == Focus::Sidebar;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::border(focused))
        .title(Span::styled(
            " Browse ",
            if focused {
                theme::brand()
            } else {
                theme::muted()
            },
        ))
        .padding(Padding::new(0, 0, 1, 0));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = app.sidebar_rows();
    let height = inner.height as usize;
    let offset = app.cursor.saturating_sub(height.saturating_sub(1));
    app.hits.sidebar_rows = inner;
    app.hits.sidebar_offset = offset;
    let lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .skip(offset)
        .take(height)
        .map(|(i, row)| sidebar_line(app, *row, i == app.cursor, focused, inner.width))
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn sidebar_line(
    app: &App,
    row: SidebarRow,
    at_cursor: bool,
    focused: bool,
    width: u16,
) -> Line<'static> {
    let (text, is_active) = match row {
        SidebarRow::Module(m) => {
            let arrow = if app.expanded[m] { "▾" } else { "▸" };
            let module = &MODULES[m];
            (format!(" {arrow} {} {}", module.icon, module.title), false)
        }
        SidebarRow::Resource(id) => (format!("     {}", id.resource().label), id == app.active),
    };
    let padded = format!("{text:<width$}", width = width as usize);
    let style = match (at_cursor, focused, is_active, row) {
        (true, true, _, _) => theme::selected_row(),
        (true, false, _, _) | (_, _, true, _) => Style::default()
            .fg(theme::ACCENT)
            .add_modifier(Modifier::BOLD),
        (_, _, _, SidebarRow::Module(_)) => theme::title(),
        _ => Style::default(),
    };
    Line::from(Span::styled(padded, style))
}

fn render_main(app: &mut App, frame: &mut Frame, area: Rect) {
    let focused = app.focus == Focus::Content;
    let crumbs = app.breadcrumb();
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
    let count = app
        .view()
        .map(|v| v.grid())
        .filter(|g| !g.is_empty())
        .map(|g| {
            if g.filter.is_empty() {
                format!(" {} rows ", g.rows.len())
            } else {
                format!(" {}/{} rows ", g.visible_len(), g.rows.len())
            }
        })
        .unwrap_or_default();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::border(focused))
        .title(Line::from(title))
        .title(Line::from(Span::styled(count, theme::muted())).alignment(Alignment::Right));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    app.hits.content = inner;
    if app.is_chat() {
        render_chat(app, frame, inner);
    } else {
        render_grid_view(app, frame, inner);
    }
}

fn render_grid_view(app: &mut App, frame: &mut Frame, area: Rect) {
    let show_filter = app.editing_filter || app.view().is_some_and(|v| !v.grid().filter.is_empty());
    let [filter, content] = Layout::vertical([
        Constraint::Length(u16::from(show_filter)),
        Constraint::Min(1),
    ])
    .areas(area);
    if show_filter {
        render_filter_bar(app, frame, filter);
    }
    let spin = spinner(app);
    let label = app.active.resource().label.to_lowercase();
    let Some(view) = app.view() else { return };
    if let Some(err) = &view.error {
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
            key_hints(&[("r", "retry"), ("←", "back")]),
        ];
        return render_centered(frame, content, lines);
    }
    if view.grid().is_empty() {
        let text = if view.is_loading() {
            Line::from(vec![
                Span::styled(format!("{spin} "), Style::default().fg(theme::ACCENT)),
                Span::styled(format!("Loading {label}…"), theme::muted()),
            ])
        } else {
            Line::from(Span::styled(
                "Nothing here yet · r to reload",
                theme::muted(),
            ))
        };
        return render_centered(frame, content, vec![text]);
    }
    if view.grid().visible_len() == 0 {
        let text = Line::from(Span::styled(
            "Nothing matches the filter · Esc clears it",
            theme::muted(),
        ));
        return render_centered(frame, content, vec![text]);
    }
    // The header row takes one line; the rest are data rows.
    let viewport = content.height.saturating_sub(1).max(1) as usize;
    app.grid_viewport = viewport;
    app.hits.grid_rows = Rect::new(
        content.x,
        content.y + 1,
        content.width,
        content.height.saturating_sub(1),
    );
    let focused = app.focus == crate::tui::app::Focus::Content;
    let Some(view) = app.grids.get_mut(&app.active) else {
        return;
    };
    let grid = view.grid_mut();
    grid.follow_selection(viewport);
    render_grid(grid, frame, content, viewport, focused);
}

fn key_hints(keys: &[(&str, &str)]) -> Line<'static> {
    let mut spans = Vec::new();
    for (key, label) in keys {
        spans.push(Span::styled(key.to_string(), theme::key_hint()));
        spans.push(Span::styled(format!(" {label}   "), theme::muted()));
    }
    Line::from(spans)
}

fn render_filter_bar(app: &App, frame: &mut Frame, area: Rect) {
    let Some(grid) = app.view().map(|v| v.grid()) else {
        return;
    };
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

fn render_centered(frame: &mut Frame, area: Rect, lines: Vec<Line>) {
    let height = (lines.len() as u16).min(area.height);
    let top = area.y + area.height.saturating_sub(height) / 2;
    let rect = Rect::new(area.x, top, area.width, height);
    frame.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
        rect,
    );
}

fn render_grid(grid: &Grid, frame: &mut Frame, area: Rect, viewport: usize, focused: bool) {
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
    let highlight = if focused {
        theme::selected_row()
    } else {
        Style::default()
            .fg(theme::ACCENT)
            .add_modifier(Modifier::BOLD)
    };
    let table = Table::new(rows, widths)
        .header(header)
        .column_spacing(COLUMN_SPACING)
        .highlight_symbol("▸ ")
        .row_highlight_style(highlight);
    frame.render_stateful_widget(table, table_area, &mut state);

    if grid.matches.len() > viewport {
        render_scrollbar(frame, area, grid.matches.len(), grid.selected);
    }
    let hidden_left = cols.start;
    let hidden_right = grid.cols.len() - cols.end;
    if hidden_left + hidden_right > 0 && grid.matches.len() < viewport {
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
        frame.render_widget(
            Paragraph::new(Span::styled(hint, theme::muted())).alignment(Alignment::Right),
            rect,
        );
    }
}

fn render_scrollbar(frame: &mut Frame, area: Rect, len: usize, position: usize) {
    let mut bar = ScrollbarState::new(len).position(position);
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

/// Truncates to `width` display chars with an ellipsis; newlines flattened.
fn fit(text: &str, width: usize) -> String {
    let flat = text.replace(['\n', '\r', '\t'], " ");
    if flat.chars().count() <= width {
        return flat;
    }
    let kept: String = flat.chars().take(width.saturating_sub(1)).collect();
    format!("{kept}…")
}

// ---------- chat ----------

fn render_chat(app: &mut App, frame: &mut Frame, area: Rect) {
    let wide = area.width >= CHAT_LIST_MIN_WIDTH;
    let show_list = wide || (app.chat.open.is_none() && app.chat.focus == ChatFocus::List);
    let show_conversation = wide || !show_list;
    let [list, divider, conversation] = if wide {
        Layout::horizontal([
            Constraint::Length(CHAT_LIST_WIDTH),
            Constraint::Length(1),
            Constraint::Min(20),
        ])
        .areas(area)
    } else if show_list {
        [area, Rect::default(), Rect::default()]
    } else {
        [Rect::default(), Rect::default(), area]
    };
    if show_list {
        render_conversation_list(app, frame, list);
    }
    if wide {
        let bar: Vec<Line> = (0..divider.height)
            .map(|_| Line::from(Span::styled("│", theme::muted())))
            .collect();
        frame.render_widget(Paragraph::new(bar), divider);
    }
    if show_conversation {
        render_conversation(app, frame, conversation);
    }
}

fn render_conversation_list(app: &mut App, frame: &mut Frame, area: Rect) {
    let [title, rows] = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).areas(area);
    let header = Line::from(vec![
        Span::styled(" Conversations ", theme::title()),
        Span::styled(" n new", theme::muted()),
    ]);
    frame.render_widget(Paragraph::new(header), title);
    app.hits.chat_list = rows;
    let chat = &app.chat;
    if let Some(err) = &chat.error {
        if chat.conversations.is_empty() {
            let text = Line::from(Span::styled(err.clone(), Style::default().fg(theme::ERROR)));
            frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }), rows);
            return;
        }
    }
    if chat.conversations.is_empty() {
        let text = if chat.list_loaded {
            " No chats yet · press n"
        } else {
            " Loading…"
        };
        frame.render_widget(Paragraph::new(Span::styled(text, theme::muted())), rows);
        return;
    }
    let focused = app.focus == Focus::Content && chat.focus == ChatFocus::List;
    let height = rows.height as usize;
    let offset = chat.selected.saturating_sub(height.saturating_sub(1));
    let width = rows.width as usize;
    let lines: Vec<Line> = chat
        .conversations
        .iter()
        .enumerate()
        .skip(offset)
        .take(height)
        .map(|(i, c)| {
            let is_open = chat.open.as_ref().is_some_and(|o| o.id == c.id);
            let marker = if is_open { "● " } else { "  " };
            let text = format!("{marker}{}", fit(&c.title, width.saturating_sub(3)));
            let padded = format!("{text:<width$}");
            let style = match (i == chat.selected, focused, is_open) {
                (true, true, _) => theme::selected_row(),
                (true, false, _) | (_, _, true) => Style::default().fg(theme::ACCENT),
                _ => Style::default(),
            };
            Line::from(Span::styled(padded, style))
        })
        .collect();
    app.hits.chat_list_offset = offset;
    frame.render_widget(Paragraph::new(lines), rows);
}

fn render_conversation(app: &mut App, frame: &mut Frame, area: Rect) {
    let composer_lines = (app.chat.composer.lines().len() as u16).clamp(1, COMPOSER_MAX_LINES);
    let [transcript, composer] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(composer_lines + 2)]).areas(area);
    app.hits.transcript = transcript;
    render_transcript(
        app,
        frame,
        transcript.inner(Margin {
            horizontal: 1,
            vertical: 0,
        }),
    );
    render_composer(app, frame, composer);
}

/// Transcript as styled lines: a label line per turn, wrapped text below.
fn transcript_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let streaming = app.chat.is_streaming();
    let count = app.chat.messages.len();
    for (i, message) in app.chat.messages.iter().enumerate() {
        let is_last = i + 1 == count;
        match message.role {
            Role::User | Role::Assistant => {
                let (label, color, bar) = if message.role == Role::User {
                    ("you", theme::ACCENT, "▌ ")
                } else {
                    ("gz", theme::ACCENT_ALT, "  ")
                };
                lines.push(Line::from(Span::styled(
                    label,
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                )));
                let body_width = width.saturating_sub(2);
                let mut body: Vec<Line<'static>> = if message.role == Role::Assistant {
                    markdown::render(&message.text, body_width)
                } else {
                    chat::wrap(&message.text, body_width)
                        .into_iter()
                        .map(Line::from)
                        .collect()
                };
                if streaming && is_last && message.role == Role::Assistant {
                    match body.last_mut() {
                        Some(line) => line.spans.push(Span::styled("▍", theme::brand())),
                        None => body.push(Line::from(Span::styled("▍", theme::brand()))),
                    }
                }
                for line in body {
                    let mut spans = vec![Span::styled(bar, Style::default().fg(color))];
                    spans.extend(line.spans);
                    lines.push(Line::from(spans));
                }
                lines.push(Line::from(""));
            }
            Role::Tool => lines.push(Line::from(Span::styled(
                format!("  {}", message.text),
                theme::muted(),
            ))),
            Role::Notice => lines.push(Line::from(Span::styled(
                format!("  {}", message.text),
                Style::default().fg(theme::WARN),
            ))),
            Role::Error => lines.push(Line::from(Span::styled(
                format!("  ✗ {}", message.text),
                Style::default().fg(theme::ERROR),
            ))),
        }
    }
    lines
}

fn render_transcript(app: &mut App, frame: &mut Frame, area: Rect) {
    if app.chat.messages.is_empty() {
        let title = app
            .chat
            .open
            .as_ref()
            .map(|c| c.title.clone())
            .unwrap_or_else(|| "New chat".to_string());
        let lines = vec![
            Line::from(Span::styled(title, theme::title())),
            Line::from(""),
            Line::from(Span::styled(
                "Ask about your data, jobs, or warehouses.",
                theme::muted(),
            )),
            key_hints(&[("enter", "send"), ("alt+enter", "newline"), ("esc", "back")]),
        ];
        return render_centered(frame, area, lines);
    }
    // The rightmost column belongs to the scrollbar; keep one more as a gap.
    let lines = transcript_lines(app, area.width.saturating_sub(2) as usize);
    let height = area.height as usize;
    let max_scroll = lines.len().saturating_sub(height);
    app.chat.scroll = app.chat.scroll.min(max_scroll);
    let start = max_scroll - app.chat.scroll;
    let visible: Vec<Line> = lines.into_iter().skip(start).take(height).collect();
    frame.render_widget(Paragraph::new(visible), area);
    if max_scroll > 0 {
        render_scrollbar(
            frame,
            area.inner(Margin {
                horizontal: 0,
                vertical: 0,
            }),
            max_scroll + 1,
            start,
        );
    }
}

fn render_composer(app: &mut App, frame: &mut Frame, area: Rect) {
    let focused = app.focus == Focus::Content && app.chat.focus == ChatFocus::Composer;
    let hint = if app.chat.is_streaming() {
        " esc cancel "
    } else if focused {
        " ⏎ send · alt+⏎ newline "
    } else {
        " i to type "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::border(focused))
        .title(Span::styled(
            " Message ",
            if focused {
                theme::brand()
            } else {
                theme::muted()
            },
        ))
        .title(Line::from(Span::styled(hint, theme::muted())).alignment(Alignment::Right));
    let cursor = if focused {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default()
    };
    app.chat.composer.set_cursor_style(cursor);
    app.chat.composer.set_block(block);
    frame.render_widget(&app.chat.composer, area);
}

// ---------- footer, overlays ----------

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
    let hints: &[(&str, &str)] = if app.palette.is_some() {
        &[("↑↓", "pick"), ("⏎", "go"), ("esc", "close")]
    } else if app.editing_filter {
        &[("⏎", "keep"), ("esc", "clear")]
    } else if app.modal.is_some() {
        &[("↑↓", "scroll"), ("esc", "close")]
    } else if app.focus == Focus::Sidebar {
        &[
            ("↑↓", "move"),
            ("→", "open"),
            ("←", "collapse"),
            ("tab", "content"),
            ("^k", "jump"),
            ("?", "help"),
        ]
    } else if app.is_chat() {
        &[
            ("⏎", "open/send"),
            ("n", "new"),
            ("pgup", "scroll"),
            ("tab", "sidebar"),
            ("^k", "jump"),
        ]
    } else {
        &[
            ("⏎", "open"),
            ("←", "back"),
            ("/", "filter"),
            ("r", "reload"),
            ("^k", "jump"),
            ("?", "help"),
        ]
    };
    // Hints get up to 60% of the row, dropping whole hints that don't fit;
    // the status takes the rest, so the two never overlap.
    let budget = (area.width as usize * 3 / 5).max(1);
    let mut spans = Vec::new();
    let mut used = 0;
    for (key, label) in hints {
        let width = key.chars().count() + label.chars().count() + 3;
        if used + width > budget {
            break;
        }
        used += width;
        spans.push(Span::styled(*key, theme::key_hint()));
        spans.push(Span::styled(format!(" {label}  "), theme::muted()));
    }
    let [left, right] =
        Layout::horizontal([Constraint::Min(1), Constraint::Length(used as u16)]).areas(area);
    frame.render_widget(Paragraph::new(status), left);
    frame.render_widget(
        Paragraph::new(Line::from(spans)).alignment(Alignment::Right),
        right,
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

fn render_palette(app: &App, frame: &mut Frame, area: Rect) {
    let Some(palette) = app.palette.as_ref() else {
        return;
    };
    let width = (area.width * 60 / 100).clamp(30.min(area.width), area.width);
    let height = (PALETTE_ROWS as u16 + 4).min(area.height);
    let rect = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + area.height / 6,
        width,
        height,
    );
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::border(true))
        .title(Span::styled(" Jump to ", theme::title()))
        .padding(Padding::horizontal(1));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let mut lines = vec![
        Line::from(vec![
            Span::styled("> ", theme::key_hint()),
            Span::raw(palette.query.clone()),
            Span::styled("▏", Style::default().fg(theme::ACCENT)),
        ]),
        Line::from(""),
    ];
    let width = inner.width as usize;
    let start = palette.selected.saturating_sub(PALETTE_ROWS - 1);
    for (pos, &index) in palette
        .matches
        .iter()
        .enumerate()
        .skip(start)
        .take(PALETTE_ROWS)
    {
        let entry = palette.entry(index);
        let label = fit(&entry.label, width.saturating_sub(entry.hint.len() + 3));
        let gap = width.saturating_sub(label.chars().count() + entry.hint.len() + 2);
        let style = if pos == palette.selected {
            theme::selected_row()
        } else {
            Style::default()
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{label}{}", " ".repeat(gap)), style),
            Span::styled(
                format!("  {}", entry.hint),
                if pos == palette.selected {
                    style
                } else {
                    theme::muted()
                },
            ),
        ]));
    }
    if palette.matches.is_empty() {
        lines.push(Line::from(Span::styled("No matches", theme::muted())));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_modal(frame: &mut Frame, area: Rect, modal: &mut Modal) {
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
        render_scrollbar(
            frame,
            rect,
            modal.lines.len().saturating_sub(modal.viewport),
            modal.scroll,
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

type HelpSection = (&'static str, &'static [(&'static str, &'static str)]);

fn section_lines(group: &[HelpSection]) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (title, keys) in group {
        lines.push(Line::from(Span::styled(*title, theme::brand())));
        for (key, what) in *keys {
            lines.push(Line::from(vec![
                Span::styled(format!("  {key:<18}"), theme::key_hint()),
                Span::raw(*what),
            ]));
        }
        lines.push(Line::from(""));
    }
    lines
}

fn render_help(frame: &mut Frame, area: Rect) {
    let rect = centered(area, 80, 80);
    frame.render_widget(Clear, rect);
    let sections: [HelpSection; 4] = [
        (
            "Browse",
            &[
                ("↑ ↓  j k", "move"),
                ("→  ⏎", "expand / open / drill in"),
                ("←  ⌫  esc", "back / collapse"),
                ("space", "toggle a module"),
                ("tab", "sidebar ↔ content"),
                ("ctrl+k  :", "jump anywhere"),
            ],
        ),
        (
            "Tables",
            &[
                ("/", "filter rows (live)"),
                ("r", "reload"),
                ("[  ]", "scroll columns"),
                ("g  G  PgUp PgDn", "first / last / page"),
            ],
        ),
        (
            "Chat",
            &[
                ("n  ctrl+n", "new chat"),
                ("⏎ / alt+⏎", "send / newline"),
                ("esc", "cancel reply / leave composer"),
                ("PgUp PgDn  wheel", "scroll transcript"),
            ],
        ),
        ("App", &[("?", "toggle help"), ("q  ctrl+c", "quit")]),
    ];
    let all = section_lines(&sections);
    // Two columns when one would be cut off and there is room side by side.
    let inner_height = rect.height.saturating_sub(3) as usize;
    let two_columns = all.len() > inner_height && rect.width >= 90;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::border(true))
        .title(Span::styled(" Keys ", theme::title()))
        .title_bottom(
            Line::from(Span::styled(" esc to close ", theme::muted())).alignment(Alignment::Right),
        )
        .padding(Padding::new(2, 2, 1, 0));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    if !two_columns {
        frame.render_widget(Paragraph::new(all), inner);
        return;
    }
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(inner);
    frame.render_widget(Paragraph::new(section_lines(&sections[..2])), left);
    frame.render_widget(Paragraph::new(section_lines(&sections[2..])), right);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::tui::chat::ChatUpdate;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{backend::TestBackend, Terminal};
    use serde_json::json;

    fn texts(line: &Line) -> Vec<String> {
        line.spans.iter().map(|s| s.content.to_string()).collect()
    }

    fn app() -> App {
        let resolved = crate::config::resolve(&Config::default(), Some("t"), None, None);
        App::new(resolved)
    }

    fn draw(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| render(app, f)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn chat_app() -> App {
        let mut app = app();
        let list = app.outbox.iter().rev().find_map(|r| match r {
            crate::tui::fetch::Request::Fetch(f) => Some(f.clone()),
            _ => None,
        });
        let list = list.unwrap();
        app.fetched(
            list.id,
            list.target,
            Ok(json!({"conversations": [
                {"conversationId": "c1", "conversationName": "Revenue by region"},
                {"conversationId": "c2", "conversationName": "gz CLI test"}
            ]})),
        );
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let transcript = app.outbox.iter().rev().find_map(|r| match r {
            crate::tui::fetch::Request::Fetch(f) => Some(f.clone()),
            _ => None,
        });
        let transcript = transcript.unwrap();
        app.fetched(
            transcript.id,
            transcript.target,
            Ok(json!({"messages": [
                {"speaker": "Human", "message": "Which region grew fastest last quarter?"},
                {"speaker": "AI", "message": "APAC grew fastest at 18% quarter over quarter, driven by enterprise renewals."}
            ]})),
        );
        app
    }

    #[test]
    fn renders_main_layout_at_common_sizes() {
        for (w, h) in [(80, 24), (200, 50), (60, 20), (40, 12)] {
            let mut app = app();
            let screen = draw(&mut app, w, h);
            assert!(screen.contains("groundzero"), "{w}x{h}");
        }
        let mut app = app();
        let screen = draw(&mut app, 100, 30);
        assert!(screen.contains("Browse"));
        assert!(screen.contains("▾ ◎ Chat"));
        assert!(screen.contains("Conversations"));
    }

    #[test]
    fn renders_chat_with_streaming_reply() {
        let mut app = chat_app();
        for c in "and EMEA?".chars() {
            app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let tag = app.chat.run.as_ref().unwrap().tag;
        app.chat_update(tag, ChatUpdate::Tool("lakehouse_query".into()));
        app.chat_update(tag, ChatUpdate::Text("EMEA grew 9%".into()));
        for (w, h) in [(80, 24), (200, 50)] {
            let screen = draw(&mut app, w, h);
            assert!(screen.contains("EMEA grew 9%▍"), "{w}x{h}\n{screen}");
            assert!(screen.contains("lakehouse_query"));
        }
        let wide = draw(&mut app, 120, 30);
        assert!(wide.contains("Revenue by region"));
    }

    #[test]
    fn renders_palette_and_help_overlays() {
        let mut app = app();
        app.on_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
        for c in "ware".chars() {
            app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        let screen = draw(&mut app, 100, 30);
        assert!(screen.contains("Jump to"));
        assert!(screen.contains("Lakehouse › Warehouses"));
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.help = true;
        assert!(draw(&mut app, 80, 24).contains("Keys"));
    }

    #[test]
    fn records_sidebar_hit_area() {
        let mut app = app();
        draw(&mut app, 100, 30);
        assert!(app.hits.sidebar_rows.height > 0);
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

    /// Prints the main screens; run with `--nocapture` to see them.
    #[test]
    fn snapshot_screens() {
        let mut home = app();
        println!("{}\n", draw(&mut home, 100, 26));
        let mut chat = chat_app();
        println!("{}\n", draw(&mut chat, 100, 26));
        chat.on_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
        for c in "dash".chars() {
            chat.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        println!("{}\n", draw(&mut chat, 100, 26));
        let mut grid = app();
        grid.on_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
        for c in "warehouses".chars() {
            grid.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        grid.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let req = grid.outbox.iter().rev().find_map(|r| match r {
            crate::tui::fetch::Request::Fetch(f) => Some(f.clone()),
            _ => None,
        });
        let req = req.unwrap();
        grid.fetched(
            req.id,
            req.target,
            Ok(json!({"status": 200, "response": [
                {"name": "GZWarehousePROD", "id": 1108, "bucket": "gz-lakehouse-prodnew", "owner": "srijith"},
                {"name": "LineageWarehouse", "id": 1114, "bucket": "gz-lakehouse-lineage", "owner": "sarvesh"}
            ]})),
        );
        println!("{}", draw(&mut grid, 100, 14));
    }
}
