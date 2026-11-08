//! Cards for slash-command output (`/usage`, `/help`, `/last`, …): a
//! rounded box with the title in its top edge, sections of aligned
//! `label  value` rows, and colored meters. The same panel prints as plain
//! text for the one-shot commands (`gz usage`, `gz approvals list`).

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::code_ui::items::fit;
use crate::tui::{chat, theme};

/// Inside width of every card (and the welcome banner), so stacked cards
/// line up; narrower terminals shrink it.
pub const CARD_INNER: usize = 76;
/// Cells in a meter's bar.
const BAR_CELLS: usize = 20;
const FILLED: &str = "█";
const EMPTY: &str = "░";
/// A narrow terminal shrinks the bar, but not below this.
const MIN_BAR_CELLS: usize = 4;
/// The ` 100%` after a bar.
const PERCENT_WIDTH: usize = 5;
/// Rows sit this far right of their section heading.
const ROW_INDENT: usize = 2;
/// Space between a label column and its values.
const GAP: usize = 2;
/// Meters at or past these fractions turn yellow, then red.
const WARN_AT: f64 = 0.6;
const ALERT_AT: f64 = 0.85;
const SEPARATOR: &str = " · ";
/// A dim frame, so the title and values carry the color.
const BORDER: Style = Style::new().fg(theme::MUTED);
/// Fewest dashes between the title and the aside.
const MIN_FILL: usize = 2;

/// One slash command's output.
#[derive(Debug, Clone, PartialEq)]
pub struct Panel {
    pub title: String,
    /// Dim text at the right end of the top edge (scope, period, counts).
    pub aside: Option<String>,
    pub sections: Vec<Section>,
    /// A dim hint under the last section.
    pub footer: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Section {
    /// Bold up to its first ` · `, dim after it.
    pub heading: Option<String>,
    pub rows: Vec<Row>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    /// Dim label, normal value; ` · ` separators in the value are dimmed.
    Field { label: String, value: String },
    /// An accented key (a command or an id) with its description.
    Entry { key: String, text: String },
    /// A bar `fraction` full, colored by how full it is, with its percent
    /// and a dim detail line under it.
    Meter {
        label: String,
        fraction: f64,
        detail: String,
    },
    /// Dim text across the row.
    Note(String),
}

impl Row {
    pub fn field(label: &str, value: impl Into<String>) -> Self {
        Row::Field {
            label: label.to_string(),
            value: value.into(),
        }
    }

    pub fn entry(key: &str, text: impl Into<String>) -> Self {
        Row::Entry {
            key: key.to_string(),
            text: text.into(),
        }
    }

    pub fn meter(label: &str, fraction: f64, detail: impl Into<String>) -> Self {
        Row::Meter {
            label: label.to_string(),
            fraction: fraction.clamp(0.0, 1.0),
            detail: detail.into(),
        }
    }

