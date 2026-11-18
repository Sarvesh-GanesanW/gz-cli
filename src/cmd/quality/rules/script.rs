//! The Spark job that runs `quality.toml` rules (and scheduled watches) on
//! one or more tables, and reading its reports back.
//!
//! Each table goes through the runtime's `runTableDataQualityChecks` (gz-
//! base-rte-spark connectors/DATAQUALITY.py) with a qualipilot config whose
//! `registered_checks` name the rules qualipilot has no built-in for. The
//! script registers them with `qualipilot.checks.register_check`; each reads
//! the snapshot qualipilot pinned (`ctx.iceberg_snapshot_id`) with
//! `VERSION AS OF`, so every check of a table judges the same data, and
//! only counts reach the report.
//!
//! Checking a branch records its verdict as the runtime always does; the
//! script then stamps that verdict with the rules' fingerprint (`rules`),
//! using the runtime's own `_branchVerdict`, so `gz jobs publish` can tell
//! the verdict came from the rules in the folder. The lakehouse publish
//! gate reads `passed` and `snapshotId` as before.
//!
//! Each table's report is printed on log lines marked `<marker>-<nnn>` the
//! way `gz quality check` prints one ([`super::super::check::script`]).

use anyhow::Result;
use serde_json::{json, Value};

use crate::cmd::quality::check::script::{read_report, spark_name, ConnectedTable, PrintedReport};
use crate::table_history::TableName;

/// One table to check.
#[derive(Debug, Clone, PartialEq)]
pub struct TableJob {
    /// `warehouse.database.table`, as shown.
    pub shown: String,
    pub connection: String,
    pub table: TableName,
    pub branch: Option<String>,
    pub fail_on: &'static str,
    /// The qualipilot config; `None` keeps the runtime's default checks.
    pub config: Option<Value>,
    pub checks: Vec<Value>,
    /// Stamped on the branch verdict when a branch is checked.
    pub fingerprint: Option<String>,
    /// Appends the results to this table of a lakehouse connection.
    pub results: Option<ConnectedTable>,
}

pub struct ScriptParams<'a> {
    pub tables: &'a [TableJob],
    pub site: &'a str,
    pub marker: &'a str,
    /// Fail the run when a table fails, so a scheduled run's failure email
    /// goes out; otherwise the run succeeds and gz reads the verdicts.
    pub fail_run: bool,
}

const SCRIPT_BODY: &str = r#"
from GZ.SparkUtils import sparkSession

session = sparkSession("gz-quality-rules", "WARN", siteName=PARAMS["site"])
CHUNK_CHARS = 8000
VERDICT_PREFIX = "gz.quality.branch."
CURRENT = {}


def quote(name):
    return "`" + name.replace("`", "``") + "`"


def literal(value):
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, (int, float)):
        return repr(value)
    return "'" + str(value).replace("\\", "\\\\").replace("'", "\\'") + "'"


def qualified(spec, name):
    catalog = session.getLakehouseCatalog(spec["connection"], PARAMS["site"])
    return ".".join(quote(part) for part in (catalog, name["database"], name["table"]))


def checked(ctx):
    snapshot = ctx.iceberg_snapshot_id
    if snapshot is None:
        raise ValueError("the table was not read at a lakehouse snapshot")
    return f"{CURRENT['qualified']} VERSION AS OF {int(snapshot)}"


def scalar(query):
    value = session.sql(query).collect()[0][0]
    return 0 if value is None else int(value)


def rule(name):
    return next(spec for spec in CURRENT["checks"] if spec["check"] == name)


def row_count(ctx):
    spec = rule("row_count")
    rows = ctx.row_count if ctx.row_count is not None else scalar(f"SELECT count(*) FROM {checked(ctx)}")
    low, high = spec.get("min"), spec.get("max")
    outside = (low is not None and rows < low) or (high is not None and rows > high)
    return ("error" if outside else "ok"), {"rows": rows, "min": low, "max": high}


def null_limits(ctx):
    limits = rule("null_limits")["columns"]
    names = list(limits)
    sums = ", ".join(
        f"sum(CASE WHEN {quote(name)} IS NULL THEN 1 ELSE 0 END)" for name in names
    )
    row = session.sql(f"SELECT count(*), {sums} FROM {checked(ctx)}").collect()[0]
    rows = int(row[0])
    columns, over = {}, []
    for index, name in enumerate(names):
        nulls = int(row[index + 1] or 0)
        share = nulls / rows if rows else 0.0
        columns[name] = {"nulls": nulls, "fraction": share, "limit": limits[name]}
        if share > limits[name]:
            over.append(name)
    return ("error" if over else "ok"), {"rows": rows, "columns": columns, "over": over}


def unique(ctx):
    keys = []
    for columns in rule("unique")["keys"]:
        group = ", ".join(quote(name) for name in columns)
        extra = scalar(
            f"SELECT sum(n - 1) FROM (SELECT count(*) AS n FROM {checked(ctx)} "
            f"GROUP BY {group} HAVING count(*) > 1)"
        )
        keys.append({"columns": columns, "duplicateRows": extra})
    failed = any(key["duplicateRows"] for key in keys)
    return ("error" if failed else "ok"), {"keys": keys}


