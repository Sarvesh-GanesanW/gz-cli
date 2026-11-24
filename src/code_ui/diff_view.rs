//! How a diff row's text is coloured: syntax highlighting by the file's
//! language, and stronger emphasis on the words that changed inside a
//! removed/added line pair.

use std::ops::Range;

use ratatui::style::Style;
use similar::{capture_diff_slices, Algorithm, DiffTag};
use syntect::parsing::SyntaxReference;

use crate::code_ui::labels::DiffRow;
use crate::tui::highlight::{self, Kind, Surface, Token};

/// Below this share of unchanged characters a line pair is a rewrite, not
/// an edit: emphasizing nearly every word would only add noise.
const MIN_SHARED_FOR_WORDS: f64 = 0.4;

/// The file a unified diff changes (`+++ b/src/a.rs`, or the `---` side
/// for a deletion), for picking its grammar.
pub fn diff_path(diff: &str) -> Option<&str> {
    let side = |prefix: &str| {
        diff.lines()
            .find_map(|line| line.strip_prefix(prefix))
            .map(|path| path.split('\t').next().unwrap_or(path).trim())
            .filter(|path| *path != "/dev/null" && !path.is_empty())
    };
    let path = side("+++ ").or_else(|| side("--- "))?;
    Some(
        path.strip_prefix("b/")
            .or_else(|| path.strip_prefix("a/"))
            .unwrap_or(path),
    )
}

/// Each row's highlighted tokens. The old file's lines (context and
/// removals) and the new file's (context and additions) are highlighted as
/// two separate sequences, so a string or comment opened on one side never
/// bleeds into the other.
pub fn row_tokens(rows: &[DiffRow], syntax: &'static SyntaxReference) -> Vec<Option<Vec<Token>>> {
    let mut old_side = String::new();
    let mut new_side = String::new();
    for row in rows {
        match row {
            DiffRow::Context { text, .. } => {
                push_line(&mut old_side, text);
                push_line(&mut new_side, text);
            }
            DiffRow::Removed { text, .. } => push_line(&mut old_side, text),
            DiffRow::Added { text, .. } => push_line(&mut new_side, text),
            DiffRow::Gap => {}
        }
    }
    let mut old_lines = highlight::highlight(&old_side, syntax).into_iter();
    let mut new_lines = highlight::highlight(&new_side, syntax).into_iter();
    rows.iter()
        .map(|row| match row {
            DiffRow::Context { .. } => {
                old_lines.next();
                new_lines.next()
            }
            DiffRow::Removed { .. } => old_lines.next(),
            DiffRow::Added { .. } => new_lines.next(),
            DiffRow::Gap => None,
        })
        .collect()
}

fn push_line(side: &mut String, text: &str) {
    side.push_str(text);
    side.push('\n');
}

/// For each row, the byte ranges of its text that changed relative to the
/// line it pairs with. Removals and additions pair up in order within one
/// change (a run of removals followed by a run of additions); rows without
/// a close partner get no ranges.
pub fn changed_words(rows: &[DiffRow]) -> Vec<Vec<Range<usize>>> {
    let mut changed = vec![Vec::new(); rows.len()];
    let mut i = 0;
    while i < rows.len() {
        let removed_start = i;
        while matches!(rows.get(i), Some(DiffRow::Removed { .. })) {
            i += 1;
        }
        let added_start = i;
        while matches!(rows.get(i), Some(DiffRow::Added { .. })) {
            i += 1;
        }
        let removed = removed_start..added_start;
        let added = added_start..i;
        for (old_index, new_index) in removed.zip(added) {
            let (DiffRow::Removed { text: old, .. }, DiffRow::Added { text: new, .. }) =
                (&rows[old_index], &rows[new_index])
            else {
                continue;
            };
            if let Some((old_ranges, new_ranges)) = word_changes(old, new) {
                changed[old_index] = old_ranges;
                changed[new_index] = new_ranges;
            }
        }
        if i == removed_start {
            i += 1;
        }
    }
    changed
}

