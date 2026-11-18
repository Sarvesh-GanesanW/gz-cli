//! `gz quality check`: run the checks on a lakehouse table without writing
//! a job.
//!
//! The way `gz run` runs a local file, with a job gz makes and removes:
//! - the table's warehouse picks a GZLAKEHOUSE connection from the
//!   connections registry (`GET /connection`, `GET /connection/{id}` →
//!   `[{"config": {"warehouseName", "databaseName"}, "name", "type"}]`);
//! - a job is created in the project (`POST /etl/etlJobs/{project}` with
//!   `Name` and `fileContent`) and given that connection (`PUT
//!   /etl/etlJobs/jobs/{project}/{job}` with `config.connectionNames` and
//!   `connectionIds`, which the ETL provider reads when the run starts);
//! - the run is submitted like `gz jobs run` (`POST /etl/job`), its status
//!   polled from its record (`POST /etl/jobStatus` → `data[0].jobstatus`),
//!   and only once it is final are its logs read (`POST /etl/logs`, paged
//!   by `cursor` until `hasMoreLogs` is false) for the report the script
//!   printed ([`script`]);
//! - the job is deleted (`DELETE /etl/etlJobs/jobs/{job}`).

mod config;
mod report;
mod script;

use anyhow::{bail, Context, Result};
use reqwest::Method;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

use super::args::{CheckArgs, FailOn};
use crate::cli::RunChoiceArgs;
use crate::cmd::apps::{job_update_body, saved_job_path};
use crate::cmd::code_report::ReportedExit;
use crate::cmd::lakehouse_table::LakehouseTable;
use crate::cmd::{jobs, seg, Runtime};
use crate::output::{self, OutputFormat};
use script::{ConnectedTable, Engine, PrintedReport, ScriptParams};

const CONNECTIONS: &str = "connections";
const PROJECTS: &str = "etlprojects";
const ETL: &str = "etl";
const LAKEHOUSE_TYPE: &str = "GZLAKEHOUSE";
const JOB_PREFIX: &str = "gz-quality-check-";
/// Exit code when the report fails the gate; other errors exit 1.
const GATE_FAILED_EXIT: i32 = 2;
/// `utils/etl_logs/completion.py` TERMINAL_STATUSES.
const FINAL_STATUSES: [&str; 3] = ["Success", "Failed", "Aborted"];
const STATUS_POLL: Duration = Duration::from_secs(5);
const LOG_POLL: Duration = Duration::from_secs(3);
/// The provider holds a finished run's log open for a minute of grace
/// (`COMPLETION_GRACE`); past this gz stops waiting for more lines.
const LOG_WAIT: Duration = Duration::from_secs(180);
/// Log lines shown when the run printed no report.
const LOG_TAIL: usize = 25;

pub async fn run(rt: &Runtime, args: &CheckArgs) -> Result<()> {
    let plan = plan(rt, args).await?;
    let outcome = execute(rt, args, &plan).await?;
    let printed = script::read_report(outcome.messages.iter().map(String::as_str), &plan.marker)?
        .with_context(|| no_report_message(&outcome))?;
    show(
        rt,
        &plan.checked,
        plan.results.as_ref(),
        args.fail_on,
        &outcome,
        &printed,
    )
}

/// What to run, worked out (and every flag checked) before anything starts.
struct Plan {
    checked: LakehouseTable,
    results: Option<LakehouseTable>,
    connections: Vec<LakehouseConnection>,
    compute: i64,
    code: String,
    marker: String,
}

