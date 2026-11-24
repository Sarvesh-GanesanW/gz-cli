//! Tools the chat server runs itself (lakehouse queries, document search,
//! …): what they were asked and what they returned, as one line each, with
//! the full redacted result kept for the expanded transcript.
//!
//! Event shapes: gz-chat-eks agent/agent_factory.py `_handleToolUse`
//! (`tool_use` with a redacted `input`) and `_toolResultEvent`
//! (`tool_result` with redacted `content` and an optional `status`).

use std::collections::HashMap;

use regex::Regex;
use serde_json::Value;

use super::AssistantEvent;
use crate::assistant::tools::MAX_OUTPUT_BYTES;
use crate::cmd::stream::{ServerToolResult, ServerToolUse};

/// Longest input or result summary kept; the screen cuts further to fit.
const MAX_SUMMARY_CHARS: usize = 200;
/// Input fields that say best what a call is about, in order of preference.
const TELLING_FIELDS: &[&str] = &[
    "query",
    "sql",
    "statement",
    "question",
    "prompt",
    "search",
    "path",
    "filePath",
    "file_path",
    "url",
    "table",
    "tableName",
    "table_name",
    "dataset",
    "pattern",
    "command",
    "name",
    "title",
    "id",
];
/// Result fields that hold the returned rows.
const ROW_FIELDS: &[&str] = &["rows", "data", "results", "items", "records"];

/// The server tools started in one turn, by tool use id, so their results
/// can be paired with the line that announced them, in whichever leg of
/// the turn they arrive.
#[derive(Default)]
pub struct ServerCalls {
    started: HashMap<String, (String, String)>,
}

impl ServerCalls {
    /// The event announcing `call`: its summary is the tool's name and a
    /// one-line view of its input.
    pub fn started(&mut self, call: &ServerToolUse) -> AssistantEvent {
        let summary = match input_summary(&call.input) {
            Some(input) => format!("{} {input}", call.name),
            None => call.name.clone(),
        };
        if let Some(id) = &call.id {
            self.started
                .insert(id.clone(), (call.name.clone(), summary.clone()));
        }
        AssistantEvent::ToolStarted {
            name: call.name.clone(),
            summary,
            local: false,
        }
    }

    /// The event finishing a call this turn started; `None` for results
    /// of calls it didn't announce (gz's own client tools among them).
    pub fn finished(&mut self, result: &ServerToolResult) -> Option<AssistantEvent> {
        let (name, summary) = self.started.remove(&result.id)?;
        let is_error = result.status.as_deref() == Some("error");
        let output = full_output(&result.content);
        let detail = result_summary(is_error, &result.content, &output);
        Some(AssistantEvent::ServerToolFinished {
            name,
            summary,
            is_error,
            detail,
            output: capped(output),
        })
    }
}

/// One line saying what the call was asked: its most telling field (the
/// query, path, SQL, …) or a short `key: value` list. The input may still
/// be a partial JSON string when the server announced the call early.
pub fn input_summary(input: &Value) -> Option<String> {
    let summary = match input {
        Value::Object(fields) => {
            let telling = TELLING_FIELDS
                .iter()
                .find_map(|key| fields.get(*key).and_then(scalar_text));
            telling.or_else(|| {
                let pairs: Vec<String> = fields
                    .iter()
                    .filter_map(|(key, value)| Some(format!("{key}: {}", scalar_text(value)?)))
                    .collect();
                (!pairs.is_empty()).then(|| pairs.join(", "))
            })?
        }
        Value::String(text) => match serde_json::from_str::<Value>(text) {
            Ok(parsed @ Value::Object(_)) => return input_summary(&parsed),
            _ => partial_field(text)?,
        },
        other => scalar_text(other)?,
    };
    let line = one_line(&summary);
    (!line.is_empty()).then(|| clip(&line))
}

/// A string value of a JSON object still being written: the most telling
/// field's (`"sql": "sel…`), else the first one's. A value may be cut off
/// mid-way, so its closing quote is not required.
fn partial_field(text: &str) -> Option<String> {
    let value_of = |key: &str| -> Option<String> {
        let pattern = format!(r#""{key}"\s*:\s*"((?:[^"\\]|\\.)*)"#);
        let field = Regex::new(&pattern).ok()?;
        let value = field.captures(text)?.get(1)?.as_str();
        let value = unescape(value);
        (!value.trim().is_empty()).then_some(value)
    };
    TELLING_FIELDS
        .iter()
        .find_map(|key| value_of(&regex::escape(key)))
        .or_else(|| value_of(r"\w+"))
}

/// A JSON string body with its escapes read, a trailing lone backslash
/// (cut off mid-escape) dropped.
fn unescape(value: &str) -> String {
    let mut text = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            text.push(ch);
            continue;
        }
        match chars.next() {
            Some('n' | 'r' | 't') => text.push(' '),
            Some(other) => text.push(other),
            None => {}
        }
    }
    text
}

fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) if !text.trim().is_empty() => Some(text.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Whitespace runs (newlines in SQL included) collapsed to one space.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clip(text: &str) -> String {
    if text.chars().count() <= MAX_SUMMARY_CHARS {
        return text.to_string();
    }
    let kept: String = text.chars().take(MAX_SUMMARY_CHARS - 1).collect();
    format!("{kept}…")
}

/// The whole result as readable text: text blocks as they are, JSON
/// pretty-printed. Strands results are a list of `{text}` / `{json}` blocks.
pub fn full_output(content: &Value) -> String {
    match content {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .map(block_text)
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        other => pretty(other),
    }
}

/// The output kept for the transcript, cut to the size gz keeps of its
/// own tools' results so a huge query result doesn't sit in memory and
/// redraw on every frame. The summary line is built from the whole result.
fn capped(output: String) -> String {
    if output.len() <= MAX_OUTPUT_BYTES {
        return output;
    }
    let mut end = MAX_OUTPUT_BYTES;
    while !output.is_char_boundary(end) {
        end -= 1;
    }
    let more = output.len() - end;
    format!(
        "{}\n… {} more not kept here (the model received it all)",
        &output[..end],
        size(more)
    )
}

fn block_text(block: &Value) -> String {
    if let Some(text) = block["text"].as_str() {
        return text.to_string();
    }
    if !block["json"].is_null() {
        return pretty(&block["json"]);
    }
    match block {
        Value::String(text) => text.clone(),
        other => pretty(other),
    }
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

/// One line saying how the call went: the error, or how many rows came
/// back, the size, and the first line of the result.
pub fn result_summary(is_error: bool, content: &Value, output: &str) -> String {
    let first_line = first_text_line(content).or_else(|| compact_json(content));
    if is_error {
        return match first_line {
            Some(line) => format!("Error: {line}"),
            None => "Error".to_string(),
        };
    }
    if output.trim().is_empty() {
        return "Done (no output)".to_string();
    }
    let mut parts = Vec::new();
    if let Some(rows) = row_count(content) {
        parts.push(format!("{rows} {}", if rows == 1 { "row" } else { "rows" }));
    }
    parts.push(size(output.len()));
    let mut line = parts.join(" · ");
    if let Some(first) = first_line {
        line.push_str(&format!(" · {first}"));
    }
    line
}

/// The first non-empty line of the result's text: a text block's, or the
/// whole content when it is a string.
fn first_text_line(content: &Value) -> Option<String> {
    let text = match content {
        Value::String(text) => text.as_str(),
        Value::Array(blocks) => blocks.iter().find_map(|block| {
            block["text"]
                .as_str()
                .or(block.as_str())
                .filter(|t| !t.trim().is_empty())
        })?,
        _ => return None,
    };
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(clip)
}

/// A JSON result on one line (`{"columns":["count"],"rows":[[12]]}`),
/// since its pretty form's first line says nothing.
fn compact_json(content: &Value) -> Option<String> {
    let json = match content {
        Value::Array(blocks) => blocks
            .iter()
            .map(|block| &block["json"])
            .find(|json| !json.is_null())?,
        Value::Object(_) => content,
        _ => return None,
    };
    Some(clip(&json.to_string()))
}

/// Rows in the result, when it carries a list of them (`rows`, `data`, …)
/// at the top level or in a JSON block.
fn row_count(content: &Value) -> Option<usize> {
    let in_object = |value: &Value| {
        ROW_FIELDS
            .iter()
            .find_map(|key| value.get(*key).and_then(Value::as_array).map(Vec::len))
    };
    match content {
        Value::Array(blocks) => blocks.iter().find_map(|block| {
            let json = &block["json"];
            match json {
                Value::Array(rows) => Some(rows.len()),
                Value::Object(_) => in_object(json),
                _ => None,
            }
        }),
        Value::Object(_) => in_object(content),
        _ => None,
    }
}

fn size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn input_summary_prefers_the_telling_field() {
        let sql = json!({"limit": 10, "sql": "select *\n  from sales.orders\n where id = 1"});
        assert_eq!(
            input_summary(&sql).as_deref(),
            Some("select * from sales.orders where id = 1")
        );
        let path = json!({"path": "/reports/q3.pdf", "page": 2});
        assert_eq!(input_summary(&path).as_deref(), Some("/reports/q3.pdf"));
        let other = json!({"workspace": "demo", "limit": 5, "nested": {"x": 1}});
        assert_eq!(
            input_summary(&other).as_deref(),
            Some("limit: 5, workspace: demo")
        );
        assert_eq!(input_summary(&json!({})), None);
        assert_eq!(input_summary(&Value::Null), None);
    }

    #[test]
    fn input_summary_reads_json_strings_whole_or_partial() {
        assert_eq!(
            input_summary(&json!("{\"query\": \"churn by region\"}")).as_deref(),
            Some("churn by region")
        );
        assert_eq!(
            input_summary(&json!("{\"query\": \"churn by reg")).as_deref(),
            Some("churn by reg")
        );
        assert_eq!(input_summary(&json!("")), None);
    }

    #[test]
    fn partial_input_shows_the_telling_field_even_when_it_is_not_first() {
        assert_eq!(
            input_summary(&json!(
                "{\"workspace\":\"demo\",\"sql\":\"select\\n  count(*) fr"
            ))
            .as_deref(),
            Some("select count(*) fr")
        );
        assert_eq!(
            input_summary(&json!("{\"workspace\":\"demo\",\"sql\":\"")).as_deref(),
            Some("demo"),
            "an empty telling value falls back to another field"
        );
        assert_eq!(
            input_summary(&json!("{\"path\":\"C:\\\\data\\\"x\\")).as_deref(),
            Some("C:\\data\"x"),
            "escapes are read and a cut-off escape dropped"
        );
        assert_eq!(input_summary(&json!("{\"limit\": 5")), None);
    }

    #[test]
    fn long_inputs_are_clipped() {
        let long = json!({"query": "x".repeat(500)});
        let summary = input_summary(&long).unwrap();
        assert_eq!(summary.chars().count(), MAX_SUMMARY_CHARS);
        assert!(summary.ends_with('…'));
    }

    #[test]
    fn results_report_rows_size_and_first_line() {
        let content = json!([{"json": {"rows": [[1], [2], [3]], "columns": ["id"]}}]);
        let output = full_output(&content);
        let line = result_summary(false, &content, &output);
        assert!(line.starts_with("3 rows · "), "{line}");
        assert!(
            line.ends_with(r#" B · {"columns":["id"],"rows":[[1],[2],[3]]}"#),
            "{line}"
        );
        let text = json!([{"text": "Found 2 documents\nfirst.pdf\nsecond.pdf"}]);
        let output = full_output(&text);
        assert_eq!(output, "Found 2 documents\nfirst.pdf\nsecond.pdf");
        assert_eq!(
            result_summary(false, &text, &output),
            "38 B · Found 2 documents"
        );
        let failed = json!([{"text": "\nTable not found: sales.x"}]);
        assert_eq!(
            result_summary(true, &failed, &full_output(&failed)),
            "Error: Table not found: sales.x"
        );
        assert_eq!(result_summary(false, &json!([]), ""), "Done (no output)");
    }

    #[test]
    fn huge_results_are_kept_cut_with_a_note() {
        let mut calls = ServerCalls::default();
        calls.started(&ServerToolUse {
            name: "Query lakehouse".into(),
            id: Some("big".into()),
            input: json!({"sql": "select *"}),
        });
        let rows = "é".repeat(MAX_OUTPUT_BYTES);
        let Some(AssistantEvent::ServerToolFinished { detail, output, .. }) =
            calls.finished(&ServerToolResult {
                id: "big".into(),
                status: None,
                content: json!([{"text": rows}]),
            })
        else {
            panic!("not paired");
        };
        assert!(
            detail.starts_with("112.0 KB · "),
            "summed from the whole result: {detail}"
        );
        let (kept, note) = output.rsplit_once('\n').unwrap();
        assert!(kept.len() <= MAX_OUTPUT_BYTES);
        assert_eq!(
            note,
            "… 56.0 KB more not kept here (the model received it all)"
        );
    }

    #[test]
    fn pairs_results_with_the_calls_this_stream_started() {
        let mut calls = ServerCalls::default();
        let started = calls.started(&ServerToolUse {
            name: "Query lakehouse".into(),
            id: Some("t1".into()),
            input: json!({"sql": "select 1"}),
        });
        assert_eq!(
            started,
            AssistantEvent::ToolStarted {
                name: "Query lakehouse".into(),
                summary: "Query lakehouse select 1".into(),
                local: false,
            }
        );
        let result = ServerToolResult {
            id: "t1".into(),
            status: Some("success".into()),
            content: json!([{"text": "1"}]),
        };
        let Some(AssistantEvent::ServerToolFinished {
            name,
            summary,
            is_error,
            detail,
            output,
        }) = calls.finished(&result)
        else {
            panic!("not paired");
        };
        assert_eq!(
            (name.as_str(), summary.as_str(), is_error),
            ("Query lakehouse", "Query lakehouse select 1", false)
        );
        assert_eq!((detail.as_str(), output.as_str()), ("1 B · 1", "1"));
        assert!(calls.finished(&result).is_none(), "paired once");
        let unknown = ServerToolResult {
            id: "client-call".into(),
            status: None,
            content: json!([]),
        };
        assert!(calls.finished(&unknown).is_none());
    }
}
