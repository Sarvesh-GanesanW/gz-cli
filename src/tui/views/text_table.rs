//! Plain text tables for views whose columns are only known at run time
//! (a table's sample rows, a schema): a header line, then one line per row,
//! columns sized to their content and squeezed to fit the width.

use ratatui::text::{Line, Span};

use crate::tui::theme;
use crate::tui::ui::widgets::fit;

const GAP: usize = 2;
const MAX_COLUMN: usize = 40;
const MIN_COLUMN: usize = 3;

/// `headers` over `rows`, indented two spaces like detail fields.
pub fn lines(headers: &[String], rows: &[Vec<String>], width: usize) -> Vec<Line<'static>> {
    let mut widths: Vec<usize> = headers
        .iter()
        .enumerate()
        .map(|(i, header)| {
            rows.iter()
                .filter_map(|row| row.get(i))
                .map(|cell| cell.chars().count())
                .chain(std::iter::once(header.chars().count()))
                .max()
                .unwrap_or(0)
                .clamp(MIN_COLUMN, MAX_COLUMN)
        })
        .collect();
    let budget = width.saturating_sub(2);
    while widths.iter().sum::<usize>() + GAP * widths.len() > budget {
        let Some((widest, _)) = widths.iter().enumerate().max_by_key(|(_, w)| **w) else {
            break;
        };
        if widths[widest] <= MIN_COLUMN {
            break;
        }
        widths[widest] -= 1;
    }
    let cell = |text: &str, w: usize| format!("{:<w$}{}", fit(text, w), " ".repeat(GAP));
    let header = std::iter::once(Span::raw("  "))
        .chain(
            headers
                .iter()
                .zip(&widths)
                .map(|(h, w)| Span::styled(cell(h, *w), theme::header_cell())),
        )
        .collect::<Vec<_>>();
    let mut out = vec![Line::from(header)];
    for row in rows {
        let text: String = widths
            .iter()
            .enumerate()
            .map(|(i, w)| cell(row.get(i).map_or("", String::as_str), *w))
            .collect();
        out.push(Line::from(format!("  {text}")));
    }
    out
}

/// Plain text of `lines`, for tests that read what a user sees.
#[cfg(test)]
pub fn plain(lines: &[Line]) -> String {
    lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_columns_and_squeezes_to_the_width() {
        let headers = vec!["id".to_string(), "name".to_string()];
        let rows = vec![vec!["1".to_string(), "a fairly long value".to_string()]];
        let text = plain(&lines(&headers, &rows, 80));
        assert!(text.contains("id   name"), "{text}");
        assert!(text.contains("1    a fairly long value"), "{text}");
        let narrow = plain(&lines(&headers, &rows, 16));
        assert!(narrow.lines().all(|l| l.trim_end().chars().count() <= 16));
        assert!(narrow.contains('…'), "{narrow}");
    }
}
