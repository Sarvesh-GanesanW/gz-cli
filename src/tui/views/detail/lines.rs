//! The detail view's text: pure functions from its state to styled lines,
//! so tests can read exactly what a user sees.

use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use serde_json::Value;

use super::{DetailView, Load, RelatedState};
use crate::tui::fields;
use crate::tui::format::{self, Shown, MASKED};
use crate::tui::screen::{Column, DetailField, SectionBody};
use crate::tui::theme;
use crate::tui::ui::widgets::{fit, highlight_json};
use crate::tui::views::{RenderCtx, View};

const COLUMN_GAP: usize = 2;
/// Widest a related-table column may get, so one long cell can't push the
/// others off screen.
const MAX_RELATED_COLUMN: usize = 40;

impl DetailView {
    pub(super) fn lines(&self, ui: RenderCtx, width: usize) -> Vec<Line<'static>> {
        if self.raw {
            let pretty = serde_json::to_string_pretty(&self.row.row).unwrap_or_default();
            return pretty.lines().map(highlight_json).collect();
        }
        let mut lines = self.header(ui.now_ms);
        match &self.load {
            Load::Loading(_) => lines.push(Line::from(vec![
                spinner(ui.tick),
                Span::styled("Loading details…", theme::muted()),
            ])),
            Load::Missing => lines.push(Line::from(Span::styled(
                self.detail.missing,
                theme::badge(theme::BadgeTone::Warn),
            ))),
            Load::Failed(err) => lines.push(Line::from(Span::styled(
                err.clone(),
                theme::badge(theme::BadgeTone::Bad),
            ))),
            Load::Ready => {}
        }
        for section in self.detail.sections {
            let body = match section.body {
                SectionBody::Fields(fields) => self.field_lines(fields, ui.now_ms),
                SectionBody::Object(path) => object_lines(fields::lookup_any(&self.row.row, path)),
                SectionBody::Text(path) => {
                    text_lines(fields::lookup_any(&self.row.row, path), width)
                }
            };
            if body.is_empty() {
                continue;
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(section.title, theme::brand())));
            lines.extend(body);
        }
        for related in &self.related {
            lines.extend(related_lines(
                related.spec.title,
                related.spec.columns,
                &related.state,
                related.spec.empty,
                ui.tick,
                width,
            ));
        }
        lines
    }

    fn header(&self, now_ms: i64) -> Vec<Line<'static>> {
        let row = &self.row.row;
        let mut title = vec![Span::styled(
            self.title(),
            theme::title().add_modifier(Modifier::BOLD),
        )];
        if let Some((path, statuses)) = self.detail.badge {
            if let Some(raw) = fields::lookup_any(row, path) {
                let (label, tone) = statuses.badge(&format::text_of(raw));
                title.push(Span::raw("  "));
                title.push(Span::styled(format!("● {label}"), theme::badge(tone)));
            }
        }
        let subtitle: Vec<String> = self
            .detail
            .subtitle
            .iter()
            .map(|(path, fmt)| format::show(fields::lookup_any(row, path), *fmt, now_ms).text)
            .filter(|text| text != format::MISSING)
            .collect();
        let mut lines = vec![Line::from(title)];
        if !subtitle.is_empty() {
            lines.push(Line::from(Span::styled(
                subtitle.join(" · "),
                theme::muted(),
            )));
        }
        lines
    }

    fn field_lines(&self, fields: &[DetailField], now_ms: i64) -> Vec<Line<'static>> {
        let row = &self.row.row;
        let shown: Vec<(&str, Shown)> = fields
            .iter()
            .filter_map(|f| {
                let value = fields::lookup_any(row, f.field);
                let missing = value
                    .is_none_or(|v| v.is_null() || v.as_str().is_some_and(|s| s.trim().is_empty()));
                if f.optional && missing {
                    return None;
                }
                Some((f.label, format::show(value, f.format, now_ms)))
            })
            .collect();
        let label_width = shown
            .iter()
            .map(|(l, _)| l.chars().count())
            .max()
            .unwrap_or(0);
        shown
            .into_iter()
            .map(|(label, value)| labelled(label, label_width, value))
            .collect()
    }
}

fn spinner(tick: u64) -> Span<'static> {
    let frame = theme::SPINNER[(tick as usize) % theme::SPINNER.len()];
    Span::styled(format!("{frame} "), Style::default().fg(theme::ACCENT))
}

fn labelled(label: &str, width: usize, value: Shown) -> Line<'static> {
    let style = value.tone.map(theme::badge).unwrap_or_default();
    Line::from(vec![
        Span::styled(format!("  {label:<width$}  "), theme::muted()),
        Span::styled(value.text, style),
    ])
}

