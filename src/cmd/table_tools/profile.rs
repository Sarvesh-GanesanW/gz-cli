//! `gz table profile`: per-column statistics of a lakehouse table, from one
//! aggregate query per batch of columns on a SQL session.
//!
//! Every statement reads the table `VERSION AS OF` one snapshot, so the
//! batches and the top values describe the same data even while jobs write
//! the table.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use super::Located;
use crate::cli::TableProfileArgs;
use crate::cmd::Runtime;
use crate::output::{self, OutputFormat};
use crate::sql::estimate::{format_size, ScanEstimate, Totals};
use crate::sql::results::Page;
use crate::sql::scan_limit::{override_size, ScanLimit, OFF};
use crate::sql::Runner;
use crate::table_history::{ident, Snapshot, Statements, TableName};
use crate::tui::format::group_digits;

/// Columns profiled per aggregate query; each takes up to five aggregates.
const COLUMNS_PER_PASS: usize = 40;
/// Decimals shown for a mean.
const MEAN_DECIMALS: usize = 4;
const PROFILE_COLUMNS: [&str; 9] = [
    "column", "type", "nulls", "distinct", "min", "max", "mean", "length", "top",
];

/// What the profile can say about a column, from its Spark type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Number,
    Date,
    Timestamp,
    Text,
    Boolean,
    Binary,
    /// Arrays, maps, structs and anything else: nulls only.
    Nested,
}

impl Kind {
    fn of(spark_type: &str) -> Self {
        let name = spark_type.trim().to_ascii_lowercase();
        const NUMBERS: [&str; 11] = [
            "byte", "tinyint", "short", "smallint", "integer", "int", "long", "bigint", "float",
            "real", "double",
        ];
        if NUMBERS.contains(&name.as_str()) || name.starts_with("decimal") {
            return Kind::Number;
        }
        if name.starts_with("timestamp") {
            return Kind::Timestamp;
        }
        if name.starts_with("varchar") || name.starts_with("char") {
            return Kind::Text;
        }
        match name.as_str() {
            "date" => Kind::Date,
            "string" => Kind::Text,
            "boolean" => Kind::Boolean,
            "binary" => Kind::Binary,
            _ => Kind::Nested,
        }
    }

    fn has_distinct(self) -> bool {
        self != Kind::Nested
    }

    fn has_range(self) -> bool {
        matches!(self, Kind::Number | Kind::Date | Kind::Timestamp)
    }

    fn has_length(self) -> bool {
        matches!(self, Kind::Text | Kind::Binary)
    }

