//! Showing a qualipilot report: one row per check with its key numbers.
//!
//! Payload shapes are qualipilot's built-in payload models
//! (qualipilot/models/payloads.py `PAYLOAD_MODELS`); a check gz doesn't know
//! shows no numbers, and `-o json` always has the full payload.

use serde_json::{json, Value};

pub const CHECK_COLUMNS: [&str; 4] = ["check", "severity", "status", "details"];
/// Columns named in one row before the rest are counted.
const SHOWN_COLUMNS: usize = 4;

/// One row per check, in the report's order.
pub fn check_rows(report: &Value) -> Value {
    report["results"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|result| {
            json!({
                "check": result["name"],
                "severity": result["severity"],
                "status": result["status"],
                "details": details(result),
            })
        })
        .collect()
}

/// What the check found, in a few words; the error when it could not run.
fn details(result: &Value) -> Value {
    if result["status"] == "failed" {
        return result["error"].clone();
    }
    let payload = &result["payload"];
    let text = match result["name"].as_str().unwrap_or_default() {
        "missing_values" => per_column(payload, "null_count", ("null", "nulls"), "no nulls"),
        "duplicates" => count(
            payload,
            "total_duplicate_rows",
            "duplicate row",
            "duplicate rows",
        ),
        "outliers" => per_column(
            payload,
            "outlier_count",
            ("outlier", "outliers"),
            "no outliers",
        ),
        "ranges" => per_column(
            payload,
            "violation_count",
            ("out of range", "out of range"),
            "no values out of range",
        ),
        "data_types" => count_of(payload, "per_column", "column typed", "columns typed"),
        "cardinality" => count_of(payload, "per_column", "column profiled", "columns profiled"),
        "freshness" => freshness(payload),
        "dataset_contract" => dataset_contract(payload),
        "quality_contract" => count(payload, "violation_count", "violation", "violations"),
        "iceberg_table" => iceberg(payload),
        "linkage" => linkage(payload),
        _ => None,
    };
    text.map_or(Value::Null, Value::from)
}

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn count(payload: &Value, field: &str, one: &str, many: &str) -> Option<String> {
    payload[field].as_u64().map(|n| plural(n, one, many))
}

fn count_of(payload: &Value, field: &str, one: &str, many: &str) -> Option<String> {
    let n = match &payload[field] {
        Value::Array(items) => items.len(),
        Value::Object(items) => items.len(),
        _ => return None,
    };
    Some(plural(n as u64, one, many))
}

/// `amount: 12 outliers, qty: 3 outliers` for the columns with findings,
/// worst first; `none` when there are none.
fn per_column(
    payload: &Value,
    field: &str,
    (one, many): (&str, &str),
    none: &str,
) -> Option<String> {
    let columns = payload["per_column"].as_array()?;
    let mut found: Vec<(&str, u64)> = columns
        .iter()
        .filter_map(|c| Some((c["column"].as_str()?, c[field].as_u64()?)))
        .filter(|(_, n)| *n > 0)
        .collect();
    if found.is_empty() {
        return Some(none.to_string());
    }
    found.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let mut parts: Vec<String> = found
        .iter()
        .take(SHOWN_COLUMNS)
        .map(|(column, n)| format!("{column}: {}", plural(*n, one, many)))
        .collect();
    if found.len() > SHOWN_COLUMNS {
        parts.push(format!("{} more columns", found.len() - SHOWN_COLUMNS));
    }
    Some(parts.join(", "))
}

