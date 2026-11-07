//! Renders chat Markdown into terminal lines: wrapped paragraphs with inline
//! styles, headings, lists, quotes, code blocks, rules, and boxed tables
//! that fit the available width.

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::tui::theme;

/// Narrowest a table column may shrink to before wrapping its text.
const MIN_COLUMN_WIDTH: usize = 3;

type Run = (String, Style);

pub fn render(markdown: &str, width: usize) -> Vec<Line<'static>> {
    let mut renderer = Renderer::new(width.max(10));
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    for event in Parser::new_ext(markdown, options) {
        renderer.event(event);
    }
    renderer.finish()
}

#[derive(Default)]
struct TableState {
    alignments: Vec<Alignment>,
    header: Vec<Vec<Run>>,
    rows: Vec<Vec<Vec<Run>>>,
    row: Vec<Vec<Run>>,
    in_header: bool,
}

struct ListLevel {
    next_number: Option<u64>,
}

struct Renderer {
    width: usize,
    lines: Vec<Line<'static>>,
    runs: Vec<Run>,
    styles: Vec<Style>,
    lists: Vec<ListLevel>,
    /// Marker for the first line of the current list item, e.g. "• " or "2. ".
    item_marker: Option<String>,
    quote_depth: usize,
    code: Option<String>,
    table: Option<TableState>,
    link: Option<String>,
}

impl Renderer {
    fn new(width: usize) -> Self {
        Self {
            width,
            lines: Vec::new(),
            runs: Vec::new(),
            styles: vec![Style::default()],
            lists: Vec::new(),
            item_marker: None,
            quote_depth: 0,
            code: None,
            table: None,
            link: None,
        }
    }

    fn style(&self) -> Style {
        *self.styles.last().unwrap_or(&Style::default())
    }

    fn push_style(&mut self, apply: impl Fn(Style) -> Style) {
        let next = apply(self.style());
        self.styles.push(next);
    }

    fn pop_style(&mut self) {
        if self.styles.len() > 1 {
            self.styles.pop();
        }
    }

    fn text(&mut self, text: &str) {
        if let Some(code) = self.code.as_mut() {
            code.push_str(text);
            return;
        }
        let run = (text.to_string(), self.style());
        match self.table.as_mut() {
            Some(table) => match table.row.last_mut() {
                Some(cell) => cell.push(run),
                None => table.row.push(vec![run]),
            },
            None => self.runs.push(run),
        }
    }

