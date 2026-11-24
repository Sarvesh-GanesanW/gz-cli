//! Soft wrapping for the input box: splits each logical line into visual
//! rows that fit the box, maps the editor's (line, char) cursor to a
//! (row, cell) on screen, and moves the cursor by visual row.
//!
//! Rows break after a space where one fits, otherwise mid-word; a space
//! that overflows hangs at the row's edge. Widths are terminal cells, so
//! CJK and emoji count as two, a tab reaches the next [`TAB_STOP`] and
//! zero-width marks stay with the char before them. Home/End and Ctrl+A/E
//! keep the editor's logical-line meaning (start/end of the whole line);
//! ↑/↓ and Ctrl+P/N move by visual row.

use unicode_width::UnicodeWidthChar;

/// Cells between tab stops when a tab is drawn as spaces.
pub const TAB_STOP: usize = 4;

/// One visual row: chars `start..end` of logical line `line`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VisualRow {
    pub line: usize,
    pub start: usize,
    pub end: usize,
}

/// Cells `c` takes when drawn `x` cells into a row.
pub fn cell_width(c: char, x: usize) -> usize {
    if c == '\t' {
        TAB_STOP - x % TAB_STOP
    } else {
        c.width().unwrap_or(0)
    }
}

/// Cells a row of `chars` takes from its first cell.
fn row_width(chars: impl IntoIterator<Item = char>) -> usize {
    chars.into_iter().fold(0, |x, c| x + cell_width(c, x))
}

/// Char ranges of one line's visual rows at `width` cells. When the cursor
/// is at the line's end (`cursor_at_end`) and the last row is full, an
/// empty row follows so the cursor has a cell inside the box.
pub fn wrap_line(line: &str, width: usize, cursor_at_end: bool) -> Vec<(usize, usize)> {
    let width = width.max(1);
    let chars: Vec<char> = line.chars().collect();
    let mut rows = Vec::new();
    let mut start = 0;
    let mut used = 0;
    let mut break_after_space: Option<usize> = None;
    for (i, &c) in chars.iter().enumerate() {
        if used + cell_width(c, used) > width && i > start {
            if c == ' ' {
                rows.push((start, i + 1));
                start = i + 1;
                used = 0;
                break_after_space = None;
                continue;
            }
            let cut = break_after_space.filter(|&b| b > start).unwrap_or(i);
            rows.push((start, cut));
            start = cut;
            used = row_width(chars[start..i].iter().copied());
            break_after_space = None;
        }
        used += cell_width(c, used);
        if c == ' ' {
            break_after_space = Some(i + 1);
        }
    }
    if start < chars.len() || rows.is_empty() || cursor_at_end {
        rows.push((start, chars.len()));
    }
    if cursor_at_end && start < chars.len() && used >= width {
        rows.push((chars.len(), chars.len()));
    }
    rows
}

/// Every visual row of `lines` at `width` cells, in order, for the editor
/// cursor at `cursor`.
pub fn layout<S: AsRef<str>>(lines: &[S], width: usize, cursor: (usize, usize)) -> Vec<VisualRow> {
    lines
        .iter()
        .enumerate()
        .flat_map(|(line, text)| {
            let text = text.as_ref();
            let cursor_at_end = cursor.0 == line && cursor.1 >= text.chars().count();
            wrap_line(text, width, cursor_at_end)
                .into_iter()
                .map(move |(start, end)| VisualRow { line, start, end })
        })
        .collect()
}

/// The visual row holding the cursor and the cell it sits on.
pub fn cursor_cell<S: AsRef<str>>(
    lines: &[S],
    rows: &[VisualRow],
    (line, col): (usize, usize),
) -> (usize, usize) {
    let index = rows
        .iter()
        .rposition(|r| r.line == line && r.start <= col)
        .unwrap_or(0);
    let row = rows[index];
    let text = lines.get(line).map(AsRef::as_ref).unwrap_or("");
    let x = row_width(
        text.chars()
            .skip(row.start)
            .take(col.saturating_sub(row.start)),
    );
    (index, x)
}

