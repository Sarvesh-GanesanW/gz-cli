//! Fitting the live region's one-line rows (the status line, menu rows) to
//! the terminal's width: lower-priority parts drop out first, and whatever
//! is still too long ends in `…`, so nothing is cut off silently.

use std::ops::Range;

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub const ELLIPSIS: char = '…';

/// One part of a status line.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub text: String,
    pub style: Style,
    /// Lower numbers stay longer; equal ones drop from the right.
    pub priority: u8,
    /// Joins the part before it with no separator (a hint on that part).
    pub glued: bool,
}

impl Segment {
    pub fn new(text: impl Into<String>, style: Style, priority: u8) -> Self {
        Self {
            text: text.into(),
            style,
            priority,
            glued: false,
        }
    }

    /// A part shown right after the one before it, without a separator.
    pub fn glued(text: impl Into<String>, style: Style, priority: u8) -> Self {
        Self {
            glued: true,
            ..Self::new(text, style, priority)
        }
    }
}

/// The segments, in order, fitted to `width`: while too wide, the
/// lowest-priority segment (rightmost among equals) drops out; one that is
/// still too wide alone is cut with `…`.
pub fn fit_segments(mut segments: Vec<Segment>, width: usize) -> Line<'static> {
    let total = |segments: &[Segment]| segments.iter().map(|s| s.text.width()).sum::<usize>();
    while segments.len() > 1 && total(&segments) > width {
        drop_lowest(&mut segments);
    }
    let mut used = 0;
    let mut spans = Vec::with_capacity(segments.len());
    for segment in segments {
        let room = width.saturating_sub(used);
        let text = truncate(&segment.text, room);
        used += text.width();
        spans.push(Span::styled(text, segment.style));
    }
    Line::from(spans)
}

/// `indent`, then the parts joined by `separator`, fitted like
/// [`fit_segments`]: a dropped part takes its separator with it.
pub fn fit_joined(
    indent: &str,
    mut parts: Vec<Segment>,
    separator: &Segment,
    width: usize,
) -> Line<'static> {
    let total = |parts: &[Segment]| {
        indent.width()
            + parts.iter().map(|s| s.text.width()).sum::<usize>()
            + separator.text.width() * parts.iter().skip(1).filter(|p| !p.glued).count()
    };
    while parts.len() > 1 && total(&parts) > width {
        drop_lowest(&mut parts);
    }
    let mut joined = vec![Segment::new(indent, Style::default(), 0)];
    for (i, part) in parts.into_iter().enumerate() {
        if i > 0 && !part.glued {
            joined.push(separator.clone());
        }
        joined.push(part);
    }
    fit_segments(joined, width)
}

fn drop_lowest(segments: &mut Vec<Segment>) {
    let drop = segments
        .iter()
        .enumerate()
        .max_by_key(|(i, s)| (s.priority, *i))
        .map(|(i, _)| i)
        .expect("a segment to drop");
    segments.remove(drop);
}

/// `line` cut to `width` columns, ending in `…` when anything was cut; each
/// part keeps its style.
pub fn fit_line(line: Line<'static>, width: usize) -> Line<'static> {
    if line.width() <= width {
        return line;
    }
    let mut used = 0;
    let mut spans = Vec::new();
    for span in line.spans {
        let room = width.saturating_sub(used);
        let span_width = span.content.width();
        if span_width < room {
            used += span_width;
            spans.push(span);
            continue;
        }
        let cut = truncate(&format!("{}{ELLIPSIS}{ELLIPSIS}", span.content), room);
        spans.push(Span::styled(cut, span.style));
        break;
    }
    Line { spans, ..line }
}

/// `text` cut to `width` columns, ending in `…` when anything was cut.
pub fn truncate(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push(ELLIPSIS);
    out
}

/// The rows of a `len`-row list to show in `rows` rows so `selected` is
/// visible, keeping it away from the edges where the list allows.
pub fn window(len: usize, selected: usize, rows: usize) -> Range<usize> {
    if len <= rows {
        return 0..len;
    }
    let start = selected.saturating_sub(rows / 2).min(len - rows);
    start..start + rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn segments() -> Vec<Segment> {
        vec![
            Segment::new("  ? for shortcuts", Style::default(), 9),
            Segment::new("  ·  Sonnet 5.5 · high", Style::default(), 0),
            Segment::new("  ·  700.9k until auto-compact", Style::default(), 2),
            Segment::new("  ·  $0.19 this session", Style::default(), 3),
        ]
    }

    #[test]
    fn everything_shows_when_it_fits() {
        let line = fit_segments(segments(), 200);
        assert_eq!(
            text(&line),
            "  ? for shortcuts  ·  Sonnet 5.5 · high  ·  700.9k until auto-compact  ·  $0.19 this session"
        );
    }

    #[test]
    fn lower_priority_parts_drop_first() {
        let line = fit_segments(segments(), 60);
        assert_eq!(
            text(&line),
            "  ·  Sonnet 5.5 · high  ·  700.9k until auto-compact"
        );
    }

    #[test]
    fn the_last_part_left_is_cut_with_an_ellipsis() {
        let line = fit_segments(segments(), 12);
        assert_eq!(text(&line), "  ·  Sonnet…");
        assert_eq!(text(&line).width(), 12);
    }

    #[test]
    fn a_dropped_part_takes_its_separator_along() {
        let sep = Segment::new("  ·  ", Style::default(), 0);
        let parts = vec![
            Segment::new("? for shortcuts", Style::default(), 9),
            Segment::new("Sonnet 5.5 · high", Style::default(), 0),
            Segment::new("1 queued", Style::default(), 2),
        ];
        let line = fit_joined("  ", parts, &sep, 34);
        assert_eq!(text(&line), "  Sonnet 5.5 · high  ·  1 queued");
    }

    #[test]
    fn a_styled_line_is_cut_where_it_overflows() {
        let line = Line::from(vec![
            Span::styled("⠋ Thinking… ", Style::default()),
            Span::styled("(12s · ↓ 1.2k tokens · esc to interrupt)", Style::default()),
        ]);
        let cut = fit_line(line.clone(), 30);
        assert_eq!(text(&cut), "⠋ Thinking… (12s · ↓ 1.2k tok…");
        assert_eq!(text(&cut).width(), 30);
        assert_eq!(fit_line(line.clone(), 80), line);
        let exact = fit_line(Line::from("abcdef"), 6);
        assert_eq!(text(&exact), "abcdef");
    }

    #[test]
    fn truncation_counts_display_columns() {
        assert_eq!(truncate("日本語テキスト", 7), "日本語…");
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("abc", 0), "");
    }

    #[test]
    fn the_window_follows_the_selection() {
        assert_eq!(window(5, 4, 10), 0..5);
        assert_eq!(window(30, 0, 10), 0..10);
        assert_eq!(window(30, 15, 10), 10..20);
        assert_eq!(window(30, 29, 10), 20..30);
    }
}