/// Byte ranges of a line that changed.
type Changed = Vec<Range<usize>>;

/// Changed byte ranges in `old` and `new`, compared word by word. `None`
/// when the lines share too little for word emphasis to help.
fn word_changes(old: &str, new: &str) -> Option<(Changed, Changed)> {
    let old_words = words(old);
    let new_words = words(new);
    let old_texts: Vec<&str> = old_words.iter().map(|r| &old[r.clone()]).collect();
    let new_texts: Vec<&str> = new_words.iter().map(|r| &new[r.clone()]).collect();
    let mut old_changed = Vec::new();
    let mut new_changed = Vec::new();
    let mut shared = 0;
    for op in capture_diff_slices(Algorithm::Myers, &old_texts, &new_texts) {
        let (tag, old_span, new_span) = op.as_tag_tuple();
        if tag == DiffTag::Equal {
            shared += old_words[old_span].iter().map(|r| r.len()).sum::<usize>();
            continue;
        }
        old_changed.extend(non_blank(old, &old_words[old_span]));
        new_changed.extend(non_blank(new, &new_words[new_span]));
    }
    let longest = old.trim().len().max(new.trim().len()).max(1);
    if (shared as f64) / (longest as f64) < MIN_SHARED_FOR_WORDS {
        return None;
    }
    Some((merge(old_changed), merge(new_changed)))
}

/// Changed words, leaving out pure whitespace so emphasis doesn't paint
/// the gaps between words.
fn non_blank<'a>(
    text: &'a str,
    words: &'a [Range<usize>],
) -> impl Iterator<Item = Range<usize>> + 'a {
    words
        .iter()
        .filter(|r| !text[(*r).clone()].trim().is_empty())
        .cloned()
}

/// Adjacent or touching ranges joined, so `foo.bar` changing reads as one
/// emphasized stretch.
fn merge(ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    let mut merged: Vec<Range<usize>> = Vec::new();
    for range in ranges {
        match merged.last_mut() {
            Some(last) if last.end >= range.start => last.end = last.end.max(range.end),
            _ => merged.push(range),
        }
    }
    merged
}

/// Byte ranges of words: runs of letters, digits and `_`; runs of
/// whitespace; and every other character on its own.
fn words(text: &str) -> Vec<Range<usize>> {
    #[derive(PartialEq)]
    enum Class {
        Word,
        Space,
        Other,
    }
    let class = |c: char| {
        if c.is_alphanumeric() || c == '_' {
            Class::Word
        } else if c.is_whitespace() {
            Class::Space
        } else {
            Class::Other
        }
    };
    let mut ranges: Vec<Range<usize>> = Vec::new();
    let mut previous: Option<Class> = None;
    for (at, ch) in text.char_indices() {
        let current = class(ch);
        let joins = current != Class::Other && previous.as_ref() == Some(&current);
        match ranges.last_mut() {
            Some(last) if joins => last.end = at + ch.len_utf8(),
            _ => ranges.push(at..at + ch.len_utf8()),
        }
        previous = Some(current);
    }
    ranges
}

