use anyhow::Result;
use comfy_table::{Cell, ContentArrangement, Table};
use owo_colors::OwoColorize;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputFormat {
    #[default]
    Table,
    Json,
    Yaml,
}

pub fn print_value(value: &Value, format: OutputFormat, limit: Option<usize>) -> Result<()> {
    let mut owned = value.clone();
    if let Some(n) = limit {
        truncate_arrays(&mut owned, n);
    }
    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&owned)?);
        }
        OutputFormat::Yaml => {
            println!("{}", serde_yaml::to_string(&owned)?);
        }
        OutputFormat::Table => print_table(&owned),
    }
    Ok(())
}

pub fn truncate_arrays(value: &mut Value, limit: usize) {
    match value {
        Value::Array(items) => {
            if items.len() > limit {
                items.truncate(limit);
            }
            for item in items.iter_mut() {
                truncate_arrays(item, limit);
            }
        }
        Value::Object(map) => {
            for item in map.values_mut() {
                truncate_arrays(item, limit);
            }
        }
        _ => {}
    }
}

/// Table output limited to `columns` (when present in the rows); JSON and
/// YAML still show everything.
pub fn print_with_columns(
    value: &Value,
    format: OutputFormat,
    limit: Option<usize>,
    columns: &[&str],
) -> Result<()> {
    if format != OutputFormat::Table {
        return print_value(value, format, limit);
    }
    let mut owned = value.clone();
    if let Some(n) = limit {
        truncate_arrays(&mut owned, n);
    }
    match tabular(&owned) {
        Value::Array(items) if items.iter().all(Value::is_object) => {
            print_array_table(items, Some(columns))
        }
        _ => print_table(&owned),
    }
    Ok(())
}

fn print_table(value: &Value) {
    match tabular(value) {
        Value::Array(items) => print_array_table(items, None),
        Value::Object(_) => print_object_table(tabular(value)),
        Value::Null => println!("{}", "(empty)".dimmed()),
        scalar => println!("{}", scalar_text(scalar)),
    }
}

/// Unwrap provider envelopes (`{success, data}`, `{status, response}`) so
/// getdata-style payloads render as data grids instead of key/value dumps.
pub(crate) fn tabular(value: &Value) -> &Value {
    let Value::Object(map) = value else {
        return value;
    };
    for key in [
        "data",
        "response",
        "rows",
        "records",
        "results",
        "items",
        "conversations",
        "connections",
        "models",
        "experiments",
        "notebooks",
        "files",
        "folders",
        "assets",
    ] {
        if let Some(inner @ Value::Array(_)) = map.get(key) {
            return inner;
        }
    }
    for key in ["response", "data"] {
        if let Some(inner @ Value::Object(_)) = map.get(key) {
            return tabular(inner);
        }
    }
    value
}

/// Identity and state columns first, timestamps last, everything else in
/// between — so the useful columns survive when a wide table gets cut.
const LEADING_COLUMNS: &[&str] = &["name", "title", "id", "Id", "ID", "type", "status", "state"];

fn column_rank(key: &str) -> usize {
    if let Some(pos) = LEADING_COLUMNS.iter().position(|c| *c == key) {
        return pos;
    }
    let lower = key.to_ascii_lowercase();
    let is_timestamp = key.ends_with("At")
        || lower.ends_with("_at")
        || lower.ends_with("date")
        || lower.ends_with("time")
        || lower.contains("timestamp");
    if is_timestamp {
        LEADING_COLUMNS.len() + 2
    } else {
        LEADING_COLUMNS.len() + 1
    }
}

/// Union of object keys across rows, in display order.
pub(crate) fn columns_of(items: &[Value]) -> Vec<String> {
    let mut columns: Vec<String> = Vec::new();
    for map in items.iter().filter_map(Value::as_object) {
        for key in map.keys() {
            if !columns.contains(key) {
                columns.push(key.clone());
            }
        }
    }
    columns.sort_by_key(|c| column_rank(c));
    columns
}

const MAX_TABLE_COLUMNS: usize = 12;

fn is_blank(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.is_empty(),
        Some(Value::Array(a)) => a.is_empty(),
        Some(Value::Object(o)) => o.is_empty(),
        _ => false,
    }
}