fn freshness(payload: &Value) -> Option<String> {
    let columns = payload["per_column"].as_array()?;
    let parts: Vec<String> = columns
        .iter()
        .filter_map(|c| {
            let column = c["column"].as_str()?;
            let age = match c["age_hours"].as_f64() {
                Some(hours) => format!("{hours:.1} h old"),
                None => "no timestamps".to_string(),
            };
            let stale = if c["is_stale"] == true { ", stale" } else { "" };
            Some(format!("{column}: {age}{stale}"))
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join(", "))
}

fn dataset_contract(payload: &Value) -> Option<String> {
    let rows = payload["row_count"].as_u64()?;
    let mut parts = vec![plural(rows, "row", "rows")];
    let missing: Vec<&str> = payload["missing_required_columns"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    if !missing.is_empty() {
        parts.push(format!("missing {}", missing.join(", ")));
    }
    let mismatches = payload["dtype_mismatches"].as_array().map_or(0, Vec::len);
    if mismatches > 0 {
        parts.push(plural(
            mismatches as u64,
            "type mismatch",
            "type mismatches",
        ));
    }
    Some(parts.join(", "))
}

fn iceberg(payload: &Value) -> Option<String> {
    let snapshot = payload["captured_snapshot_id"].as_i64()?;
    let mut parts = vec![format!("snapshot {snapshot}")];
    if let Some(files) = payload["data_files"].as_u64() {
        parts.push(plural(files, "data file", "data files"));
    }
    if let Some(hours) = payload["snapshot_age_hours"].as_f64() {
        parts.push(format!("{hours:.1} h old"));
    }
    Some(parts.join(", "))
}

fn linkage(payload: &Value) -> Option<String> {
    if payload["skipped"] == true {
        return Some("skipped".to_string());
    }
    let matched = payload["matched_pairs"].as_u64()?;
    let clusters = payload["clusters"].as_u64()?;
    Some(format!(
        "{}, {}",
        plural(matched, "matched pair", "matched pairs"),
        plural(clusters, "cluster", "clusters")
    ))
}

/// `DemoDB.ProductionMetrics: 1,200 rows at snapshot 82340… (branch nightly)`.
pub fn headline(table: &str, report: &Value) -> String {
    let dataset = &report["dataset"];
    let rows = dataset["row_count"]
        .as_u64()
        .map(|n| format!(": {}", plural(n, "row", "rows")))
        .unwrap_or_default();
    let snapshot = dataset["source_version"]
        .as_str()
        .map(|s| format!(" at snapshot {s}"))
        .unwrap_or_default();
    let reference = dataset["source_ref"]
        .as_str()
        .map(|r| format!(" ({r})"))
        .unwrap_or_default();
    format!("{table}{rows}{snapshot}{reference}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(name: &str, severity: &str, payload: Value) -> Value {
        json!({"name": name, "severity": severity, "status": "completed", "payload": payload, "error": null})
    }

    #[test]
    fn each_check_shows_its_key_numbers() {
        let report = json!({"results": [
            result("outliers", "warn", json!({
                "per_column": [
                    {"column": "units", "lower_bound": 0.0, "upper_bound": 9.0, "outlier_count": 3},
                    {"column": "defects", "lower_bound": 0.0, "upper_bound": 2.0, "outlier_count": 41},
                    {"column": "line", "skipped": "not numeric"},
                    {"column": "shift", "lower_bound": 1.0, "upper_bound": 3.0, "outlier_count": 0},
                ],
                "quantile_provenance": {"method": "exact"},
            })),
            result("missing_values", "ok", json!({"total_null_count": 0, "worst_column_pct": 0.0, "per_column": []})),
            result("duplicates", "ok", json!({"total_duplicate_rows": 1})),
            result("data_types", "ok", json!({"per_column": {"units": "int", "line": "str"}})),
            result("dataset_contract", "ok", json!({"row_count": 1200, "min_rows": 1, "missing_required_columns": [], "dtype_mismatches": []})),
            result("custom_rule", "ok", json!({"anything": 1})),
            {"name": "ranges", "severity": "error", "status": "failed", "payload": {}, "error": "ValueError: no column"},
        ]});
        assert_eq!(
            check_rows(&report),
            json!([
                {"check": "outliers", "severity": "warn", "status": "completed", "details": "defects: 41 outliers, units: 3 outliers"},
                {"check": "missing_values", "severity": "ok", "status": "completed", "details": "no nulls"},
                {"check": "duplicates", "severity": "ok", "status": "completed", "details": "1 duplicate row"},
                {"check": "data_types", "severity": "ok", "status": "completed", "details": "2 columns typed"},
                {"check": "dataset_contract", "severity": "ok", "status": "completed", "details": "1200 rows"},
                {"check": "custom_rule", "severity": "ok", "status": "completed", "details": null},
                {"check": "ranges", "severity": "error", "status": "failed", "details": "ValueError: no column"},
            ])
        );
    }

    #[test]
    fn clean_columns_say_so_in_words() {
        let row = &check_rows(
            &json!({"results": [result("ranges", "ok", json!({"per_column": [
                {"column": "units", "violation_count": 0},
            ]}))]}),
        )[0];
        assert_eq!(row["details"], "no values out of range");
    }

    #[test]
    fn many_columns_are_counted_after_the_worst() {
        let columns: Vec<Value> = (1..=6)
            .map(|n| json!({"column": format!("c{n}"), "violation_count": n}))
            .collect();
        let row = &check_rows(
            &json!({"results": [result("ranges", "warn", json!({"per_column": columns}))]}),
        )[0];
        assert_eq!(
            row["details"],
            "c6: 6 out of range, c5: 5 out of range, c4: 4 out of range, c3: 3 out of range, 2 more columns"
        );
    }

    #[test]
    fn the_headline_names_rows_snapshot_and_ref() {
        let report = json!({"dataset": {"row_count": 1200, "source_version": "82", "source_ref": "nightly"}});
        assert_eq!(
            headline("W.d.t", &report),
            "W.d.t: 1200 rows at snapshot 82 (nightly)"
        );
        assert_eq!(headline("W.d.t", &json!({})), "W.d.t");
    }
}
