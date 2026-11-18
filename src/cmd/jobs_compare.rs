//! `gz jobs compare <run> <run>`: two finished ETL runs side by side.
//!
//! Each run is read from:
//! - its record, `POST /etl/jobStatus` → `data[0]` (`jobstatus`,
//!   `startedat`, `stoppedat`); runs still going are refused;
//! - its observability detail, `GET /etl/consumption-metrics/{run}`
//!   (`identity`: job, times, duration, engine; `runtime.summary`: rows and
//!   bytes read and written; `usage.allocation`: requested CPU and memory),
//!   as the web app's run history reads it (`etlUtlis.js`
//!   `getExecutionConsumptionDetail`);
//! - its log, `POST /etl/logs`, for the `===== JOB METRICS =====` block a
//!   Spark run prints at the end;
//! - the job's lineage, `GET /etl/lineage/jobs/{job}` with `versionId` and
//!   `runLimit`/`runOffset` (`lineageApi.js` `getJobLineage`): the job
//!   version (code hash) each run executed, and the tables it read
//!   (`SOURCE`) and wrote (`TARGET`) from the run's observations.
//!
//! The platform keeps each version's code in storage by hash but serves no
//! API to read it back, keeps no per-run copy of the job's config, and
//! returns no run's compute, runtime image or worker count (the run record
//! has compute and image; no route exposes them). The output says so
//! instead of leaving the rows blank.

use anyhow::{bail, Context, Result};
use reqwest::Method;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

use crate::cmd::civil::http_date_seconds;
use crate::cmd::datafs::human_bytes;
use crate::cmd::run_wait::{self, is_final};
use crate::cmd::{seg, Runtime};
use crate::output::OutputFormat;

const ETL: &str = "etl";
const METRICS_START: &str = "===== JOB METRICS =====";
const LINEAGE_PAGE: usize = 100;
/// What the platform does not keep or return per run.
const NOT_RECORDED: &str = "not returned by the platform";
const CODE_NOTE: &str = "the platform keeps each version's code by hash but has no API to read it back, so only whether the code changed is shown";
const CONFIG_NOTE: &str =
    "the job's config is not kept per run; only its current config can be read (gz projects job <project> <job>)";
const COMPUTE_NOTE: &str = "a run's compute size and runtime image are stored in its record but no API returns them, and its worker count is not recorded; requested CPU and memory are the closest measure";

/// Everything gz can read about one finished run.
struct RunFacts {
    execution: String,
    status: String,
    record: Value,
    metrics: Option<Value>,
    spark: Vec<(String, String)>,
    lineage: Option<RunLineage>,
}

/// The job version a run executed and the tables it touched.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct RunLineage {
    version: Option<i64>,
    code_hash: Option<String>,
    read: Vec<String>,
    written: Vec<String>,
}

/// One compared value.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    section: &'static str,
    field: String,
    first: String,
    second: String,
}

impl Row {
    fn new(section: &'static str, field: &str, first: String, second: String) -> Self {
        Self {
            section,
            field: field.to_string(),
            first,
            second,
        }
    }

    fn changed(&self) -> bool {
        self.first != self.second
    }
}

pub async fn run(rt: &Runtime, first: &str, second: &str) -> Result<()> {
    for id in [first, second] {
        check_run_id(id)?;
    }
    if first == second {
        bail!(
            "both runs are {first}; pass two different runs (gz projects job-runs <project> <job>)"
        );
    }
    let (a, b) = tokio::try_join!(RunFacts::load(rt, first), RunFacts::load(rt, second))?;
    let rows = rows(&a, &b);
    let notes = notes(&a, &b);
    if rt.format != OutputFormat::Table {
        return rt.show(as_json(&a, &b, &rows, &notes)).await;
    }
    print_table(rt, &a, &b, &rows)?;
    for note in &notes {
        crate::output::info(note);
    }
    Ok(())
}

