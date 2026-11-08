//! The chat screen: conversation list, transcript and composer.

use ratatui::{
    layout::{Alignment, Constraint, Layout, Margin, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph, Wrap},
    Frame,
};

use super::widgets::{fit, key_hints, render_centered, render_scrollbar};
use super::{CHAT_LIST_MIN_WIDTH, CHAT_LIST_WIDTH, COMPOSER_MAX_LINES};
use crate::tui::app::{App, Focus};
use crate::tui::chat::{self, ChatFocus, Role};
use crate::tui::markdown;
use crate::tui::theme;

pub fn render_chat(app: &mut App, frame: &mut Frame, area: Rect) {
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
