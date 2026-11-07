//! The Ctrl+O transcript: every item of the session, fully expanded, in a
//! full-screen scrollable view with search. The inline view stays compact;
//! this is where the detail lives.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::code_ui::items::Item;
use crate::tui::theme;

/// What a key in the viewer asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewerAction {
    Stay,
    Close,
}

/// Scroll position and search state of the open transcript.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Viewer {
    /// First line shown.
    pub scroll: usize,
    /// Rows available for lines, set by the renderer each frame.
    pub height: usize,
    pub query: String,
    /// True while typing the query after `/`.
    pub editing: bool,
    /// Lines that match the query, in order.
    pub matches: Vec<usize>,
    /// Index into `matches` of the match last jumped to.
    pub current: Option<usize>,
    /// Starts at the bottom (the newest items) on the first frame.
    pinned_to_bottom: bool,
}

impl Viewer {
    pub fn open() -> Self {
        Self {
            height: 20,
            pinned_to_bottom: true,
            ..Self::default()
        }
    }

    /// Keeps the scroll inside `total` lines; called by the renderer.
    pub fn clamp(&mut self, total: usize) {
        let max = total.saturating_sub(self.height.max(1));
        if self.pinned_to_bottom {
            self.scroll = max;
            self.pinned_to_bottom = false;
        }
        self.scroll = self.scroll.min(max);
    }

    /// Handles one key against the transcript's plain-text lines.
    pub fn on_key(&mut self, key: KeyEvent, lines: &[String]) -> ViewerAction {
        if self.editing {
            self.on_query_key(key, lines);
            return ViewerAction::Stay;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let page = self.height.max(1);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return ViewerAction::Close,
            KeyCode::Char('o') if ctrl => return ViewerAction::Close,
            KeyCode::Down | KeyCode::Char('j') => self.scroll += 1,
            KeyCode::Up | KeyCode::Char('k') => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll += page,
            KeyCode::PageUp | KeyCode::Char('b') => self.scroll = self.scroll.saturating_sub(page),
            KeyCode::Home | KeyCode::Char('g') => self.scroll = 0,
            KeyCode::End | KeyCode::Char('G') => self.scroll = lines.len(),
            KeyCode::Char('/') => {
                self.editing = true;
                self.query.clear();
            }
            KeyCode::Char('n') => self.step_match(true),
            KeyCode::Char('N') => self.step_match(false),
            _ => {}
        }
        ViewerAction::Stay
    }

    fn on_query_key(&mut self, key: KeyEvent, lines: &[String]) {
        match key.code {
            KeyCode::Esc => {
                self.editing = false;
                self.query.clear();
                self.matches.clear();
                self.current = None;
            }
            KeyCode::Enter => {
                self.editing = false;
                self.search(lines);
            }
            KeyCode::Backspace => {
                self.query.pop();
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.query.push(c)
            }
            _ => {}
        }
    }

    /// Finds every line containing the query (case-insensitive) and jumps
    /// to the first one at or below the current position.
    fn search(&mut self, lines: &[String]) {
        let needle = self.query.to_lowercase();
        self.matches = if needle.is_empty() {
            Vec::new()
        } else {
            lines
                .iter()
                .enumerate()
                .filter(|(_, line)| line.to_lowercase().contains(&needle))
                .map(|(i, _)| i)
                .collect()
        };
        let next = self
            .matches
            .iter()
            .position(|&line| line >= self.scroll)
            .or((!self.matches.is_empty()).then_some(0));
        self.jump(next);
    }

    fn step_match(&mut self, forward: bool) {
        if self.matches.is_empty() {
            return;
        }
        let count = self.matches.len();
        let next = match (self.current, forward) {
            (None, _) => 0,
            (Some(i), true) => (i + 1) % count,
            (Some(i), false) => (i + count - 1) % count,
        };
        self.jump(Some(next));
    }

    fn jump(&mut self, index: Option<usize>) {
        self.current = index;
        if let Some(i) = index {
            // Show the match a few lines below the top for context.
            self.scroll = self.matches[i].saturating_sub(2);
        }
    }