    fn has_top(self) -> bool {
        !matches!(self, Kind::Binary | Kind::Nested)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Column {
    name: String,
    spark_type: String,
    kind: Kind,
}

/// The columns to profile: all, or the ones asked for, in table order.
fn chosen(page: &Page, wanted: &[String]) -> Result<Vec<Column>> {
    let all: Vec<Column> = page
        .columns
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let spark_type = page.column_types.get(i).cloned().unwrap_or_default();
            Column {
                name: name.clone(),
                kind: Kind::of(&spark_type),
                spark_type,
            }
        })
        .collect();
    let unknown: Vec<&String> = wanted
        .iter()
        .filter(|w| !all.iter().any(|c| c.name.eq_ignore_ascii_case(w)))
        .collect();
    if !unknown.is_empty() {
        let names: Vec<&str> = all.iter().map(|c| c.name.as_str()).collect();
        bail!(
            "the table has no column {}; its columns are {}",
            unknown
                .iter()
                .map(|n| n.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            names.join(", ")
        );
    }
    Ok(all
        .into_iter()
        .filter(|c| wanted.is_empty() || wanted.iter().any(|w| c.name.eq_ignore_ascii_case(w)))
        .collect())
}

// ---------- SQL ----------

fn columns_sql(from: &str) -> String {
    format!("SELECT * FROM {from} LIMIT 0")
}

/// The aggregates for one column, aliased `c{i}_…` so no column name ends
/// up as an alias.
fn aggregates(i: usize, column: &Column) -> Vec<String> {
    let c = ident(&column.name);
    let mut list = vec![format!("count({c}) AS c{i}_values")];
    if column.kind.has_distinct() {
        list.push(format!("approx_count_distinct({c}) AS c{i}_distinct"));
    }
    if column.kind.has_range() {
        list.push(format!("min({c}) AS c{i}_min"));
        list.push(format!("max({c}) AS c{i}_max"));
    }
    match column.kind {
        Kind::Number => list.push(format!("avg({c}) AS c{i}_mean")),
        Kind::Date => list.push(format!(
            "date_from_unix_date(CAST(round(avg(unix_date({c}))) AS INT)) AS c{i}_mean"
        )),
        Kind::Timestamp => list.push(format!(
            "timestamp_micros(CAST(round(avg(unix_micros(CAST({c} AS TIMESTAMP)))) AS BIGINT)) AS c{i}_mean"
        )),
        _ => {}
    }
    if column.kind.has_length() {
        list.push(format!("min(length({c})) AS c{i}_minlen"));
        list.push(format!("max(length({c})) AS c{i}_maxlen"));
    }
    list
}

/// One aggregate pass over `columns` (numbered from `first`).
fn stats_sql(from: &str, first: usize, columns: &[Column]) -> String {
    let selected: Vec<String> = std::iter::once("count(*) AS total_rows".to_string())
        .chain(
            columns
                .iter()
                .enumerate()
                .flat_map(|(i, column)| aggregates(first + i, column)),
        )
        .collect();
    format!("SELECT {} FROM {from}", selected.join(", "))
}

/// The `top` most common values of each column in one pass: `stack` turns
/// every row into one (column number, value) pair per column, then the
/// pairs are counted and ranked per column.
fn top_sql(from: &str, numbered: &[(usize, &Column)], top: u32) -> String {
    let pairs: Vec<String> = numbered
        .iter()
        .map(|(i, column)| format!("{i}, CAST({} AS STRING)", ident(&column.name)))
        .collect();
    format!(
        "SELECT col, value, n FROM (\
         SELECT col, value, n, row_number() OVER (PARTITION BY col ORDER BY n DESC, value) AS rk FROM (\
         SELECT col, value, count(*) AS n FROM (\
         SELECT stack({}, {}) AS (col, value) FROM {from}) GROUP BY col, value)) \
         WHERE rk <= {top}",
        numbered.len(),
        pairs.join(", ")
    )
}

/// Columns per top-values pass, so one pass's rows fit on the first page
/// of the result.
fn columns_per_top_pass(top: u32) -> usize {
    (crate::sql::session::PAGE_SIZE as usize / top.max(1) as usize).max(1)
}

/// Statements the profile runs, for the scan estimate.
fn passes(columns: &[Column], top: Option<u32>) -> usize {
    let stats = columns.len().div_ceil(COLUMNS_PER_PASS);
    let top_passes = top.map_or(0, |top| {
        let with_top = columns.iter().filter(|c| c.kind.has_top()).count();
        with_top.div_ceil(columns_per_top_pass(top))
    });
    stats + top_passes
}

// ---------- results ----------

#[derive(Debug, Clone, PartialEq)]
struct ColumnProfile {
    name: String,
    spark_type: String,
    kind: Kind,
    nulls: u64,
    /// Approximate.
    distinct: Option<u64>,
    min: Option<Value>,
    max: Option<Value>,
    mean: Option<Value>,
    min_length: Option<u64>,
    max_length: Option<u64>,
    /// Most common first.
    top: Vec<(Value, u64)>,
}

#[derive(Debug, Clone, PartialEq)]
struct Profile {
    rows: u64,
    columns: Vec<ColumnProfile>,
}

fn count(value: &Value) -> Option<u64> {
    match value {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn present(value: &Value) -> Option<Value> {
    (!value.is_null()).then(|| value.clone())
}

fn first_row(page: &Page) -> Result<&Value> {
    page.rows
        .first()
        .context("the aggregate query returned no row")
}

fn read_stats(row: &Value, first: usize, columns: &[Column]) -> Result<(u64, Vec<ColumnProfile>)> {
    let rows = count(&row["total_rows"]).context("the aggregate query returned no row count")?;
    let profiles = columns
        .iter()
        .enumerate()
        .map(|(offset, column)| {
            let i = first + offset;
            let field = |name: &str| &row[format!("c{i}_{name}").as_str()];
            let values = count(field("values"))
                .with_context(|| format!("no count came back for {}", column.name))?;
            Ok(ColumnProfile {
                name: column.name.clone(),
                spark_type: column.spark_type.clone(),
                kind: column.kind,
                nulls: rows.saturating_sub(values),
                distinct: count(field("distinct")),
                min: present(field("min")),
                max: present(field("max")),
                mean: present(field("mean")),
                min_length: count(field("minlen")),
                max_length: count(field("maxlen")),
                top: Vec::new(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((rows, profiles))
}

/// Puts each (column number, value, count) row under its column.
fn read_top(page: &Page, profiles: &mut [ColumnProfile]) -> Result<()> {
    for row in &page.rows {
        let column = count(&row["col"]).context("a top value came without its column")? as usize;
        let n = count(&row["n"]).context("a top value came without its count")?;
        let profile = profiles
            .get_mut(column)
            .context("a top value named a column that was not asked for")?;
        profile.top.push((row["value"].clone(), n));
    }
    for profile in profiles {
        profile.top.sort_by(|a, b| b.1.cmp(&a.1));
    }
    Ok(())
}

async fn collect(
    session: &impl Statements,
    from: &str,
    columns: &[Column],
    top: Option<u32>,
) -> Result<Profile> {
    let mut rows = 0;
    let mut profiles = Vec::with_capacity(columns.len());
    for (batch, chunk) in columns.chunks(COLUMNS_PER_PASS).enumerate() {
        let first = batch * COLUMNS_PER_PASS;
        let page = session.rows(&stats_sql(from, first, chunk)).await?;
        let (counted, mut read) = read_stats(first_row(&page)?, first, chunk)?;
        rows = counted;
        profiles.append(&mut read);
    }
    if let Some(top) = top {
        let numbered: Vec<(usize, &Column)> = columns
            .iter()
            .enumerate()
            .filter(|(_, c)| c.kind.has_top())
            .collect();
        for chunk in numbered.chunks(columns_per_top_pass(top)) {
            let page = session.rows(&top_sql(from, chunk, top)).await?;
            read_top(&page, &mut profiles)?;
        }
    }
    Ok(Profile {
        rows,
        columns: profiles,
    })
}

// ---------- output ----------

fn grouped(number: u64) -> String {
    group_digits(&number.to_string())
}

fn percent(part: u64, whole: u64) -> String {
    if whole == 0 || part == 0 {
        return "0%".to_string();
    }
    let share = part as f64 * 100.0 / whole as f64;
    if share < 0.1 {
        return "<0.1%".to_string();
    }
    format!("{share:.1}%")
}

/// The session sends a date as a midnight timestamp; a date column shows
/// the date alone.
const MIDNIGHT: &str = " 00:00:00";

/// A value as it reads in a table: numbers grouped, means rounded, dates
/// without a time.
fn shown_for(kind: Kind, value: &Value) -> String {
    let text = shown(value);
    match (kind, text.strip_suffix(MIDNIGHT)) {
        (Kind::Date, Some(date)) => date.to_string(),
        _ => text,
    }
}

fn shown(value: &Value) -> String {
    match value {
        Value::Number(n) if n.is_f64() => {
            let text = format!("{:.MEAN_DECIMALS$}", n.as_f64().unwrap_or_default());
            text.trim_end_matches('0').trim_end_matches('.').to_string()
        }
        Value::Number(n) => group_digits(&n.to_string()),
        other => output::scalar_text(other),
    }
}

fn top_text(kind: Kind, top: &[(Value, u64)], rows: u64) -> String {
    top.iter()
        .map(|(value, n)| {
            let value = if value.is_null() {
                "NULL".to_string()
            } else {
                shown_for(kind, &Value::String(output::scalar_text(value)))
            };
            format!("{value} ({})", percent(*n, rows))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn table_rows(profile: &Profile) -> Value {
    let rows: Vec<Value> = profile
        .columns
        .iter()
        .map(|c| {
            let length = match (c.min_length, c.max_length) {
                (Some(lo), Some(hi)) => json!(format!("{}–{}", grouped(lo), grouped(hi))),
                _ => Value::Null,
            };
            json!({
                "column": c.name,
                "type": c.spark_type,
                "nulls": format!("{} ({})", grouped(c.nulls), percent(c.nulls, profile.rows)),
                "distinct": c.distinct.map(|d| format!("~{}", grouped(d))),
                "min": c.min.as_ref().map(|v| shown_for(c.kind, v)),
                "max": c.max.as_ref().map(|v| shown_for(c.kind, v)),
                "mean": c.mean.as_ref().map(|v| shown_for(c.kind, v)),
                "length": length,
                "top": (!c.top.is_empty()).then(|| top_text(c.kind, &c.top, profile.rows)),
            })
        })
        .collect();
    Value::Array(rows)
}

struct Subject<'a> {
    table: &'a TableName,
    warehouse: &'a str,
    snapshot: &'a Snapshot,
}

fn profile_json(subject: &Subject, profile: &Profile) -> Value {
    json!({
        "table": subject.table.to_string(),
        "warehouse": subject.warehouse,
        "snapshotId": subject.snapshot.id,
        "committedAt": subject.snapshot.committed_at,
        "rows": profile.rows,
        "distinctIsApproximate": true,
        "columns": profile.columns.iter().map(|c| json!({
            "name": c.name,
            "type": c.spark_type,
            "nulls": c.nulls,
            "nullPercent": if profile.rows == 0 { 0.0 } else { c.nulls as f64 * 100.0 / profile.rows as f64 },
            "distinct": c.distinct,
            "min": c.min,
            "max": c.max,
            "mean": c.mean,
            "minLength": c.min_length,
            "maxLength": c.max_length,
            "top": c.top.iter().map(|(value, n)| json!({"value": value, "count": n})).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

fn heading(subject: &Subject, profile: &Profile) -> String {
    format!(
        "{} · {} · snapshot {} ({})\n{} rows · distinct counts are approximate (within a few percent)\n",
        subject.table,
        subject.warehouse,
        subject.snapshot.id,
        subject.snapshot.committed(),
        grouped(profile.rows),
    )
}

// ---------- the command ----------

/// What the profile reads: the snapshot's files, once per pass.
fn scan_estimate(table: &TableName, snapshot: &Snapshot, passes: usize) -> Result<ScanEstimate> {
    let total = |key: &str| {
        snapshot
            .count(key)
            .with_context(|| format!("snapshot {} does not record {key}, so its size is unknown; run with --max-scan off to skip the check", snapshot.id))
    };
    let times = passes as u64;
    let read = Totals {
        bytes: total("total-files-size")? * times,
        files: total("total-data-files")? * times,
        rows: total("total-records")? * times,
    };
    Ok(ScanEstimate::upper_bound(
        vec![(table.clone(), read)],
        Vec::new(),
    ))
}

fn check_scan(
    rt: &Runtime,
    args: &TableProfileArgs,
    estimate: Option<ScanEstimate>,
    passes: usize,
) -> Result<()> {
    let saved = rt.http.resolved().profile.sql_max_scan.clone();
    let limit = ScanLimit::resolve(args.scan.max_scan.as_deref(), saved.as_deref())?;
    let estimate = match (estimate, limit) {
        (Some(estimate), _) => estimate,
        (None, ScanLimit::Off) => return Ok(()),
        (None, ScanLimit::Bytes { .. }) => bail!(
            "the snapshot does not record its size, so the scan limit cannot be checked; run with --max-scan off to profile it anyway"
        ),
    };
    if !rt.quiet {
        let passes = if passes > 1 {
            format!(" · {passes} passes over the table")
        } else {
            String::new()
        };
        output::info(&format!("scan check: {}{passes}", estimate.headline()));
    }
    match refusal(limit, &estimate) {
        Some(reason) => bail!("{reason}"),
        None => Ok(()),
    }
}

/// Why the profile may not read this much, with what to do instead.
fn refusal(limit: ScanLimit, estimate: &ScanEstimate) -> Option<String> {
    let ScanLimit::Bytes { bytes, from_flag } = limit else {
        return None;
    };
    limit.refusal(estimate)?;
    let source = if from_flag {
        "--max-scan"
    } else {
        "sql_max_scan in this profile"
    };
    Some(format!(
        "This profile reads {}, over the {} limit ({source}). Nothing ran. Profile fewer \
         columns with --columns (the estimate counts every column, so it may already fit), \
         or run it anyway with --max-scan {} or --max-scan {OFF}.",
        estimate.amount(),
        format_size(bytes),
        override_size(estimate.read.bytes),
    ))
}

pub async fn run(rt: &Runtime, args: &TableProfileArgs) -> Result<()> {
    let located = Located::find(rt, &args.table, &args.shell).await?;
    let snapshot = located
        .history
        .resolve(args.at.as_deref().unwrap_or("current"))
        .context("--at")?
        .clone();
    let from = located.table.at(&snapshot);
    // The first pass is checked before a session starts; more passes, for a
    // wide table or top values, once the columns are known.
    check_scan(
        rt,
        args,
        scan_estimate(&located.table, &snapshot, 1).ok(),
        1,
    )?;

    let session = Runner::open(rt, &located.shell).await?;
    let profiled = profile_on(rt, &session, args, &located.table, &snapshot, &from).await;
    session.close(rt, args.shell.keep_session).await;
    let profile = profiled?;

    let subject = Subject {
        table: &located.table,
        warehouse: &located.warehouse,
        snapshot: &snapshot,
    };
    if rt.format != OutputFormat::Table {
        return rt.show(profile_json(&subject, &profile)).await;
    }
    print!("{}", heading(&subject, &profile));
    rt.show_columns(&table_rows(&profile), &PROFILE_COLUMNS)
}

/// Reads the columns, checks the scan, then profiles.
async fn profile_on(
    rt: &Runtime,
    session: &Runner,
    args: &TableProfileArgs,
    table: &TableName,
    snapshot: &Snapshot,
    from: &str,
) -> Result<Profile> {
    let columns = chosen(&session.rows(&columns_sql(from)).await?, &args.columns)?;
    let passes = passes(&columns, args.top);
    if passes > 1 {
        check_scan(
            rt,
            args,
            scan_estimate(table, snapshot, passes).ok(),
            passes,
        )?;
    }
    if !rt.quiet {
        output::info(&format!("Profiling {} column(s)…", columns.len()));
    }
    collect(session, from, &columns, args.top).await
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::table_history::test_support::{history, page, Scripted};

    fn column(name: &str, spark_type: &str) -> Column {
        Column {
            name: name.into(),
            spark_type: spark_type.into(),
            kind: Kind::of(spark_type),
        }
    }

    #[test]
    fn a_columns_type_decides_what_is_measured() {
        assert_eq!(Kind::of("decimal(10,2)"), Kind::Number);
        assert_eq!(Kind::of("integer"), Kind::Number);
        assert_eq!(Kind::of("timestamp_ntz"), Kind::Timestamp);
        assert_eq!(Kind::of("array<string>"), Kind::Nested);

        let number = aggregates(0, &column("amount", "double")).join(", ");
        assert_eq!(
            number,
            "count(`amount`) AS c0_values, approx_count_distinct(`amount`) AS c0_distinct, \
             min(`amount`) AS c0_min, max(`amount`) AS c0_max, avg(`amount`) AS c0_mean"
        );
        let text = aggregates(3, &column("na`me", "string")).join(", ");
        assert!(
            text.contains("min(length(`na``me`)) AS c3_minlen"),
            "{text}"
        );
        assert!(
            !text.contains("c3_min,") && !text.contains("c3_mean"),
            "{text}"
        );
        let date = aggregates(1, &column("day", "date")).join(", ");
        assert!(date.contains("date_from_unix_date"), "{date}");
        let nested = aggregates(2, &column("tags", "map<string,string>"));
        assert_eq!(nested, ["count(`tags`) AS c2_values"]);
    }

    #[test]
    fn unknown_columns_are_named_with_the_ones_there_are() {
        let schema = page(&[("id", "long"), ("Region", "string")], vec![]);
        let picked = chosen(&schema, &["region".into()]).unwrap();
        assert_eq!(picked, vec![column("Region", "string")]);
        let err = format!("{:#}", chosen(&schema, &["regoin".into()]).unwrap_err());
        assert!(
            err.contains("no column regoin; its columns are id, Region"),
            "{err}"
        );
    }

    #[test]
    fn wide_tables_and_top_values_are_read_in_batches() {
        let wide: Vec<Column> = (0..85).map(|i| column(&format!("c{i}"), "long")).collect();
        assert_eq!(passes(&wide, None), 3);
        // 100 rows a page / 10 values a column = 10 columns a pass.
        assert_eq!(passes(&wide, Some(10)), 3 + 9);
        assert_eq!(columns_per_top_pass(50), 2);
    }

    #[tokio::test]
    async fn a_profile_reads_counts_ranges_and_top_values_at_one_snapshot() {
        let from = "`sales`.`orders` VERSION AS OF 104";
        let columns = vec![column("amount", "double"), column("region", "string")];
        let stats = page(
            &[("total_rows", "long")],
            vec![json!({
                "total_rows": 10, "c0_values": 8, "c0_distinct": 7, "c0_min": 1.5,
                "c0_max": 99, "c0_mean": 20.123456, "c1_values": 10, "c1_distinct": "3",
                "c1_minlen": 2, "c1_maxlen": 4,
            })],
        );
        let top = page(
            &[("col", "int"), ("value", "string"), ("n", "long")],
            vec![
                json!({"col": 1, "value": "us", "n": 3}),
                json!({"col": 0, "value": null, "n": 2}),
                json!({"col": 1, "value": "eu", "n": 6}),
            ],
        );
        let session = Scripted::new(vec![Ok(stats), Ok(top)]);

        let profile = collect(&session, from, &columns, Some(2)).await.unwrap();

        let asked = session.asked();
        assert!(asked.iter().all(|sql| sql.contains(from)), "{asked:?}");
        assert!(
            asked[1].contains("stack(2, 0, CAST(`amount` AS STRING), 1, CAST(`region` AS STRING))")
        );
        assert!(asked[1].ends_with("WHERE rk <= 2"));
        assert_eq!(profile.rows, 10);
        let amount = &profile.columns[0];
        assert_eq!((amount.nulls, amount.distinct), (2, Some(7)));
        assert_eq!(shown(amount.mean.as_ref().unwrap()), "20.1235");
        let region = &profile.columns[1];
        assert_eq!((region.min_length, region.max_length), (Some(2), Some(4)));
        assert_eq!(
            top_text(region.kind, &region.top, profile.rows),
            "eu (60.0%), us (30.0%)"
        );
        assert_eq!(
            top_text(amount.kind, &amount.top, profile.rows),
            "NULL (20.0%)"
        );

        let rows = table_rows(&profile);
        assert_eq!(rows[0]["nulls"], "2 (20.0%)");
        assert_eq!(rows[0]["distinct"], "~7");
        assert_eq!(rows[1]["length"], "2–4");
    }

    #[test]
    fn dates_drop_the_midnight_time_and_tiny_shares_stay_visible() {
        let midnight = json!("2023-01-01 00:00:00");
        assert_eq!(shown_for(Kind::Date, &midnight), "2023-01-01");
        assert_eq!(shown_for(Kind::Timestamp, &midnight), "2023-01-01 00:00:00");
        assert_eq!(percent(1, 250_000), "<0.1%");
        assert_eq!(percent(0, 10), "0%");
    }

    #[test]
    fn the_scan_is_the_snapshots_files_once_per_pass() {
        let history = history();
        let snapshot = history.resolve("current").unwrap();
        let table = TableName::parse("sales.orders").unwrap();
        let estimate = scan_estimate(&table, snapshot, 2).unwrap();
        // The fixture's current snapshot holds 40 records in 4,000 bytes.
        assert_eq!(estimate.read.bytes, 8_000);
        assert_eq!(estimate.read.rows, 80);
        assert!(estimate.headline().starts_with("Reads up to 7.8 KB"));

        let over = refusal(ScanLimit::parse("4KB", true).unwrap(), &estimate).unwrap();
        assert!(
            over.contains("reads up to 7.8 KB, over the 4.0 KB limit (--max-scan)"),
            "{over}"
        );
        assert!(
            over.contains("--columns") && over.contains("--max-scan 1MB"),
            "{over}"
        );
        assert_eq!(
            refusal(ScanLimit::parse("8KB", false).unwrap(), &estimate),
            None
        );
        assert_eq!(refusal(ScanLimit::Off, &estimate), None);
    }
}
