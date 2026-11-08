//! `gz jobs fix` / `gz fix`: from a failed job run to a prepared gz code
//! task. gz finds the run, gathers its evidence, puts the job's code in a
//! local folder and opens gz code there with a first message to diagnose,
//! patch, sync and rerun. gz code asks before every change and command.
//!
//! Read-only by construction (groundzero-etl-provider-api):
//! - history `GET /etl/consumption-metrics?jobId=…` only runs `fn_r_*`
//!   reads (app.py:1458, execution_observability.py:429);
//! - the run's record `POST /etl/jobStatus` reads `fn_r_jobExecutionStatus`
//!   (app.py:5081, app.py:5828);
//! - logs `POST /etl/logs` can mark a run Failed, but only while that record
//!   says `Queued`/`Running` (etl_logs/handler.py:148,
//!   etl_logs/stale_run.py:110). gz reads logs only after the record says
//!   the run is final, and never for a run the history lists as going.
//! - `GET /etl/jobs/{id}/current-run` settles stale runs too and is never
//!   called here.

pub mod evidence;
pub mod history;
pub mod prompt;

use anyhow::{anyhow, bail, Context, Result};
use reqwest::Method;
use serde_json::{json, Value};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::Duration;

use self::evidence::{Evidence, StatusRecord, LIMITS};
use self::history::{Chosen, NoRun, RunRow};
use self::prompt::{LocalFile, Rerun};
use crate::cli::FixArgs;
use crate::cmd::devloop::{self, PulledFile};
use crate::cmd::jobs::{self, JobRef};
use crate::cmd::Runtime;
use crate::output;

const SVC: &str = "etl";
const PROJECTS: &str = "etlprojects";
/// Runs per history page, and how many pages are searched at most.
const HISTORY_PAGE: usize = 50;
const MAX_HISTORY_PAGES: usize = 4;
/// The provider lists at most 366 days at once; the web app asks for 365.
const HISTORY_DAYS: i64 = 365;
const DAY_MS: i64 = 24 * 60 * 60 * 1000;
/// A finished run's log is read to its end, within these bounds.
const MAX_LOG_PAGES: usize = 200;
const MAX_LOG_LINES: usize = 100_000;
const LOG_THROTTLE_WAIT: Duration = Duration::from_secs(2);
const MAX_LOG_THROTTLES: usize = 15;

/// What to fix and where.
#[derive(Debug, Clone)]
pub struct FixRequest {
    /// Job id or name.
    pub job: String,
    pub project: Option<i64>,
    /// A run id; default: the newest failed run.
    pub run: Option<String>,
    /// Where the job file goes and gz code starts.
    pub dir: PathBuf,
    pub rerun: Rerun,
}

impl FixRequest {
    fn from_args(args: &FixArgs) -> Self {
        Self {
            job: args.job.clone(),
            project: args.project,
            run: args.run.clone(),
            dir: PathBuf::from(&args.dir),
            rerun: Rerun {
                compute: args.compute,
                rte: args.rte.clone(),
            },
        }
    }
}

pub async fn run(rt: &Runtime, args: &FixArgs) -> Result<()> {
    if !args.print && !std::io::stdin().is_terminal() {
        bail!("gz jobs fix opens gz code, which needs a terminal; pass --print to print the evidence and prompt instead");
    }
    let request = FixRequest::from_args(args);
    let prompt = prepare(rt, &request).await?;
    if args.print {
        println!("{prompt}");
        return Ok(());
    }
    launch(rt, &request.dir, prompt).await
}

/// The whole flow, for callers that already hold a terminal (the TUI).
pub async fn fix(rt: &Runtime, request: &FixRequest) -> Result<()> {
    let prompt = prepare(rt, request).await?;
    launch(rt, &request.dir, prompt).await
}

/// Shows the prepared message, then opens gz code with it in the input box.
async fn launch(rt: &Runtime, dir: &Path, prompt: String) -> Result<()> {
    eprintln!(
        "{}",
        output::dim("── gz code starts with this message in its input box: review or edit it, then press Enter ──")
    );
    println!("{prompt}\n");
    crate::cmd::code::run_with_draft(rt, dir, prompt).await
}

/// Finds the run, reads its evidence, makes the job file local and returns
/// the message for gz code. Logs are read only for a run that is over.
async fn prepare(rt: &Runtime, request: &FixRequest) -> Result<String> {
    let dir = &request.dir;
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let project = request.project.or_else(|| devloop::synced_project(dir));
    let (project, job) = resolve_job(rt, &request.job, project).await?;
    let Chosen { run, newer_success } =
        choose_run(rt, project, &job, request.run.as_deref()).await?;
    let record = run_record(rt, &run).await?;
    if !record.is_terminal() {
        bail!(
            "run {} of {} is {} by its own record; gz won't read the logs of a run that isn't over. Follow it: gz jobs logs --job {} --follow",
            run.execution_id, job.name, record.status, run.execution_id
        );
    }
    output::info(&format!(
        "run {} of {} is {}; reading its logs",
        run.execution_id, job.name, record.status
    ));
    let lines = read_logs(rt, &run).await?;
    let saved = devloop::saved_job(rt, project, job.id).await?;
    let file = local_file(dir, project, &job, &saved)?;
    let evidence = Evidence {
        project,
        job,
        run,
        record,
        log: evidence::excerpt(&lines, &LIMITS),
        config: saved["config"].clone(),
        newer_success,
    };
    Ok(prompt::build(&evidence, &file, &request.rerun))
}

