//! Query results: reading the lakehouse page format and drawing it as a
//! table (or one field per line in expanded mode).
//!
//! A page is `{data: [rows], schema: [{name, type}], pageNumber, hasMore,
//! totalRecords, …}` (groundzero-iceberg iceberghelper/query_results.py
//! `page_meta`/`Page::to_json`); a statement without rows answers
//! `{status: "Success", message}`; a query still running answers
//! `{status: "RUNNING", queryExecutionId}`.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

use crate::output;
use crate::tui::theme;

/// Widest a column is drawn before its cells are cut with `…`.
const MAX_COLUMN_WIDTH: usize = 40;
const MIN_COLUMN_WIDTH: usize = 3;
const NULL: &str = "—";

#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    /// Column names in the query's own order.
    pub columns: Vec<String>,
    pub rows: Vec<Value>,
    pub page_number: u32,
    pub has_more: bool,
    pub total_records: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Rows(Page),
    /// Finished; the session kept the result to be read page by page.
    Stored,
    /// A statement without rows (`USE`, DDL), with the server's message.
    Done(String),
    /// Still running on the session; poll its status.
    Running,
}

pub fn parse_reply(response: &Value) -> Reply {
    if response["status"] == "RUNNING" {
        return Reply::Running;
    }
    if response["resultFormat"] == "arrow" {
        return Reply::Stored;
    }
    if response.get("data").is_none() && response.get("pageNumber").is_none() {
        let message = response["message"]
            .as_str()
            .unwrap_or("Statement executed.")
            .to_string();
        return Reply::Done(message);
    }
    Reply::Rows(parse_page(response))
}

pub fn parse_page(response: &Value) -> Page {
    let rows: Vec<Value> = response["data"].as_array().cloned().unwrap_or_default();
    let mut columns: Vec<String> = response["schema"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["name"].as_str().map(str::to_string))
        .collect();
    if columns.is_empty() {
        columns = rows
            .first()
            .and_then(Value::as_object)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
    }
    Page {
        columns,
        rows,
        page_number: response["pageNumber"].as_u64().unwrap_or(1) as u32,
        has_more: response["hasMore"] == Value::Bool(true),
        total_records: response["totalRecords"].as_u64(),
    }
}

fn cell(row: &Value, column: &str) -> String {
    match row.get(column) {
        None | Some(Value::Null) => NULL.to_string(),
        Some(value) => output::scalar_text(value).replace(['\n', '\r', '\t'], " "),
    }
}

fn fit(text: &str, width: usize) -> String {
    if text.width() <= width {
        return format!("{text}{}", " ".repeat(width - text.width()));
    }
    let mut out = String::new();
    for ch in text.chars() {
        if out.width() + 1 >= width {
            break;
        }
        out.push(ch);
    }
    out.push('…');
    format!("{out}{}", " ".repeat(width.saturating_sub(out.width())))
}

/// Column widths that fit `width`, shrinking the widest first; `None` when
/// even the narrowest layout overflows.
fn column_widths(page: &Page, width: usize) -> Option<Vec<usize>> {
    let mut widths: Vec<usize> = page
        .columns
        .iter()
        .map(|c| {
            page.rows
                .iter()
                .map(|r| cell(r, c).width())
                .chain(std::iter::once(c.width()))
                .max()
                .unwrap_or(1)
                .clamp(MIN_COLUMN_WIDTH, MAX_COLUMN_WIDTH)
        })
        .collect();
    // " a │ b " — one leading space, " │ " between columns.
    let chrome = 1 + 3 * page.columns.len().saturating_sub(1);
    let available = width.checked_sub(chrome)?;
    if widths.len() * MIN_COLUMN_WIDTH > available {
        return None;
    }
    while widths.iter().sum::<usize>() > available {
        let widest = (0..widths.len()).max_by_key(|&i| widths[i])?;
        widths[widest] -= 1;
    }
    Some(widths)
}

/// The page as styled lines: a table, or expanded records when asked for
/// (or when the table cannot fit).
pub fn render(page: &Page, width: usize, expanded: bool) -> Vec<Line<'static>> {
    if page.columns.is_empty() {
        return vec![Line::from(Span::styled("(no columns)", theme::muted()))];
    }
    match (expanded, column_widths(page, width)) {
        (false, Some(widths)) => table(page, &widths),
        _ => records(page, width),
    }
}

fn table(page: &Page, widths: &[usize]) -> Vec<Line<'static>> {
    let sep = || Span::styled(" │ ", theme::muted());
    let mut header = vec![Span::raw(" ")];
    for (i, (column, w)) in page.columns.iter().zip(widths).enumerate() {
        if i > 0 {
            header.push(sep());
        }
        header.push(Span::styled(fit(column, *w), theme::header_cell()));
    }
    let rule: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
    let mut lines = vec![
        Line::from(header),
        Line::from(Span::styled(
            format!("─{}", rule.join("─┼─")),
            theme::muted(),
        )),
    ];
    for row in &page.rows {
        let mut spans = vec![Span::raw(" ")];
        for (i, (column, w)) in page.columns.iter().zip(widths).enumerate() {
            if i > 0 {
                spans.push(sep());
            }
            let text = cell(row, column);
            let style = if text == NULL {
                theme::muted()
            } else {
                Style::default()
            };
            spans.push(Span::styled(fit(&text, *w), style));
        }
        lines.push(Line::from(spans));
    }
    lines
}

