//! `gz quality watch`: a table's checks on a schedule, run on Groundzero so
//! the watch keeps going when this machine is off.
//!
//! A watch is a job plus a schedule, both named `gz-quality-watch-<table>-
//! <id>` ([`plan::watch_name`]), with the watch's own record in the job's
//! config (`gzQualityWatch`). Endpoints, as the web app's Scheduler calls
//! them (`src/pages/Utils/etlUtlis.js`, `schedulerEmailUtils.js`) and the
//! Schedule Lambda serves them (`amplify/backend/function/Schedule/src/src/
//! controllers/index.mjs`):
//! - `POST /schedule` `{name, cron, description, scheduleType, jobDetails:
//!   {projectId, projectName, jobId, jobName, computeId, rteName}, config}`
//!   → `{schedule: id}` (`ScheduleAddSchema`); each run is an ordinary ETL
//!   run of the job, with the job's lakehouse connection;
//! - `GET /schedule` (names), `GET /schedule/{id}` (`enabled`, `cron`),
//!   `GET /schedule/{id}/runs` (latest runs), `PUT /schedule/run/{id}/{name}`
//!   (run once now), `DELETE /schedule/{id}/{name}`;
//! - `GET /schedule/notificationTypes` → `[{name, notificationTypeId}]` and
//!   `POST|GET /schedule/{id}/email`, `DELETE /schedule/{id}/email/{notificationId}`:
//!   the failure email the runtime sends when a scheduled run fails
//!   (`jobManager/notificationManager.py` `sendJobCompletionNotification`).
//!
//! The job is the rules script with `failRun`: a failing check fails the
//! run, which is what sends that email. A run's report is read from its log
//! only once its own record says it is final (`POST /etl/jobStatus`).

mod plan;

use anyhow::{bail, Context, Result};
use reqwest::Method;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

use super::args::{FailOn, WatchAction, WatchArgs, WatchCreateArgs};
use super::check::{self, compute_id, TempJob};
use super::rules::file::QualityFile;
use super::rules::run::{self as rules_run, CheckSpec, ChecksRun, TableOutcome};
use super::rules::script::read_table_report;
use crate::cmd::apps::saved_job_path;
use crate::cmd::lakehouse_table::LakehouseTable;
use crate::cmd::run_wait::{self, parse_timeout};
use crate::cmd::{confirm, jobs, seg, Runtime};
use crate::output::{self, OutputFormat};
use plan::{WatchRecord, METADATA_FIELD, NAME_PREFIX};

const SCHEDULES: &str = "schedules";
const PROJECTS: &str = "etlprojects";
const FAILURE_TYPE: &str = "Failure";
const SCHEDULE_TYPE: &str = "BATCH";
const RUN_POLL: Duration = Duration::from_secs(3);
/// How long a run started with `--run-now` may take to appear.
const RUN_APPEAR_WAIT: Duration = Duration::from_secs(120);
const LIST_COLUMNS: [&str; 8] = [
    "id",
    "table",
    "every",
    "max age",
    "active",
    "last run",
    "last status",
    "project",
];
const RUN_COLUMNS: [&str; 4] = ["run", "status", "started", "stopped"];

pub async fn run(rt: &Runtime, args: &WatchArgs) -> Result<()> {
    match &args.action {
        None => create(rt, &args.create).await,
        Some(WatchAction::List) => list(rt).await,
        Some(WatchAction::Show { watch, wait }) => show(rt, watch, *wait).await,
        Some(WatchAction::Stop { watch, yes }) => stop(rt, watch, *yes).await,
    }
}

// ---------- finding watches ----------

/// A watch's schedule and job, and its record when the job still has it.
#[derive(Debug, Clone)]
struct Watch {
    schedule: i64,
    name: String,
    project: i64,
    project_name: String,
    job: i64,
    job_name: String,
    compute: Value,
    rte: String,
    cron: String,
    description: String,
    record: Option<WatchRecord>,
}

impl Watch {
    fn from_row(row: &Value) -> Option<Self> {
        let text = |field: &str| row[field].as_str().unwrap_or_default().to_string();
        let name = row["name"].as_str()?;
        if !name.starts_with(NAME_PREFIX) {
            return None;
        }
        Some(Self {
            schedule: row["Id"].as_i64()?,
            name: name.to_string(),
            project: row["projectId"].as_i64()?,
            project_name: text("projectName"),
            job: row["jobId"].as_i64()?,
            job_name: text("jobName"),
            compute: row["computeId"].clone(),
            rte: text("rteName"),
            cron: text("cron"),
            description: text("description"),
            record: None,
        })
    }