fn check_run_id(id: &str) -> Result<()> {
    let is_run_id = id.len() == 32 && id.chars().all(|c| c.is_ascii_hexdigit());
    if !is_run_id {
        bail!("{id:?} is not a run id (32 hex characters); list runs with gz projects job-runs <project> <job>");
    }
    Ok(())
}

impl RunFacts {
    async fn load(rt: &Runtime, execution: &str) -> Result<Self> {
        let record = run_wait::read_record(rt, execution)
            .await?
            .with_context(|| format!("run {execution} not found; list runs with gz projects job-runs <project> <job>"))?;
        let status = record["jobstatus"].as_str().unwrap_or_default().to_string();
        if !is_final(Some(&status)) {
            bail!(
                "run {execution} is still {}; compare it once it ends (gz jobs logs --job {execution} --follow)",
                if status.is_empty() { "starting" } else { &status }
            );
        }
        let metrics = execution_metrics(rt, execution).await?;
        let log = run_wait::finished_log_lines(rt, execution).await?;
        let job = metrics
            .as_ref()
            .and_then(|m| m["identity"]["jobId"].as_i64());
        let lineage = match job {
            Some(job) => run_lineage(rt, job, execution).await?,
            None => None,
        };
        Ok(Self {
            execution: execution.to_string(),
            status,
            record,
            metrics,
            spark: spark_metrics(&log),
            lineage,
        })
    }

    fn identity(&self, key: &str) -> &Value {
        match &self.metrics {
            Some(metrics) => &metrics["identity"][key],
            None => &Value::Null,
        }
    }

    fn metric(&self, path: &[&str]) -> &Value {
        let Some(mut value) = self.metrics.as_ref() else {
            return &Value::Null;
        };
        for key in path {
            value = &value[*key];
        }
        value
    }

    fn duration_seconds(&self) -> Option<f64> {
        if let Some(seconds) = self.identity("durationSeconds").as_f64() {
            return Some(seconds);
        }
        let start = http_date_seconds(self.record["startedat"].as_str()?)?;
        let stop = http_date_seconds(self.record["stoppedat"].as_str()?)?;
        Some((stop - start) as f64)
    }
}

/// The run's observability detail; `None` when none was collected for it.
async fn execution_metrics(rt: &Runtime, execution: &str) -> Result<Option<Value>> {
    let path = format!("/etl/consumption-metrics/{}", seg(execution));
    match rt
        .http
        .request_json(ETL, Method::GET, &path, &[], None)
        .await
    {
        Ok(detail) => Ok(Some(detail)),
        Err(err) if err.to_string().contains("HTTP 404") => Ok(None),
        Err(err) => Err(err.context(format!("cannot read the metrics of run {execution}"))),
    }
}

/// The `key : value` lines of the last `===== JOB METRICS =====` block.
fn spark_metrics(log: &[String]) -> Vec<(String, String)> {
    let Some(start) = log.iter().rposition(|l| l.trim() == METRICS_START) else {
        return Vec::new();
    };
    log[start + 1..]
        .iter()
        .map(|line| line.trim())
        .take_while(|line| !line.chars().all(|c| c == '='))
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
        .collect()
}

// ---------- lineage ----------