    pub fn note(text: impl Into<String>) -> Self {
        Row::Note(text.into())
    }
}

impl Panel {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            aside: None,
            sections: Vec::new(),
            footer: None,
        }
    }

    pub fn aside(mut self, text: impl Into<String>) -> Self {
        self.aside = Some(text.into());
        self
    }

    /// Adds a section; empty ones are skipped.
    pub fn section(mut self, heading: Option<&str>, rows: Vec<Row>) -> Self {
        if !rows.is_empty() {
            self.sections.push(Section {
                heading: heading.map(str::to_string),
                rows,
            });
        }
        self
    }

    pub fn footer(mut self, text: impl Into<String>) -> Self {
        self.footer = Some(text.into());
        self
    }

    /// The panel as plain text, one string per line, for non-TUI output.
    pub fn plain_lines(&self) -> Vec<String> {
        let mut lines = vec![match &self.aside {
            Some(aside) => format!("{}{SEPARATOR}{aside}", self.title),
            None => self.title.clone(),
        }];
        let columns = Columns::of(self);
        // Headings one step in, every row two, so values line up across
        // sections with and without a heading.
        let pad = " ".repeat(2 * ROW_INDENT);
        for (i, section) in self.sections.iter().enumerate() {
            if i > 0 {
                lines.push(String::new());
            }
            if let Some(heading) = &section.heading {
                lines.push(format!("{}{heading}", " ".repeat(ROW_INDENT)));
            }
            for row in &section.rows {
                lines.push(match row {
                    Row::Field { label, value } => {
                        format!("{pad}{label:<w$}{value}", w = columns.label + GAP)
                    }
                    Row::Entry { key, text } => {
                        format!("{pad}{key:<w$}{text}", w = columns.key + GAP)
                    }
                    Row::Meter {
                        label,
                        fraction,
                        detail,
                    } => format!(
                        "{pad}{label:<w$}{} {:>3.0}%{SEPARATOR}{detail}",
                        plain_bar(*fraction),
                        fraction * 100.0,
                        w = columns.label + GAP
                    ),
                    Row::Note(text) => format!("{pad}{text}"),
                });
            }
        }
        if let Some(footer) = &self.footer {
            lines.push(String::new());
            lines.push(format!("{}{footer}", " ".repeat(ROW_INDENT)));
        }
        lines
    }

    /// The card at `width` columns; no line is wider than that.
    pub fn render(&self, width: usize) -> Vec<Line<'static>> {
        let columns = Columns::of(self);
        let inner = card_inner(width);
        let mut body: Vec<Vec<Span<'static>>> = Vec::new();
        for (i, section) in self.sections.iter().enumerate() {
            if i > 0 {
                body.push(Vec::new());
            }
            if let Some(heading) = &section.heading {
                body.push(heading_spans(heading));
            }
            for row in &section.rows {
                body.extend(row_lines(row, &columns, inner));
            }
        }
        if let Some(footer) = &self.footer {
            body.push(Vec::new());
            for piece in chat::wrap(footer, inner) {
                body.push(vec![Span::styled(piece, theme::muted())]);
            }
        }
        let mut lines = vec![self.top_edge(inner)];
        lines.push(framed(Vec::new(), inner, BORDER));
        lines.extend(body.into_iter().map(|spans| framed(spans, inner, BORDER)));
        lines.push(framed(Vec::new(), inner, BORDER));
        lines.push(Line::from(Span::styled(
            format!("╰{}╯", "─".repeat(inner + 2)),
            BORDER,
        )));
        lines
    }

    /// `╭─ Title ───────── aside ─╮`, dropping the aside when it won't fit.
    fn top_edge(&self, inner: usize) -> Line<'static> {
        let span = inner + 2;
        let title = fit(&self.title, span.saturating_sub(4));
        // `─ title ` before the fill, ` aside ─` after it.
        let used = title.width() + 3;
        let aside = self
            .aside
            .as_deref()
            .filter(|a| used + a.width() + 3 + MIN_FILL <= span)
            .map(|a| format!(" {a} "));
        let tail = aside.as_deref().map_or(0, |a| a.width() + 1);
        let fill = span.saturating_sub(used + tail);
        let mut spans = vec![
            Span::styled("╭─ ", BORDER),
            Span::styled(title, theme::brand()),
            Span::styled(format!(" {}", "─".repeat(fill)), BORDER),
        ];
        if let Some(aside) = aside {
            spans.push(Span::styled(aside, theme::muted()));
            spans.push(Span::styled("─", BORDER));
        }
        spans.push(Span::styled("╮", BORDER));
        Line::from(spans)
    }
}

/// Label and key column widths, shared by every section so values line up.
struct Columns {
    label: usize,
    key: usize,
}