    fn table(&self) -> String {
        self.record
            .as_ref()
            .map_or_else(|| "?".to_string(), |r| r.table.clone())
    }

    /// The body `PUT /schedule/run` validates (`ScheduleUpdateSchema`); the
    /// run uses only `jobDetails`.
    fn run_body(&self) -> Value {
        json!({
            "cron": self.cron,
            "description": self.description,
            "scheduleType": SCHEDULE_TYPE,
            "jobDetails": {
                "projectId": self.project, "projectName": self.project_name,
                "jobId": self.job, "jobName": self.job_name,
                "computeId": self.compute, "rteName": self.rte,
            },
        })
    }

    fn named_path(&self, verb: &str) -> String {
        format!(
            "/schedule{verb}/{}/{}",
            seg(&self.schedule.to_string()),
            seg(&self.name)
        )
    }
}

/// Every watch the user can see, with its record read from its job.
async fn watches(rt: &Runtime) -> Result<Vec<Watch>> {
    let listed = rt
        .http
        .request_json(SCHEDULES, Method::GET, "/schedule", &[], None)
        .await
        .context("cannot list schedules")?;
    let mut found = Vec::new();
    for mut watch in listed
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Watch::from_row)
    {
        let path = saved_job_path(&watch.project.to_string(), &watch.job.to_string());
        match rt
            .http
            .request_json(PROJECTS, Method::GET, &path, &[], None)
            .await
        {
            Ok(saved) => watch.record = WatchRecord::from_config(&saved["config"]),
            Err(err) => output::warn(&format!(
                "cannot read job {} of watch {}: {err:#}",
                watch.job, watch.schedule
            )),
        }
        found.push(watch);
    }
    Ok(found)
}

