//! Query cost preview: how much data a query will read, before it runs.
//!
//! The lakehouse plans it from Iceberg metadata with
//! `POST /lakehouse/scanestimate {catalog, query}`
//! (groundzero-iceberg-provider-api app_routes/scan_estimate.py, branch
//! feat/scan-estimate; the web app does not call it yet): each table's files
//! are pruned by the query's filters, so the bytes are what Spark would
//! open. Where that route is not deployed, gz falls back to an upper bound:
//! the current snapshot's totals of every table the query reads
//! (`GET /lakehouse/tables/{wh}/{db}/{t}/versions`, see
//! [`crate::table_history`]), found with [`super::tables`].
//!
//! There is no price: Groundzero has no rate for a lakehouse query (per byte
//! scanned or per session hour), so only the data is shown.

use anyhow::{bail, Context, Result};
use reqwest::Method;
use serde_json::{json, Value};

use crate::http::ApiClient;
use crate::sql::statements;
use crate::sql::tables;
use crate::table_history::{History, TableName};

const SVC: &str = "lakehouse";
const ESTIMATE_PATH: &str = "/lakehouse/scanestimate";
const TOTAL_SIZE: &str = "total-files-size";
const TOTAL_FILES: &str = "total-data-files";
const TOTAL_ROWS: &str = "total-records";
/// Why the preview shows data and no money.
pub const NO_PRICE_NOTE: &str = "No cost shown: Groundzero has no price for a lakehouse query \
     (per byte scanned or per session hour), only for job runs.";

/// Bytes, files and rows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    pub bytes: u64,
    pub files: u64,
    pub rows: u64,
}

impl std::ops::Add for Totals {
    type Output = Totals;
    fn add(self, other: Totals) -> Totals {
        Totals {
            bytes: self.bytes + other.bytes,
            files: self.files + other.files,
            rows: self.rows + other.rows,
        }
    }
}

/// How the estimate was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Basis {
    /// The lakehouse planned the scan with the query's filters.
    Planned,
    /// Every table read whole: the query reads at most this.
    UpperBound,
}

impl Basis {
    fn word(self) -> &'static str {
        match self {
            Basis::Planned => "planned",
            Basis::UpperBound => "upper-bound",
        }
    }
}

/// One table's part of the scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableScan {
    pub name: String,
    pub read: Totals,
    /// The whole table, when that differs from what is read.
    pub whole: Option<Totals>,
    /// The filter the files were pruned with.
    pub filter: Option<String>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanEstimate {
    pub basis: Basis,
    pub read: Totals,
    pub tables: Vec<TableScan>,
    /// Bare names the query reads that could not be sized.
    pub not_sized: Vec<String>,
}