/// The cursor one visual row up (`up`) or down, keeping its cell where it
/// can; `None` on the first (last) row, where ↑ (↓) browse history.
pub fn step_row<S: AsRef<str>>(
    lines: &[S],
    width: usize,
    cursor: (usize, usize),
    up: bool,
) -> Option<(usize, usize)> {
    let rows = layout(lines, width, cursor);
    let (index, x) = cursor_cell(lines, &rows, cursor);
    let target = if up {
        index.checked_sub(1)?
    } else {
        Some(index + 1).filter(|&i| i < rows.len())?
    };
    let row = rows[target];
    let text = lines[row.line].as_ref();
    let ends_line = rows
        .get(target + 1)
        .is_none_or(|next| next.line != row.line);
    let is_full = row_width(text.chars().skip(row.start).take(row.end - row.start)) >= width;
    // A cursor at a mid-line row's end, or a full row's end, would show on
    // the next row instead.
    let last_col = if row.end == row.start || (ends_line && !is_full) {
        row.end
    } else {
        row.end - 1
    };
    let mut col = row.start;
    let mut used = 0;
    for c in text.chars().skip(row.start).take(last_col - row.start) {
        let w = cell_width(c, used);
        if used + w > x {
            break;
        }
        used += w;
        col += 1;
    }
    Some((row.line, col))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows_text(line: &str, width: usize) -> Vec<String> {
        let chars: Vec<char> = line.chars().collect();
        wrap_line(line, width, true)
            .into_iter()
            .map(|(s, e)| chars[s..e].iter().collect())
            .collect()
    }

    #[test]
    fn a_short_line_is_one_row() {
        assert_eq!(rows_text("hello", 10), ["hello"]);
        assert_eq!(rows_text("", 10), [""]);
    }

    #[test]
    fn long_lines_break_after_a_space() {
        assert_eq!(
            rows_text("the quick brown fox jumps", 10),
            ["the quick ", "brown fox ", "jumps"]
        );
    }

    #[test]
    fn a_word_longer_than_the_row_breaks_mid_word() {
        assert_eq!(rows_text("abcdefghijkl", 5), ["abcde", "fghij", "kl"]);
    }

    #[test]
    fn wide_chars_count_two_cells_and_never_split() {
        assert_eq!(
            rows_text("日本語のテキスト", 5),
            ["日本", "語の", "テキ", "スト"]
        );
        assert_eq!(rows_text("ab🙂🙂", 5), ["ab🙂", "🙂"]);
    }

    #[test]
    fn a_full_row_leaves_room_for_the_cursor() {
        assert_eq!(rows_text("abcde", 5), ["abcde", ""]);
        let lines = ["abcde"];
        let rows = layout(&lines, 5, (0, 5));
        assert_eq!(cursor_cell(&lines, &rows, (0, 5)), (1, 0));
    }

    #[test]
    fn the_cursor_maps_to_its_wrapped_row_and_cell() {
        let lines = ["the quick brown fox", "日本語"];
        let rows = layout(&lines, 10, (0, 0));
        assert_eq!(cursor_cell(&lines, &rows, (0, 3)), (0, 3));
        assert_eq!(cursor_cell(&lines, &rows, (0, 10)), (1, 0));
        assert_eq!(cursor_cell(&lines, &rows, (0, 12)), (1, 2));
        assert_eq!(cursor_cell(&lines, &rows, (1, 2)), (2, 4));
    }

    #[test]
    fn up_and_down_move_by_visual_row() {
        let lines = ["the quick brown fox", "end"];
        assert_eq!(step_row(&lines, 10, (0, 12), true), Some((0, 2)));
        assert_eq!(step_row(&lines, 10, (0, 2), false), Some((0, 12)));
        assert_eq!(step_row(&lines, 10, (0, 12), false), Some((1, 2)));
        assert_eq!(step_row(&lines, 10, (0, 2), true), None);
        assert_eq!(step_row(&lines, 10, (1, 1), false), None);
    }

    #[test]
    fn moving_down_onto_a_shorter_mid_row_stays_on_that_row() {
        // Landing past a mid-line row's last char would put the cursor on
        // the next row.
        let lines = ["abcdefghij klmno"];
        assert_eq!(step_row(&lines, 11, (0, 16), true), Some((0, 5)));
        let lines = ["ab cdefghijkl"];
        assert_eq!(step_row(&lines, 9, (0, 11), true), Some((0, 2)));
    }

    #[test]
    fn vertical_moves_land_beside_wide_chars() {
        let lines = ["日本語日本語"];
        // rows "日本語" / "日本語" at width 7.
        assert_eq!(step_row(&lines, 7, (0, 4), true), Some((0, 1)));
    }

    #[test]
    fn a_tab_reaches_the_next_stop() {
        assert_eq!(row_width("col1\tc".chars()), 9);
        assert_eq!(row_width("ab\t".chars()), 4);
        assert_eq!(rows_text("ab\tcd\tef", 8), ["ab\tcd\t", "ef"]);
        let lines = ["col1\tcol2"];
        let rows = layout(&lines, 30, (0, 6));
        assert_eq!(cursor_cell(&lines, &rows, (0, 5)), (0, 8));
        assert_eq!(cursor_cell(&lines, &rows, (0, 6)), (0, 9));
    }

    #[test]
    fn a_combining_mark_stays_with_its_char() {
        assert_eq!(rows_text("cafe\u{301} ok", 4), ["cafe\u{301} ", "ok"]);
        assert_eq!(rows_text("abcde\u{301}f", 5), ["abcde\u{301}", "f"]);
    }

    #[test]
    fn only_the_cursor_line_gets_a_row_past_a_full_row() {
        let lines = ["x".repeat(48), "next".to_string()];
        assert_eq!(layout(&lines, 48, (1, 4)).len(), 2);
        assert_eq!(layout(&lines, 48, (0, 48)).len(), 3);
        // ↑ from "next" lands on the full row, not a blank one.
        assert_eq!(step_row(&lines, 48, (1, 4), true), Some((0, 4)));
    }

    #[test]
    fn a_space_that_overflows_hangs_at_the_edge() {
        let line = format!("{} {} cc", "a".repeat(10), "b".repeat(37));
        assert_eq!(
            rows_text(&line, 48),
            [
                format!("{} {} ", "a".repeat(10), "b".repeat(37)),
                "cc".to_string()
            ]
        );
    }
}