    fn event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.text(&text),
            Event::Code(code) => {
                self.push_style(|s| s.fg(theme::WARN));
                self.text(&code);
                self.pop_style();
            }
            Event::SoftBreak => self.text(" "),
            Event::HardBreak => self.flush(),
            Event::Rule => {
                self.flush();
                self.lines.push(Line::from(Span::styled(
                    "─".repeat(self.width),
                    theme::muted(),
                )));
                self.blank();
            }
            Event::TaskListMarker(done) => self.text(if done { "[x] " } else { "[ ] " }),
            Event::Html(html) | Event::InlineHtml(html) => self.text(&html),
            Event::FootnoteReference(name) => self.text(&format!("[{name}]")),
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { level, .. } => {
                self.flush();
                let underline = matches!(level, HeadingLevel::H1 | HeadingLevel::H2);
                self.push_style(|s| {
                    let s = s.fg(theme::ACCENT).add_modifier(Modifier::BOLD);
                    if underline {
                        s.add_modifier(Modifier::UNDERLINED)
                    } else {
                        s
                    }
                });
            }
            Tag::BlockQuote(_) => {
                self.flush();
                self.quote_depth += 1;
            }
            Tag::CodeBlock(kind) => {
                self.flush();
                if let CodeBlockKind::Fenced(lang) = kind {
                    if !lang.is_empty() {
                        self.lines.push(Line::from(Span::styled(
                            format!("  {lang}"),
                            theme::muted(),
                        )));
                    }
                }
                self.code = Some(String::new());
            }
            Tag::List(start) => {
                self.flush();
                self.lists.push(ListLevel { next_number: start });
            }
            Tag::Item => {
                self.flush();
                let marker = match self.lists.last_mut() {
                    Some(ListLevel {
                        next_number: Some(n),
                    }) => {
                        let marker = format!("{n}. ");
                        *n += 1;
                        marker
                    }
                    _ => "• ".to_string(),
                };
                self.item_marker = Some(marker);
            }
            Tag::Emphasis => self.push_style(|s| s.add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.push_style(|s| s.add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => self.push_style(|s| s.add_modifier(Modifier::CROSSED_OUT)),
            Tag::Link { dest_url, .. } => {
                self.link = Some(dest_url.to_string());
                self.push_style(|s| s.fg(theme::ACCENT).add_modifier(Modifier::UNDERLINED));
            }
            Tag::Table(alignments) => {
                self.flush();
                self.table = Some(TableState {
                    alignments,
                    ..TableState::default()
                });
            }
            Tag::TableHead => {
                if let Some(table) = self.table.as_mut() {
                    table.in_header = true;
                }
            }
            Tag::TableRow => {}
            Tag::TableCell => {
                if let Some(table) = self.table.as_mut() {
                    table.row.push(Vec::new());
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush();
                if self.lists.is_empty() {
                    self.blank();
                }
            }
            TagEnd::Heading(_) => {
                self.flush();
                self.pop_style();
                self.blank();
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.quote_depth = self.quote_depth.saturating_sub(1);
                self.blank();
            }
            TagEnd::CodeBlock => {
                let code = self.code.take().unwrap_or_default();
                let style = Style::default().fg(theme::WARN);
                let room = self.width.saturating_sub(4).max(1);
                for line in code.trim_end_matches('\n').lines() {
                    for piece in hard_wrap(line, room) {
                        self.lines.push(Line::from(vec![
                            Span::styled("  │ ", theme::muted()),
                            Span::styled(piece, style),
                        ]));
                    }
                }
                self.blank();
            }
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
                if self.lists.is_empty() {
                    self.blank();
                }
            }
            TagEnd::Item => self.flush(),
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => self.pop_style(),
            TagEnd::Link => {
                self.pop_style();
                if let Some(url) = self.link.take() {
                    let shown: String = self.runs.iter().map(|(t, _)| t.as_str()).collect();
                    if !shown.ends_with(url.as_str()) {
                        let note = (format!(" ({url})"), theme::muted());
                        match self.table.as_mut().and_then(|t| t.row.last_mut()) {
                            Some(cell) => cell.push(note),
                            None => self.runs.push(note),
                        }
                    }
                }
            }
            TagEnd::TableHead => {
                if let Some(table) = self.table.as_mut() {
                    table.header = std::mem::take(&mut table.row);
                    table.in_header = false;
                }
            }
            TagEnd::TableRow => {
                if let Some(table) = self.table.as_mut() {
                    let row = std::mem::take(&mut table.row);
                    if table.in_header {
                        table.header = row;
                    } else {
                        table.rows.push(row);
                    }
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    let lines = render_table(&table, self.width);
                    self.lines.extend(lines);
                    self.blank();
                }
            }
            _ => {}
        }
    }

    /// Prefix for the current block: quote bars plus list indentation.
    fn prefixes(&mut self) -> (Vec<Run>, Vec<Run>) {
        let mut first: Vec<Run> = Vec::new();
        for _ in 0..self.quote_depth {
            first.push(("▌ ".to_string(), theme::muted()));
        }
        let mut rest = first.clone();
        if !self.lists.is_empty() {
            let indent = "  ".repeat(self.lists.len() - 1);
            let marker = self.item_marker.take();
            let marker_width = marker.as_deref().map(UnicodeWidthStr::width).unwrap_or(2);
            first.push((indent.clone(), Style::default()));
            rest.push((indent, Style::default()));
            match marker {
                Some(marker) => first.push((marker, Style::default().fg(theme::ACCENT))),
                None => first.push((" ".repeat(marker_width), Style::default())),
            }
            rest.push((" ".repeat(marker_width), Style::default()));
        }
        (first, rest)
    }

