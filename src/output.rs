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

fn print_table(value: &Value) {
    match tabular(value) {
        Value::Array(items) => print_array_table(items),
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
    for key in ["data", "response", "rows", "records", "results", "items"] {
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

fn print_array_table(items: &[Value]) {
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
    let mut columns: Vec<String> = Vec::new();
    for item in items {
        if let Value::Object(map) = item {
            for key in map.keys() {
                if !columns.iter().any(|c| c == key) {
                    columns.push(key.clone());
                }
            }
        }
    }
    if columns.len() > 12 {
        columns.truncate(12);
    }
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
                let text = item
                    .get(col)
                    .map(scalar_text)
                    .unwrap_or_else(|| "—".to_string());
                Cell::new(truncate_cell(&text))
            })
            .collect();
        table.add_row(row);
    }
    println!("{table}");
    eprintln!("{}", format!("{} row(s)", items.len()).dimmed());
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

fn scalar_text(value: &Value) -> String {
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
    fn truncates_arrays_recursively() {
        let mut value = json!({"rows": [1, 2, 3, 4], "nested": {"deep": [1, 2, 3]}});
        truncate_arrays(&mut value, 2);
        assert_eq!(value["rows"].as_array().unwrap().len(), 2);
        assert_eq!(value["nested"]["deep"].as_array().unwrap().len(), 2);
    }
}