impl Columns {
    fn of(panel: &Panel) -> Self {
        let rows = || panel.sections.iter().flat_map(|s| &s.rows);
        let label = rows()
            .filter_map(|row| match row {
                Row::Field { label, .. } | Row::Meter { label, .. } => Some(label.width()),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        let key = rows()
            .filter_map(|row| match row {
                Row::Entry { key, .. } => Some(key.width()),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        Self { label, key }
    }
}

fn row_lines(row: &Row, columns: &Columns, inner: usize) -> Vec<Vec<Span<'static>>> {
    let indent = " ".repeat(ROW_INDENT);
    match row {
        Row::Field { label, value } => hanging(
            vec![
                Span::raw(indent),
                Span::styled(pad(label, columns.label + GAP), theme::muted()),
            ],
            value,
            inner,
            Style::default(),
        ),
        Row::Entry { key, text } => hanging(
            vec![
                Span::raw(indent),
                Span::styled(pad(key, columns.key + GAP), theme::key_hint()),
            ],
            text,
            inner,
            Style::default(),
        ),
        Row::Meter {
            label,
            fraction,
            detail,
        } => {
            let lead = ROW_INDENT + columns.label + GAP;
            let cells = BAR_CELLS
                .min(inner.saturating_sub(lead + PERCENT_WIDTH))
                .max(MIN_BAR_CELLS);
            let mut first = vec![
                Span::raw(indent),
                Span::styled(pad(label, columns.label + GAP), theme::muted()),
            ];
            first.extend(meter_spans(*fraction, cells));
            let mut lines = vec![first];
            lines.extend(hanging(
                vec![Span::raw(" ".repeat(lead))],
                detail,
                inner,
                theme::muted(),
            ));
            lines
        }
        Row::Note(text) => chat::wrap(text, inner.saturating_sub(ROW_INDENT))
            .into_iter()
            .map(|piece| {
                vec![
                    Span::raw(indent.clone()),
                    Span::styled(piece, theme::muted()),
                ]
            })
            .collect(),
    }
}

/// `lead` then `text` wrapped under itself, so continuation lines start at
/// the value column.
fn hanging(
    lead: Vec<Span<'static>>,
    text: &str,
    inner: usize,
    style: Style,
) -> Vec<Vec<Span<'static>>> {
    let column: usize = lead.iter().map(|s| s.content.width()).sum();
    let room = inner.saturating_sub(column);
    // Leading spaces right-align numbers; wrapping would drop them.
    let body = text.trim_start_matches(' ');
    let align = &text[..text.len() - body.len()];
    let mut pieces = chat::wrap(body, room.saturating_sub(align.len()));
    if let Some(first) = pieces.first_mut() {
        first.insert_str(0, align);
    }
    pieces
        .into_iter()
        .enumerate()
        .map(|(i, piece)| {
            let mut spans = if i == 0 {
                lead.clone()
            } else {
                vec![Span::raw(" ".repeat(column))]
            };
            spans.extend(dim_separators(&piece, style));
            spans
        })
        .collect()
}

/// `text` in `style`, with each ` · ` dimmed so the values stand out.
fn dim_separators(text: &str, style: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (i, part) in text.split(SEPARATOR).enumerate() {
        if i > 0 {
            spans.push(Span::styled(SEPARATOR, theme::muted()));
        }
        if !part.is_empty() {
            spans.push(Span::styled(part.to_string(), style));
        }
    }
    spans
}

fn heading_spans(heading: &str) -> Vec<Span<'static>> {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    match heading.split_once(SEPARATOR) {
        Some((name, rest)) => vec![
            Span::styled(name.to_string(), bold),
            Span::styled(format!("{SEPARATOR}{rest}"), theme::muted()),
        ],
        None => vec![Span::styled(heading.to_string(), bold)],
    }
}

/// `█████░░░░░░░░░░░░░░░  25%`: the filled part colored by how full it is,
/// the rest a dim track that still reads as a bar at 0%.
fn meter_spans(fraction: f64, cells: usize) -> Vec<Span<'static>> {
    let fraction = fraction.clamp(0.0, 1.0);
    let filled = (fraction * cells as f64).round() as usize;
    let color = if fraction >= ALERT_AT {
        theme::ERROR
    } else if fraction >= WARN_AT {
        theme::WARN
    } else {
        theme::OK
    };
    vec![
        Span::styled(FILLED.repeat(filled), Style::default().fg(color)),
        Span::styled(EMPTY.repeat(cells - filled), theme::muted()),
        Span::styled(
            format!(" {:>3.0}%", fraction * 100.0),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ]
}

/// `▕██████░░░░░░░░░░░░░░▏` for plain text, where color isn't available.
pub fn plain_bar(fraction: f64) -> String {
    let filled = (fraction.clamp(0.0, 1.0) * BAR_CELLS as f64).round() as usize;
    format!(
        "▕{}{}▏",
        FILLED.repeat(filled),
        EMPTY.repeat(BAR_CELLS - filled)
    )
}

fn pad(text: &str, width: usize) -> String {
    format!("{text}{}", " ".repeat(width.saturating_sub(text.width())))
}

/// A card's inside width at `width` terminal columns.
pub fn card_inner(width: usize) -> usize {
    width.saturating_sub(4).min(CARD_INNER)
}