    fn flush(&mut self) {
        if self.runs.is_empty() {
            return;
        }
        let runs = std::mem::take(&mut self.runs);
        let (first, rest) = self.prefixes();
        let prefix_width = |p: &[Run]| p.iter().map(|(t, _)| t.width()).sum::<usize>();
        let room = self
            .width
            .saturating_sub(prefix_width(&first).max(prefix_width(&rest)))
            .max(1);
        for (i, line) in wrap_runs(&runs, room).into_iter().enumerate() {
            let prefix = if i == 0 { &first } else { &rest };
            let mut spans: Vec<Span<'static>> = prefix
                .iter()
                .map(|(t, s)| Span::styled(t.clone(), *s))
                .collect();
            spans.extend(line.into_iter().map(|(t, s)| Span::styled(t, s)));
            self.lines.push(Line::from(spans));
        }
    }

    fn blank(&mut self) {
        if self.lines.last().is_some_and(|l| l.width() > 0) {
            self.lines.push(Line::from(""));
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        if let Some(code) = self.code.take() {
            self.runs.push((code, Style::default()));
        }
        self.flush();
        while self.lines.last().is_some_and(|l| l.width() == 0) {
            self.lines.pop();
        }
        self.lines
    }
}

/// Greedy word wrap that keeps each word's style; breaks words longer than
/// the line.
fn wrap_runs(runs: &[Run], width: usize) -> Vec<Vec<Run>> {
    let mut lines: Vec<Vec<Run>> = vec![Vec::new()];
    let mut used = 0;
    let mut pending_space = false;
    for (text, style) in runs {
        for (i, segment) in text.split(' ').enumerate() {
            if i > 0 {
                pending_space = true;
            }
            if segment.is_empty() {
                continue;
            }
            let space = usize::from(pending_space && used > 0);
            if used > 0 && used + space + segment.width() > width {
                lines.push(Vec::new());
                used = 0;
            } else if space == 1 {
                // A gap keeps a style only when both neighbours share it, so
                // multi-word links stay underlined but bold doesn't leak.
                let line = lines.last_mut().unwrap();
                let gap_style = match line.last() {
                    Some((_, previous)) if previous == style => *style,
                    _ => Style::default(),
                };
                push_run(line, " ", gap_style);
                used += 1;
            }
            pending_space = false;
            for piece in hard_wrap(segment, width) {
                if used > 0 && used + piece.width() > width {
                    lines.push(Vec::new());
                    used = 0;
                }
                used += piece.width();
                push_run(lines.last_mut().unwrap(), &piece, *style);
            }
        }
    }
    lines
}

fn push_run(line: &mut Vec<Run>, text: &str, style: Style) {
    match line.last_mut() {
        Some((last, last_style)) if *last_style == style => last.push_str(text),
        _ => line.push((text.to_string(), style)),
    }
}

/// Splits `text` into pieces no wider than `width` display columns.
fn hard_wrap(text: &str, width: usize) -> Vec<String> {
    let mut pieces = vec![String::new()];
    let mut used = 0;
    for ch in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > width && used > 0 {
            pieces.push(String::new());
            used = 0;
        }
        pieces.last_mut().unwrap().push(ch);
        used += w;
    }
    pieces
}

fn plain(cell: &[Run]) -> String {
    cell.iter()
        .map(|(t, _)| t.as_str())
        .collect::<String>()
        .trim()
        .to_string()
}

/// Widest a column's "keep words whole" floor may be; longer words wrap.
const MAX_WORD_FLOOR: usize = 18;

/// Fits columns into `available`: first shrink the widest column that is
/// still above its longest word (so words stay whole), then, if that is not
/// enough, shrink the widest column down to the hard minimum.
fn fit_columns(natural: &[usize], floors: &[usize], available: usize) -> Option<Vec<usize>> {
    let mut widths: Vec<usize> = natural.iter().map(|w| (*w).max(1)).collect();
    if widths.len() * MIN_COLUMN_WIDTH > available {
        return None;
    }
    let shrink = |widths: &mut Vec<usize>, floor_of: &dyn Fn(usize) -> usize| {
        while widths.iter().sum::<usize>() > available {
            let candidate = (0..widths.len())
                .filter(|&i| widths[i] > floor_of(i))
                .max_by_key(|&i| widths[i]);
            match candidate {
                Some(i) => widths[i] -= 1,
                None => break,
            }
        }
    };
    shrink(&mut widths, &|i| {
        floors[i].clamp(MIN_COLUMN_WIDTH, MAX_WORD_FLOOR)
    });
    shrink(&mut widths, &|_| MIN_COLUMN_WIDTH);
    (widths.iter().sum::<usize>() <= available).then_some(widths)
}

