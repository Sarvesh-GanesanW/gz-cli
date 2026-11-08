//! The "Save view" name prompt: the name being typed and what it keeps.

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
    Frame,
};

use super::overlays::dialog;
use crate::tui::app::App;
use crate::tui::theme;

pub fn render(app: &App, name: &str, frame: &mut Frame, area: Rect) {
    let place = app.view_to_save();
    let mut keeps = app.breadcrumb().join(" › ");
    if !place.filter.is_empty() {
        keeps.push_str(&format!(" · filter “{}”", place.filter));
    }
    if let Some(sort) = &place.sort {
        let arrow = if sort.descending { "▼" } else { "▲" };
        keeps.push_str(&format!(" · sorted by {} {arrow}", sort.column));
    }
    let inner = dialog(frame, area, "Save view", 5, false);
    let lines = vec![
        Line::from(vec![
            Span::styled("Name  ", theme::muted()),
            Span::raw(name.to_string()),
            Span::styled("▏", Style::default().fg(theme::ACCENT)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("Keeps ", theme::muted()),
            Span::raw(keeps),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
}