/// `│ spans … │`, padded to `inner`; overlong rows are cut to fit.
pub(crate) fn framed(spans: Vec<Span<'static>>, inner: usize, border: Style) -> Line<'static> {
    let mut row = vec![Span::styled("│ ", border)];
    let mut used = 0;
    for span in spans {
        let room = inner.saturating_sub(used);
        if room == 0 {
            break;
        }
        let width = span.content.width();
        if width <= room {
            used += width;
            row.push(span);
        } else {
            let cut = fit(&span.content, room);
            used += cut.width();
            row.push(Span::styled(cut, span.style));
        }
    }
    row.push(Span::raw(" ".repeat(inner.saturating_sub(used))));
    row.push(Span::styled(" │", border));
    Line::from(row)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Panel {
        Panel::new("Account usage")
            .aside("tenant · 2026-10-08 → 2026-11-07")
            .section(
                Some("Tokens"),
                vec![
                    Row::field("input", "  2.0M"),
                    Row::field("cache write", "287.9M"),
                ],
            )
            .section(
                None,
                vec![
                    Row::field("cost", "$71.96"),
                    Row::meter("rate limit", 0.9, "27.0M of 30.0M tokens per 5 hours"),
                ],
            )
            .footer("/usage refreshes")
    }

    fn text(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn find<'a>(line: &'a Line, content: &str) -> &'a Span<'a> {
        line.spans
            .iter()
            .find(|s| s.content.contains(content))
            .unwrap_or_else(|| panic!("no span {content:?} in {line:?}"))
    }

    #[test]
    fn renders_a_card_with_the_title_in_its_top_edge() {
        let out = text(&sample().render(100));
        assert!(out[0].starts_with("╭─ Account usage ─"), "{}", out[0]);
        assert!(out[0].ends_with(" tenant · 2026-10-08 → 2026-11-07 ─╮"));
        assert!(out.last().unwrap().starts_with("╰─"));
        let width = out[0].width();
        assert!(out.iter().all(|l| l.width() == width), "{out:#?}");
    }

    #[test]
    fn values_line_up_across_sections() {
        let out = text(&sample().render(100));
        let column = |needle: &str| {
            let line = out.iter().find(|l| l.contains(needle)).unwrap();
            line.find(needle).unwrap()
        };
        assert_eq!(column("2.0M") - 2, column("287.9M"));
        assert_eq!(column("287.9M"), column("$71.96"));
    }

    #[test]
    fn meters_color_by_how_full_they_are() {
        let lines = sample().render(100);
        let meter = lines
            .iter()
            .find(|l| text(std::slice::from_ref(l))[0].contains("rate limit"))
            .unwrap();
        assert_eq!(find(meter, FILLED).style.fg, Some(theme::ERROR));
        assert!(text(std::slice::from_ref(meter))[0].contains(" 90%"));
        assert_eq!(meter_spans(0.2, 10)[0].style.fg, Some(theme::OK));
        assert_eq!(meter_spans(0.7, 10)[0].style.fg, Some(theme::WARN));
        let over = meter_spans(1.5, 10);
        assert_eq!(
            over[0].content,
            FILLED.repeat(10),
            "overfull meters stay full"
        );
    }

    #[test]
    fn labels_are_dim_values_are_not() {
        let lines = sample().render(100);
        let cost = lines
            .iter()
            .find(|l| text(std::slice::from_ref(l))[0].contains("$71.96"))
            .unwrap();
        assert_eq!(find(cost, "cost").style, theme::muted());
        assert_eq!(find(cost, "$71.96").style, Style::default());
    }

    #[test]
    fn long_values_wrap_under_their_column_and_fit_narrow_terminals() {
        let panel = Panel::new("Permissions").section(
            None,
            vec![Row::entry(
                "L1",
                "command `cargo test --workspace --all-features` (in /a/very/long/path/to/the/project)",
            )],
        );
        for width in [30, 60, 120] {
            let out = text(&panel.render(width));
            assert!(out.iter().all(|l| l.width() <= width), "{out:#?}");
        }
        let out = text(&panel.render(60));
        let first = out.iter().position(|l| l.contains("L1")).unwrap();
        let byte = out[first].find("command").unwrap();
        let column = out[first][..byte].width();
        let next = &out[first + 1];
        let indent = next.width() - next.trim_start_matches(['│', ' ']).width();
        assert_eq!(
            indent, column,
            "continuation starts under the text:\n{out:#?}"
        );
    }

    #[test]
    fn plain_text_keeps_every_value() {
        let plain = sample().plain_lines().join("\n");
        for part in [
            "Account usage · tenant · 2026-10-08 → 2026-11-07",
            "  Tokens",
            "    cache write  287.9M",
            "  cost         $71.96",
            "  rate limit   ▕██████████████████░░▏  90% · 27.0M of 30.0M tokens per 5 hours",
            "  /usage refreshes",
        ] {
            assert!(plain.contains(part), "missing {part:?} in\n{plain}");
        }
    }

    #[test]
    fn every_card_has_the_same_width() {
        let small = Panel::new("x").section(None, vec![Row::field("a", "1")]);
        for width in [50, 100, 200] {
            let a = text(&small.render(width));
            let b = text(&sample().render(width));
            assert_eq!(a[0].width(), b[0].width());
            assert_eq!(a[0].width(), card_inner(width) + 4);
        }
    }

    #[test]
    fn empty_sections_are_skipped() {
        let panel = Panel::new("x").section(Some("Nothing"), Vec::new());
        assert!(panel.sections.is_empty());
    }
}