impl ScanEstimate {
    /// The lakehouse's planned estimate (`response` of the route).
    pub fn from_planned(response: &Value) -> Result<Self> {
        let tables = response["tables"]
            .as_array()
            .context("the scan estimate came without its tables")?
            .iter()
            .map(planned_table)
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            basis: Basis::Planned,
            read: totals_of(response, "bytes", "files", "rows")?,
            tables,
            not_sized: Vec::new(),
        })
    }

    /// Every table read whole, from its snapshot totals.
    pub fn upper_bound(tables: Vec<(TableName, Totals)>, not_sized: Vec<String>) -> Self {
        let read = tables
            .iter()
            .fold(Totals::default(), |sum, (_, t)| sum + *t);
        Self {
            basis: Basis::UpperBound,
            read,
            tables: tables
                .into_iter()
                .map(|(table, totals)| TableScan {
                    name: table.to_string(),
                    read: totals,
                    whole: None,
                    filter: None,
                    note: None,
                })
                .collect(),
            not_sized,
        }
    }

    /// The statements of a script together: the sum of what each reads.
    pub fn combine(parts: Vec<ScanEstimate>) -> Self {
        let mut combined = Self {
            basis: Basis::Planned,
            read: Totals::default(),
            tables: Vec::new(),
            not_sized: Vec::new(),
        };
        for part in parts {
            if part.basis == Basis::UpperBound {
                combined.basis = Basis::UpperBound;
            }
            combined.read = combined.read + part.read;
            combined.tables.extend(part.tables);
            for name in part.not_sized {
                if !combined.not_sized.contains(&name) {
                    combined.not_sized.push(name);
                }
            }
        }
        combined
    }

    /// "about 1.2 GB" or "up to 1.2 GB".
    pub fn amount(&self) -> String {
        let size = format_size(self.read.bytes);
        match self.basis {
            Basis::Planned => format!("about {size}"),
            Basis::UpperBound => format!("up to {size}"),
        }
    }

    /// One line: the amount, files and rows, and how it was estimated.
    pub fn headline(&self) -> String {
        let how = match self.basis {
            Basis::Planned => "planned from table metadata with the query's filters",
            Basis::UpperBound => "every table read whole, from snapshot totals",
        };
        format!(
            "Reads {} · {} files · {} rows ({how})",
            self.amount(),
            group_digits(self.read.files),
            group_digits(self.read.rows)
        )
    }

    /// The headline, a line per table, and the notes.
    pub fn lines(&self) -> Vec<String> {
        let mut lines = vec![self.headline()];
        let width = self.tables.iter().map(|t| t.name.len()).max().unwrap_or(0);
        for table in &self.tables {
            let mut line = format!(
                "  {:<width$}  {:>10}  {:>6} files  {:>14} rows",
                table.name,
                format_size(table.read.bytes),
                group_digits(table.read.files),
                group_digits(table.read.rows),
            );
            if let Some(whole) = table.whole.filter(|w| *w != table.read) {
                line.push_str(&format!("  of {}", format_size(whole.bytes)));
            }
            if let Some(filter) = &table.filter {
                line.push_str(&format!("  pruned by {filter}"));
            }
            if let Some(note) = &table.note {
                line.push_str(&format!("  ({note})"));
            }
            lines.push(line);
        }
        if !self.not_sized.is_empty() {
            lines.push(format!(
                "  Not sized: {} (name tables as database.table)",
                self.not_sized.join(", ")
            ));
        }
        lines.push(NO_PRICE_NOTE.to_string());
        lines
    }

    pub fn to_json(&self) -> Value {
        json!({
            "basis": self.basis.word(),
            "bytes": self.read.bytes,
            "files": self.read.files,
            "rows": self.read.rows,
            "tables": self.tables.iter().map(|t| json!({
                "table": t.name,
                "bytes": t.read.bytes,
                "files": t.read.files,
                "rows": t.read.rows,
                "tableBytes": t.whole.map(|w| w.bytes),
                "filter": t.filter,
                "note": t.note,
            })).collect::<Vec<_>>(),
            "notSized": self.not_sized,
            "cost": Value::Null,
            "costNote": NO_PRICE_NOTE,
        })
    }
}

fn planned_table(value: &Value) -> Result<TableScan> {
    Ok(TableScan {
        name: value["table"]
            .as_str()
            .context("a table of the scan estimate came without its name")?
            .to_string(),
        read: totals_of(value, "bytes", "files", "rows")?,
        whole: totals_of(value, "tableBytes", "tableFiles", "tableRows").ok(),
        filter: value["filter"].as_str().map(str::to_string),
        note: value["note"].as_str().map(str::to_string),
    })
}

fn totals_of(value: &Value, bytes: &str, files: &str, rows: &str) -> Result<Totals> {
    let number = |key: &str| {
        value[key]
            .as_u64()
            .with_context(|| format!("the scan estimate came without {key}"))
    };
    Ok(Totals {
        bytes: number(bytes)?,
        files: number(files)?,
        rows: number(rows)?,
    })
}

/// Estimates what `sql` (one statement or a script) reads in `warehouse`.
pub async fn estimate(http: &ApiClient, warehouse: &str, sql: &str) -> Result<ScanEstimate> {
    let statements = statements_of(sql);
    if statements.is_empty() {
        bail!("there is no SQL statement to estimate");
    }
    let mut planned = Vec::new();
    for statement in &statements {
        match planned_estimate(http, warehouse, statement).await {
            Ok(estimate) => planned.push(estimate),
            Err(err) if is_missing_route(&err) => {
                return upper_bound_estimate(http, warehouse, &statements).await;
            }
            Err(err) => return Err(err),
        }
    }
    Ok(ScanEstimate::combine(planned))
}

fn statements_of(sql: &str) -> Vec<String> {
    let parts = statements::split(sql);
    let mut list = parts.statements;
    if statements::has_code(&parts.rest) {
        list.push(parts.rest);
    }
    list
}

async fn planned_estimate(http: &ApiClient, warehouse: &str, sql: &str) -> Result<ScanEstimate> {
    let body = json!({"catalog": warehouse, "query": sql});
    let reply = http
        .request_json(SVC, Method::POST, ESTIMATE_PATH, &[], Some(body))
        .await?;
    ScanEstimate::from_planned(&reply["response"])
}