def accepted_values(ctx):
    columns = {}
    for name, values in rule("accepted_values")["columns"].items():
        allowed = ", ".join(literal(value) for value in values)
        columns[name] = {
            "unexpectedRows": scalar(
                f"SELECT count(*) FROM {checked(ctx)} "
                f"WHERE {quote(name)} IS NOT NULL AND {quote(name)} NOT IN ({allowed})"
            )
        }
    failed = any(column["unexpectedRows"] for column in columns.values())
    return ("error" if failed else "ok"), {"columns": columns}


def commit_age(ctx):
    from datetime import datetime, timezone

    limit = rule("commit_age")["maxHours"]
    facts = ctx.iceberg_table.snapshot_facts(ctx.iceberg_snapshot_id)
    if facts is None:
        raise ValueError(f"snapshot {ctx.iceberg_snapshot_id} is no longer in the table")
    committed = datetime.fromtimestamp(facts.timestamp_ms / 1000, tz=timezone.utc)
    age = (datetime.now(timezone.utc) - committed).total_seconds() / 3600
    payload = {
        "committedAt": committed.strftime("%Y-%m-%dT%H:%M:%SZ"),
        "ageHours": round(age, 2),
        "maxAgeHours": limit,
    }
    return ("error" if age > limit else "ok"), payload


def sql_check(name):
    def run(ctx):
        spec = rule(name)
        query = spec["query"].replace("{table}", checked(ctx))
        rows = scalar(f"SELECT count(*) FROM ({query}) AS gz_rule")
        return (spec["severity"] if rows else "ok"), {"rows": rows}

    return run


CHECKS = {
    "row_count": row_count,
    "null_limits": null_limits,
    "unique": unique,
    "accepted_values": accepted_values,
    "commit_age": commit_age,
}


def register_all():
    names = {spec["check"] for table in PARAMS["tables"] for spec in table["checks"]}
    for name in sorted(names):
        check = sql_check(name) if name.startswith("sql:") else CHECKS[name]
        register_check(name, check)


def stamp_verdict(spec, report):
    from connectors.DATAQUALITY import _branchVerdict

    verdict = _branchVerdict(report, spec["failOn"])
    verdict["rules"] = spec["fingerprint"]
    text = json.dumps(verdict, separators=(",", ":"))
    session.sql(
        f"ALTER TABLE {CURRENT['qualified']} SET TBLPROPERTIES "
        f"({literal(VERDICT_PREFIX + spec['branch'])} = {literal(text)})"
    )


def print_chunks(marker, body):
    text = json.dumps(body, separators=(",", ":"))
    chunks = [text[at : at + CHUNK_CHARS] for at in range(0, len(text), CHUNK_CHARS)]
    for number, chunk in enumerate(chunks, 1):
        print(f"{marker} {number}/{len(chunks)} {chunk}", flush=True)


def check_table(index, spec):
    marker = f"{PARAMS['marker']}-{index:03d}"
    CURRENT.clear()
    CURRENT.update(spec)
    CURRENT["qualified"] = qualified(spec, spec["table"])
    config = dict(spec["config"] or {})
    names = [check["check"] for check in spec["checks"]]
    if names:
        config["checks"] = {**config.get("checks", {}), "registered_checks": names}
    options = {
        "tableName": spec["sparkName"],
        "config": QualipilotConfig.model_validate(config) if config else None,
        "includeLlm": False,
        "failOn": spec["failOn"],
        "branch": spec["branch"],
        "lakehouseConnection": spec["connection"],
    }
    if spec["results"] is not None:
        options["resultsTable"] = qualified(spec["results"], spec["results"]["table"])
    try:
        try:
            report = session.runTableDataQualityChecks(**options)
        except QualityGateError as error:
            report = error.report
        if spec["branch"] is not None and spec["fingerprint"] is not None:
            stamp_verdict(spec, report)
    except Exception as error:
        message = f"{type(error).__name__}: {error}"
        print_chunks(marker, {"report": {"error": message}, "gateFailures": []})
        return f"{spec['shown']}: could not be checked ({message})"
    body = report.model_dump(mode="json")
    for result in body["results"]:
        result["payload"] = json.loads(history_payload_json(result["payload"]))
    failures = [result.name for result in report.gate_failures(spec["failOn"])]
    print_chunks(marker, {"report": body, "gateFailures": failures})
    if failures:
        return f"{spec['shown']}: {', '.join(failures)}"
    return None


register_all()
problems = [
    problem
    for problem in (check_table(index, spec) for index, spec in enumerate(PARAMS["tables"], 1))
    if problem is not None
]
if problems and PARAMS["failRun"]:
    raise RuntimeError("quality checks failed: " + "; ".join(problems))
"#;

/// The job's Python source.
pub fn script(params: &ScriptParams) -> String {
    // A JSON string literal is also a valid Python string literal.
    let params_literal = Value::String(params_json(params).to_string()).to_string();
    format!(
        "import json\n\n\
         from qualipilot import QualipilotConfig, QualityGateError\n\
         from qualipilot.checks import register_check\n\
         from qualipilot.models.history import history_payload_json\n\n\
         PARAMS = json.loads({params_literal})\n\
         {SCRIPT_BODY}"
    )
}