async fn plan(rt: &Runtime, args: &CheckArgs) -> Result<Plan> {
    let checked = LakehouseTable::parse(&args.table)?;
    let results = args
        .results
        .as_deref()
        .map(LakehouseTable::parse)
        .transpose()
        .context("write --results as warehouse.database.table")?;
    let engine = Engine::of_runtime(&args.rte)?;
    if engine == Engine::DuckDb && results.is_some() {
        bail!("--results needs a Spark runtime: DuckDB reads the lakehouse only. Pass --rte gz-rte-spark-sklearn:1.0.0");
    }
    let qualipilot_config = config::qualipilot_config(args.config.as_deref(), &args.severities)?;
    let site = rt
        .http
        .resolved()
        .site()
        .context("no site is set; run `gz configure` first")?;

    let lakehouses = lakehouse_connections(rt).await?;
    let checked_conn = pick_connection(&lakehouses, &checked, args.connection.as_deref())?;
    let results_conn = match &results {
        Some(results) if results.warehouse.eq_ignore_ascii_case(&checked.warehouse) => {
            Some(checked_conn)
        }
        Some(results) => Some(pick_connection(&lakehouses, results, None)?),
        None => None,
    };
    let compute = compute_id(rt, &args.compute).await?;

    let marker = format!("GZQ-{}", short_id());
    let checked_target = ConnectedTable {
        connection: checked_conn.name.clone(),
        table: checked.name.clone(),
    };
    let results_target = results
        .as_ref()
        .zip(results_conn)
        .map(|(table, conn)| ConnectedTable {
            connection: conn.name.clone(),
            table: table.name.clone(),
        });
    let code = script::script(
        engine,
        &ScriptParams {
            checked: &checked_target,
            results: results_target.as_ref(),
            snapshot: args.snapshot,
            branch: args.branch.as_deref(),
            fail_on: args.fail_on,
            config: qualipilot_config.as_ref(),
            site: &site,
            marker: &marker,
        },
    );
    let mut connections = vec![checked_conn.clone()];
    if let Some(conn) = results_conn.filter(|c| c.id != checked_conn.id) {
        connections.push(conn.clone());
    }
    Ok(Plan {
        checked,
        results,
        connections,
        compute,
        code,
        marker,
    })
}

/// Runs the plan as a temporary job and reads the finished run's log.
async fn execute(rt: &Runtime, args: &CheckArgs, plan: &Plan) -> Result<RunOutcome> {
    let job = TempJob::create(rt, args.project, &plan.code, &plan.connections).await?;
    let execution = match job.start(rt, plan.compute, &args.rte).await {
        Ok(execution) => execution,
        Err(err) => {
            job.delete(rt).await;
            return Err(err);
        }
    };
    // Until the run is final its job must stay: the run still needs it.
    let status = tokio::select! {
        status = wait_for_run(rt, &execution) => status,
        _ = tokio::signal::ctrl_c() => Err(anyhow::anyhow!("stopped waiting")),
    }
    .with_context(|| job.left_running(&execution))?;
    let messages = finished_run_log(rt, &execution).await;
    job.delete(rt).await;
    Ok(RunOutcome {
        execution,
        status,
        messages: messages?,
    })
}

// ---------- connections ----------

#[derive(Debug, Clone, PartialEq, Eq)]
struct LakehouseConnection {
    id: i64,
    name: String,
    warehouse: String,
    database: Option<String>,
}

/// Every GZLAKEHOUSE connection with the warehouse it points at.
async fn lakehouse_connections(rt: &Runtime) -> Result<Vec<LakehouseConnection>> {
    let listed = rt
        .http
        .request_json(CONNECTIONS, Method::GET, "/connection", &[], None)
        .await
        .context("cannot list connections")?;
    let mut found = Vec::new();
    for row in listed.as_array().into_iter().flatten() {
        let is_lakehouse = row["type"]
            .as_str()
            .is_some_and(|t| t.eq_ignore_ascii_case(LAKEHOUSE_TYPE));
        let (Some(id), Some(name)) = (row["Id"].as_i64(), row["Name"].as_str()) else {
            continue;
        };
        if !is_lakehouse {
            continue;
        }
        let reply = rt
            .http
            .request_json(
                CONNECTIONS,
                Method::GET,
                &format!("/connection/{id}"),
                &[],
                None,
            )
            .await
            .with_context(|| format!("cannot read connection {name}"))?;
        let detail = if reply.is_array() { &reply[0] } else { &reply };
        let config = &detail["config"];
        let Some(warehouse) = config["warehouseName"].as_str().filter(|w| !w.is_empty()) else {
            continue;
        };
        found.push(LakehouseConnection {
            id,
            name: name.to_string(),
            warehouse: warehouse.to_string(),
            database: config["databaseName"].as_str().map(str::to_string),
        });
    }
    Ok(found)
}

