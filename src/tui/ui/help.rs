//! The `?` overlay, drawn from the help registry's sections.

use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph},
    Frame,
};

use super::widgets::centered;
use crate::tui::help::HelpSection;
use crate::tui::theme;

fn section_lines(sections: &[HelpSection]) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for section in sections {
        lines.push(Line::from(Span::styled(
            section.title.clone(),
            theme::brand(),
        )));
        for (keys, what) in &section.entries {
            lines.push(Line::from(vec![
                Span::styled(format!("  {keys:<18}"), theme::key_hint()),
                Span::raw(what.clone()),
            ]));
        }
        lines.push(Line::from(""));
    }
    lines
}

pub fn render_help(sections: &[HelpSection], frame: &mut Frame, area: Rect) {
    let rect = centered(area, 80, 80);
    frame.render_widget(Clear, rect);
    let all = section_lines(sections);
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
    // Split by line count so both columns are about the same height.
    let half = all.len() / 2;
    let mut split = 0;
    let mut lines = 0;
    for (i, section) in sections.iter().enumerate() {
        if lines >= half {
            break;
        }
        lines += section.entries.len() + 2;
        split = i + 1;
    }
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(inner);
    frame.render_widget(Paragraph::new(section_lines(&sections[..split])), left);
    frame.render_widget(Paragraph::new(section_lines(&sections[split..])), right);
}
