//! The job `gz quality check` runs, and reading its report back.
//!
//! The script calls the runtime's `runTableDataQualityChecks` (gz-base-rte-
//! spark / gz-base-rte-duckdb connectors/DATAQUALITY.py) with the table as
//! `database.table` of the job's lakehouse connection, then prints the report
//! as JSON on log lines that start with a marker unique to the run, cut into
//! numbered chunks: container runtimes split long log lines. Payloads go
//! through qualipilot's `history_payload_json`, which drops row values
//! (`sample`, `top_values`), so no table data reaches the job log.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use super::super::args::FailOn;
use crate::table_history::TableName;

/// Which runtime the image is, as the ETL provider tells them apart
/// (`validRTEs` in app.py: the name contains `spark` or `duckdb`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Spark,
    DuckDb,
}

impl Engine {
    pub fn of_runtime(rte: &str) -> Result<Self> {
        let rte_lower = rte.to_lowercase();
        if rte_lower.contains("spark") {
            Ok(Engine::Spark)
        } else if rte_lower.contains("duckdb") {
            Ok(Engine::DuckDb)
        } else {
            bail!("--rte {rte} is neither a Spark nor a DuckDB runtime; see `gz rte list`")
        }
    }
}

/// A table of a lakehouse connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectedTable {
    pub connection: String,
    pub table: TableName,
}

pub struct ScriptParams<'a> {
    pub checked: &'a ConnectedTable,
    pub results: Option<&'a ConnectedTable>,
    pub snapshot: Option<i64>,
    pub branch: Option<&'a str>,
    pub fail_on: FailOn,
    pub config: Option<&'a Value>,
    pub site: &'a str,
    pub marker: &'a str,
}

const SPARK_SESSION: &str = "from GZ.SparkUtils import sparkSession