fn render_table(table: &TableState, width: usize) -> Vec<Line<'static>> {
    let columns = table
        .header
        .len()
        .max(table.rows.iter().map(Vec::len).max().unwrap_or(0));
    if columns == 0 {
        return Vec::new();
    }
    let cell_text = |row: &[Vec<Run>], i: usize| row.get(i).map(|c| plain(c)).unwrap_or_default();
    let natural: Vec<usize> = (0..columns)
        .map(|i| {
            std::iter::once(&table.header)
                .chain(table.rows.iter())
                .map(|row| cell_text(row, i).width())
                .max()
                .unwrap_or(1)
        })
        .collect();
    let floors: Vec<usize> = (0..columns)
        .map(|i| {
            std::iter::once(&table.header)
                .chain(table.rows.iter())
                .flat_map(|row| {
                    cell_text(row, i)
                        .split_whitespace()
                        .map(UnicodeWidthStr::width)
                        .collect::<Vec<_>>()
                })
                .max()
                .unwrap_or(1)
        })
        .collect();
    // Each column costs its width plus " │ " padding; the edges add 4.
    let available = width.saturating_sub(3 * columns + 1);
    let Some(widths) = fit_columns(&natural, &floors, available) else {
        return records_fallback(table, columns, width);
    };
    let border = theme::muted();
    let rule = |left: &str, mid: &str, right: &str| {
        let body: Vec<String> = widths.iter().map(|w| "─".repeat(w + 2)).collect();
        Line::from(Span::styled(
            format!("{left}{}{right}", body.join(mid)),
            border,
        ))
    };
    let mut lines = vec![rule("┌", "┬", "┐")];
    let header_style = Style::default()
        .fg(theme::ACCENT)
        .add_modifier(Modifier::BOLD);
    if !table.header.is_empty() {
        lines.extend(table_row(
            &table.header,
            &widths,
            &table.alignments,
            Some(header_style),
        ));
        lines.push(rule("├", "┼", "┤"));
    }
    for row in &table.rows {
        lines.extend(table_row(row, &widths, &table.alignments, None));
    }
    lines.push(rule("└", "┴", "┘"));
    lines
}

fn table_row(
    row: &[Vec<Run>],
    widths: &[usize],
    alignments: &[Alignment],
    force: Option<Style>,
) -> Vec<Line<'static>> {
    let wrapped: Vec<Vec<Vec<Run>>> = widths
        .iter()
        .enumerate()
        .map(|(i, w)| {
            let cell: Vec<Run> = row
                .get(i)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|(t, s)| (t, force.unwrap_or(s)))
                .collect();
            let trimmed = trim_runs(cell);
            if trimmed.is_empty() {
                vec![Vec::new()]
            } else {
                wrap_runs(&trimmed, *w)
            }
        })
        .collect();
    let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
    let border = theme::muted();
    (0..height)
        .map(|line_no| {
            let mut spans = vec![Span::styled("│ ", border)];
            for (i, w) in widths.iter().enumerate() {
                let runs = wrapped[i].get(line_no).cloned().unwrap_or_default();
                let used: usize = runs.iter().map(|(t, _)| t.width()).sum();
                let gap = w.saturating_sub(used);
                let (left, right) = match alignments.get(i) {
                    Some(Alignment::Right) => (gap, 0),
                    Some(Alignment::Center) => (gap / 2, gap - gap / 2),
                    _ => (0, gap),
                };
                spans.push(Span::raw(" ".repeat(left)));
                spans.extend(runs.into_iter().map(|(t, s)| Span::styled(t, s)));
                spans.push(Span::raw(" ".repeat(right)));
                let sep = if i + 1 == widths.len() {
                    " │"
                } else {
                    " │ "
                };
                spans.push(Span::styled(sep, border));
            }
            Line::from(spans)
        })
        .collect()
}