/// The job by id or name: in `project`, or looked up across projects.
async fn resolve_job(rt: &Runtime, wanted: &str, project: Option<i64>) -> Result<(i64, JobRef)> {
    if let Some(project) = project {
        let job = devloop::find_job(&jobs::project_jobs(rt, project).await?, wanted).with_context(
            || format!("no job '{wanted}' in project {project}; see `gz projects jobs {project}`"),
        )?;
        return Ok((project, job));
    }
    let projects = rt
        .http
        .request_json(PROJECTS, Method::GET, "/etl/etlProjects", &[], None)
        .await?;
    let mut found = Vec::new();
    for id in projects
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p["Id"].as_i64())
    {
        if let Some(job) = devloop::find_job(&jobs::project_jobs(rt, id).await?, wanted) {
            found.push((id, job));
        }
    }
    match found.len() {
        0 => bail!("no job '{wanted}' in any project you can see; check `gz projects list`"),
        1 => Ok(found.remove(0)),
        _ => bail!(
            "job '{wanted}' is in several projects ({}); pass --project",
            found
                .iter()
                .map(|(p, j)| format!("project {p}: job {}", j.id))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Reads the job's history page by page until the run to fix is found.
async fn choose_run(
    rt: &Runtime,
    project: i64,
    job: &JobRef,
    wanted: Option<&str>,
) -> Result<Chosen> {
    let now = now_ms();
    let mut runs = Vec::new();
    let mut page = 0;
    loop {
        let query = history_query(job.id, now, page * HISTORY_PAGE);
        let reply = rt
            .http
            .request_json(SVC, Method::GET, "/etl/consumption-metrics", &query, None)
            .await?;
        let rows = history::rows(&reply);
        let full = rows.len() == HISTORY_PAGE;
        runs.extend(rows);
        page += 1;
        let decided = history::choose(&runs, wanted);
        let searching = matches!(&decided, Err(why) if matches!(**why, NoRun::NotFound(_) | NoRun::NoFailure { .. }));
        if searching && full && page < MAX_HISTORY_PAGES {
            continue;
        }
        let chosen =
            decided.map_err(|why| anyhow!(no_run_message(&why, project, job, runs.len())))?;
        if chosen.newer_success {
            output::warn(&format!(
                "a newer run of {} succeeded; run {} may already be fixed",
                job.name, chosen.run.execution_id
            ));
        }
        return Ok(chosen);
    }
}

/// `GET /etl/consumption-metrics` query for one page of a job's runs, as
/// the web app's run history asks (useJobRunHistory.js:46).
fn history_query(job: i64, now_ms: i64, offset: usize) -> Vec<(String, String)> {
    use crate::tui::format::iso8601_utc;
    [
        ("jobId", job.to_string()),
        ("workloadType", "job".to_string()),
        ("from", iso8601_utc(now_ms - HISTORY_DAYS * DAY_MS)),
        ("to", iso8601_utc(now_ms + 60_000)),
        ("limit", HISTORY_PAGE.to_string()),
        ("offset", offset.to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

fn no_run_message(why: &NoRun, project: i64, job: &JobRef, searched: usize) -> String {
    let follow = |run: &RunRow| format!("gz jobs logs --job {} --follow", run.execution_id);
    match why {
        NoRun::LatestActive { latest, failed } => {
            let mut text = format!(
                "the latest run {} of {} is still {}; gz won't read its logs while it runs. Follow it: {}",
                latest.execution_id,
                job.name,
                latest.state.label(),
                follow(latest)
            );
            if let Some(failed) = failed {
                text.push_str(&format!(
                    "\nor fix the earlier failure: gz jobs fix {} --project {project} --run {}",
                    job.id, failed.execution_id
                ));
            }
            text
        }
        NoRun::RequestedNotOver(run) => format!(
            "run {} of {} is {}; gz reads the logs of finished runs only. Wait for it to end ({})",
            run.execution_id,
            job.name,
            run.state.label(),
            follow(run)
        ),
        NoRun::NotFound(id) => format!(
            "run {id} isn't among the last {searched} runs of {} (job {}); check the run id",
            job.name, job.id
        ),
        NoRun::NoFailure { latest } => {
            let latest = latest.as_ref().map_or_else(
                || "it has no runs".to_string(),
                |run| format!("its latest run {} {}", run.execution_id, run.state.label()),
            );
            format!(
                "no failed run of {} in the last {HISTORY_DAYS} days ({latest}); pass --run to pick one",
                job.name
            )
        }
    }
}

/// The run's own record; its status is the one `/etl/logs` acts on.
async fn run_record(rt: &Runtime, run: &RunRow) -> Result<StatusRecord> {
    let reply = rt
        .http
        .request_json(
            SVC,
            Method::POST,
            "/etl/jobStatus",
            &[],
            Some(json!({"jobIdentifier": run.execution_id})),
        )
        .await?;
    StatusRecord::from_reply(&reply).with_context(|| {
        format!(
            "Groundzero has no status record for run {}; gz won't read its logs",
            run.execution_id
        )
    })
}

/// Every line of a finished run's log, page by page with the cursor, like
/// the web app's past-run log reader (runLogPager.js:46).
async fn read_logs(rt: &Runtime, run: &RunRow) -> Result<Vec<String>> {
    let mut body = json!({"jobIdentifier": run.execution_id});
    if let Some(session) = &run.session_id {
        body["sessionId"] = json!(session);
    }
    let mut lines = Vec::new();
    let mut throttled = 0;
    for _ in 0..MAX_LOG_PAGES {
        let page = match rt
            .http
            .request_json(SVC, Method::POST, "/etl/logs", &[], Some(body.clone()))
            .await
        {
            Ok(page) => page,
            Err(err) if err.to_string().contains("HTTP 429") && throttled < MAX_LOG_THROTTLES => {
                throttled += 1;
                tokio::time::sleep(LOG_THROTTLE_WAIT).await;
                continue;
            }
            Err(err) => return Err(err.context("cannot read the run's logs")),
        };
        lines.extend(
            page["events"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|event| event["message"].as_str())
                .flat_map(str::lines)
                .map(str::to_string),
        );
        if lines.len() >= MAX_LOG_LINES || log_ended(&page) {
            break;
        }
        match page["cursor"].as_str() {
            Some(cursor) => body["cursor"] = json!(cursor),
            None => break,
        }
    }
    Ok(lines)
}

/// The last page: nothing more to come, or nothing new and not mid-page
/// (a just-finished run reports more for a short grace period).
fn log_ended(page: &Value) -> bool {
    let empty = page["events"].as_array().is_none_or(Vec::is_empty);
    page["hasMoreLogs"] != Value::Bool(true) || (empty && !page["nextToken"].is_string())
}

/// Puts the job's saved code in `dir`, asking before replacing local edits.
fn local_file(dir: &Path, project: i64, job: &JobRef, saved: &Value) -> Result<LocalFile> {
    let code = saved["fileContent"]
        .as_str()
        .filter(|c| !c.trim().is_empty())
        .with_context(|| format!("job {} has no saved code to fix", job.name))?;
    let (rel, pulled) = devloop::pull_job_file(dir, project, &job.name, code, |rel| {
        replace_local_edits(dir, rel)
    })?;
    let shown = dir.join(&rel);
    let shown = shown.display();
    match pulled {
        PulledFile::Downloaded => output::info(&format!("{shown} ← {} (downloaded)", job.name)),
        PulledFile::Unchanged => output::info(&format!("{shown} is up to date with {}", job.name)),
        PulledFile::ReplacedLocalEdits => {
            output::info(&format!("{shown} ← {} (local edits replaced)", job.name))
        }
        PulledFile::KeptLocalEdits => {
            output::warn(&format!("{shown} has local edits; keeping them"))
        }
    }
    Ok(LocalFile {
        rel,
        has_local_edits: pulled == PulledFile::KeptLocalEdits,
    })
}

/// Asks whether the project's code should replace local edits; without a
/// terminal the edits are kept.
fn replace_local_edits(dir: &Path, rel: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        return Ok(false);
    }
    let question = format!(
        "{} has edits that aren't on Groundzero. Replace them with the project's code? [y/N] ",
        dir.join(rel).display()
    );
    let answer = crate::cmd::prompt_line(&question)?;
    Ok(matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes"))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_finished_runs_log_ends_when_nothing_more_comes() {
        let done = json!({"events": [{"message": "x"}], "hasMoreLogs": false});
        assert!(log_ended(&done));
        let grace = json!({"events": [], "hasMoreLogs": true, "cursor": "c"});
        assert!(
            log_ended(&grace),
            "an empty page past the end stops the read"
        );
        let more = json!({"events": [{"message": "x"}], "hasMoreLogs": true, "cursor": "c"});
        assert!(!log_ended(&more));
        let mid = json!({"events": [], "hasMoreLogs": true, "nextToken": "t"});
        assert!(!log_ended(&mid), "mid-page reads go on");
    }

    #[test]
    fn asks_for_a_years_history_newest_page_first() {
        let query = history_query(9, 400 * DAY_MS, 50);
        let get = |key: &str| {
            query
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("jobId"), Some("9"));
        assert_eq!(get("workloadType"), Some("job"));
        assert_eq!(get("from"), Some("1970-02-05T00:00:00Z"));
        assert_eq!(get("to"), Some("1971-02-05T00:01:00Z"));
        assert_eq!(get("offset"), Some("50"));
    }
}
