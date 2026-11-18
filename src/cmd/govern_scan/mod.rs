//! `gz govern scan`: which columns of a lakehouse table look like personal
//! data, from a bounded sample read through a lakehouse SQL session.
//!
//! The sample is one statement: the first `--sample` rows, every scanned
//! column gathered into a list (`collect_list`), so a few thousand rows come
//! back as one result row instead of many pages. Raw values never leave this
//! module unmasked. `--apply` tags each flagged column `pii=<kind>` through
//! the provider's column tag route, the one the web app's tag editor uses.

mod apply;
mod detect;
mod values;

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::Args;
use reqwest::Method;
use serde_json::{json, Value};

use crate::cli::{Confirm, ShellArgs};
use crate::cmd::lakehouse_table::LakehouseTable;
use crate::cmd::{seg, Runtime};
use crate::output::{self, OutputFormat};
use crate::sql::results::Page;
use crate::sql::scan_limit::{self, ScanLimit};
use crate::sql::Runner;
use crate::table_history::ident;
use detect::{classify, ColumnType, Finding};

const LAKEHOUSE: &str = "lakehouse";
const DEFAULT_SAMPLE: u32 = 2000;
/// The sample comes back as one row; past this it gets too large to send.
const MAX_SAMPLE: u32 = 50_000;
const ROWS_ALIAS: &str = "gz_rows";
const SECONDS_PER_DAY: u64 = 86_400;
const REPORT_COLUMNS: [&str; 5] = ["column", "kind", "matched", "confidence", "example"];

#[derive(Args, Debug)]
#[command(
    about = "Flag columns that look like personal data (read-only unless --apply)",
    long_about = "Reads the first --sample rows of a table through a lakehouse SQL session \
(under your scan limit) and flags columns that look like personal data: email, phone \
(Indian, North American, UK and international formats), payment card (Luhn), IP address, \
Indian PAN and Aadhaar (Verhoeff), US SSN, IBAN, and, when the column's name agrees, dates of \
birth, names and addresses. Each flagged column shows its kind, how many sampled values \
matched, a confidence and a masked example; raw values are never printed.\n\n\
--apply tags each medium or high confidence column `pii=<kind>` (a sensitive tag, so AI \
models get it masked), after you confirm. It leaves a column's existing `pii` tag as it is. \
Tagging columns needs the admin tier."
)]
pub struct ScanArgs {
    #[arg(help = "WAREHOUSE.DATABASE.TABLE")]
    pub table: String,
    #[arg(
        long,
        default_value_t = DEFAULT_SAMPLE,
        value_parser = clap::value_parser!(u32).range(1..=i64::from(MAX_SAMPLE)),
        help = "Rows to read (the table's first N)"
    )]
    pub sample: u32,
    #[arg(
        long,
        value_name = "SIZE",
        help = "Refuse the sample if it would scan more (e.g. 2GB, or off); default: the profile's sql_max_scan"
    )]
    pub max_scan: Option<String>,
    #[arg(long, help = "Compute size name or id, e.g. small (remembered)")]
    pub compute: Option<String>,
    #[arg(long, help = "Leave the session running; the next gz sql reuses it")]
    pub keep_session: bool,
    #[arg(long, help = "Tag flagged columns pii=<kind> (asks first)")]
    pub apply: bool,
    #[command(flatten)]
    pub confirm: Confirm,
}

/// A column the scan reads.
#[derive(Debug, Clone, PartialEq)]
struct ScanColumn {
    id: u64,
    name: String,
    data_type: String,
    column_type: ColumnType,
}

/// A scanned column judged to hold personal data.
#[derive(Debug, Clone)]
struct Flagged {
    column: ScanColumn,
    finding: Finding,
}

pub async fn run(rt: &Runtime, args: &ScanArgs) -> Result<()> {
    let table = LakehouseTable::parse(&args.table)?;
    let schema = fetch_schema(rt, &table).await?;
    let columns = scan_columns(&schema)?;
    if columns.is_empty() {
        output::info(&format!(
            "{table} has no text, whole-number or date columns, so nothing can hold personal data"
        ));
        return Ok(());
    }
    let statement = sample_sql(&table, &columns, args.sample);
    check_scan(rt, &table, &statement, args.max_scan.as_deref()).await?;
    let page = read_sample(rt, args, &table, &statement).await?;
    let sampled_rows = page
        .rows
        .first()
        .and_then(|row| row[ROWS_ALIAS].as_u64())
        .unwrap_or(0);
    let flagged = flag_columns(&columns, &page, current_year())?;

    let applied = if args.apply {
        Some(apply::run(rt, &table, &flagged, args.confirm.yes).await?)
    } else {
        None
    };
    let report = Report {
        table: &table,
        sampled_rows,
        columns_checked: columns.len(),
        flagged: &flagged,
    };
    show(rt, &report, applied.as_deref())
}