/// The watch `wanted` names: a schedule id, a watch name or a table.
fn pick<'a>(watches: &'a [Watch], wanted: &str) -> Result<&'a Watch> {
    let table = LakehouseTable::parse(wanted).ok();
    let matches: Vec<&Watch> = watches
        .iter()
        .filter(|watch| {
            watch.schedule.to_string() == wanted
                || watch.name == wanted
                || table.as_ref().is_some_and(|table| {
                    watch.record.as_ref().is_some_and(|r| {
                        LakehouseTable::parse(&r.table).is_ok_and(|t| t.key() == table.key())
                    })
                })
        })
        .collect();
    match matches.as_slice() {
        [one] => Ok(one),
        [] => bail!("no watch matches {wanted}; `gz quality watch list` shows them"),
        many => bail!(
            "{wanted} matches {} watches ({}); name one by its id",
            many.len(),
            many.iter()
                .map(|w| w.schedule.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

async fn find(rt: &Runtime, wanted: &str) -> Result<Watch> {
    let all = watches(rt).await?;
    pick(&all, wanted).cloned()
}

// ---------- create ----------

async fn create(rt: &Runtime, args: &WatchCreateArgs) -> Result<()> {
    let (Some(table), Some(project), Some(every)) = (&args.table, args.project, &args.every) else {
        bail!("name the table, --project and --every, or use `gz quality watch list|show|stop`");
    };
    let table = LakehouseTable::parse(table)?;
    check_runtime(&args.rte)?;
    let now = now_secs();
    let cron = plan::every_cron(every, now)?;
    let spec = watch_spec(&table, args)?;
    if let Some(existing) = watches(rt).await?.iter().find(|w| {
        w.record
            .as_ref()
            .is_some_and(|r| same_table(&r.table, &table))
    }) {
        bail!(
            "{table} is already watched ({} {}); stop it first: gz quality watch stop {}",
            existing.schedule,
            existing.name,
            existing.schedule
        );
    }
    let failure_type = if args.notify_email.is_empty() {
        None
    } else {
        Some(failure_type_id(rt).await?)
    };
    let id = check::short_id();
    let name = plan::watch_name(&table, &id);
    let marker = format!("GZW-{id}");
    let record = WatchRecord {
        table: table.to_string(),
        every: every.clone(),
        cron: cron.clone(),
        max_age: args.max_age.clone(),
        rules_file: args.rules.as_ref().map(|p| p.display().to_string()),
        fail_on: spec.fail_on.as_str().to_string(),
        marker: marker.clone(),
        notify_email: args.notify_email.clone(),
    };
    let compute = compute_id(rt, &args.compute).await?;
    let planned = rules_run::plan(rt, &[spec], args.connection.as_deref(), marker, true).await?;
    let job = TempJob::create_named(
        rt,
        project,
        name.clone(),
        &planned.code,
        &planned.connections,
        Some(json!({ METADATA_FIELD: record.to_json() })),
    )
    .await?;
    let project_name = jobs::project_name(rt, project).await?;
    let mut watch = Watch {
        schedule: 0,
        name: name.clone(),
        project,
        project_name,
        job: job.id,
        job_name: name.clone(),
        compute: json!(compute),
        rte: args.rte.clone(),
        cron: cron.clone(),
        description: record.description(),
        record: Some(record.clone()),
    };
    watch.schedule = match add_schedule(rt, &watch, &record).await {
        Ok(id) => id,
        Err(err) => {
            job.delete(rt).await;
            return Err(err);
        }
    };
    if let Some(type_id) = failure_type {
        if let Err(err) = add_failure_email(rt, &watch, type_id, args).await {
            remove_schedule(rt, &watch).await.ok();
            job.delete(rt).await;
            return Err(err.context("the watch was not made"));
        }
    }
    let started = if args.run_now {
        Some(trigger_run(rt, &watch).await?)
    } else {
        None
    };
    if rt.format != OutputFormat::Table {
        return rt
            .show(json!({
                "id": watch.schedule,
                "name": watch.name,
                "project": project,
                "job": job.id,
                "cron": cron,
                "watch": record.to_json(),
                "run": started.flatten(),
            }))
            .await;
    }
    output::success(&format!(
        "watching {table} every {every} (schedule {} {}, cron {cron} UTC; job {} in project {project})",
        watch.schedule, watch.name, job.id
    ));
    if !args.notify_email.is_empty() {
        output::info(&format!(
            "a failing run emails {}",
            args.notify_email.join(", ")
        ));
    }
    match started {
        Some(Some(execution)) => output::info(&format!(
            "run {execution} started; see it with: gz quality watch show {} --wait",
            watch.schedule
        )),
        Some(None) => output::warn(&format!(
            "the run started but has not shown up yet; see it with: gz quality watch show {}",
            watch.schedule
        )),
        None => output::info(&format!(
            "see it with: gz quality watch show {}; stop it with: gz quality watch stop {}",
            watch.schedule, watch.schedule
        )),
    }
    Ok(())
}

fn same_table(recorded: &str, table: &LakehouseTable) -> bool {
    LakehouseTable::parse(recorded).is_ok_and(|t| t.key() == table.key())
}

/// The Schedule API takes `gz-rte-<name>:<x.y.z>` images, and the checks
/// need Spark.
fn check_runtime(rte: &str) -> Result<()> {
    let Some((image, version)) = rte.split_once(':') else {
        bail!("--rte {rte} needs a version, e.g. gz-rte-spark-sklearn:1.0.0");
    };
    let image_ok = image.strip_prefix("gz-rte-").is_some_and(|rest| {
        !rest.is_empty() && rest.chars().all(|c| c.is_ascii_alphabetic() || c == '-')
    });
    let version_ok = version.split('.').count() == 3
        && version
            .split('.')
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()));
    if !image_ok || !version_ok {
        bail!("--rte {rte} can't be scheduled; use an image like gz-rte-spark-sklearn:1.0.0 (see `gz rte list`)");
    }
    if !image.contains("spark") {
        bail!("--rte {rte} is not a Spark runtime; the checks need one, e.g. gz-rte-spark-sklearn:1.0.0");
    }
    Ok(())
}

/// The checks a watch runs: the file's rules for the table, or the default
/// checks, plus the last-commit age from `--max-age`.
fn watch_spec(table: &LakehouseTable, args: &WatchCreateArgs) -> Result<CheckSpec> {
    let mut spec = match &args.rules {
        Some(path) => {
            let file = QualityFile::load(path)?;
            let rules = file
                .rules_for(table)
                .with_context(|| format!("{} declares no rules for {table}", path.display()))?;
            CheckSpec::from_rules(rules, None)
        }
        None => CheckSpec {
            table: table.clone(),
            branch: None,
            fail_on: args.fail_on.unwrap_or(FailOn::Error),
            config: None,
            checks: Vec::new(),
            fingerprint: None,
            results: None,
        },
    };
    if let Some(age) = &args.max_age {
        let hours = parse_timeout(age)
            .map_err(|why| anyhow::anyhow!("--max-age {age}: {why}"))?
            .as_secs_f64()
            / 3600.0;
        spec.checks.retain(|c| c["check"] != "commit_age");
        spec.checks
            .push(json!({"check": "commit_age", "maxHours": hours}));
    }
    Ok(spec)
}

async fn failure_type_id(rt: &Runtime) -> Result<i64> {
    let types = rt
        .http
        .request_json(
            SCHEDULES,
            Method::GET,
            "/schedule/notificationTypes",
            &[],
            None,
        )
        .await
        .context("cannot read the schedule notification types")?;
    types
        .as_array()
        .into_iter()
        .flatten()
        .find(|t| t["name"] == FAILURE_TYPE)
        .and_then(|t| t["notificationTypeId"].as_i64())
        .context("the Schedule API has no Failure notification type")
}

async fn add_schedule(rt: &Runtime, watch: &Watch, record: &WatchRecord) -> Result<i64> {
    let mut body = watch.run_body();
    body["name"] = json!(watch.name);
    body["config"] = json!({ METADATA_FIELD: record.to_json() });
    let reply = rt
        .http
        .request_json(SCHEDULES, Method::POST, "/schedule", &[], Some(body))
        .await
        .context("cannot create the watch's schedule")?;
    reply["schedule"]
        .as_i64()
        .context("the Schedule API made the schedule but sent no id")
}

async fn add_failure_email(
    rt: &Runtime,
    watch: &Watch,
    failure_type: i64,
    args: &WatchCreateArgs,
) -> Result<()> {
    let from = args
        .from_email
        .clone()
        .unwrap_or_else(|| args.notify_email[0].clone());
    let body = plan::failure_email(
        failure_type,
        &from,
        &args.notify_email,
        &watch.table(),
        &watch.schedule.to_string(),
    )?;
    let path = format!("/schedule/{}/email", watch.schedule);
    rt.http
        .request_json(SCHEDULES, Method::POST, &path, &[], Some(body))
        .await
        .context("cannot set the failure email")?;
    Ok(())
}

/// Runs the watch once now; the run's id once it shows up.
async fn trigger_run(rt: &Runtime, watch: &Watch) -> Result<Option<String>> {
    let known: Vec<String> = recent_runs(rt, watch)
        .await?
        .iter()
        .filter_map(|r| r["executionId"].as_str().map(str::to_string))
        .collect();
    rt.http
        .request_json(
            SCHEDULES,
            Method::PUT,
            &watch.named_path("/run"),
            &[],
            Some(watch.run_body()),
        )
        .await
        .context("cannot run the watch now")?;
    let started = Instant::now();
    while started.elapsed() < RUN_APPEAR_WAIT {
        tokio::time::sleep(RUN_POLL).await;
        let fresh = recent_runs(rt, watch).await?.into_iter().find_map(|r| {
            r["executionId"]
                .as_str()
                .filter(|id| !known.iter().any(|k| k == id))
                .map(str::to_string)
        });
        if fresh.is_some() {
            return Ok(fresh);
        }
    }
    Ok(None)
}

// ---------- list / show / stop ----------

/// The schedule's latest runs, newest first.
async fn recent_runs(rt: &Runtime, watch: &Watch) -> Result<Vec<Value>> {
    let path = format!("/schedule/{}/runs", watch.schedule);
    let reply = rt
        .http
        .request_json(SCHEDULES, Method::GET, &path, &[], None)
        .await
        .with_context(|| format!("cannot read the runs of watch {}", watch.schedule))?;
    let mut runs: Vec<Value> = reply.as_array().cloned().unwrap_or_default();
    runs.sort_by(|a, b| {
        let at = |run: &Value| run["startTime"].as_str().unwrap_or_default().to_string();
        at(b).cmp(&at(a))
    });
    Ok(runs)
}

async fn schedule_details(rt: &Runtime, watch: &Watch) -> Result<Value> {
    let path = format!("/schedule/{}", watch.schedule);
    rt.http
        .request_json(SCHEDULES, Method::GET, &path, &[], None)
        .await
        .with_context(|| format!("cannot read schedule {}", watch.schedule))
}

async fn list(rt: &Runtime) -> Result<()> {
    let mut rows = Vec::new();
    for watch in watches(rt).await? {
        let details = schedule_details(rt, &watch).await?;
        let runs = recent_runs(rt, &watch).await?;
        let latest = runs.first();
        let record = watch.record.as_ref();
        rows.push(json!({
            "id": watch.schedule,
            "name": watch.name,
            "table": watch.table(),
            "every": record.map(|r| r.every.clone()),
            "max age": record.and_then(|r| r.max_age.clone()),
            "active": details["enabled"],
            "last run": latest.and_then(|r| r["startTime"].as_str()),
            "last status": latest.and_then(|r| r["status"].as_str()),
            "project": watch.project,
            "job": watch.job,
            "cron": watch.cron,
            "notifyEmail": record.map(|r| r.notify_email.clone()),
        }));
    }
    if rows.is_empty() && rt.format == OutputFormat::Table {
        output::info("no watches; `gz quality watch <table> --project <id> --every 1h` makes one");
        return Ok(());
    }
    rt.show_columns(&Value::Array(rows), &LIST_COLUMNS)
}

async fn show(rt: &Runtime, wanted: &str, wait: bool) -> Result<()> {
    let watch = find(rt, wanted).await?;
    let details = schedule_details(rt, &watch).await?;
    let emails = rt
        .http
        .request_json(
            SCHEDULES,
            Method::GET,
            &format!("/schedule/{}/email", watch.schedule),
            &[],
            None,
        )
        .await
        .with_context(|| format!("cannot read the emails of watch {}", watch.schedule))?;
    let mut runs = recent_runs(rt, &watch).await?;
    let latest_id = runs
        .first()
        .and_then(|r| r["executionId"].as_str())
        .map(str::to_string);
    let mut latest_status = match &latest_id {
        Some(id) => run_wait::read_status(rt, id).await?,
        None => None,
    };
    if let (Some(id), true) = (&latest_id, wait) {
        if !run_wait::is_final(latest_status.as_deref()) {
            if rt.format == OutputFormat::Table {
                output::info(&format!("waiting for run {id} to end…"));
            }
            latest_status = Some(run_wait::wait_until_final(rt, id).await?);
            runs = recent_runs(rt, &watch).await?;
        }
    }
    let outcome = match (&latest_id, &watch.record) {
        (Some(id), Some(record)) if run_wait::is_final(latest_status.as_deref()) => {
            Some(finished_outcome(rt, id, latest_status.as_deref(), record).await?)
        }
        _ => None,
    };
    if rt.format != OutputFormat::Table {
        rt.show(json!({
            "id": watch.schedule,
            "name": watch.name,
            "project": watch.project,
            "job": watch.job,
            "cron": watch.cron,
            "enabled": details["enabled"],
            "watch": watch.record.as_ref().map(WatchRecord::to_json),
            "emails": emails,
            "runs": runs,
            "latest": outcome.as_ref().map(super::rules::outcomes_json),
        }))
        .await?;
        return outcome.map_or(Ok(()), |outcome| outcome.exit());
    }
    print_watch(&watch, &details, &emails);
    let rows: Vec<Value> = runs
        .iter()
        .map(|r| {
            json!({
                "run": r["executionId"], "status": r["status"],
                "started": r["startTime"], "stopped": r["stopTime"],
            })
        })
        .collect();
    if rows.is_empty() {
        output::info("no runs yet");
        return Ok(());
    }
    rt.show_columns(&Value::Array(rows), &RUN_COLUMNS)?;
    match (&outcome, &latest_id) {
        (Some(outcome), _) => {
            super::rules::show_outcomes(rt, outcome)?;
            return outcome.exit();
        }
        (None, Some(id)) => output::info(&format!(
            "run {id} is {}; `gz quality watch show {} --wait` waits for its checks",
            latest_status.as_deref().unwrap_or("starting"),
            watch.schedule
        )),
        (None, None) => {}
    }
    Ok(())
}

fn print_watch(watch: &Watch, details: &Value, emails: &Value) {
    let enabled = if details["enabled"] == json!(false) {
        "paused"
    } else {
        "active"
    };
    println!("watch {} {} ({enabled})", watch.schedule, watch.name);
    if let Some(record) = &watch.record {
        println!("  {}", record.description());
        if let Some(file) = &record.rules_file {
            println!("  rules from {file}, failing at {}", record.fail_on);
        }
    }
    println!(
        "  cron {} UTC; job {} in project {}",
        watch.cron, watch.job, watch.project
    );
    let failure_to: Vec<&str> = emails
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| e["type"] == FAILURE_TYPE)
        .filter_map(|e| e["toEmail"].as_str())
        .collect();
    if !failure_to.is_empty() {
        println!("  a failing run emails {}", failure_to.join(", "));
    }
    println!();
}