    /// The bottom status line.
    pub fn footer(&self, total: usize) -> Line<'static> {
        if self.editing {
            return Line::from(vec![
                Span::styled(" /", theme::key_hint()),
                Span::raw(self.query.clone()),
                Span::styled("▏", theme::brand()),
                Span::styled("  enter search · esc cancel", theme::muted()),
            ]);
        }
        let last = (self.scroll + self.height).min(total);
        let mut text = format!(
            " transcript · lines {}–{last} of {total}",
            (self.scroll + 1).min(total)
        );
        if !self.query.is_empty() {
            match self.current {
                Some(i) => text.push_str(&format!(
                    " · “{}” {}/{}",
                    self.query,
                    i + 1,
                    self.matches.len()
                )),
                None => text.push_str(&format!(" · “{}” not found", self.query)),
            }
        }
        text.push_str("  ·  ↑↓ pgup pgdn g G · / search · n N · esc back");
        Line::from(Span::styled(text, theme::muted()))
    }
}

/// The whole session, expanded, at `width`.
pub fn lines(items: &[Item], width: usize) -> Vec<Line<'static>> {
    items
        .iter()
        .flat_map(|item| item.render_expanded(width))
        .collect()
}

pub fn plain(lines: &[Line]) -> Vec<String> {
    lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
}

/// Re-draws a matching line with each occurrence of `query` reversed.
pub fn highlight(line: &str, query: &str) -> Line<'static> {
    let lower = line.to_lowercase();
    let needle = query.to_lowercase();
    if needle.is_empty() || lower.len() != line.len() {
        return Line::from(line.to_string());
    }
    let mut spans = Vec::new();
    let mut at = 0;
    while let Some(found) = lower[at..].find(&needle) {
        let start = at + found;
        let end = start + needle.len();
        spans.push(Span::raw(line[at..start].to_string()));
        spans.push(Span::styled(
            line[start..end].to_string(),
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD),
        ));
        at = end;
    }
    spans.push(Span::raw(line[at..].to_string()));
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn sample() -> Vec<String> {
        (0..100)
            .map(|i| {
                if i % 30 == 5 {
                    format!("line {i} has the needle")
                } else {
                    format!("line {i}")
                }
            })
            .collect()
    }

    #[test]
    fn opens_at_the_bottom_and_scrolls() {
        let lines = sample();
        let mut v = Viewer::open();
        v.height = 10;
        v.clamp(lines.len());
        assert_eq!(v.scroll, 90);
        v.on_key(key(KeyCode::Char('g')), &lines);
        assert_eq!(v.scroll, 0);
        v.on_key(key(KeyCode::PageDown), &lines);
        v.on_key(key(KeyCode::Char('j')), &lines);
        assert_eq!(v.scroll, 11);
        v.on_key(key(KeyCode::Char('G')), &lines);
        v.clamp(lines.len());
        assert_eq!(v.scroll, 90);
    }

    #[test]
    fn searches_and_steps_through_matches() {
        let lines = sample();
        let mut v = Viewer::open();
        v.height = 10;
        v.clamp(lines.len());
        v.scroll = 0;
        v.on_key(key(KeyCode::Char('/')), &lines);
        for c in "NEEDLE".chars() {
            v.on_key(key(KeyCode::Char(c)), &lines);
        }
        assert!(v.editing);
        v.on_key(key(KeyCode::Enter), &lines);
        assert_eq!(v.matches, vec![5, 35, 65, 95]);
        assert_eq!((v.current, v.scroll), (Some(0), 3));
        v.on_key(key(KeyCode::Char('n')), &lines);
        assert_eq!(v.current, Some(1));
        v.on_key(key(KeyCode::Char('N')), &lines);
        v.on_key(key(KeyCode::Char('N')), &lines);
        assert_eq!(v.current, Some(3), "wraps backwards");
        assert!(plain(&[v.footer(100)])[0].contains("“NEEDLE” 4/4"));
    }

    #[test]
    fn esc_and_ctrl_o_close() {
        let lines = sample();
        let mut v = Viewer::open();
        assert_eq!(v.on_key(key(KeyCode::Esc), &lines), ViewerAction::Close);
        let ctrl_o = KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert_eq!(v.on_key(ctrl_o, &lines), ViewerAction::Close);
        v.on_key(key(KeyCode::Char('/')), &lines);
        assert_eq!(
            v.on_key(key(KeyCode::Esc), &lines),
            ViewerAction::Stay,
            "esc first leaves the search box"
        );
    }

    #[test]
    fn highlights_every_occurrence() {
        let line = highlight("a needle and a Needle", "needle");
        let reversed: Vec<String> = line
            .spans
            .iter()
            .filter(|s| s.style.add_modifier.contains(Modifier::REVERSED))
            .map(|s| s.content.to_string())
            .collect();
        assert_eq!(reversed, vec!["needle", "Needle"]);
    }
}