fn trim_runs(mut runs: Vec<Run>) -> Vec<Run> {
    if let Some((first, _)) = runs.first_mut() {
        *first = first.trim_start().to_string();
    }
    if let Some((last, _)) = runs.last_mut() {
        *last = last.trim_end().to_string();
    }
    runs.retain(|(t, _)| !t.is_empty());
    runs
}

/// Too narrow for a grid: one "header: value" block per row.
fn records_fallback(table: &TableState, columns: usize, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for row in &table.rows {
        for i in 0..columns {
            let key = table.header.get(i).map(|c| plain(c)).unwrap_or_default();
            let value = row.get(i).map(|c| plain(c)).unwrap_or_default();
            let runs = vec![
                (format!("{key}: "), Style::default().fg(theme::ACCENT)),
                (value, Style::default()),
            ];
            for line in wrap_runs(&runs, width) {
                lines.push(Line::from(
                    line.into_iter()
                        .map(|(t, s)| Span::styled(t, s))
                        .collect::<Vec<_>>(),
                ));
            }
        }
        lines.push(Line::from(Span::styled(
            "─".repeat(width.min(20)),
            theme::muted(),
        )));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn draws_tables_as_boxes() {
        let md = "| Agent | Status |\n|---|---:|\n| Invoice | Approved |\n| Churn | 3 |";
        let out = text(&render(md, 40));
        assert_eq!(out[0], "┌─────────┬──────────┐");
        assert_eq!(
            out[1], "│ Agent   │   Status │",
            "headers follow column alignment"
        );
        assert_eq!(out[2], "├─────────┼──────────┤");
        assert_eq!(out[3], "│ Invoice │ Approved │");
        assert_eq!(out[4], "│ Churn   │        3 │", "right-aligned column");
        assert_eq!(out[5], "└─────────┴──────────┘");
    }

    #[test]
    fn wraps_cells_to_fit_narrow_widths() {
        let md = "| Name | Description |\n|---|---|\n| a | a long description that cannot fit |";
        let out = text(&render(md, 30));
        assert!(out.iter().all(|l| l.width() <= 30), "{out:#?}");
        assert!(out.len() > 5, "the long cell wraps onto extra lines");
    }

    #[test]
    fn shrinks_prose_columns_before_splitting_words() {
        let md = "| Agent | Notes |\n|---|---|\n| Invoice-Extractor | pulls structured data out of invoice files every night |";
        let out = text(&render(md, 50));
        assert!(
            out.iter().any(|l| l.contains("Invoice-Extractor")),
            "{out:#?}"
        );
        assert!(out.iter().all(|l| l.width() <= 50));
    }

    #[test]
    fn falls_back_to_records_when_too_narrow() {
        let md = "| a | b | c | d |\n|---|---|---|---|\n| 1 | 2 | 3 | 4 |";
        let out = text(&render(md, 12));
        assert!(out.contains(&"a: 1".to_string()), "{out:#?}");
    }

    #[test]
    fn styles_inline_markup_and_lists() {
        let lines = render("Use **bold** and `code`.\n\n- one\n- two\n\n1. first", 40);
        let out = text(&lines);
        assert_eq!(out[0], "Use bold and code.");
        assert!(lines[0]
            .spans
            .iter()
            .any(|s| s.content == "bold" && s.style.add_modifier.contains(Modifier::BOLD)));
        assert_eq!(out[2], "• one");
        assert_eq!(out[3], "• two");
        assert_eq!(out[5], "1. first");
    }

    #[test]
    fn wraps_list_items_with_hanging_indent() {
        let out = text(&render("- alpha beta gamma delta", 14));
        assert_eq!(out, vec!["• alpha beta", "  gamma delta"]);
    }

    #[test]
    fn keeps_code_blocks_verbatim() {
        let out = text(&render("```sql\nselect *\n  from t\n```", 40));
        assert_eq!(out, vec!["  sql", "  │ select *", "  │   from t"]);
    }

    #[test]
    fn headings_and_quotes() {
        let out = text(&render("## Title\n\n> note here", 40));
        assert_eq!(out[0], "Title");
        assert_eq!(out[2], "▌ note here");
    }

    #[test]
    fn never_exceeds_width() {
        let md = "word ".repeat(50) + &"x".repeat(80);
        for line in text(&render(&md, 25)) {
            assert!(line.width() <= 25, "{line}");
        }
    }
}
