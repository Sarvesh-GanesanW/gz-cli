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
    Csv,
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
        OutputFormat::Csv => print!("{}", value_csv(&owned)),
    }
    Ok(())
}

/// Rows (after envelope unwrapping) as CSV with a header; an object
/// becomes `key,value` rows and a scalar a single cell.
pub fn value_csv(value: &Value) -> String {
    match tabular(value) {
        Value::Array(items) if items.iter().all(Value::is_object) => {
            csv_text(&columns_of(items), items)
        }
        Value::Array(items) => items.iter().map(|v| csv_cell(v) + "\n").collect(),
        Value::Object(map) => {
            let mut out = String::from("key,value\n");
            for (key, val) in map {
                out.push_str(&format!("{},{}\n", csv_quote(key), csv_cell(val)));
            }
            out
        }
        scalar => csv_cell(scalar) + "\n",
    }
}

/// CSV with a header row; values are written as-is (no display tidying).
pub fn csv_text(columns: &[String], rows: &[Value]) -> String {
    let mut out = columns
        .iter()
        .map(|c| csv_quote(c))
        .collect::<Vec<_>>()
        .join(",");
    out.push('\n');
    for row in rows {
        let cells: Vec<String> = columns
            .iter()
            .map(|c| row.get(c).map(csv_cell).unwrap_or_default())
            .collect();
        out.push_str(&cells.join(","));
        out.push('\n');
    }
    out
}

fn csv_cell(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => csv_quote(s),
        Value::Number(_) | Value::Bool(_) => value.to_string(),
        other => csv_quote(&other.to_string()),
    }
}

fn csv_quote(text: &str) -> String {
    if text.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text.to_string()
    }
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
        "runs",
        "streams",
        "schedules",
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

/// Most digits a displayed decimal keeps after the point.
const DISPLAY_DECIMALS: usize = 6;

/// `"0.000000000000000000"` → `"0"`, `"12.500000"` → `"12.5"`: providers
/// send money and usage as long fixed-point strings.
fn tidy_decimal(text: &str) -> Option<String> {
    let (whole, fraction) = text.split_once('.')?;
    let digits = whole.strip_prefix('-').unwrap_or(whole);
    let is_decimal = !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
        && fraction.bytes().all(|b| b.is_ascii_digit());
    if !is_decimal || fraction.len() <= 2 {
        return None;
    }
    let kept = &fraction[..fraction.len().min(DISPLAY_DECIMALS)];
    let kept = kept.trim_end_matches('0');
    let whole = if whole == "-0" && kept.is_empty() {
        "0"
    } else {
        whole
    };
    Some(if kept.is_empty() {
        whole.to_string()
    } else {
        format!("{whole}.{kept}")
    })
}

/// `Thu, 01 Jan 2015 17:10:53 GMT` → `2015-01-01 17:10:53`: Flask's JSON
/// encoder writes datetimes as HTTP dates.
fn tidy_http_date(text: &str) -> Option<String> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let rest = text.strip_suffix(" GMT")?;
    let (_weekday, rest) = rest.split_once(", ")?;
    let mut parts = rest.split(' ');
    let (day, month, year, time) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || day.len() != 2 || year.len() != 4 || time.len() != 8 {
        return None;
    }
    let month = MONTHS.iter().position(|m| *m == month)? + 1;
    Some(format!("{year}-{month:02}-{day} {time}"))
}

/// `2026-05-04T04:15:08.298Z` → `2026-05-04 04:15` (UTC, as sent).
fn tidy_timestamp(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let shaped = bytes.len() >= 16
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15]
            .iter()
            .all(|&i| bytes[i].is_ascii_digit());
    shaped.then(|| format!("{} {}", &text[..10], &text[11..16]))
}

pub(crate) fn scalar_text(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => tidy_decimal(s)
            .or_else(|| tidy_timestamp(s))
            .or_else(|| tidy_http_date(s))
            .unwrap_or_else(|| s.clone()),
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
    fn writes_csv_with_quoting_and_raw_values() {
        let rows = json!([{"name": "a,b", "note": "say \"hi\"", "n": 1.500000, "x": null}]);
        let text = csv_text(
            &["name".into(), "note".into(), "n".into(), "x".into()],
            rows.as_array().unwrap(),
        );
        assert_eq!(text, "name,note,n,x\n\"a,b\",\"say \"\"hi\"\"\",1.5,\n");
        assert_eq!(value_csv(&json!({"data": [{"a": 1}]})), "a\n1\n");
    }

    #[test]
    fn tidies_http_dates() {
        assert_eq!(
            tidy_http_date("Thu, 01 Jan 2015 17:10:53 GMT").as_deref(),
            Some("2015-01-01 17:10:53")
        );
        assert_eq!(tidy_http_date("Thu, 1 Jan 2015 GMT"), None);
        assert_eq!(tidy_http_date("hello GMT"), None);
    }

    #[test]
    fn shortens_iso_timestamps() {
        assert_eq!(
            tidy_timestamp("2026-05-04T04:15:08.298Z").as_deref(),
            Some("2026-05-04 04:15")
        );
        assert_eq!(tidy_timestamp("2026-05-04").as_deref(), None);
        assert_eq!(tidy_timestamp("not a timestamp at all").as_deref(), None);
    }

    #[test]
    fn tidies_long_decimal_strings() {
        assert_eq!(tidy_decimal("0.000000000000000000").as_deref(), Some("0"));
        assert_eq!(tidy_decimal("12.500000").as_deref(), Some("12.5"));
        assert_eq!(tidy_decimal("3.14159265").as_deref(), Some("3.141592"));
        assert_eq!(tidy_decimal("1.50"), None, "short decimals are left alone");
        assert_eq!(tidy_decimal("v1.2.3"), None);
        assert_eq!(tidy_decimal("2026-11-05"), None);
    }

    #[test]
    fn truncates_arrays_recursively() {
        let mut value = json!({"rows": [1, 2, 3, 4], "nested": {"deep": [1, 2, 3]}});
        truncate_arrays(&mut value, 2);
        assert_eq!(value["rows"].as_array().unwrap().len(), 2);
        assert_eq!(value["nested"]["deep"].as_array().unwrap().len(), 2);
    }
}