async fn fetch_schema(rt: &Runtime, table: &LakehouseTable) -> Result<Value> {
    let url = format!(
        "/lakehouse/schema/{}/{}/{}",
        seg(&table.warehouse),
        seg(&table.name.database),
        seg(&table.name.table)
    );
    let reply = rt
        .http
        .request_json(LAKEHOUSE, Method::GET, &url, &[], None)
        .await
        .with_context(|| format!("cannot read the columns of {table}"))?;
    Ok(reply["response"].clone())
}

/// The schema's columns whose type can hold personal data.
fn scan_columns(schema: &Value) -> Result<Vec<ScanColumn>> {
    let entries = schema
        .as_array()
        .context("the provider returned no column list for the table")?;
    Ok(entries
        .iter()
        .filter_map(|entry| {
            let data_type = entry["dataType"].as_str()?;
            Some(ScanColumn {
                id: entry["columnId"].as_u64()?,
                name: entry["columnName"].as_str()?.to_string(),
                data_type: data_type.to_string(),
                column_type: ColumnType::of(data_type)?,
            })
        })
        .collect())
}

/// One row: the sample's row count, then each column's non-null values as
/// text (`collect_list` skips nulls), under positional aliases so no column
/// name can clash.
fn sample_sql(table: &LakehouseTable, columns: &[ScanColumn], rows: u32) -> String {
    let lists: Vec<String> = columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            format!(
                "collect_list(CAST({} AS STRING)) AS {}",
                ident(&column.name),
                alias(index)
            )
        })
        .collect();
    format!(
        "SELECT count(*) AS {ROWS_ALIAS}, {} FROM (SELECT * FROM {}.{} LIMIT {rows}) AS gz_sample",
        lists.join(", "),
        ident(&table.name.database),
        ident(&table.name.table)
    )
}

fn alias(index: usize) -> String {
    format!("c{index}")
}

/// Refuses the sample when its estimated scan is over the limit in force,
/// as every `gz sql` script is.
async fn check_scan(
    rt: &Runtime,
    table: &LakehouseTable,
    statement: &str,
    max_scan: Option<&str>,
) -> Result<()> {
    let saved = rt.http.resolved().profile.sql_max_scan.clone();
    let limit = ScanLimit::resolve(max_scan, saved.as_deref())?;
    let checked = scan_limit::enforce(&rt.http, &table.warehouse, statement, limit).await?;
    if let (Some(estimate), false) = (checked, rt.quiet) {
        output::info(&format!("scan check: {}", estimate.headline()));
    }
    Ok(())
}

async fn read_sample(
    rt: &Runtime,
    args: &ScanArgs,
    table: &LakehouseTable,
    statement: &str,
) -> Result<Page> {
    let shell = ShellArgs {
        warehouse: Some(table.warehouse.clone()),
        compute: args.compute.clone(),
        keep_session: args.keep_session,
    };
    let session = Runner::open(rt, &shell).await?;
    if !rt.quiet {
        output::info(&format!("Sampling up to {} rows of {table}…", args.sample));
    }
    let page = session.rows(statement).await;
    session.close(rt, args.keep_session).await;
    page
}

fn flag_columns(columns: &[ScanColumn], page: &Page, current_year: i64) -> Result<Vec<Flagged>> {
    let row = page
        .rows
        .first()
        .context("the sample query returned no row")?;
    let mut flagged: Vec<Flagged> = columns
        .iter()
        .enumerate()
        .filter_map(|(index, column)| {
            let values = sampled_values(&row[alias(index)]);
            classify(&column.name, column.column_type, &values, current_year).map(|finding| {
                Flagged {
                    column: column.clone(),
                    finding,
                }
            })
        })
        .collect();
    flagged.sort_by(|a, b| b.finding.confidence.cmp(&a.finding.confidence));
    Ok(flagged)
}

