//! The `?` overlay: a search line, then the help sections, in two columns
//! when they fit side by side and as one scrolling list otherwise.

use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph},
    Frame,
};

use super::widgets::{centered, render_scrollbar};
use crate::tui::help::{HelpSection, HelpState};
use crate::tui::theme;

const KEY_COLUMN: usize = 18;
/// Two columns need at least this much room.
const TWO_COLUMN_MIN_WIDTH: u16 = 90;

fn section_lines(sections: &[HelpSection]) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for section in sections {
        lines.push(Line::from(Span::styled(
            section.title.clone(),
            theme::brand(),
        )));
        for (keys, what) in &section.entries {
            lines.push(Line::from(vec![
                Span::styled(format!("  {keys:<KEY_COLUMN$}"), theme::key_hint()),
                Span::raw(what.clone()),
            ]));
        }
        lines.push(Line::from(""));
    }
    lines
}

fn search_line(state: &HelpState, matches: usize) -> Line<'static> {
    if state.query.is_empty() && !state.searching {
        return Line::from(vec![
            Span::styled("/ ", theme::key_hint()),
            Span::styled("search keys", theme::muted()),
        ]);
    }
    let cursor = if state.searching { "▏" } else { "" };
    Line::from(vec![
        Span::styled("/ ", theme::key_hint()),
        Span::raw(state.query.clone()),
        Span::styled(cursor, Style::default().fg(theme::ACCENT)),
        Span::styled(format!("   {matches} match(es)"), theme::muted()),
    ])
}

pub fn render_help(sections: &[HelpSection], state: &mut HelpState, frame: &mut Frame, area: Rect) {
    let rect = centered(area, 80, 80);
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::border(true))
        .title(Span::styled(" Keys ", theme::title()))
        .title_bottom(
            Line::from(Span::styled(
                " / search · ↑↓ scroll · esc close ",
                theme::muted(),
            ))
            .alignment(Alignment::Right),
        )
        .padding(Padding::new(2, 2, 0, 0));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let [search, _, body] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
    ])
    .areas(inner);

    let shown = state.filter(sections);
    let matches = shown.iter().map(|s| s.entries.len()).sum();
    frame.render_widget(Paragraph::new(search_line(state, matches)), search);
    if shown.is_empty() {
        let text = format!("No keys match \"{}\" · esc clears", state.query);
        frame.render_widget(Paragraph::new(Span::styled(text, theme::muted())), body);
        return;
    }

    let all = section_lines(&shown);
    let height = body.height as usize;
    if let Some(split) = two_column_split(&shown, height, body.width) {
        state.scroll = 0;
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                .areas(body);
        frame.render_widget(Paragraph::new(section_lines(&shown[..split])), left);
        frame.render_widget(Paragraph::new(section_lines(&shown[split..])), right);
        return;
    }
    let max_scroll = all.len().saturating_sub(height);
    state.scroll = state.scroll.min(max_scroll);
    let visible: Vec<Line> = all.into_iter().skip(state.scroll).take(height).collect();
    frame.render_widget(Paragraph::new(visible), body);
    if max_scroll > 0 {
        render_scrollbar(frame, rect, max_scroll, state.scroll);
    }
}

/// Where to split `sections` into two columns of about equal height, when
/// one column would be cut off but two fit.
fn two_column_split(sections: &[HelpSection], height: usize, width: u16) -> Option<usize> {
    let lines_of = |s: &HelpSection| s.entries.len() + 2;
    let total: usize = sections.iter().map(lines_of).sum();
    if total <= height || width < TWO_COLUMN_MIN_WIDTH {
        return None;
    }
    let mut split = 0;
    let mut left = 0;
    for section in sections {
        if left >= total / 2 {
            break;
        }
        left += lines_of(section);
        split += 1;
    }
    let right = total - left;
    (left.max(right) <= height && split < sections.len()).then_some(split)
}