fn print_array_table(items: &[Value], wanted: Option<&[&str]>) {
    if items.is_empty() {
        println!("{}", "(no rows)".dimmed());
        return;
    }
    if items.iter().any(|v| !v.is_object()) {
        for item in items {
            println!("{}", scalar_text(item));
        }
        return;
    }
    let all = columns_of(items);
    let mut columns: Vec<String> = match wanted {
        Some(wanted) => wanted
            .iter()
            .filter(|w| all.iter().any(|c| c == *w))
            .map(|w| w.to_string())
            .collect(),
        // Columns with no value in any row only add width.
        None => all
            .iter()
            .filter(|c| !items.iter().all(|row| is_blank(row.get(c.as_str()))))
            .cloned()
            .collect(),
    };
    if columns.is_empty() {
        columns = all.clone();
    }
    let hidden = all
        .len()
        .saturating_sub(columns.len().min(MAX_TABLE_COLUMNS));
    columns.truncate(MAX_TABLE_COLUMNS);
    let mut table = Table::new();
    table
        .load_preset(comfy_table::presets::UTF8_FULL_CONDENSED)
        .set_content_arrangement(ContentArrangement::Dynamic);
    table.set_header(
        columns
            .iter()
            .map(|c| Cell::new(c).fg(comfy_table::Color::Cyan)),
    );
    for item in items {
        let row: Vec<Cell> = columns
            .iter()
            .map(|col| {
                let text = match item.get(col) {
                    None | Some(Value::Null) => "—".to_string(),
                    Some(value) => scalar_text(value),
                };
                Cell::new(truncate_cell(&text))
            })
            .collect();
        table.add_row(row);
    }
    println!("{table}");
    let mut summary = format!("{} row(s)", items.len());
    if hidden > 0 {
        summary.push_str(&format!(
            " · {hidden} more column(s) hidden, use -o json to see all"
        ));
    }
    eprintln!("{}", summary.dimmed());
}

fn print_object_table(value: &Value) {
    let mut table = Table::new();
    table
        .load_preset(comfy_table::presets::UTF8_FULL_CONDENSED)
        .set_content_arrangement(ContentArrangement::Dynamic);
    table.set_header(vec!["key", "value"]);
    if let Value::Object(map) = value {
        let mut keys: Vec<&String> = map.keys().collect();
        keys.sort();
        for key in keys {
            let raw = scalar_text(&map[key]);
            table.add_row(vec![
                Cell::new(key).fg(comfy_table::Color::Cyan),
                Cell::new(truncate_cell(&raw)),
            ]);
        }
    }
    println!("{table}");
}

pub(crate) fn scalar_text(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        Value::Array(items) if items.is_empty() => "[]".to_string(),
        Value::Object(map) if map.is_empty() => "{}".to_string(),
        other => serde_json::to_string(other).unwrap_or_else(|_| "?".to_string()),
    }
}

fn truncate_cell(text: &str) -> String {
    const MAX: usize = 120;
    if text.chars().count() <= MAX {
        return text.to_string();
    }
    let head: String = text.chars().take(MAX - 1).collect();
    format!("{head}…")
}

pub fn success(message: &str) {
    println!("{} {}", "✓".green(), message);
}

pub fn info(message: &str) {
    eprintln!("{} {}", "›".cyan(), message.dimmed());
}

pub fn warn(message: &str) {
    eprintln!("{} {}", "!".yellow(), message);
}

pub fn dim(text: &str) -> String {
    text.dimmed().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unwraps_tabular_envelopes() {
        let rows = json!([{"a": 1}, {"a": 2}]);
        assert_eq!(tabular(&json!({"success": true, "data": rows})), &rows);
        assert_eq!(tabular(&json!({"status": 200, "response": rows})), &rows);
        assert_eq!(tabular(&json!({"rows": rows})), &rows);
        assert_eq!(tabular(&json!({"response": {"data": rows}})), &rows);
        let scalar = json!({"success": true, "data": {"count": 3}});
        assert_eq!(tabular(&scalar), &json!({"count": 3}));
        let plain = json!({"a": 1});
        assert_eq!(tabular(&plain), &plain);
    }

    #[test]
    fn orders_identity_columns_first_and_timestamps_last() {
        let rows = json!([{"createdAt": 1, "zone": "a", "name": "x", "id": 2, "status": "ok"}]);
        let cols = columns_of(rows.as_array().unwrap());
        assert_eq!(cols, vec!["name", "id", "status", "zone", "createdAt"]);
    }

    #[test]
    fn truncates_arrays_recursively() {
        let mut value = json!({"rows": [1, 2, 3, 4], "nested": {"deep": [1, 2, 3]}});
        truncate_arrays(&mut value, 2);
        assert_eq!(value["rows"].as_array().unwrap().len(), 2);
        assert_eq!(value["nested"]["deep"].as_array().unwrap().len(), 2);
    }
}