/// A column's sampled values as text, blanks left out.
fn sampled_values(list: &Value) -> Vec<String> {
    list.as_array()
        .map(|items| {
            items
                .iter()
                .map(|item| match item {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                })
                .filter(|text| !text.trim().is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn current_year() -> i64 {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default();
    let days = i64::try_from(seconds / SECONDS_PER_DAY).unwrap_or_default();
    crate::cmd::civil::civil_from_days(days).0
}

struct Report<'a> {
    table: &'a LakehouseTable,
    sampled_rows: u64,
    columns_checked: usize,
    flagged: &'a [Flagged],
}

fn finding_json(flagged: &Flagged) -> Value {
    let finding = &flagged.finding;
    json!({
        "column": flagged.column.name,
        "columnId": flagged.column.id,
        "dataType": flagged.column.data_type,
        "kind": finding.kind,
        "matched": finding.matched,
        "values": finding.values,
        "matchRate": (finding.match_rate() * 1000.0).round() / 1000.0,
        "confidence": finding.confidence,
        "example": finding.example,
    })
}

fn report_json(report: &Report, applied: Option<&[Value]>) -> Value {
    let mut value = json!({
        "table": report.table.to_string(),
        "sampledRows": report.sampled_rows,
        "columnsChecked": report.columns_checked,
        "findings": report.flagged.iter().map(finding_json).collect::<Vec<_>>(),
    });
    if let Some(applied) = applied {
        value["applied"] = Value::Array(applied.to_vec());
    }
    value
}

/// One table row per flagged column; the match as a share and a count.
fn report_rows(report: &Report) -> Value {
    let rows = report.flagged.iter().map(|flagged| {
        let finding = &flagged.finding;
        json!({
            "column": flagged.column.name,
            "kind": finding.kind.tag(),
            // Rounded down, so 100% means every value matched.
            "matched": format!(
                "{}% ({} of {})",
                finding.matched * 100 / finding.values,
                finding.matched,
                finding.values
            ),
            "confidence": finding.confidence,
            "example": finding.example,
        })
    });
    Value::Array(rows.collect())
}

fn show(rt: &Runtime, report: &Report, applied: Option<&[Value]>) -> Result<()> {
    if rt.format != OutputFormat::Table {
        return output::print_value(&report_json(report, applied), rt.format, None);
    }
    if report.sampled_rows == 0 {
        output::info(&format!("{} has no rows to sample", report.table));
        return Ok(());
    }
    println!(
        "Sampled {} rows of {}; {} of {} columns look like personal data.",
        report.sampled_rows,
        report.table,
        report.flagged.len(),
        report.columns_checked
    );
    if !report.flagged.is_empty() {
        rt.show_columns(&report_rows(report), &REPORT_COLUMNS)?;
    }
    if let Some(applied) = applied {
        apply::show(applied);
    } else if !report.flagged.is_empty() {
        output::info(&format!(
            "Tag them with `gz govern scan {} --apply`",
            report.table
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> LakehouseTable {
        LakehouseTable::parse("wh.sales.customers").unwrap()
    }

    #[test]
    fn scans_only_columns_that_can_hold_personal_data() {
        let schema = json!([
            {"columnId": 1, "columnName": "email", "dataType": "string"},
            {"columnId": 2, "columnName": "score", "dataType": "double"},
            {"columnId": 3, "columnName": "dob", "dataType": "date"},
            {"columnId": 4, "columnName": "tags", "dataType": "array<string>"},
        ]);
        let names: Vec<String> = scan_columns(&schema)
            .unwrap()
            .into_iter()
            .map(|column| column.name)
            .collect();
        assert_eq!(names, ["email", "dob"]);
    }

    #[test]
    fn the_sample_is_one_bounded_statement_with_quoted_names() {
        let columns = scan_columns(&json!([
            {"columnId": 1, "columnName": "e`mail", "dataType": "string"},
            {"columnId": 2, "columnName": "phone", "dataType": "bigint"},
        ]))
        .unwrap();
        assert_eq!(
            sample_sql(&table(), &columns, 500),
            "SELECT count(*) AS gz_rows, collect_list(CAST(`e``mail` AS STRING)) AS c0, \
             collect_list(CAST(`phone` AS STRING)) AS c1 \
             FROM (SELECT * FROM `sales`.`customers` LIMIT 500) AS gz_sample"
        );
    }

    #[test]
    fn findings_never_carry_raw_values() {
        let columns = scan_columns(&json!([
            {"columnId": 7, "columnName": "contact", "dataType": "string"},
        ]))
        .unwrap();
        let page = Page {
            columns: vec!["gz_rows".into(), "c0".into()],
            column_types: vec![],
            rows: vec![json!({"gz_rows": 2, "c0": ["asha@example.in", "ravi@example.in"]})],
            page_number: 1,
            has_more: false,
            total_records: Some(1),
        };
        let flagged = flag_columns(&columns, &page, 2026).unwrap();
        let report = Report {
            table: &table(),
            sampled_rows: 2,
            columns_checked: 1,
            flagged: &flagged,
        };
        let text = report_json(&report, None).to_string();
        assert!(!text.contains("asha@example.in"));
        assert!(!text.contains("ravi"));
        assert_eq!(
            report_json(&report, None)["findings"][0]["example"],
            "a***@e***.in"
        );
    }

    #[test]
    fn a_share_short_of_every_value_never_shows_as_100_percent() {
        let finding = Finding {
            kind: detect::Kind::Name,
            matched: 1995,
            values: 2000,
            confidence: detect::Confidence::Medium,
            example: "A***".into(),
        };
        let column = scan_columns(&json!([
            {"columnId": 1, "columnName": "name", "dataType": "string"},
        ]))
        .unwrap()
        .remove(0);
        let flagged = [Flagged { column, finding }];
        let report = Report {
            table: &table(),
            sampled_rows: 2000,
            columns_checked: 1,
            flagged: &flagged,
        };
        assert_eq!(report_rows(&report)[0]["matched"], "99% (1995 of 2000)");
    }
}