/// `wanted` when given (it must point at the table's warehouse), else a
/// connection of the table's warehouse: one whose default database is the
/// table's first, then the oldest.
fn pick_connection<'a>(
    connections: &'a [LakehouseConnection],
    table: &LakehouseTable,
    wanted: Option<&str>,
) -> Result<&'a LakehouseConnection> {
    let in_warehouse =
        |c: &&LakehouseConnection| c.warehouse.eq_ignore_ascii_case(&table.warehouse);
    if let Some(wanted) = wanted {
        let named = connections
            .iter()
            .find(|c| c.name == wanted)
            .with_context(|| {
                format!("no lakehouse connection named {wanted}; see `gz connections list`")
            })?;
        if !in_warehouse(&named) {
            bail!(
                "connection {wanted} points at warehouse {}, not {}",
                named.warehouse,
                table.warehouse
            );
        }
        return Ok(named);
    }
    connections
        .iter()
        .filter(in_warehouse)
        .min_by_key(|c| {
            let same_database = c
                .database
                .as_deref()
                .is_some_and(|d| d.eq_ignore_ascii_case(&table.name.database));
            (!same_database, c.id)
        })
        .with_context(|| {
            format!(
                "no lakehouse connection points at warehouse {}; create one, or pass --connection",
                table.warehouse
            )
        })
}

async fn compute_id(rt: &Runtime, wanted: &str) -> Result<i64> {
    let sizes = crate::sql::session::Lakehouse::new(rt.http.clone())
        .computes()
        .await
        .context("cannot list compute sizes")?;
    crate::sql::session::pick_compute(&sizes, wanted)
        .map(|c| c.id)
        .with_context(|| {
            let names: Vec<&str> = sizes.iter().map(|c| c.name.as_str()).collect();
            format!("no compute size '{wanted}' (have: {})", names.join(", "))
        })
}

// ---------- the temporary job ----------

struct TempJob {
    project: i64,
    id: i64,
    name: String,
}

/// What the run left: its id, final status and log lines.
struct RunOutcome {
    execution: String,
    status: String,
    messages: Vec<String>,
}

impl TempJob {
    async fn create(
        rt: &Runtime,
        project: i64,
        code: &str,
        connections: &[LakehouseConnection],
    ) -> Result<Self> {
        let name = format!("{JOB_PREFIX}{}", short_id());
        let created = rt
            .http
            .request_json(
                PROJECTS,
                Method::POST,
                &format!("/etl/etlJobs/{project}"),
                &[],
                Some(json!({"Name": name, "fileContent": code})),
            )
            .await
            .with_context(|| format!("cannot create a job in project {project}"))?;
        let id = created["Id"]
            .as_i64()
            .with_context(|| format!("creating job {name} returned no id"))?;
        let job = TempJob { project, id, name };
        if let Err(err) = job.attach(rt, code, connections).await {
            job.delete(rt).await;
            return Err(err);
        }
        Ok(job)
    }

    /// Gives the job its lakehouse connections, as the job editor saves them.
    async fn attach(
        &self,
        rt: &Runtime,
        code: &str,
        connections: &[LakehouseConnection],
    ) -> Result<()> {
        let path = saved_job_path(&self.project.to_string(), &self.id.to_string());
        let saved = rt
            .http
            .request_json(PROJECTS, Method::GET, &path, &[], None)
            .await?;
        let patch = json!({
            "connectionNames": connections.iter().map(|c| c.name.clone()).collect::<Vec<_>>(),
            "connectionIds": connections.iter().map(|c| c.id).collect::<Vec<_>>(),
        });
        let body = job_update_body(
            &saved,
            &self.project.to_string(),
            Some(code.to_string()),
            Some(patch),
        )?;
        rt.http
            .request_json(PROJECTS, Method::PUT, &path, &[], Some(body))
            .await
            .with_context(|| format!("cannot give job {} its lakehouse connection", self.name))?;
        Ok(())
    }

