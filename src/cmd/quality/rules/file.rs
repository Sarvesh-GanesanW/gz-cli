//! `quality.toml`: the checks gz runs on the tables the jobs of a `gz sync`
//! folder write, read and validated with the line and column of each
//! problem.
//!
//! ```toml
//! [tables."Lineage.sales.orders"]
//! fail_on = "error"                      # or "warn"
//! rows = { min = 1, max = 5_000_000 }
//! not_null = ["order_id"]
//! null_fraction = { coupon = 0.9 }       # at most 90% nulls
//! unique = ["order_id", ["customer_id", "ordered_at"]]
//! accepted_values = { status = ["new", "paid", "shipped"] }
//! ranges = { amount = { min = 0, max = 100000 } }
//! freshness = { column = "ordered_at", max_age = "24h" }
//! max_commit_age = "24h"
//!
//! [[tables."Lineage.sales.orders".sql]]
//! name = "no_negative_totals"
//! query = "SELECT * FROM {table} WHERE total < 0"
//! severity = "error"                     # or "warn"
//! ```
//!
//! `ranges` and `freshness` are qualipilot's own `ranges` and `freshness`
//! checks (`column_ranges`, `freshness_columns`, `freshness_max_age_hours`);
//! the others run as registered checks of the same run ([`super::script`]).

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use toml::Spanned;

use crate::cmd::lakehouse_table::LakehouseTable;
use crate::cmd::quality::args::FailOn;
use crate::cmd::run_wait::parse_timeout;

/// The file `gz jobs try`, `gz jobs publish` and `gz quality lint` look for.
pub const FILE_NAME: &str = "quality.toml";
const SECONDS_PER_HOUR: f64 = 3600.0;