session = sparkSession(\"gz-quality-check\", \"WARN\", siteName=PARAMS[\"site\"])
";
const DUCKDB_SESSION: &str = "from GZ.DuckDBUtils import duckSession

session = duckSession(siteName=PARAMS[\"site\"])
";

/// Everything but the session: the same for both runtimes. A results table
/// of another connection is named with that connection's catalog, since
/// `lakehouseConnection=` only picks the checked table's.
const CHECK_AND_REPORT: &str = r#"
CHUNK_CHARS = 8000


def results_table():
    results = PARAMS["results"]
    if results is None or results["connection"] == PARAMS["connection"]:
        return None if results is None else results["table"]
    catalog = session.getLakehouseCatalog(results["connection"], PARAMS["site"])
    return f"{catalog}.{results['table']}"


def print_report(report):
    body = report.model_dump(mode="json")
    for result in body["results"]:
        result["payload"] = json.loads(history_payload_json(result["payload"]))
    failures = [result.name for result in report.gate_failures(PARAMS["failOn"])]
    text = json.dumps(
        {"report": body, "gateFailures": failures}, separators=(",", ":")
    )
    chunks = [text[at : at + CHUNK_CHARS] for at in range(0, len(text), CHUNK_CHARS)]
    for number, chunk in enumerate(chunks, 1):
        print(f"{PARAMS['marker']} {number}/{len(chunks)} {chunk}", flush=True)


config = PARAMS["config"]
options = {
    "tableName": PARAMS["table"],
    "config": None if config is None else QualipilotConfig.model_validate(config),
    "includeLlm": False,
    "failOn": PARAMS["failOn"],
    "snapshotId": PARAMS["snapshotId"],
    "branch": PARAMS["branch"],
    "lakehouseConnection": PARAMS["connection"],
}
if PARAMS["results"] is not None:
    options["resultsTable"] = results_table()
try:
    report = session.runTableDataQualityChecks(**options)
except QualityGateError as error:
    report = error.report
print_report(report)
"#;

/// The job's Python source.
pub fn script(engine: Engine, params: &ScriptParams) -> String {
    let session = match engine {
        Engine::Spark => SPARK_SESSION,
        Engine::DuckDb => DUCKDB_SESSION,
    };
    // A JSON string literal is also a valid Python string literal.
    let params_literal = Value::String(params_json(params).to_string()).to_string();
    format!(
        "import json\n\n\
         from qualipilot import QualipilotConfig, QualityGateError\n\
         from qualipilot.models.history import history_payload_json\n\n\
         PARAMS = json.loads({params_literal})\n\n\
         {session}{CHECK_AND_REPORT}"
    )
}

fn params_json(params: &ScriptParams) -> Value {
    json!({
        "table": spark_name(&params.checked.table),
        "connection": params.checked.connection,
        "results": params.results.map(|r| json!({
            "connection": r.connection,
            "table": spark_name(&r.table),
        })),
        "snapshotId": params.snapshot,
        "branch": params.branch,
        "failOn": params.fail_on.as_str(),
        "config": params.config,
        "site": params.site,
        "marker": params.marker,
    })
}

/// `database.table`, each part back-quoted only when Spark needs it, so
/// the report's `source` keeps the plain name `gz quality history` matches.
fn spark_name(table: &TableName) -> String {
    let part = |name: &str| {
        if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            name.to_string()
        } else {
            format!("`{}`", name.replace('`', "``"))
        }
    };
    format!("{}.{}", part(&table.database), part(&table.table))
}

/// What the run printed: the report and the checks that failed the gate.
#[derive(Debug, Clone, PartialEq)]
pub struct PrintedReport {
    pub report: Value,
    pub gate_failures: Vec<String>,
}

/// Puts the marked chunks back together; `None` when the run printed none.
pub fn read_report<'a>(
    messages: impl IntoIterator<Item = &'a str>,
    marker: &str,
) -> Result<Option<PrintedReport>> {
    let mut total = None;
    let mut chunks: Vec<Option<String>> = Vec::new();
    for message in messages {
        let Some(at) = message.find(marker) else {
            continue;
        };
        let line = message[at + marker.len()..].trim_end_matches(['\r', '\n']);
        let (number, of, chunk) = chunk_parts(line)
            .with_context(|| format!("the run printed a broken report line: {line:.80}"))?;
        if *total.get_or_insert(of) != of {
            bail!("the run's report lines disagree on how many there are");
        }
        chunks.resize(of, None);
        chunks[number - 1] = Some(chunk.to_string());
    }
    let Some(total) = total else {
        return Ok(None);
    };
    let missing = chunks.iter().filter(|c| c.is_none()).count();
    if missing > 0 {
        bail!("the run's log lost {missing} of the {total} report lines; run the check again");
    }
    let text: String = chunks.into_iter().flatten().collect();
    let printed: Value =
        serde_json::from_str(&text).context("the run printed a report gz cannot read")?;
    Ok(Some(PrintedReport {
        gate_failures: printed["gateFailures"]
            .as_array()
            .context("the run's report has no gate result")?
            .iter()
            .filter_map(|name| name.as_str().map(str::to_string))
            .collect(),
        report: printed["report"].clone(),
    }))
}

/// ` 2/3 <chunk>` → (2, 3, chunk).
fn chunk_parts(line: &str) -> Option<(usize, usize, &str)> {
    let rest = line.strip_prefix(' ')?;
    let (position, chunk) = rest.split_once(' ').unwrap_or((rest, ""));
    let (number, of) = position.split_once('/')?;
    let (number, of): (usize, usize) = (number.parse().ok()?, of.parse().ok()?);
    (number >= 1 && number <= of).then_some((number, of, chunk))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(database: &str, name: &str) -> TableName {
        TableName {
            database: database.into(),
            table: name.into(),
        }
    }

    fn params<'a>(
        checked: &'a ConnectedTable,
        results: Option<&'a ConnectedTable>,
        config: Option<&'a Value>,
    ) -> ScriptParams<'a> {
        ScriptParams {
            checked,
            results,
            snapshot: Some(8234019231840123),
            branch: None,
            fail_on: FailOn::Warn,
            config,
            site: "preview",
            marker: "GZQ-1a2b",
        }
    }

    /// The PARAMS line of a script, as Python would load it.
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
    fn the_runtime_comes_from_the_image_name() {
        assert_eq!(
            Engine::of_runtime("gz-rte-spark-sklearn:1.0.0").unwrap(),
            Engine::Spark
        );
        assert_eq!(
            Engine::of_runtime("demo-rte-custom:duckdb-TestNewRTE").unwrap(),
            Engine::DuckDb
        );
        assert!(Engine::of_runtime("gz-rte-pandas:1.0.0").is_err());
    }

    #[test]
    fn spark_scripts_check_the_table_of_the_jobs_connection() {
        let checked = ConnectedTable {
            connection: "GZLakehouseProd".into(),
            table: table("DemoDB", "ProductionMetrics"),
        };
        let results = ConnectedTable {
            connection: "GZLakehouseProd".into(),
            table: table("DemoDB", "ProductionMetrics_quality"),
        };
        let config = json!({"checks": {"severity_overrides": {"outliers": "error"}}});
        let script = script(
            Engine::Spark,
            &params(&checked, Some(&results), Some(&config)),
        );
        assert!(script.contains("from GZ.SparkUtils import sparkSession\n"));
        assert!(script.contains("session.runTableDataQualityChecks(**options)"));
        assert_eq!(
            loaded_params(&script),
            json!({
                "table": "DemoDB.ProductionMetrics",
                "connection": "GZLakehouseProd",
                "results": {"connection": "GZLakehouseProd", "table": "DemoDB.ProductionMetrics_quality"},
                "snapshotId": 8234019231840123_i64,
                "branch": null,
                "failOn": "warn",
                "config": config,
                "site": "preview",
                "marker": "GZQ-1a2b",
            })
        );
    }

    #[test]
    fn duckdb_scripts_open_a_duckdb_session() {
        let checked = ConnectedTable {
            connection: "lake".into(),
            table: table("sales", "orders"),
        };
        let script = script(Engine::DuckDb, &params(&checked, None, None));
        assert!(script.contains("from GZ.DuckDBUtils import duckSession\n"));
        assert!(!script.contains("sparkSession"));
        assert_eq!(loaded_params(&script)["results"], Value::Null);
    }

    #[test]
    fn names_spark_cannot_read_bare_are_back_quoted() {
        assert_eq!(
            spark_name(&table("sales", "orders_2026")),
            "sales.orders_2026"
        );
        assert_eq!(
            spark_name(&table("sales-eu", "or`ders")),
            "`sales-eu`.`or``ders`"
        );
    }

    #[test]
    fn quotes_and_newlines_in_values_stay_inside_the_literal() {
        let checked = ConnectedTable {
            connection: "it's \"lake\"\n".into(),
            table: table("sales", "orders"),
        };
        let script = script(Engine::Spark, &params(&checked, None, None));
        assert_eq!(loaded_params(&script)["connection"], "it's \"lake\"\n");
    }

    #[test]
    fn chunks_are_put_back_in_order_whatever_else_the_log_holds() {
        let text = json!({"report": {"results": []}, "gateFailures": ["outliers"]}).to_string();
        let (first, second) = text.split_at(10);
        let lines = [
            "Spark session started".to_string(),
            format!("2026-11-18 10:00:01 GZQ-1a2b 2/2 {second}\n"),
            format!("GZQ-1a2b 1/2 {first}"),
            "GZQ-other 1/1 {}".to_string(),
        ];
        let printed = read_report(lines.iter().map(String::as_str), "GZQ-1a2b")
            .unwrap()
            .unwrap();
        assert_eq!(printed.gate_failures, vec!["outliers".to_string()]);
        assert_eq!(printed.report, json!({"results": []}));
    }

    #[test]
    fn a_run_without_report_lines_has_no_report() {
        let lines = ["Traceback (most recent call last):", "ValueError: no table"];
        assert_eq!(read_report(lines, "GZQ-1a2b").unwrap(), None);
    }

    #[test]
    fn missing_or_broken_chunks_are_errors() {
        let lost = ["GZQ-1a2b 1/2 {\"report\""];
        assert!(read_report(lost, "GZQ-1a2b")
            .unwrap_err()
            .to_string()
            .contains("lost 1 of the 2"));
        let broken = ["GZQ-1a2b 3/2 {}"];
        assert!(read_report(broken, "GZQ-1a2b").is_err());
    }
}