/// Finds the job version that recorded `execution` among the job's
/// versions, newest first, and reads the tables of that run.
async fn run_lineage(rt: &Runtime, job: i64, execution: &str) -> Result<Option<RunLineage>> {
    let path = format!("/etl/lineage/jobs/{job}");
    let query = [("limit".to_string(), LINEAGE_PAGE.to_string())];
    let listing = match rt
        .http
        .request_json(ETL, Method::GET, &path, &query, None)
        .await
    {
        Ok(listing) => listing,
        Err(err) if err.to_string().contains("HTTP 404") => return Ok(None),
        Err(err) => return Err(err.context(format!("cannot read the lineage of job {job}"))),
    };
    let versions: Vec<i64> = listing["versions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v["job_version_id"].as_i64())
        .collect();
    for version in versions {
        if let Some(found) = find_run_in_version(rt, &path, version, execution).await? {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

async fn find_run_in_version(
    rt: &Runtime,
    path: &str,
    version: i64,
    execution: &str,
) -> Result<Option<RunLineage>> {
    let mut offset = 0;
    loop {
        let query = [
            ("versionId".to_string(), version.to_string()),
            ("runLimit".to_string(), LINEAGE_PAGE.to_string()),
            ("runOffset".to_string(), offset.to_string()),
            ("eventLimit".to_string(), "1".to_string()),
        ];
        let page = rt
            .http
            .request_json(ETL, Method::GET, path, &query, None)
            .await
            .with_context(|| format!("cannot read version {version} of the job's lineage"))?;
        let graph = &page["graph"];
        if let Some(found) = lineage_of_run(graph, execution) {
            return Ok(Some(found));
        }
        let runs = &graph["pagination"]["runs"];
        match runs["nextOffset"].as_u64() {
            Some(next) if runs["hasMore"] == Value::Bool(true) => offset = next as usize,
            _ => return Ok(None),
        }
    }
}

/// The run's version and tables, if `graph` lists the run.
fn lineage_of_run(graph: &Value, execution: &str) -> Option<RunLineage> {
    let run_id = graph["runs"]
        .as_array()?
        .iter()
        .find(|run| run["jobExecutionId"].as_str() == Some(execution))?["runId"]
        .as_i64();
    let names: BTreeMap<i64, String> = graph["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|asset| Some((asset["assetId"].as_i64()?, asset_name(asset)?)))
        .collect();
    let tables = |role: &str| -> Vec<String> {
        let mut found: Vec<String> = graph["observations"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|o| o["runId"].as_i64() == run_id && o["role"].as_str() == Some(role))
            .filter_map(|o| names.get(&o["assetId"].as_i64()?).cloned())
            .collect();
        found.sort();
        found.dedup();
        found
    };
    let version = &graph["jobVersion"];
    Some(RunLineage {
        version: version["versionNumber"].as_i64(),
        code_hash: version["codeHash"].as_str().map(str::to_string),
        read: tables("SOURCE"),
        written: tables("TARGET"),
    })
}

fn asset_name(asset: &Value) -> Option<String> {
    asset["qualifiedName"]
        .as_str()
        .or_else(|| asset["displayName"].as_str())
        .map(str::to_string)
}

// ---------- comparison ----------

fn rows(a: &RunFacts, b: &RunFacts) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut both = |section, field: &str, read: &dyn Fn(&RunFacts) -> String| {
        rows.push(Row::new(section, field, read(a), read(b)));
    };
    both("run", "job", &|r| {
        format!(
            "{} {}",
            text(r.identity("jobId")),
            r.identity("jobName").as_str().unwrap_or_default()
        )
        .trim()
        .to_string()
    });
    both("run", "status", &|r| r.status.clone());
    both("run", "started", &|r| started(r));
    both("run", "ended", &|r| ended(r));
    both("run", "duration", &|r| {
        r.duration_seconds()
            .map(|s| crate::cmd::flow_run::format_duration((s * 1000.0) as u64))
            .unwrap_or_else(|| "—".into())
    });
    both("run", "started by", &|r| text(r.identity("startedBy")));
    both("run", "engine", &|r| {
        let provider = text(r.identity("provider"));
        match r.metric(&["runtime", "engineVersion"]).as_str() {
            Some(version) => format!("{provider} {version}"),
            None => provider,
        }
    });
    for field in ["compute", "runtime image", "workers"] {
        both("compute", field, &|_| NOT_RECORDED.to_string());
    }
    both("compute", "requested CPU cores", &|r| {
        text(r.metric(&["usage", "allocation", "cpuRequestCores"]))
    });
    both("compute", "requested memory GiB", &|r| {
        text(r.metric(&["usage", "allocation", "memoryRequestGiB"]))
    });
    both("compute", "peak memory GiB", &|r| {
        text(r.metric(&["usage", "memory", "peakGiB"]))
    });
    both("compute", "CPU core-hours", &|r| {
        text(r.metric(&["usage", "cpu", "coreHours"]))
    });
    let summary = |r: &RunFacts, key: &str| r.metric(&["runtime", "summary", key]).clone();
    both("data", "rows read", &|r| text(&summary(r, "inputRows")));
    both("data", "rows written", &|r| text(&summary(r, "outputRows")));
    both("data", "bytes read", &|r| bytes(&summary(r, "inputBytes")));
    both("data", "bytes written", &|r| {
        bytes(&summary(r, "outputBytes"))
    });
    both("data", "tables read", &|r| tables(r, |l| &l.read));
    both("data", "tables written", &|r| tables(r, |l| &l.written));
    both("code", "job version", &|r| match &r.lineage {
        Some(RunLineage {
            version: Some(v), ..
        }) => format!("v{v}"),
        _ => "no lineage recorded".into(),
    });
    both("code", "code hash", &|r| {
        r.lineage
            .as_ref()
            .and_then(|l| l.code_hash.as_deref())
            .map(|h| h[..h.len().min(12)].to_string())
            .unwrap_or_else(|| "—".into())
    });
    for key in spark_keys(a, b) {
        both("spark", &key, &|r| {
            r.spark
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| "—".into())
        });
    }
    rows
}

/// Keys of both runs' Spark blocks, in the order they were printed.
fn spark_keys(a: &RunFacts, b: &RunFacts) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for (key, _) in a.spark.iter().chain(&b.spark) {
        if !keys.contains(key) {
            keys.push(key.clone());
        }
    }
    keys
}

fn started(r: &RunFacts) -> String {
    r.identity("startedAt")
        .as_str()
        .or_else(|| r.record["startedat"].as_str())
        .unwrap_or("—")
        .to_string()
}

fn ended(r: &RunFacts) -> String {
    r.identity("completedAt")
        .as_str()
        .or_else(|| r.record["stoppedat"].as_str())
        .unwrap_or("—")
        .to_string()
}

fn tables(r: &RunFacts, pick: impl Fn(&RunLineage) -> &Vec<String>) -> String {
    match &r.lineage {
        None => "no lineage recorded".into(),
        Some(lineage) if pick(lineage).is_empty() => "none recorded".into(),
        Some(lineage) => pick(lineage).join(", "),
    }
}

fn text(value: &Value) -> String {
    match value {
        Value::Null => "—".into(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn bytes(value: &Value) -> String {
    value
        .as_u64()
        .map(human_bytes)
        .unwrap_or_else(|| text(value))
}

/// What the comparison could not cover, and why.
fn notes(a: &RunFacts, b: &RunFacts) -> Vec<String> {
    let mut notes = vec![COMPUTE_NOTE.to_string()];
    let hashes = [a, b].map(|r| r.lineage.as_ref().and_then(|l| l.code_hash.clone()));
    let code = match hashes {
        [Some(x), Some(y)] if x == y => "both runs executed the same code".to_string(),
        [Some(_), Some(_)] => "the runs executed different code".to_string(),
        _ => "no lineage recorded for at least one run, so gz cannot tell whether the code changed"
            .to_string(),
    };
    notes.push(format!("{code}; {CODE_NOTE}"));
    notes.push(CONFIG_NOTE.to_string());
    for r in [a, b] {
        if r.metrics.is_none() {
            notes.push(format!(
                "run {} has no collected metrics; rows, bytes and capacity are unknown",
                r.execution
            ));
        }
        if r.spark.is_empty() {
            notes.push(format!(
                "run {} printed no {METRICS_START} block",
                r.execution
            ));
        }
    }
    if a.identity("jobId") != b.identity("jobId") {
        notes.push("the runs belong to different jobs".to_string());
    }
    notes
}

fn short(execution: &str) -> String {
    execution[..8].to_string()
}

fn print_table(rt: &Runtime, a: &RunFacts, b: &RunFacts, rows: &[Row]) -> Result<()> {
    let (first, second) = (short(&a.execution), short(&b.execution));
    let table: Vec<Value> = rows
        .iter()
        .map(|row| {
            let mut cells = Map::new();
            cells.insert("section".into(), json!(row.section));
            cells.insert("field".into(), json!(row.field));
            cells.insert(first.clone(), json!(row.first));
            cells.insert(second.clone(), json!(row.second));
            cells.insert(
                "changed".into(),
                json!(if row.changed() { "*" } else { "" }),
            );
            Value::Object(cells)
        })
        .collect();
    rt.show_columns(
        &Value::Array(table),
        &["section", "field", &first, &second, "changed"],
    )
}

fn as_json(a: &RunFacts, b: &RunFacts, rows: &[Row], notes: &[String]) -> Value {
    let run = |r: &RunFacts| {
        json!({
            "jobIdentifier": r.execution,
            "status": r.status,
            "record": r.record,
            "metrics": r.metrics,
            "sparkMetrics": r.spark.iter().map(|(k, v)| json!({"name": k, "value": v})).collect::<Vec<_>>(),
            "lineage": r.lineage.as_ref().map(|l| json!({
                "versionNumber": l.version, "codeHash": l.code_hash,
                "tablesRead": l.read, "tablesWritten": l.written,
            })),
        })
    };
    let rows: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({"section": row.section, "field": row.field, "first": row.first,
                   "second": row.second, "changed": row.changed()})
        })
        .collect();
    json!({"runs": [run(a), run(b)], "comparison": rows, "notes": notes})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_string).collect()
    }

    #[test]
    fn reads_the_last_job_metrics_block() {
        let log = lines(
            "Execution started.\n\
             ===== JOB METRICS =====\n  Jobs : succeeded: 1\n=========================\n\
             ===== JOB METRICS =====\n\
             \x20 Application      : app-1\n\
             \x20 Source           : Spark UI REST API (http://10.0.0.1:4040)\n\
             \x20 Input            : 1,250,007 records (41.91 KB)\n\
             =========================\n\
             Execution complete.",
        );
        assert_eq!(
            spark_metrics(&log),
            vec![
                ("Application".to_string(), "app-1".to_string()),
                (
                    "Source".to_string(),
                    "Spark UI REST API (http://10.0.0.1:4040)".to_string()
                ),
                (
                    "Input".to_string(),
                    "1,250,007 records (41.91 KB)".to_string()
                ),
            ]
        );
        assert!(spark_metrics(&lines("no block here")).is_empty());
    }

    #[test]
    fn finds_a_runs_version_and_the_tables_it_read_and_wrote() {
        let graph = json!({
            "jobVersion": {"versionNumber": 3, "codeHash": "abc"},
            "runs": [{"runId": 7, "jobExecutionId": "run-a"}, {"runId": 8, "jobExecutionId": "run-b"}],
            "assets": [
                {"assetId": 1, "qualifiedName": "lake.sales.orders"},
                {"assetId": 2, "displayName": "quality_history"},
            ],
            "observations": [
                {"runId": 7, "assetId": 1, "role": "SOURCE"},
                {"runId": 7, "assetId": 2, "role": "TARGET"},
                {"runId": 7, "assetId": 2, "role": "TARGET"},
                {"runId": 8, "assetId": 1, "role": "TARGET"},
            ],
        });
        assert_eq!(
            lineage_of_run(&graph, "run-a"),
            Some(RunLineage {
                version: Some(3),
                code_hash: Some("abc".into()),
                read: vec!["lake.sales.orders".into()],
                written: vec!["quality_history".into()],
            })
        );
        assert_eq!(lineage_of_run(&graph, "run-z"), None);
    }

    #[test]
    fn run_ids_are_checked_before_any_call() {
        assert!(check_run_id("c9d6338a7ad6475a8c9e1e52991f45ee").is_ok());
        assert!(check_run_id("3013").is_err());
    }
}