// ---------- the file as written ----------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    #[serde(default)]
    tables: BTreeMap<Spanned<String>, Spanned<RawTable>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTable {
    fail_on: Option<Spanned<String>>,
    rows: Option<Spanned<RawRows>>,
    not_null: Option<Vec<Spanned<String>>>,
    null_fraction: Option<BTreeMap<Spanned<String>, Spanned<f64>>>,
    unique: Option<Vec<Spanned<RawKey>>>,
    accepted_values: Option<BTreeMap<Spanned<String>, Spanned<Vec<toml::Value>>>>,
    ranges: Option<BTreeMap<Spanned<String>, Spanned<RawRange>>>,
    freshness: Option<Spanned<RawFreshness>>,
    max_commit_age: Option<Spanned<String>>,
    #[serde(default)]
    sql: Vec<Spanned<RawSql>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRows {
    min: Option<i64>,
    max: Option<i64>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawKey {
    Column(String),
    Columns(Vec<String>),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRange {
    min: f64,
    max: f64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFreshness {
    column: Spanned<String>,
    max_age: Spanned<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSql {
    name: Spanned<String>,
    query: Spanned<String>,
    severity: Option<Spanned<String>>,
}

// ---------- the checked rules ----------

/// A table's rules, checked and in a fixed order, so the same rules always
/// hash the same ([`TableRules::fingerprint`]).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TableRules {
    #[serde(skip)]
    pub table: LakehouseTable,
    #[serde(rename = "failOn")]
    pub fail_on: &'static str,
    pub rows: Option<RowBounds>,
    /// Column → the largest share of nulls allowed (0 for `not_null`).
    pub null_limits: BTreeMap<String, f64>,
    pub unique: Vec<Vec<String>>,
    pub accepted_values: BTreeMap<String, Vec<Value>>,
    pub ranges: BTreeMap<String, ColumnRange>,
    pub freshness: Option<Freshness>,
    pub max_commit_age_hours: Option<f64>,
    pub sql: Vec<SqlRule>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RowBounds {
    pub min: Option<i64>,
    pub max: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ColumnRange {
    pub min: f64,
    pub max: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Freshness {
    pub column: String,
    pub max_age_hours: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SqlRule {
    pub name: String,
    pub query: String,
    pub severity: &'static str,
}

impl TableRules {
    /// Rules with nothing declared yet, judged at `fail_on`.
    pub fn empty(table: LakehouseTable, fail_on: FailOn) -> Self {
        Self {
            table,
            fail_on: fail_on.as_str(),
            rows: None,
            null_limits: BTreeMap::new(),
            unique: Vec::new(),
            accepted_values: BTreeMap::new(),
            ranges: BTreeMap::new(),
            freshness: None,
            max_commit_age_hours: None,
            sql: Vec::new(),
        }
    }

    pub fn fail_on(&self) -> FailOn {
        if self.fail_on == FailOn::Warn.as_str() {
            FailOn::Warn
        } else {
            FailOn::Error
        }
    }

    /// How many checks the rules make.
    pub fn check_count(&self) -> usize {
        usize::from(self.rows.is_some())
            + self.null_limits.len()
            + self.unique.len()
            + self.accepted_values.len()
            + self.ranges.len()
            + usize::from(self.freshness.is_some())
            + usize::from(self.max_commit_age_hours.is_some())
            + self.sql.len()
    }

    /// A short hash of the rules, recorded with a branch's verdict so a
    /// publish can tell the verdict came from these rules.
    pub fn fingerprint(&self) -> String {
        use sha2::{Digest, Sha256};
        let canonical = serde_json::to_string(self).unwrap_or_default();
        let digest = Sha256::digest(canonical.as_bytes());
        digest[..8].iter().map(|b| format!("{b:02x}")).collect()
    }

    /// The qualipilot config: only the built-in checks the rules use, so
    /// the run judges exactly what the file declares.
    pub fn qualipilot_config(&self) -> Value {
        let mut checks = json!({
            "missing_values": false,
            "duplicates": false,
            "data_types": false,
            "outliers": false,
            "ranges": !self.ranges.is_empty(),
            "cardinality": false,
            "freshness": self.freshness.is_some(),
            "quality_contract": false,
            "min_rows": 0,
        });
        if !self.ranges.is_empty() {
            checks["column_ranges"] = json!(self.ranges);
        }
        if let Some(freshness) = &self.freshness {
            checks["freshness_columns"] = json!([freshness.column]);
            checks["freshness_max_age_hours"] = json!(freshness.max_age_hours);
        }
        json!({ "checks": checks })
    }

    /// The registered checks the job script runs, one entry per check name
    /// ([`super::script`] reads `check` and the fields beside it).
    pub fn registered_checks(&self) -> Vec<Value> {
        let mut checks = Vec::new();
        if let Some(rows) = &self.rows {
            checks.push(json!({"check": "row_count", "min": rows.min, "max": rows.max}));
        }
        if !self.null_limits.is_empty() {
            checks.push(json!({"check": "null_limits", "columns": self.null_limits}));
        }
        if !self.unique.is_empty() {
            checks.push(json!({"check": "unique", "keys": self.unique}));
        }
        if !self.accepted_values.is_empty() {
            checks.push(json!({"check": "accepted_values", "columns": self.accepted_values}));
        }
        if let Some(hours) = self.max_commit_age_hours {
            checks.push(json!({"check": "commit_age", "maxHours": hours}));
        }
        for rule in &self.sql {
            checks.push(json!({
                "check": format!("sql:{}", rule.name),
                "query": rule.query,
                "severity": rule.severity,
            }));
        }
        checks
    }
}

/// A checked `quality.toml`.
#[derive(Debug, Clone)]
pub struct QualityFile {
    pub path: PathBuf,
    pub tables: Vec<TableRules>,
}

impl QualityFile {
    /// `quality.toml` in `dir`; `None` when there is none.
    pub fn find(dir: &Path) -> Result<Option<Self>> {
        let path = dir.join(FILE_NAME);
        if !path.exists() {
            return Ok(None);
        }
        Self::load(&path).map(Some)
    }

    /// Reads and checks the file; the error lists every problem found.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        match parse(&text) {
            Ok(tables) => Ok(Self {
                path: path.to_path_buf(),
                tables,
            }),
            Err(problems) => bail!("{}", render_problems(path, &text, &problems)),
        }
    }

    /// The rules for `table`, matched as the lakehouse matches names.
    pub fn rules_for(&self, table: &LakehouseTable) -> Option<&TableRules> {
        self.tables
            .iter()
            .find(|rules| rules.table.key() == table.key())
    }
}

/// One thing wrong with the file, at a byte range of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub span: Range<usize>,
    pub message: String,
}

impl Problem {
    fn at(span: Range<usize>, message: impl Into<String>) -> Self {
        Self {
            span,
            message: message.into(),
        }
    }
}

/// `path:line:column: message`, one per line, in file order.
pub fn render_problems(path: &Path, text: &str, problems: &[Problem]) -> String {
    let mut sorted: Vec<&Problem> = problems.iter().collect();
    sorted.sort_by_key(|p| p.span.start);
    sorted
        .iter()
        .map(|problem| {
            let (line, column) = line_column(text, problem.span.start);
            format!("{}:{line}:{column}: {}", path.display(), problem.message)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 1-based line and column (in characters) of a byte offset.
fn line_column(text: &str, offset: usize) -> (usize, usize) {
    let before = &text[..offset.min(text.len())];
    let line = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |at| at + 1);
    (line, before[line_start..].chars().count() + 1)
}

/// Every table's rules, or every problem found.
pub fn parse(text: &str) -> Result<Vec<TableRules>, Vec<Problem>> {
    let raw: RawFile = toml::from_str(text).map_err(|err| {
        let span = err.span().unwrap_or(0..0);
        vec![Problem::at(span, err.message().trim().to_string())]
    })?;
    let mut problems = Vec::new();
    let mut tables: Vec<TableRules> = Vec::new();
    for (name, table) in raw.tables {
        let Some(rules) = check_table(&name, table, &mut problems) else {
            continue;
        };
        if tables.iter().any(|t| t.table.key() == rules.table.key()) {
            problems.push(Problem::at(
                name.span(),
                format!("{} is declared twice", name.get_ref()),
            ));
            continue;
        }
        tables.push(rules);
    }
    if raw_is_empty(&tables, &problems) {
        problems.push(Problem::at(
            0..0,
            "declares no tables; add [tables.\"WAREHOUSE.DATABASE.TABLE\"] with its checks",
        ));
    }
    if problems.is_empty() {
        Ok(tables)
    } else {
        Err(problems)
    }
}

fn raw_is_empty(tables: &[TableRules], problems: &[Problem]) -> bool {
    tables.is_empty() && problems.is_empty()
}

fn check_table(
    name: &Spanned<String>,
    table: Spanned<RawTable>,
    problems: &mut Vec<Problem>,
) -> Option<TableRules> {
    let parsed = LakehouseTable::parse(name.get_ref());
    let table_span = table.span();
    let raw = table.into_inner();
    let fail_on = match &raw.fail_on {
        Some(level) => match level.get_ref().as_str() {
            "warn" => FailOn::Warn,
            "error" => FailOn::Error,
            other => {
                problems.push(Problem::at(
                    level.span(),
                    format!("fail_on must be \"warn\" or \"error\", not \"{other}\""),
                ));
                FailOn::Error
            }
        },
        None => FailOn::Error,
    };
    let Ok(lakehouse_table) = parsed else {
        problems.push(Problem::at(
            name.span(),
            format!(
                "\"{}\" is not a table name; write \"WAREHOUSE.DATABASE.TABLE\"",
                name.get_ref()
            ),
        ));
        return None;
    };
    let mut rules = TableRules::empty(lakehouse_table, fail_on);
    rules.rows = raw.rows.and_then(|rows| check_rows(rows, problems));
    for column in raw.not_null.into_iter().flatten() {
        if let Some(column) = check_column(&column, problems) {
            rules.null_limits.insert(column, 0.0);
        }
    }
    for (column, fraction) in raw.null_fraction.into_iter().flatten() {
        let Some(name) = check_column(&column, problems) else {
            continue;
        };
        if !(0.0..=1.0).contains(fraction.get_ref()) {
            problems.push(Problem::at(
                fraction.span(),
                "a null fraction is a share between 0 and 1, e.g. 0.05 for 5%",
            ));
            continue;
        }
        if rules
            .null_limits
            .insert(name.clone(), *fraction.get_ref())
            .is_some()
        {
            problems.push(Problem::at(
                column.span(),
                format!("{name} is in both not_null and null_fraction; keep one"),
            ));
        }
    }
    for key in raw.unique.into_iter().flatten() {
        if let Some(columns) = check_key(&key, problems) {
            rules.unique.push(columns);
        }
    }
    for (column, values) in raw.accepted_values.into_iter().flatten() {
        let Some(name) = check_column(&column, problems) else {
            continue;
        };
        if let Some(values) = check_values(&values, problems) {
            rules.accepted_values.insert(name, values);
        }
    }
    for (column, range) in raw.ranges.into_iter().flatten() {
        let Some(name) = check_column(&column, problems) else {
            continue;
        };
        let bounds = range.get_ref();
        if !bounds.min.is_finite() || !bounds.max.is_finite() || bounds.min > bounds.max {
            problems.push(Problem::at(
                range.span(),
                "a range needs finite numbers with min no larger than max",
            ));
            continue;
        }
        rules.ranges.insert(
            name,
            ColumnRange {
                min: bounds.min,
                max: bounds.max,
            },
        );
    }
    if let Some(freshness) = raw.freshness {
        let freshness = freshness.into_inner();
        let column = check_column(&freshness.column, problems);
        let hours = check_age(&freshness.max_age, problems);
        if let (Some(column), Some(max_age_hours)) = (column, hours) {
            rules.freshness = Some(Freshness {
                column,
                max_age_hours,
            });
        }
    }
    if let Some(age) = &raw.max_commit_age {
        rules.max_commit_age_hours = check_age(age, problems);
    }
    for rule in raw.sql {
        if let Some(rule) = check_sql(&rule, &rules.sql, problems) {
            rules.sql.push(rule);
        }
    }
    if rules.check_count() == 0 && !problems.iter().any(|p| table_span.contains(&p.span.start)) {
        problems.push(Problem::at(
            name.span(),
            format!(
                "{} declares no checks; add rows, not_null, unique, accepted_values, ranges, freshness, max_commit_age or [[...sql]]",
                name.get_ref()
            ),
        ));
    }
    Some(rules)
}

fn check_rows(rows: Spanned<RawRows>, problems: &mut Vec<Problem>) -> Option<RowBounds> {
    let span = rows.span();
    let RawRows { min, max } = rows.into_inner();
    let problem = match (min, max) {
        (None, None) => Some("rows needs min, max or both"),
        (Some(min), _) if min < 0 => Some("rows.min can't be negative"),
        (_, Some(max)) if max < 0 => Some("rows.max can't be negative"),
        (Some(min), Some(max)) if min > max => Some("rows.min is larger than rows.max"),
        _ => None,
    };
    if let Some(problem) = problem {
        problems.push(Problem::at(span, problem));
        return None;
    }
    Some(RowBounds { min, max })
}

fn check_column(column: &Spanned<String>, problems: &mut Vec<Problem>) -> Option<String> {
    let name = column.get_ref();
    if name.trim().is_empty() || name.trim() != name {
        problems.push(Problem::at(
            column.span(),
            "a column name can't be empty or start or end with spaces",
        ));
        return None;
    }
    Some(name.clone())
}

fn check_key(key: &Spanned<RawKey>, problems: &mut Vec<Problem>) -> Option<Vec<String>> {
    let columns = match key.get_ref() {
        RawKey::Column(column) => vec![column.clone()],
        RawKey::Columns(columns) => columns.clone(),
    };
    let blank = columns.iter().any(|c| c.trim().is_empty() || c.trim() != c);
    let mut seen = columns.clone();
    seen.sort();
    seen.dedup();
    let problem = if columns.is_empty() {
        Some("a unique key needs at least one column")
    } else if blank {
        Some("a column name can't be empty or start or end with spaces")
    } else if seen.len() != columns.len() {
        Some("a unique key names a column twice")
    } else {
        None
    };
    if let Some(problem) = problem {
        problems.push(Problem::at(key.span(), problem));
        return None;
    }
    Some(columns)
}

fn check_values(
    values: &Spanned<Vec<toml::Value>>,
    problems: &mut Vec<Problem>,
) -> Option<Vec<Value>> {
    if values.get_ref().is_empty() {
        problems.push(Problem::at(
            values.span(),
            "accepted_values needs at least one value",
        ));
        return None;
    }
    let mut accepted = Vec::new();
    for value in values.get_ref() {
        let json = match value {
            toml::Value::String(text) => json!(text),
            toml::Value::Integer(number) => json!(number),
            toml::Value::Float(number) if number.is_finite() => json!(number),
            toml::Value::Boolean(flag) => json!(flag),
            _ => {
                problems.push(Problem::at(
                    values.span(),
                    "accepted values are strings, numbers or true/false",
                ));
                return None;
            }
        };
        accepted.push(json);
    }
    Some(accepted)
}

fn check_age(age: &Spanned<String>, problems: &mut Vec<Problem>) -> Option<f64> {
    match parse_timeout(age.get_ref()) {
        Ok(duration) => Some(duration.as_secs() as f64 / SECONDS_PER_HOUR),
        Err(why) => {
            problems.push(Problem::at(
                age.span(),
                format!("an age like 30m, 6h or 2d: {why}"),
            ));
            None
        }
    }
}

fn check_sql(
    rule: &Spanned<RawSql>,
    earlier: &[SqlRule],
    problems: &mut Vec<Problem>,
) -> Option<SqlRule> {
    let raw = rule.get_ref();
    let name = raw.name.get_ref();
    let valid_name = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !valid_name {
        problems.push(Problem::at(
            raw.name.span(),
            "an sql check's name uses letters, digits, '_' and '-'",
        ));
        return None;
    }
    if earlier.iter().any(|other| &other.name == name) {
        problems.push(Problem::at(
            raw.name.span(),
            format!("two sql checks are named {name}"),
        ));
        return None;
    }
    let query = raw.query.get_ref().trim();
    if !query.contains("{table}") {
        problems.push(Problem::at(
            raw.query.span(),
            "the query must read the checked table as {table}, e.g. SELECT * FROM {table} WHERE total < 0",
        ));
        return None;
    }
    let severity = match raw.severity.as_ref().map(|s| s.get_ref().as_str()) {
        None | Some("error") => "error",
        Some("warn") => "warn",
        Some(other) => {
            let span = raw.severity.as_ref().map_or(rule.span(), Spanned::span);
            problems.push(Problem::at(
                span,
                format!("severity must be \"warn\" or \"error\", not \"{other}\""),
            ));
            return None;
        }
    };
    Some(SqlRule {
        name: name.clone(),
        query: query.trim_end_matches(';').to_string(),
        severity,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"
[tables."Lineage.sales.orders"]
fail_on = "warn"
rows = { min = 1, max = 5_000_000 }
not_null = ["order_id"]
null_fraction = { coupon = 0.9 }
unique = ["order_id", ["customer_id", "ordered_at"]]
accepted_values = { status = ["new", "paid"], priority = [1, 2] }
ranges = { amount = { min = 0, max = 100000 } }
freshness = { column = "ordered_at", max_age = "24h" }
max_commit_age = "2d"

[[tables."Lineage.sales.orders".sql]]
name = "no_negative_totals"
query = "SELECT * FROM {table} WHERE total < 0;"
severity = "warn"
"#;

    fn problems(text: &str) -> String {
        render_problems(Path::new("quality.toml"), text, &parse(text).unwrap_err())
    }

    #[test]
    fn every_rule_is_read() {
        let tables = parse(FULL).unwrap();
        assert_eq!(tables.len(), 1);
        let rules = &tables[0];
        assert_eq!(rules.table.to_string(), "Lineage.sales.orders");
        assert_eq!(rules.fail_on(), FailOn::Warn);
        assert_eq!(
            rules.rows,
            Some(RowBounds {
                min: Some(1),
                max: Some(5_000_000)
            })
        );
        assert_eq!(rules.null_limits["order_id"], 0.0);
        assert_eq!(rules.null_limits["coupon"], 0.9);
        assert_eq!(
            rules.unique,
            vec![
                vec!["order_id".to_string()],
                vec!["customer_id".into(), "ordered_at".into()]
            ]
        );
        assert_eq!(rules.accepted_values["priority"], vec![json!(1), json!(2)]);
        assert_eq!(
            rules.ranges["amount"],
            ColumnRange {
                min: 0.0,
                max: 100000.0
            }
        );
        assert_eq!(rules.freshness.as_ref().unwrap().max_age_hours, 24.0);
        assert_eq!(rules.max_commit_age_hours, Some(48.0));
        assert_eq!(rules.sql[0].query, "SELECT * FROM {table} WHERE total < 0");
        assert_eq!(rules.sql[0].severity, "warn");
        assert_eq!(rules.check_count(), 11);
    }

    #[test]
    fn built_in_checks_run_only_when_declared() {
        let rules = &parse(FULL).unwrap()[0];
        let config = rules.qualipilot_config();
        assert_eq!(config["checks"]["outliers"], json!(false));
        assert_eq!(config["checks"]["ranges"], json!(true));
        assert_eq!(
            config["checks"]["column_ranges"]["amount"],
            json!({"min": 0.0, "max": 100000.0})
        );
        assert_eq!(config["checks"]["freshness_columns"], json!(["ordered_at"]));
        assert_eq!(config["checks"]["min_rows"], json!(0));
        let names: Vec<String> = rules
            .registered_checks()
            .iter()
            .map(|c| c["check"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            names,
            [
                "row_count",
                "null_limits",
                "unique",
                "accepted_values",
                "commit_age",
                "sql:no_negative_totals"
            ]
        );
    }

    #[test]
    fn the_fingerprint_follows_the_rules_not_the_layout() {
        let a = &parse(FULL).unwrap()[0];
        let reordered = FULL.replace(
            "not_null = [\"order_id\"]\nnull_fraction = { coupon = 0.9 }",
            "null_fraction = { coupon = 0.9 }\nnot_null = [\"order_id\"]",
        );
        let b = &parse(&reordered).unwrap()[0];
        assert_eq!(a.fingerprint(), b.fingerprint());
        let changed = FULL.replace("max = 5_000_000", "max = 6_000_000");
        assert_ne!(a.fingerprint(), parse(&changed).unwrap()[0].fingerprint());
        assert_eq!(a.fingerprint().len(), 16);
    }

    #[test]
    fn syntax_errors_say_where() {
        let text = "[tables.\"a.b.c\"]\nrows = { min = 1 \n";
        assert!(
            problems(text).starts_with("quality.toml:2:"),
            "{}",
            problems(text)
        );
    }

    #[test]
    fn unknown_keys_say_where() {
        let text = "[tables.\"a.b.c\"]\nrows = { min = 1 }\nnot_nul = [\"x\"]\n";
        let shown = problems(text);
        assert!(
            shown.starts_with("quality.toml:3:1: unknown field `not_nul`"),
            "{shown}"
        );
    }

    #[test]
    fn wrong_types_say_where() {
        let text = "[tables.\"a.b.c\"]\nrows = { min = \"one\" }\n";
        let shown = problems(text);
        assert!(shown.starts_with("quality.toml:2:16:"), "{shown}");
    }

    #[test]
    fn every_meaning_problem_is_listed_with_its_place() {
        let text = r#"[tables."a.b"]
rows = { min = 1 }

[tables."w.d.t"]
fail_on = "fatal"
rows = { min = 5, max = 1 }
null_fraction = { x = 1.5 }
unique = [[]]
accepted_values = { s = [] }
ranges = { n = { min = 2, max = 1 } }
max_commit_age = "soon"

[[tables."w.d.t".sql]]
name = "bad name"
query = "SELECT 1"
"#;
        let shown = problems(text);
        let lines: Vec<&str> = shown.lines().collect();
        assert_eq!(lines.len(), 9, "{shown}");
        assert!(lines[0].starts_with("quality.toml:1:9: \"a.b\" is not a table name"));
        assert!(lines[1].starts_with("quality.toml:5:11: fail_on must be"));
        assert!(lines[2].starts_with("quality.toml:6:8: rows.min is larger"));
        assert!(lines[3].starts_with("quality.toml:7:23: a null fraction"));
        assert!(lines[4].starts_with("quality.toml:8:11: a unique key needs"));
        assert!(lines[5].starts_with("quality.toml:9:25: accepted_values needs"));
        assert!(lines[6].starts_with("quality.toml:10:16: a range needs"));
        assert!(lines[7].starts_with("quality.toml:11:18: an age like"));
        assert!(lines[8].starts_with("quality.toml:14:8: an sql check's name"));
    }

    #[test]
    fn sql_checks_must_read_the_table_and_have_unique_names() {
        let text = r#"[tables."w.d.t"]
[[tables."w.d.t".sql]]
name = "a"
query = "SELECT * FROM orders"
[[tables."w.d.t".sql]]
name = "b"
query = "SELECT * FROM {table}"
[[tables."w.d.t".sql]]
name = "b"
query = "SELECT * FROM {table}"
severity = "loud"
"#;
        let shown = problems(text);
        assert!(
            shown.contains(":4:9: the query must read the checked table as {table}"),
            "{shown}"
        );
        assert!(
            shown.contains(":9:8: two sql checks are named b"),
            "{shown}"
        );
    }

    #[test]
    fn tables_without_checks_or_duplicates_are_refused() {
        let empty = "[tables.\"w.d.t\"]\nfail_on = \"warn\"\n";
        assert!(problems(empty).contains("w.d.t declares no checks"));
        let twice =
            "[tables.\"w.d.t\"]\nrows = { min = 1 }\n[tables.\"W.D.T\"]\nrows = { min = 1 }\n";
        assert!(
            problems(twice).contains("declared twice"),
            "{}",
            problems(twice)
        );
        assert!(problems("").contains("declares no tables"));
    }

    #[test]
    fn rules_are_found_whatever_the_case() {
        let file = QualityFile {
            path: PathBuf::from(FILE_NAME),
            tables: parse(FULL).unwrap(),
        };
        let table = LakehouseTable::parse("lineage.SALES.orders").unwrap();
        assert!(file.rules_for(&table).is_some());
        let other = LakehouseTable::parse("lineage.sales.returns").unwrap();
        assert!(file.rules_for(&other).is_none());
    }
}
