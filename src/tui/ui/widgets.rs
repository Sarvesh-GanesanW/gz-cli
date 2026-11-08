//! Small drawing helpers every screen and view shares.

use ratatui::{
    layout::{Alignment, Margin, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap},
    Frame,
};

use crate::tui::theme;

pub use crate::tui::format::truncate as fit;

pub fn key_hints(keys: &[(&str, &str)]) -> Line<'static> {
    let mut spans = Vec::new();
    for (key, label) in keys {
        spans.push(Span::styled(key.to_string(), theme::key_hint()));
        spans.push(Span::styled(format!(" {label}   "), theme::muted()));
    }
    Line::from(spans)
}

pub fn render_centered(frame: &mut Frame, area: Rect, lines: Vec<Line>) {
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

pub fn render_scrollbar(frame: &mut Frame, area: Rect, len: usize, position: usize) {
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

pub fn centered(area: Rect, width_pct: u16, height_pct: u16) -> Rect {
    let width = (area.width * width_pct / 100).max(30).min(area.width);
    let height = (area.height * height_pct / 100).max(8).min(area.height);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

/// Colors one line of pretty-printed JSON: keys, strings, numbers, literals.
pub fn highlight_json(line: &str) -> Line<'static> {
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
}