fn records(page: &Page, width: usize) -> Vec<Line<'static>> {
    let key_width = page
        .columns
        .iter()
        .map(|c| c.width())
        .max()
        .unwrap_or(1)
        .min(width / 2);
    let value_width = width.saturating_sub(key_width + 4).max(MIN_COLUMN_WIDTH);
    let mut lines = Vec::new();
    for (i, row) in page.rows.iter().enumerate() {
        let title = format!("─[ row {} ]", i + 1);
        let fill = width.saturating_sub(title.width());
        lines.push(Line::from(Span::styled(
            format!("{title}{}", "─".repeat(fill)),
            theme::muted(),
        )));
        for column in &page.columns {
            lines.push(Line::from(vec![
                Span::styled(format!(" {}", fit(column, key_width)), theme::header_cell()),
                Span::styled(" │ ", theme::muted()),
                Span::raw(fit(&cell(row, column), value_width).trim_end().to_string()),
            ]));
        }
    }
    lines
}

/// `3 rows · 0.42s · \more for the next page`.
pub fn summary(page: &Page, elapsed: Option<std::time::Duration>) -> Line<'static> {
    let count = page.rows.len();
    let mut text = format!("{count} row{}", if count == 1 { "" } else { "s" });
    if let Some(total) = page.total_records.filter(|t| *t as usize > count) {
        text.push_str(&format!(" of {total}"));
    }
    if page.page_number > 1 {
        text.push_str(&format!(" · page {}", page.page_number));
    }
    if let Some(elapsed) = elapsed {
        text.push_str(&format!(" · {:.2}s", elapsed.as_secs_f64()));
    }
    if page.has_more {
        text.push_str(" · space or \\more for the next page");
    }
    Line::from(Span::styled(
        text,
        theme::muted().add_modifier(Modifier::ITALIC),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    fn page() -> Page {
        parse_page(&json!({
            "schema": [{"name": "name", "type": "string"}, {"name": "amount", "type": "decimal"}, {"name": "note", "type": "string"}],
            "data": [
                {"name": "a", "amount": "12.500000", "note": null},
                {"name": "b", "amount": "3", "note": "ok"},
            ],
            "pageNumber": 1, "hasMore": true, "totalRecords": 40,
        }))
    }

    #[test]
    fn reads_pages_messages_and_running_replies() {
        assert_eq!(
            parse_reply(&json!({"status": "RUNNING", "queryExecutionId": "q"})),
            Reply::Running
        );
        assert_eq!(
            parse_reply(
                &json!({"status": "SUCCEEDED", "resultFormat": "arrow", "cachedRecords": 3})
            ),
            Reply::Stored
        );
        assert_eq!(
            parse_reply(&json!({"status": "Success", "message": "Query executed successfully."})),
            Reply::Done("Query executed successfully.".into())
        );
        let Reply::Rows(page) = parse_reply(&json!({"data": [{"x": 1}], "pageNumber": 1})) else {
            panic!("expected rows");
        };
        assert_eq!(
            page.columns,
            vec!["x"],
            "falls back to row keys without a schema"
        );
    }

    #[test]
    fn keeps_query_column_order_and_tidies_values() {
        let out = text(&render(&page(), 60, false));
        assert!(out[0].starts_with(" name │ amount │ note"), "{out:#?}");
        assert!(out[2].contains("12.5"), "decimals are tidied: {out:#?}");
        assert!(out[2].contains("—"), "null is a dash: {out:#?}");
    }

    #[test]
    fn narrow_terminals_cut_cells_or_fall_back_to_records() {
        let wide = parse_page(
            &json!({"data": [{"description": "a very long description indeed", "id": 1}]}),
        );
        for line in text(&render(&wide, 24, false)) {
            assert!(line.width() <= 24, "{line}");
        }
        let many: serde_json::Map<String, Value> =
            (0..12).map(|i| (format!("c{i}"), json!(i))).collect();
        let crowded = parse_page(&json!({"data": [many]}));
        assert!(text(&render(&crowded, 30, false))[0].starts_with("─[ row 1 ]"));
    }

    #[test]
    fn expanded_mode_shows_one_field_per_line() {
        let out = text(&render(&page(), 40, true));
        assert!(out[0].starts_with("─[ row 1 ]"));
        assert!(out[1].contains("name") && out[1].contains("│ a"));
        assert!(out.iter().any(|l| l.starts_with("─[ row 2 ]")));
    }

    #[test]
    fn summarises_counts_paging_and_time() {
        let line = summary(&page(), Some(std::time::Duration::from_millis(420)));
        let s: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(
            s,
            "2 rows of 40 · 0.42s · space or \\more for the next page"
        );
    }
}