/// The checks of a finished run, read from its log.
async fn finished_outcome(
    rt: &Runtime,
    execution: &str,
    status: Option<&str>,
    record: &WatchRecord,
) -> Result<ChecksRun> {
    let messages = check::finished_run_log(rt, execution).await?;
    let report = read_table_report(messages.iter().map(String::as_str), &record.marker, 1)?;
    let report = report.unwrap_or_else(|| {
        super::rules::script::TableReport::NotChecked(format!(
            "run {execution} ended {} without a report; `gz jobs logs --job {execution}` shows why",
            status.unwrap_or("?")
        ))
    });
    let fail_on = if record.fail_on == FailOn::Warn.as_str() {
        FailOn::Warn
    } else {
        FailOn::Error
    };
    Ok(ChecksRun {
        execution: execution.to_string(),
        tables: vec![TableOutcome {
            table: LakehouseTable::parse(&record.table)?,
            fail_on,
            report,
        }],
    })
}

async fn stop(rt: &Runtime, wanted: &str, yes: bool) -> Result<()> {
    let watch = find(rt, wanted).await?;
    let runs = recent_runs(rt, &watch).await?;
    if let Some(id) = runs.first().and_then(|r| r["executionId"].as_str()) {
        let status = run_wait::read_status(rt, id).await?;
        if status.is_some() && !run_wait::is_final(status.as_deref()) {
            bail!(
                "run {id} of watch {} is still going and needs its job; wait for it (`gz quality watch show {} --wait`), then stop the watch",
                watch.schedule,
                watch.schedule
            );
        }
    }
    confirm(
        yes,
        &format!(
            "Stop watch {} of {}? Its schedule, failure email and job are deleted",
            watch.schedule,
            watch.table()
        ),
    )?;
    remove_schedule(rt, &watch).await?;
    let job_path = format!("/etl/etlJobs/jobs/{}", seg(&watch.job.to_string()));
    rt.http
        .request_json(PROJECTS, Method::DELETE, &job_path, &[], None)
        .await
        .with_context(|| {
            format!(
                "the schedule is gone, but job {} could not be deleted; remove it with `gz projects delete-job {}`",
                watch.job, watch.job
            )
        })?;
    if rt.format != OutputFormat::Table {
        return rt
            .show(json!({"id": watch.schedule, "name": watch.name, "job": watch.job, "stopped": true}))
            .await;
    }
    output::success(&format!(
        "stopped watch {} of {}: schedule, failure email and job {} deleted",
        watch.schedule,
        watch.table(),
        watch.job
    ));
    Ok(())
}