/// A row's text as styled runs: `base` for the line, `emphasis` over the
/// changed ranges, syntax colours on top when `tokens` are known.
pub fn styled_text(
    text: &str,
    tokens: Option<&[Token]>,
    changed: &[Range<usize>],
    base: Style,
    emphasis: Style,
    surface: Surface,
) -> Vec<(String, Style)> {
    let plain;
    let tokens = match tokens {
        Some(tokens) => tokens,
        None => {
            plain = vec![(text.to_string(), Kind::Plain)];
            &plain
        }
    };
    let mut runs: Vec<(String, Style)> = Vec::new();
    let mut at = 0;
    for (piece, kind) in tokens {
        for ch in piece.chars() {
            let emphasized = changed.iter().any(|r| r.contains(&at));
            let under = if emphasized { emphasis } else { base };
            let style = highlight::style(*kind, surface, under);
            at += ch.len_utf8();
            let shown = if ch == '\t' {
                "    ".to_string()
            } else {
                ch.to_string()
            };
            match runs.last_mut() {
                Some((last, last_style)) if *last_style == style => last.push_str(&shown),
                _ => runs.push((shown, style)),
            }
        }
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code_ui::labels::diff_rows;
    use crate::tui::theme;

    fn changed_text<'a>(text: &'a str, ranges: &[Range<usize>]) -> Vec<&'a str> {
        ranges.iter().map(|r| &text[r.clone()]).collect()
    }

    #[test]
    fn finds_the_file_a_diff_changes() {
        assert_eq!(
            diff_path("--- a/src/x.rs\n+++ b/src/x.rs\n"),
            Some("src/x.rs")
        );
        assert_eq!(diff_path("--- /dev/null\n+++ b/new.py\n"), Some("new.py"));
        assert_eq!(diff_path("--- a/gone.go\n+++ /dev/null\n"), Some("gone.go"));
        assert_eq!(diff_path("@@ -1 +1 @@\n-a\n+b\n"), None);
    }

    #[test]
    fn emphasizes_only_the_changed_words_of_a_pair() {
        let rows = diff_rows(
            "@@ -1,2 +1,2 @@\n-let total = price * count;\n+let total = price * amount;\n",
        );
        let changed = changed_words(&rows);
        let DiffRow::Removed { text: old, .. } = &rows[0] else {
            panic!()
        };
        let DiffRow::Added { text: new, .. } = &rows[1] else {
            panic!()
        };
        assert_eq!(changed_text(old, &changed[0]), ["count"]);
        assert_eq!(changed_text(new, &changed[1]), ["amount"]);
    }

    #[test]
    fn pairs_removals_and_additions_in_order() {
        let rows = diff_rows("@@ -1,4 +1,4 @@\n keep\n-a = 1\n-b = 2\n+a = 10\n+b = 20\n");
        let changed = changed_words(&rows);
        assert!(changed[0].is_empty(), "context rows are never emphasized");
        assert_eq!(changed[1], vec![4..5]);
        assert_eq!(changed[3], vec![4..6]);
        assert_eq!(changed[2], vec![4..5]);
        assert_eq!(changed[4], vec![4..6]);
    }

    #[test]
    fn rewrites_and_unpaired_lines_get_no_word_emphasis() {
        let rows = diff_rows("@@ -1,1 +1,2 @@\n-import os\n+print(\"hello world\")\n+extra\n");
        let changed = changed_words(&rows);
        assert!(changed.iter().all(Vec::is_empty), "{changed:?}");
    }

    #[test]
    fn styled_text_keeps_the_text_and_marks_changed_words() {
        let runs = styled_text(
            "a\tbig cat",
            None,
            std::slice::from_ref(&(6..9)),
            theme::diff_added(),
            theme::diff_added_word(),
            Surface::Bar,
        );
        let text: String = runs.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(text, "a    big cat", "tabs become spaces");
        assert_eq!(
            runs.last().unwrap(),
            &("cat".to_string(), theme::diff_added_word())
        );
        assert_eq!(runs[0].1, theme::diff_added());
    }

    #[test]
    fn both_sides_are_highlighted_separately() {
        let rows = diff_rows("@@ -1,2 +1,2 @@\n-let s = \"open\n+let s = 1;\n let t = 2;\n");
        let syntax = highlight::syntax_for_tag("rust").unwrap();
        let tokens = row_tokens(&rows, syntax);
        let added = tokens[1].as_ref().unwrap();
        assert_eq!(added[0], ("let".to_string(), Kind::Keyword));
        assert!(
            added.iter().any(|(t, k)| t == "1" && *k == Kind::Constant),
            "the unclosed string on the old side doesn't swallow the new line: {added:?}"
        );
    }
}
