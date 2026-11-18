//! The tab-completion menu of the SQL shell's input: the first tab fills
//! in what all candidates share (or the only one) and lists them; further
//! tabs step through them, putting each in place of the word.

use tui_textarea::TextArea;

use super::catalog::Catalog;
use super::complete::{complete, Completion};

/// Candidates on screen for the word before the cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Menu {
    /// Char index where the word starts; it runs to the cursor.
    start: usize,
    pub candidates: Vec<String>,
    /// The candidate in place, once tab has stepped onto one.
    pub selected: Option<usize>,
}

/// The cursor as a char index into the input's text (lines joined by `\n`).
fn cursor_index(input: &TextArea) -> usize {
    let (row, col) = input.cursor();
    let before: usize = input.lines()[..row]
        .iter()
        .map(|line| line.chars().count() + 1)
        .sum();
    before + col
}

/// Puts `text` in place of the input from char `start` to the cursor (on
/// the cursor's line; a word never spans lines).
fn replace_word(input: &mut TextArea, start: usize, text: &str) {
    for _ in start..cursor_index(input) {
        input.delete_char();
    }
    input.insert_str(text);
}

/// What `candidates` all begin with, ignoring case, in the first one's
/// spelling.
fn shared_prefix(candidates: &[String]) -> String {
    let Some(first) = candidates.first() else {
        return String::new();
    };
    let mut length = first.chars().count();
    for other in &candidates[1..] {
        length = first
            .chars()
            .zip(other.chars())
            .take(length)
            .take_while(|(a, b)| a.eq_ignore_ascii_case(b))
            .count();
    }
    first.chars().take(length).collect()
}

impl Menu {
    /// The first tab: completes the input in place; returns a menu when
    /// more than one candidate is left to choose from.
    pub fn open(input: &mut TextArea, catalog: Option<&Catalog>) -> Option<Self> {
        let text = input.lines().join("\n");
        let cursor = cursor_index(input);
        let Completion { start, candidates } = complete(&text, cursor, catalog)?;
        if let [only] = candidates.as_slice() {
            replace_word(input, start, only);
            return None;
        }
        let typed: String = text.chars().skip(start).take(cursor - start).collect();
        let prefix = shared_prefix(&candidates);
        if prefix.chars().count() > typed.chars().count() {
            replace_word(input, start, &prefix);
        }
        Some(Self {
            start,
            candidates,
            selected: None,
        })
    }

    /// A further tab (`forward`) or shift+tab: the next or previous
    /// candidate goes in place of the word.
    pub fn step(&mut self, input: &mut TextArea, forward: bool) {
        let count = self.candidates.len();
        let next = match (self.selected, forward) {
            (None, true) => 0,
            (None, false) => count - 1,
            (Some(at), true) => (at + 1) % count,
            (Some(at), false) => (at + count - 1) % count,
        };
        self.selected = Some(next);
        replace_word(input, self.start, &self.candidates[next]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::catalog::tests::sample;

    fn input(text: &str) -> TextArea<'static> {
        let mut area = TextArea::default();
        area.insert_str(text);
        area
    }

    fn text(area: &TextArea) -> String {
        area.lines().join("\n")
    }

    #[test]
    fn a_single_candidate_completes_at_once() {
        let mut area = input("select * from De");
        assert!(Menu::open(&mut area, Some(&sample())).is_none());
        assert_eq!(text(&area), "select * from DemoDB");
    }

    #[test]
    fn several_fill_in_their_shared_start_then_step_through() {
        let mut area = input("select *\nfrom demodb.p");
        let mut menu = Menu::open(&mut area, Some(&sample())).unwrap();
        assert_eq!(text(&area), "select *\nfrom demodb.Product");
        assert_eq!(menu.candidates, vec!["ProductionMetrics", "product"]);
        menu.step(&mut area, true);
        assert_eq!(text(&area), "select *\nfrom demodb.ProductionMetrics");
        menu.step(&mut area, true);
        assert_eq!(text(&area), "select *\nfrom demodb.product");
        menu.step(&mut area, false);
        assert_eq!(text(&area), "select *\nfrom demodb.ProductionMetrics");
    }

    #[test]
    fn nothing_to_complete_leaves_the_input_alone() {
        let mut area = input("select zzz");
        assert!(Menu::open(&mut area, Some(&sample())).is_none());
        assert_eq!(text(&area), "select zzz");
    }
}