    /// Submits a run of the job, like `gz jobs run`; returns its id.
    async fn start(&self, rt: &Runtime, compute: i64, rte: &str) -> Result<String> {
        let path = saved_job_path(&self.project.to_string(), &self.id.to_string());
        let saved = rt
            .http
            .request_json(PROJECTS, Method::GET, &path, &[], None)
            .await?;
        let choice = RunChoiceArgs {
            compute,
            rte: rte.to_string(),
            workers: None,
            gpu: None,
        };
        let spec = jobs::run_spec(
            &jobs::project_name(rt, self.project).await?,
            self.project,
            self.id,
            &self.name,
            &saved,
            &choice,
        );
        let started = rt
            .http
            .request_json(ETL, Method::POST, "/etl/job", &[], Some(spec))
            .await?;
        let execution = started["jobIdentifier"]
            .as_str()
            .context("the ETL provider started a run but sent no run id")?
            .to_string();
        if rt.format == OutputFormat::Table {
            output::info(&format!(
                "run {execution} started (job {}); waiting for it to finish",
                self.name
            ));
        }
        Ok(execution)
    }

    fn left_running(&self, execution: &str) -> String {
        format!(
            "run {execution} may still be going, so its job {} was kept; delete it when the run ends: gz projects delete-job {}",
            self.name, self.id
        )
    }

    /// Best effort: a job left behind is named so it can be removed by hand.
    async fn delete(&self, rt: &Runtime) {
        let path = format!("/etl/etlJobs/jobs/{}", seg(&self.id.to_string()));
        if let Err(err) = rt
            .http
            .request_json(PROJECTS, Method::DELETE, &path, &[], None)
            .await
        {
            output::warn(&format!(
                "could not delete the temporary job {} ({err:#}); remove it with `gz projects delete-job {}`",
                self.name, self.id
            ));
        }
    }
}

/// The run's status from its own record until it is final. The log route
/// is left alone while the run is going.
async fn wait_for_run(rt: &Runtime, execution: &str) -> Result<String> {
    loop {
        let reply = rt
            .http
            .request_json(
                ETL,
                Method::POST,
                "/etl/jobStatus",
                &[],
                Some(json!({"jobIdentifier": execution})),
            )
            .await?;
        let status = reply["data"][0]["jobstatus"].as_str().unwrap_or_default();
        if FINAL_STATUSES.contains(&status) {
            return Ok(status.to_string());
        }
        tokio::time::sleep(STATUS_POLL).await;
    }
}

/// Every log line of a finished run.
async fn finished_run_log(rt: &Runtime, execution: &str) -> Result<Vec<String>> {
    let started = Instant::now();
    let mut cursor: Option<String> = None;
    let mut messages = Vec::new();
    loop {
        let mut body = json!({"jobIdentifier": execution});
        if let Some(cursor) = &cursor {
            body["cursor"] = json!(cursor);
        }
        let page = match rt
            .http
            .request_json(ETL, Method::POST, "/etl/logs", &[], Some(body))
            .await
        {
            Ok(page) => page,
            Err(err) if err.to_string().contains("HTTP 429") => {
                tokio::time::sleep(LOG_POLL).await;
                continue;
            }
            Err(err) => return Err(err.context(format!("cannot read the log of run {execution}"))),
        };
        messages.extend(
            page["events"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|e| e["message"].as_str().map(str::to_string)),
        );
        cursor = page["cursor"].as_str().map(str::to_string).or(cursor);
        if page["hasMoreLogs"] != Value::Bool(true) || started.elapsed() > LOG_WAIT {
            return Ok(messages);
        }
        tokio::time::sleep(LOG_POLL).await;
    }
}

fn no_report_message(outcome: &RunOutcome) -> String {
    let skip = outcome.messages.len().saturating_sub(LOG_TAIL);
    let tail: Vec<&str> = outcome.messages[skip..]
        .iter()
        .map(|m| m.trim_end())
        .collect();
    format!(
        "run {} ended {} without printing a report. Its last log lines:\n{}",
        outcome.execution,
        outcome.status,
        tail.join("\n")
    )
}

/// Eight hex digits, so concurrent checks never share a job or a marker.
fn short_id() -> String {
    crate::cmd::new_uuid().replace('-', "")[..8].to_string()
}

// ---------- output ----------