/// A lakehouse without the estimate route answers with its HTML 404 page;
/// the route's own 404s (an unknown warehouse or table) carry a message.
pub fn is_missing_route(err: &anyhow::Error) -> bool {
    let text = format!("{err:#}");
    text.contains("HTTP 404") && text.to_ascii_lowercase().contains("<html")
}

async fn upper_bound_estimate(
    http: &ApiClient,
    warehouse: &str,
    statements: &[String],
) -> Result<ScanEstimate> {
    let mut tables: Vec<TableName> = Vec::new();
    let mut not_sized: Vec<String> = Vec::new();
    for statement in statements {
        let reads = tables::tables_read(statement).lakehouse_reads(warehouse);
        for table in reads.tables {
            if !tables.contains(&table) {
                tables.push(table);
            }
        }
        for name in reads.unqualified {
            if !not_sized.contains(&name) {
                not_sized.push(name);
            }
        }
    }
    let mut sized = Vec::new();
    for table in tables {
        let versions = History::fetch(http, warehouse, &table).await?;
        sized.push((table, snapshot_totals(&versions)?));
    }
    Ok(ScanEstimate::upper_bound(sized, not_sized))
}

/// The current snapshot's totals; zero for a table with no snapshot yet.
pub fn snapshot_totals(versions: &Value) -> Result<Totals> {
    let history = History::parse(versions)?;
    let Some(current) = history.current.as_deref() else {
        return Ok(Totals::default());
    };
    let snapshot = history
        .snapshot(current)
        .context("the table's current snapshot is not in its history")?;
    let count = |key: &str| {
        snapshot
            .count(key)
            .with_context(|| format!("the table's current snapshot does not record {key}"))
    };
    Ok(Totals {
        bytes: count(TOTAL_SIZE)?,
        files: count(TOTAL_FILES)?,
        rows: count(TOTAL_ROWS)?,
    })
}

// ---------- sizes ----------

const UNITS: [(&str, u64); 5] = [
    ("B", 1),
    ("KB", 1 << 10),
    ("MB", 1 << 20),
    ("GB", 1 << 30),
    ("TB", 1 << 40),
];

/// A size such as `10GB`, `1.5 TB` or `500mb`, in bytes. Units are powers
/// of 1024, as gz shows sizes; `KiB`-style spellings mean the same.
pub fn parse_size(text: &str) -> Result<u64> {
    let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    let split = compact
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(compact.len());
    let (number, unit) = compact.split_at(split);
    let unit = unit.to_ascii_uppercase().replace("IB", "B");
    let unit = if unit.is_empty() { "B" } else { unit.as_str() };
    let Some((_, scale)) = UNITS.iter().find(|(name, _)| *name == unit) else {
        bail!("'{text}' is not a size; write it like 500MB, 10GB or 1.5TB");
    };
    let value: f64 = number
        .parse()
        .ok()
        .filter(|v: &f64| v.is_finite() && *v >= 0.0)
        .with_context(|| format!("'{text}' is not a size; write it like 500MB, 10GB or 1.5TB"))?;
    Ok((value * *scale as f64).round() as u64)
}

/// `9.0 GB`, `25 KB`, `700 B`.
pub fn format_size(bytes: u64) -> String {
    let Some((name, scale)) = UNITS.iter().rev().find(|(_, scale)| bytes >= *scale) else {
        return "0 B".to_string();
    };
    if *scale == 1 {
        return format!("{bytes} B");
    }
    let value = bytes as f64 / *scale as f64;
    if value >= 100.0 {
        format!("{value:.0} {name}")
    } else {
        format!("{value:.1} {name}")
    }
}

/// `bytes` in the largest unit that holds it exactly (`20GB`, `1536MB`),
/// so a saved size reads back the same.
pub fn exact_size(bytes: u64) -> String {
    let (name, scale) = UNITS
        .iter()
        .rev()
        .find(|(_, scale)| bytes >= *scale && bytes % scale == 0)
        .copied()
        .unwrap_or(("B", 1));
    format!("{}{name}", bytes / scale)
}