/// Deletes the schedule's emails, then the schedule.
async fn remove_schedule(rt: &Runtime, watch: &Watch) -> Result<()> {
    let emails_path = format!("/schedule/{}/email", watch.schedule);
    let emails = rt
        .http
        .request_json(SCHEDULES, Method::GET, &emails_path, &[], None)
        .await
        .with_context(|| format!("cannot read the emails of watch {}", watch.schedule))?;
    for id in emails
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e["notificationId"].as_i64())
    {
        rt.http
            .request_json(
                SCHEDULES,
                Method::DELETE,
                &format!("{emails_path}/{id}"),
                &[],
                None,
            )
            .await
            .with_context(|| format!("cannot delete email {id} of watch {}", watch.schedule))?;
    }
    rt.http
        .request_json(SCHEDULES, Method::DELETE, &watch.named_path(""), &[], None)
        .await
        .with_context(|| format!("cannot delete schedule {}", watch.schedule))?;
    Ok(())
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: i64, name: &str) -> Value {
        json!({"Id": id, "name": name, "projectId": 1999, "projectName": "P", "jobId": id + 1,
               "jobName": name, "computeId": 1000, "rteName": "gz-rte-spark-sklearn:1.0.0",
               "cron": "cron(0/15 * * * ? *)", "description": ""})
    }

    fn watched(id: i64, table: &str) -> Watch {
        let mut watch = Watch::from_row(&row(id, &format!("{NAME_PREFIX}x-{id}"))).unwrap();
        watch.record = Some(WatchRecord {
            table: table.into(),
            every: "15m".into(),
            cron: watch.cron.clone(),
            max_age: None,
            rules_file: None,
            fail_on: "error".into(),
            marker: "GZW-x".into(),
            notify_email: Vec::new(),
        });
        watch
    }

    #[test]
    fn only_schedules_named_as_watches_are_watches() {
        assert!(Watch::from_row(&row(1, "nightly-load")).is_none());
        let watch = Watch::from_row(&row(2, "gz-quality-watch-w-d-t-1a2b3c4d")).unwrap();
        assert_eq!(watch.schedule, 2);
        assert_eq!(watch.job, 3);
        assert_eq!(
            watch.named_path("/run"),
            "/schedule/run/2/gz-quality-watch-w-d-t-1a2b3c4d"
        );
        assert_eq!(watch.run_body()["jobDetails"]["computeId"], 1000);
        assert_eq!(watch.run_body()["scheduleType"], "BATCH");
    }

    #[test]
    fn watches_are_picked_by_id_name_or_table() {
        let all = vec![
            watched(10, "Lineage.sales.orders"),
            watched(20, "Lineage.sales.returns"),
        ];
        assert_eq!(pick(&all, "20").unwrap().schedule, 20);
        assert_eq!(pick(&all, "lineage.SALES.orders").unwrap().schedule, 10);
        assert_eq!(pick(&all, &all[1].name).unwrap().schedule, 20);
        assert!(pick(&all, "Lineage.sales.other")
            .unwrap_err()
            .to_string()
            .contains("no watch matches"));
        let twice = vec![watched(10, "w.d.t"), watched(11, "W.D.T")];
        assert!(pick(&twice, "w.d.t")
            .unwrap_err()
            .to_string()
            .contains("10, 11"));
    }

    #[test]
    fn only_versioned_spark_images_can_be_scheduled() {
        assert!(check_runtime("gz-rte-spark-sklearn:1.0.0").is_ok());
        assert!(check_runtime("gz-rte-duckdb-sklearn:1.0.0")
            .unwrap_err()
            .to_string()
            .contains("not a Spark"));
        assert!(check_runtime("gz-rte-spark-sklearn").is_err());
        assert!(check_runtime("demo-rte-custom:spark-1").is_err());
    }

    fn create_args(rules: Option<&str>, max_age: Option<&str>) -> WatchCreateArgs {
        WatchCreateArgs {
            table: Some("w.d.t".into()),
            project: Some(1999),
            every: Some("1h".into()),
            max_age: max_age.map(str::to_string),
            rules: rules.map(Into::into),
            fail_on: None,
            notify_email: Vec::new(),
            from_email: None,
            compute: "small".into(),
            rte: "gz-rte-spark-sklearn:1.0.0".into(),
            connection: None,
            run_now: false,
        }
    }

    #[test]
    fn without_rules_the_default_checks_run_with_the_commit_age() {
        let table = LakehouseTable::parse("w.d.t").unwrap();
        let spec = watch_spec(&table, &create_args(None, Some("24h"))).unwrap();
        assert_eq!(spec.config, None);
        assert_eq!(
            spec.checks,
            vec![json!({"check": "commit_age", "maxHours": 24.0})]
        );
        assert_eq!(spec.branch, None);
    }

    #[test]
    fn max_age_replaces_the_files_commit_age() {
        let dir = std::env::temp_dir().join(format!("gz-watch-rules-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("quality.toml");
        std::fs::write(
            &path,
            "[tables.\"w.d.t\"]\nrows = { min = 1 }\nmax_commit_age = \"2d\"\n",
        )
        .unwrap();
        let table = LakehouseTable::parse("W.D.T").unwrap();
        let spec = watch_spec(&table, &create_args(path.to_str(), Some("6h"))).unwrap();
        let ages: Vec<&Value> = spec
            .checks
            .iter()
            .filter(|c| c["check"] == "commit_age")
            .collect();
        assert_eq!(ages, [&json!({"check": "commit_age", "maxHours": 6.0})]);
        assert!(spec.config.is_some());
        let other = LakehouseTable::parse("w.d.other").unwrap();
        assert!(watch_spec(&other, &create_args(path.to_str(), None)).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
