//! Finds where a streaming Markdown answer can be cut so the finished part
//! prints into scrollback early. A cut is only taken at a blank line where
//! rendering the two pieces one after the other gives exactly the lines the
//! whole answer would: outside fenced code, not between items of one list,
//! and not before an indented continuation.

/// A safe cut in the streamed text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cut {
    /// End of the finished blocks (the blank lines after them excluded).
    pub commit: usize,
    /// Where the next, unfinished block starts.
    pub rest: usize,
}

/// The last safe cut in `text`, if any. Only complete lines are judged, so
/// a block is never committed on a guess about a half-received line.
pub fn last_safe_cut(text: &str) -> Option<Cut> {
    let mut fence: Option<Fence> = None;
    let mut blank_run_start: Option<usize> = None;
    let mut block_is_list = false;
    let mut cut = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        if !line.ends_with('\n') {
            break;
        }
        let content = line.trim_end_matches(['\n', '\r']);
        if let Some(open) = &fence {
            if open.closes(content) {
                fence = None;
            }
            continue;
        }
        if content.trim().is_empty() {
            blank_run_start.get_or_insert(start);
            continue;
        }
        let starts_block = blank_run_start.is_some();
        if let Some(blank) = blank_run_start.take() {
            let indented = content.starts_with([' ', '\t']);
            let same_list = block_is_list && is_list_item(content);
            if !indented && !same_list {
                cut = Some(Cut {
                    commit: blank,
                    rest: start,
                });
            }
        }
        if starts_block || start == 0 {
            block_is_list = is_list_item(content);
        }
        if let Some(open) = Fence::opens(content) {
            fence = Some(open);
        }
    }
    cut
}

/// An open fenced code block: its marker character and length.
struct Fence {
    marker: char,
    len: usize,
}

impl Fence {
    fn opens(line: &str) -> Option<Self> {
        let trimmed = line.trim_start();
        if line.len() - trimmed.len() > 3 {
            return None;
        }
        let marker = trimmed.chars().next()?;
        if marker != '`' && marker != '~' {
            return None;
        }
        let len = trimmed.chars().take_while(|c| *c == marker).count();
        (len >= 3).then_some(Self { marker, len })
    }

    fn closes(&self, line: &str) -> bool {
        let trimmed = line.trim();
        trimmed.chars().take_while(|c| *c == self.marker).count() >= self.len
            && trimmed.chars().all(|c| c == self.marker)
    }
}

/// `- a`, `* a`, `+ a`, `1. a`, `2) a` at the start of a line.
fn is_list_item(line: &str) -> bool {
    let bullet = ["- ", "* ", "+ "].iter().any(|b| line.starts_with(b));
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    let numbered = digits > 0
        && matches!(line[digits..].chars().next(), Some('.' | ')'))
        && line[digits + 1..].starts_with(' ');
    bullet || numbered
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pieces(text: &str) -> (&str, &str) {
        let cut = last_safe_cut(text).expect("a cut");
        (&text[..cut.commit], &text[cut.rest..])
    }

    #[test]
    fn cuts_at_the_last_blank_line_between_paragraphs() {
        let (done, rest) = pieces("One.\n\nTwo.\n\nThree is here\n");
        assert_eq!(done, "One.\n\nTwo.\n");
        assert_eq!(rest, "Three is here\n");
        let (done, rest) = pieces("One.\n\nTwo.\n\nThree is sti");
        assert_eq!(done, "One.\n", "a half-received line isn't judged yet");
        assert_eq!(rest, "Two.\n\nThree is sti");
    }

    #[test]
    fn waits_for_the_next_line_to_be_complete() {
        assert!(last_safe_cut("One.\n\nTw").is_none());
        assert!(last_safe_cut("One.\n\n").is_none());
    }

    #[test]
    fn never_cuts_inside_fenced_code() {
        let text = "```py\nx = 1\n\ny = 2\n```\n\nAfter\n";
        let (done, rest) = pieces(text);
        assert_eq!(done, "```py\nx = 1\n\ny = 2\n```\n");
        assert_eq!(rest, "After\n");
        assert!(last_safe_cut("```\na\n\nb\n").is_none(), "fence still open");
    }

    #[test]
    fn keeps_a_loose_list_together() {
        assert!(last_safe_cut("1. one\n\n2. two\n").is_none());
        assert!(last_safe_cut("- a\n\n  more about a\n").is_none());
        let (done, _) = pieces("- a\n- b\n\nAfter the list\n");
        assert_eq!(done, "- a\n- b\n");
    }

    #[test]
    fn a_table_commits_once_a_blank_line_closes_it() {
        let open = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        assert!(last_safe_cut(open).is_none());
        let closed = format!("{open}\nNext\n");
        let (done, rest) = pieces(&closed);
        assert_eq!(done, open);
        assert_eq!(rest, "Next\n");
    }

    #[test]
    fn recognises_list_items() {
        assert!(is_list_item("- a") && is_list_item("12. a") && is_list_item("3) a"));
        assert!(!is_list_item("-a") && !is_list_item("1.5 kg") && !is_list_item("Hello"));
    }
}