fn group_digits(n: u64) -> String {
    let text = n.to_string();
    let mut out = String::new();
    for (i, ch) in text.chars().enumerate() {
        if i > 0 && (text.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn versions(current: &str, summary: Value) -> Value {
        json!({
            "currentSnapshotId": current,
            "snapshots": [
                {"snapshotId": current, "summary": summary, "committedAt": "2026-11-01T00:00:00Z"},
                {"snapshotId": "1", "summary": {"total-files-size": "1"}, "committedAt": "2026-10-01T00:00:00Z"},
            ],
            "branches": [], "tags": [],
        })
    }

    #[test]
    fn the_upper_bound_adds_each_tables_current_snapshot() {
        let big = versions(
            "65",
            json!({"total-files-size": "9530959833", "total-data-files": "350", "total-records": "400000000"}),
        );
        let small = versions(
            "70",
            json!({"total-files-size": "25116", "total-data-files": "1", "total-records": "700"}),
        );
        let table = |t: &str| TableName {
            database: "DemoDB".into(),
            table: t.into(),
        };
        let estimate = ScanEstimate::upper_bound(
            vec![
                (table("inventory"), snapshot_totals(&big).unwrap()),
                (table("sales"), snapshot_totals(&small).unwrap()),
            ],
            vec!["orders".into()],
        );
        assert_eq!(
            estimate.read,
            Totals {
                bytes: 9_530_984_949,
                files: 351,
                rows: 400_000_700
            }
        );
        assert_eq!(estimate.amount(), "up to 8.9 GB");
        let lines = estimate.lines().join("\n");
        assert!(lines.contains("Not sized: orders"), "{lines}");
        assert!(lines.contains("No cost shown"), "{lines}");
    }

    #[test]
    fn a_table_without_a_snapshot_reads_nothing_and_missing_totals_are_an_error() {
        let empty = json!({"currentSnapshotId": null, "snapshots": [], "branches": [], "tags": []});
        assert_eq!(snapshot_totals(&empty).unwrap(), Totals::default());
        let partial = versions("5", json!({"total-records": "3"}));
        let err = snapshot_totals(&partial).unwrap_err().to_string();
        assert!(err.contains("total-files-size"), "{err}");
    }

    #[test]
    fn a_planned_estimate_keeps_the_filter_and_the_whole_table() {
        let response = json!({
            "bytes": 1024, "files": 1, "rows": 4,
            "tables": [{"table": "sales.orders", "bytes": 1024, "files": 1, "rows": 4,
                        "tableBytes": 3072, "tableFiles": 3, "tableRows": 12,
                        "basis": "planned", "filter": "region = 'eu'", "note": null}],
        });
        let estimate = ScanEstimate::from_planned(&response).unwrap();
        assert_eq!(estimate.amount(), "about 1.0 KB");
        let line = &estimate.lines()[1];
        assert!(
            line.contains("of 3.0 KB") && line.contains("pruned by region = 'eu'"),
            "{line}"
        );
    }

    #[test]
    fn a_script_estimate_is_an_upper_bound_when_any_statement_is() {
        let planned =
            ScanEstimate::from_planned(&json!({"bytes": 10, "files": 1, "rows": 1, "tables": []}))
                .unwrap();
        let bound = ScanEstimate::upper_bound(Vec::new(), vec!["x".into()]);
        let combined = ScanEstimate::combine(vec![planned, bound]);
        assert_eq!(
            (combined.basis, combined.read.bytes),
            (Basis::UpperBound, 10)
        );
    }

    #[test]
    fn sizes_parse_in_powers_of_1024_and_print_back() {
        assert_eq!(parse_size("10GB").unwrap(), 10 << 30);
        assert_eq!(parse_size("1.5 tb").unwrap(), 3 << 39);
        assert_eq!(parse_size("500MiB").unwrap(), 500 << 20);
        assert_eq!(parse_size("2048").unwrap(), 2048);
        assert!(parse_size("ten GB").is_err());
        assert!(parse_size("10PB").is_err());
        assert_eq!(format_size(10 << 30), "10.0 GB");
        assert_eq!(format_size(148_143 << 20), "145 GB");
        assert_eq!(format_size(700), "700 B");
        assert_eq!(exact_size(20 << 30), "20GB");
        assert_eq!(exact_size(3 << 29), "1536MB");
    }

    #[test]
    fn only_an_html_404_means_the_route_is_missing() {
        let page = anyhow::anyhow!("POST u → HTTP 404 Not Found: <!doctype html>\n<html lang=en>");
        let table = anyhow::anyhow!("POST u → HTTP 404 Not Found: Table sales.x not found");
        assert!(is_missing_route(&page));
        assert!(!is_missing_route(&table));
    }
}