fn show(
    rt: &Runtime,
    checked: &LakehouseTable,
    results: Option<&LakehouseTable>,
    fail_on: FailOn,
    outcome: &RunOutcome,
    printed: &PrintedReport,
) -> Result<()> {
    let passed = printed.gate_failures.is_empty();
    if rt.format == OutputFormat::Table {
        println!(
            "{}",
            report::headline(&checked.to_string(), &printed.report)
        );
        rt.show_columns(&report::check_rows(&printed.report), &report::CHECK_COLUMNS)?;
        if let Some(results) = results {
            output::info(&format!("results appended to {results}"));
        }
    } else {
        output::print_value(
            &json!({
                "table": checked.to_string(),
                "run": outcome.execution,
                "failOn": fail_on.as_str(),
                "passed": passed,
                "gateFailures": printed.gate_failures,
                "resultsTable": results.map(ToString::to_string),
                "report": printed.report,
            }),
            rt.format,
            None,
        )?;
    }
    if passed {
        if rt.format == OutputFormat::Table {
            output::success(&format!("passed at --fail-on {}", fail_on.as_str()));
        }
        return Ok(());
    }
    output::warn(&format!(
        "failed at --fail-on {}: {}",
        fail_on.as_str(),
        printed.gate_failures.join(", ")
    ));
    Err(ReportedExit(GATE_FAILED_EXIT).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(id: i64, name: &str, warehouse: &str, database: Option<&str>) -> LakehouseConnection {
        LakehouseConnection {
            id,
            name: name.into(),
            warehouse: warehouse.into(),
            database: database.map(str::to_string),
        }
    }

    fn registry() -> Vec<LakehouseConnection> {
        vec![
            conn(1366, "GZLakehouseReal", "GZWarehouseREAL", Some("DemoDB")),
            conn(
                1327,
                "Manufacture",
                "GZWarehousePROD",
                Some("Manufacturing"),
            ),
            conn(1321, "GZLakehouseProd", "GZWarehousePROD", Some("DemoDB")),
            conn(1300, "OldProd", "gzwarehouseprod", None),
        ]
    }

    #[test]
    fn the_connection_of_the_tables_database_wins_then_the_oldest() {
        let list = registry();
        let metrics = LakehouseTable::parse("GZWarehousePROD.DemoDB.ProductionMetrics").unwrap();
        assert_eq!(
            pick_connection(&list, &metrics, None).unwrap().name,
            "GZLakehouseProd"
        );
        let other = LakehouseTable::parse("GZWarehousePROD.Ops.lines").unwrap();
        assert_eq!(
            pick_connection(&list, &other, None).unwrap().name,
            "OldProd"
        );
    }

    #[test]
    fn a_named_connection_must_point_at_the_tables_warehouse() {
        let list = registry();
        let metrics = LakehouseTable::parse("GZWarehousePROD.DemoDB.ProductionMetrics").unwrap();
        assert_eq!(
            pick_connection(&list, &metrics, Some("Manufacture"))
                .unwrap()
                .id,
            1327
        );
        let wrong = pick_connection(&list, &metrics, Some("GZLakehouseReal")).unwrap_err();
        assert!(wrong
            .to_string()
            .contains("points at warehouse GZWarehouseREAL"));
        assert!(pick_connection(&list, &metrics, Some("nope")).is_err());
    }

    #[test]
    fn a_warehouse_without_a_connection_says_so() {
        let table = LakehouseTable::parse("Elsewhere.sales.orders").unwrap();
        let err = pick_connection(&registry(), &table, None).unwrap_err();
        assert!(err
            .to_string()
            .contains("no lakehouse connection points at warehouse Elsewhere"));
    }

    #[test]
    fn a_run_without_a_report_shows_its_last_lines() {
        let outcome = RunOutcome {
            execution: "ab12".into(),
            status: "Failed".into(),
            messages: (1..=30).map(|n| format!("line {n}\n")).collect(),
        };
        let message = no_report_message(&outcome);
        assert!(message.starts_with("run ab12 ended Failed without printing a report"));
        assert!(message.contains("line 6\nline 7"));
        assert!(!message.contains("line 5\n"));
        assert!(message.ends_with("line 30"));
    }
}