fn table_json(name: &TableName) -> Value {
    json!({"database": name.database, "table": name.table})
}

fn params_json(params: &ScriptParams) -> Value {
    let tables: Vec<Value> = params
        .tables
        .iter()
        .map(|table| {
            json!({
                "shown": table.shown,
                "connection": table.connection,
                "table": table_json(&table.table),
                "sparkName": spark_name(&table.table),
                "branch": table.branch,
                "failOn": table.fail_on,
                "config": table.config,
                "checks": table.checks,
                "fingerprint": table.fingerprint,
                "results": table.results.as_ref().map(|results| json!({
                    "connection": results.connection,
                    "table": table_json(&results.table),
                })),
            })
        })
        .collect();
    json!({
        "tables": tables,
        "site": params.site,
        "marker": params.marker,
        "failRun": params.fail_run,
    })
}

/// The marker of the `index`th table (from 1); fixed width, so one never
/// starts another.
pub fn table_marker(marker: &str, index: usize) -> String {
    format!("{marker}-{index:03}")
}

/// What one table's check printed: its report, or why it could not run.
#[derive(Debug, Clone, PartialEq)]
pub enum TableReport {
    Checked(PrintedReport),
    NotChecked(String),
}

/// The `index`th table's report from the run's log; `None` when the run
/// printed none for it.
pub fn read_table_report<'a>(
    messages: impl IntoIterator<Item = &'a str>,
    marker: &str,
    index: usize,
) -> Result<Option<TableReport>> {
    let Some(printed) = read_report(messages, &table_marker(marker, index))? else {
        return Ok(None);
    };
    Ok(Some(match printed.report["error"].as_str() {
        Some(error) => TableReport::NotChecked(error.to_string()),
        None => TableReport::Checked(printed),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(branch: Option<&str>) -> TableJob {
        TableJob {
            shown: "Lineage.sales.orders".into(),
            connection: "lake's \"main\"".into(),
            table: TableName {
                database: "sales".into(),
                table: "orders".into(),
            },
            branch: branch.map(str::to_string),
            fail_on: "error",
            config: Some(json!({"checks": {"outliers": false}})),
            checks: vec![
                json!({"check": "sql:neg", "query": "SELECT * FROM {table} WHERE '\\'' = x", "severity": "error"}),
            ],
            fingerprint: Some("0123456789abcdef".into()),
            results: None,
        }
    }

    fn loaded_params(script: &str) -> Value {
        let line = script
            .lines()
            .find(|l| l.starts_with("PARAMS = json.loads("))
            .unwrap();
        let literal = &line["PARAMS = json.loads(".len()..line.len() - 1];
        let text: String = serde_json::from_str(literal).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    #[test]
    fn the_params_carry_every_table_as_data() {
        let tables = [job(Some("try-1")), job(None)];
        let script = script(&ScriptParams {
            tables: &tables,
            site: "preview",
            marker: "GZR-1a2b",
            fail_run: true,
        });
        let params = loaded_params(&script);
        assert_eq!(params["tables"][0]["connection"], "lake's \"main\"");
        assert_eq!(
            params["tables"][0]["checks"][0]["query"],
            "SELECT * FROM {table} WHERE '\\'' = x"
        );
        assert_eq!(params["tables"][0]["branch"], "try-1");
        assert_eq!(params["tables"][1]["branch"], Value::Null);
        assert_eq!(
            params["tables"][0]["table"],
            json!({"database": "sales", "table": "orders"})
        );
        assert_eq!(params["failRun"], json!(true));
        assert!(script.contains("register_check(name, check)"));
        assert!(script.contains("raise RuntimeError(\"quality checks failed: \""));
        assert_eq!(params["tables"][0]["sparkName"], "sales.orders");
    }

    #[test]
    fn markers_never_prefix_one_another() {
        assert_eq!(table_marker("GZR-1a2b", 1), "GZR-1a2b-001");
        assert!(!table_marker("GZR-1a2b", 12).starts_with(&table_marker("GZR-1a2b", 1)));
    }

    #[test]
    fn a_table_that_could_not_be_checked_says_why() {
        let lines = [
            "GZR-1a2b-001 1/1 {\"report\":{\"error\":\"ValueError: no column x\"},\"gateFailures\":[]}",
            "GZR-1a2b-002 1/1 {\"report\":{\"results\":[]},\"gateFailures\":[\"unique\"]}",
        ];
        assert_eq!(
            read_table_report(lines, "GZR-1a2b", 1).unwrap(),
            Some(TableReport::NotChecked("ValueError: no column x".into()))
        );
        let Some(TableReport::Checked(second)) = read_table_report(lines, "GZR-1a2b", 2).unwrap()
        else {
            panic!("the second table was checked");
        };
        assert_eq!(second.gate_failures, ["unique"]);
        assert_eq!(read_table_report(lines, "GZR-1a2b", 3).unwrap(), None);
    }
}
