//! The `?` overlay: a search line, then the help sections, in two columns
//! when they fit side by side and as one scrolling list otherwise. A
//! description longer than its column continues on the next line.

use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph},
    Frame,
};

use super::counted_matches;
use super::widgets::{centered, render_scrollbar};
use crate::tui::format::wrap;
use crate::tui::help::{HelpSection, HelpState};
use crate::tui::theme;

/// Indent plus the keys, padded so the descriptions line up.
const KEY_COLUMN: usize = 20;
/// Two columns need at least this much room.
const TWO_COLUMN_MIN_WIDTH: u16 = 90;
const COLUMN_GAP: u16 = 2;

/// One section as lines no wider than `width`: its title, its entries
/// (descriptions wrapped under themselves) and a blank line.
fn section_lines(section: &HelpSection, width: usize) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        section.title.clone(),
        theme::brand(),
    ))];
    for (keys, what) in &section.entries {
        // Keys wider than their column keep one space before the text.
        let keys = format!("  {:<pad$} ", keys, pad = KEY_COLUMN - 3);
        let room = width.saturating_sub(keys.chars().count()).max(1);
        let mut wrapped = wrap(what, room).into_iter();
        lines.push(Line::from(vec![
            Span::styled(keys, theme::key_hint()),
            Span::raw(wrapped.next().unwrap_or_default()),
        ]));
        lines.extend(wrapped.map(|rest| Line::from(format!("{:KEY_COLUMN$}{rest}", ""))));
    }
    lines.push(Line::from(""));
    lines
}

fn all_lines(sections: &[HelpSection], width: usize) -> Vec<Line<'static>> {
    sections
        .iter()
        .flat_map(|section| section_lines(section, width))
        .collect()
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
        Span::styled(format!("   {}", counted_matches(matches)), theme::muted()),
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

    let height = body.height as usize;
    let all = all_lines(&shown, body.width as usize);
    if all.len() > height && body.width >= TWO_COLUMN_MIN_WIDTH {
        let [left, right] = Layout::horizontal([Constraint::Fill(1), Constraint::Fill(1)])
            .spacing(COLUMN_GAP)
            .areas(body);
        let column = left.width.min(right.width) as usize;
        let heights: Vec<usize> = shown
            .iter()
            .map(|section| section_lines(section, column).len())
            .collect();
        if let Some(split) = two_column_split(&heights, height) {
            state.scroll = 0;
            frame.render_widget(Paragraph::new(all_lines(&shown[..split], column)), left);
            frame.render_widget(Paragraph::new(all_lines(&shown[split..], column)), right);
            return;
        }
    }
    let max_scroll = all.len().saturating_sub(height);
    state.scroll = state.scroll.min(max_scroll);
    let visible: Vec<Line> = all.into_iter().skip(state.scroll).take(height).collect();
    frame.render_widget(Paragraph::new(visible), body);
    if max_scroll > 0 {
        render_scrollbar(frame, rect, max_scroll, state.scroll);
    }
}

/// Where to split sections of these line counts into two columns of about
/// equal height, or None when two columns of `height` lines can't hold them.
fn two_column_split(heights: &[usize], height: usize) -> Option<usize> {
    let total: usize = heights.iter().sum();
    let mut split = 0;
    let mut left = 0;
    for lines in heights {
        if left >= total / 2 {
            break;
        }
        left += lines;
        split += 1;
    }
    let right = total - left;
    (left.max(right) <= height && split < heights.len()).then_some(split)
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;
    use serde_json::json;

    use super::*;
    use crate::tui::app::test_support::*;
    use crate::tui::help;

    fn text_of(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn descriptions_wrap_instead_of_being_cut() {
        let section = help::table(None);
        for width in [30, 38, 58, 120] {
            let lines: Vec<String> = section_lines(&section, width).iter().map(text_of).collect();
            let widest = lines.iter().map(|l| l.chars().count()).max().unwrap();
            assert!(widest <= width, "{widest} cells in a {width} column");
            let shown = lines.join(" ");
            let shown: Vec<&str> = shown.split_whitespace().collect();
            let mut expected: Vec<&str> = section.title.split_whitespace().collect();
            for (keys, what) in &section.entries {
                expected.extend(keys.split_whitespace());
                expected.extend(what.split_whitespace());
            }
            assert_eq!(shown, expected, "every word shows at width {width}");
        }
    }

    #[test]
    fn continuation_lines_start_under_the_description() {
        let section = HelpSection::new("Keys", &[("o  O", "sort by the next column")]);
        let lines: Vec<String> = section_lines(&section, KEY_COLUMN + 12)
            .iter()
            .map(text_of)
            .collect();
        assert_eq!(lines[1], format!("  {:<18}sort by the", "o  O"));
        assert_eq!(lines[2], format!("{:20}next column", ""));
    }

    /// Every word of every entry of `sections` shows somewhere on `screen`.
    fn assert_all_words_show(sections: &[HelpSection], screen: &str) {
        for section in sections {
            for (keys, what) in &section.entries {
                for word in what.split_whitespace() {
                    assert!(screen.contains(word), "{keys}: \"{word}\" is cut\n{screen}");
                }
            }
        }
    }

    #[test]
    fn a_table_screens_help_shows_every_description_in_full() {
        let mut app = app();
        app.open_and_focus(resource("Lakehouse", "Warehouses"));
        answer(
            &mut app,
            json!({"status": 200, "response": [{"name": "wh1"}]}),
        );
        press(&mut app, &[KeyCode::Char('?')]);
        let sections = app.help_sections();
        for (width, height) in [(150, 42), (200, 50), (100, 60), (80, 80)] {
            let screen = draw(&mut app, width, height);
            assert_all_words_show(&sections, &screen);
        }
        let wide = draw(&mut app, 150, 42);
        let this_screen = wide.lines().find(|l| l.contains("This screen")).unwrap();
        assert!(this_screen.contains("Views"), "two columns at 150: {wide}");
    }
}