/// Every key of a config-like object, humanized, secrets masked.
fn object_lines(object: Option<&Value>) -> Vec<Line<'static>> {
    let Some(Value::Object(map)) = object else {
        return Vec::new();
    };
    let entries: Vec<(String, Shown)> = map
        .iter()
        .map(|(key, value)| {
            let text = if format::is_secret_key(key) {
                MASKED.to_string()
            } else {
                format::text_of(value)
            };
            let shown = if value.is_null() {
                Shown {
                    text: format::MISSING.to_string(),
                    tone: Some(theme::BadgeTone::Muted),
                }
            } else {
                Shown { text, tone: None }
            };
            (format::humanize_key(key), shown)
        })
        .collect();
    let width = entries
        .iter()
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(0);
    entries
        .into_iter()
        .map(|(label, value)| labelled(&label, width, value))
        .collect()
}

/// Indent of wrapped text, matching labelled field lines.
const TEXT_INDENT: &str = "  ";

fn text_lines(value: Option<&Value>, width: usize) -> Vec<Line<'static>> {
    let text = value.map(format::text_of).unwrap_or_default();
    if text.trim().is_empty() || text == format::MISSING {
        return Vec::new();
    }
    let room = width.saturating_sub(TEXT_INDENT.len()).max(1);
    wrap(text.trim_end(), room)
        .into_iter()
        .map(|line| Line::from(format!("{TEXT_INDENT}{line}")))
        .collect()
}

/// Word-wraps `text` to `width` characters, keeping its line breaks; a word
/// longer than the width is split.
pub(crate) fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for paragraph in text.lines() {
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            let mut word = word.to_string();
            while word.chars().count() > width {
                if !line.is_empty() {
                    out.push(std::mem::take(&mut line));
                }
                let head: String = word.chars().take(width).collect();
                word = word.chars().skip(width).collect();
                out.push(head);
            }
            let needed =
                line.chars().count() + usize::from(!line.is_empty()) + word.chars().count();
            if needed > width && !line.is_empty() {
                out.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(&word);
        }
        out.push(line);
    }
    out
}

fn related_lines(
    title: &'static str,
    columns: &'static [Column],
    state: &RelatedState,
    empty: &'static str,
    tick: u64,
    width: usize,
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from("")];
    let heading = |extra: String| {
        Line::from(vec![
            Span::styled(title, theme::brand()),
            Span::styled(extra, theme::muted()),
        ])
    };
    match state {
        RelatedState::Skipped | RelatedState::Hidden => return Vec::new(),
        RelatedState::Loading(_) => {
            lines.push(heading(String::new()));
            lines.push(Line::from(vec![
                Span::raw("  "),
                spinner(tick),
                Span::styled("Loading…", theme::muted()),
            ]));
        }
        RelatedState::Failed(err) => {
            lines.push(heading(String::new()));
            lines.push(Line::from(Span::styled(
                format!("  {err}"),
                theme::badge(theme::BadgeTone::Bad),
            )));
        }
        RelatedState::Rows(rows) if rows.is_empty() => {
            lines.push(heading(String::new()));
            lines.push(Line::from(Span::styled(
                format!("  {empty}"),
                theme::muted(),
            )));
        }
        RelatedState::Rows(rows) => {
            lines.push(heading(format!("  {}", rows.len())));
            lines.extend(table_lines(columns, rows, width.saturating_sub(2)));
        }
    }
    lines
}

/// A compact table: header, then one line per row, columns sized to fit.
fn table_lines(columns: &[Column], rows: &[Vec<Shown>], width: usize) -> Vec<Line<'static>> {
    let mut widths: Vec<usize> = columns
        .iter()
        .enumerate()
        .map(|(i, c)| {
            rows.iter()
                .map(|r| r[i].text.chars().count())
                .chain(std::iter::once(c.header.chars().count()))
                .max()
                .unwrap_or(0)
                .min(MAX_RELATED_COLUMN)
        })
        .collect();
    // Shrink the widest column until the table fits.
    while widths.iter().sum::<usize>() + COLUMN_GAP * widths.len() > width {
        let Some((widest, _)) = widths.iter().enumerate().max_by_key(|(_, w)| **w) else {
            break;
        };
        if widths[widest] <= 4 {
            break;
        }
        widths[widest] -= 1;
    }
    let cell = |text: &str, w: usize| format!("{:<w$}{}", fit(text, w), " ".repeat(COLUMN_GAP));
    let header: Vec<Span> = std::iter::once(Span::raw("  "))
        .chain(
            columns
                .iter()
                .zip(&widths)
                .map(|(c, w)| Span::styled(cell(c.header, *w), theme::header_cell())),
        )
        .collect();
    let mut lines = vec![Line::from(header)];
    for row in rows {
        let spans: Vec<Span> = std::iter::once(Span::raw("  "))
            .chain(row.iter().zip(&widths).map(|(shown, w)| {
                Span::styled(
                    cell(&shown.text, *w),
                    shown.tone.map(theme::badge).unwrap_or_default(),
                )
            }))
            .collect();
        lines.push(Line::from(spans));
    }
    lines
}
