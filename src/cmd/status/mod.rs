//! `gz status`: one screen of what needs attention. Job runs that failed
//! since you last looked (with the reason the run recorded), runs still in
//! progress, approvals waiting for you, compute left running, AI spend
//! against your budget, and whether a newer gz exists.
//!
//! Every request runs at once, and each section stands alone: one that
//! fails is reported on its own line and the rest still show. The TUI home
//! dashboard reads the same endpoints; its parsers (`views::home::tiles`)
//! are reused here.
//!
//! Endpoints, checked against the backends and the web app:
//! - failed and open runs: `GET /etl/consumption-metrics` (etl provider
//!   app.py:1524; web app Utils/etlUtlis.js:2473), the run's recorded
//!   reason from `POST /etl/jobStatus` (app.py:5543, `data[0].output`).
//! - agent versions to review: `GET /v1/approval-requests?status=…`
//!   (agent builder api.py:1003, `status` is the alias of approval_status).
//! - paused agent runs you may answer: `GET /v1/tool-approvals`
//!   (api.py:955; web app Utils/agentUtils.js:203), `{total, items}`.
//! - paused chats: `pendingApproval` on `GET /chat/conversations/{id}`
//!   (gz-chat-eks api/routes/conversations.py:864). The chat service has
//!   no list of them, so the newest few conversations are read.
//! - AI spend: `GET /chat/metrics?from=` (gz-chat-eks routes/budgets.py:185).

mod last_look;
mod render;
pub mod sessions;

use std::time::Duration;

use anyhow::Result;
use clap::Args;
use reqwest::Method;
use serde_json::{json, Value};

use crate::cmd::job_fix::evidence::StatusRecord;
use crate::cmd::job_fix::now_ms;
use crate::cmd::{seg, update, window, Runtime};
use crate::http::{readable_error, ApiClient};
use crate::impact::lookup::parallel;
use crate::output::OutputFormat;
use crate::tui::chat::parse_conversations;
use crate::tui::format::iso8601_utc;
use crate::tui::views::home::tiles::{self, Approval, JobRun, Runs};
use last_look::LastLook;
use sessions::{NotebookScan, SqlSession};

const ETL: &str = "etl";
const AGENTS: &str = "agents";
const CHAT: &str = "chat";
const CONSUMPTION: &str = "/etl/consumption-metrics";
/// The window used the first time, before there is a last look.
const FIRST_LOOK_MS: i64 = 24 * 60 * 60 * 1000;
/// The server's page limit for consumption metrics.
const RUNS_PAGE: usize = 100;
/// Failed runs whose recorded reason is looked up (one request each).
const REASONS_LOOKED_UP: usize = 10;
/// Open runs overlap any window, so a short one returns them all.
const OPEN_RUNS_WINDOW_MS: i64 = 60 * 1000;
/// Paused agent runs asked for; the answer carries the full total.
const PAUSED_RUNS_PAGE: usize = 50;
/// Chats read for a pending approval (one request each).
pub const CHATS_CHECKED: usize = 5;
/// A slow release lookup must not hold up the rest.
const RELEASE_TIMEOUT: Duration = Duration::from_secs(5);
const REASON_CHARS: usize = 160;

#[derive(Args, Debug)]
#[command(
    about = "What needs attention: failed and running jobs, approvals, running compute, AI spend"
)]
pub struct StatusArgs {
    #[arg(
        long,
        help = "Show failures since this age or UTC time (30m, 6h, 7d, 2w, 2026-11-01T08:00) instead of your last look; doesn't move the last look"
    )]
    pub since: Option<String>,
}

/// One section's data, or why it could not be had.
pub type Section<T> = Result<T, String>;

/// Where the failure window starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Since {
    LastLook(i64),
    Flag(i64),
    FirstLook(i64),
}

impl Since {
    pub fn at_ms(self) -> i64 {
        match self {
            Since::LastLook(ms) | Since::Flag(ms) | Since::FirstLook(ms) => ms,
        }
    }
}

/// What the run recorded about why it failed.
#[derive(Debug, Clone, PartialEq)]
pub enum Reason {
    Recorded(String),
    NotRecorded,
    Unavailable(String),
    /// Beyond the runs whose reason is looked up.
    NotAsked,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FailedRun {
    pub run: JobRun,
    pub reason: Reason,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FailedRuns {
    pub runs: Vec<FailedRun>,
    /// Every failure in the window, more than one page may hold.
    pub total: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PausedRuns {
    pub items: Vec<Value>,
    pub total: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatPending {
    pub conversation_id: String,
    pub name: String,
    /// The conversation's `pendingApproval`.
    pub approval: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatApprovals {
    pub checked: usize,
    pub pending: Vec<ChatPending>,
}

/// Everything `gz status` shows.
#[derive(Debug, Clone)]
pub struct Report {
    pub site: Option<String>,
    pub since: Since,
    pub now_ms: i64,
    pub failed: Section<FailedRuns>,
    pub open: Section<Runs>,
    pub agent_versions: Section<Vec<Approval>>,
    pub paused_agent_runs: Section<PausedRuns>,
    pub chats: Section<ChatApprovals>,
    pub sql_session: Section<Option<SqlSession>>,
    pub notebooks: Section<NotebookScan>,
    /// `/chat/metrics` for this month.
    pub usage: Section<Value>,
    /// The newest release's tag.
    pub latest_release: Section<String>,
}

pub async fn run(rt: &Runtime, args: &StatusArgs) -> Result<()> {
    let now = now_ms();
    let resolved = rt.http.resolved();
    let last_look = LastLook::open(&resolved.profile_name)?;
    let since = match &args.since {
        Some(text) => Since::Flag(window::parse_since(text, now)?),
        None => match last_look.get()? {
            Some(at) => Since::LastLook(at),
            None => Since::FirstLook(now - FIRST_LOOK_MS),
        },
    };
    let report = gather(&rt.http, since, now).await;
    if rt.format == OutputFormat::Table {
        for line in render::panel(&report).plain_lines() {
            println!("{line}");
        }
    } else {
        rt.show(render::to_json(&report)).await?;
    }
    // A failed lookup must not move the mark past failures never shown.
    if args.since.is_none() && report.failed.is_ok() {
        last_look.set(now)?;
    }
    Ok(())
}

/// Asks every endpoint at once.
pub async fn gather(http: &ApiClient, since: Since, now_ms: i64) -> Report {
    let (
        failed,
        open,
        agent_versions,
        paused_agent_runs,
        chats,
        sql_session,
        notebooks,
        usage,
        latest,
    ) = tokio::join!(
        failed_runs(http, since.at_ms(), now_ms),
        open_runs(http, now_ms),
        agent_versions(http),
        paused_agent_runs(http),
        chat_approvals(http),
        sessions::kept_sql_session(http),
        sessions::running_notebooks(http),
        month_usage(http, now_ms),
        update::latest_release(RELEASE_TIMEOUT),
    );
    Report {
        site: http.resolved().site(),
        since,
        now_ms,
        failed: section(failed),
        open: section(open),
        agent_versions: section(agent_versions),
        paused_agent_runs: section(paused_agent_runs),
        chats: section(chats),
        sql_session: section(sql_session),
        notebooks: section(notebooks),
        usage: section(usage),
        latest_release: section(latest),
    }
}

fn section<T>(result: Result<T>) -> Section<T> {
    result.map_err(|err| readable_error(&format!("{err:#}")))
}

fn runs_query(pairs: &[(&str, String)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(key, value)| (key.to_string(), value.clone()))
        .collect()
}

async fn failed_runs(http: &ApiClient, since_ms: i64, now_ms: i64) -> Result<FailedRuns> {
    let query = runs_query(&[
        ("status", "failed".into()),
        ("workloadType", "job".into()),
        ("from", iso8601_utc(since_ms)),
        ("to", iso8601_utc(now_ms)),
        ("limit", RUNS_PAGE.to_string()),
    ]);
    let answer = http
        .request_json(ETL, Method::GET, CONSUMPTION, &query, None)
        .await?;
    let failed = tiles::runs(&answer, now_ms).failed;
    let total = answer["pagination"]["total"]
        .as_u64()
        .unwrap_or(failed.len() as u64);
    let asks: Vec<_> = failed
        .iter()
        .take(REASONS_LOOKED_UP)
        .map(|run| recorded_reason(http.clone(), execution_id(&run.row)))
        .collect();
    let mut reasons = parallel(asks).await?.into_iter();
    let runs = failed
        .into_iter()
        .map(|run| FailedRun {
            reason: reasons.next().unwrap_or(Reason::NotAsked),
            run,
        })
        .collect();
    Ok(FailedRuns { runs, total })
}

pub fn execution_id(row: &Value) -> String {
    row["executionId"].as_str().unwrap_or_default().to_string()
}

/// The `output` the run's status record holds, its first line.
async fn recorded_reason(http: ApiClient, execution: String) -> Reason {
    let body = json!({"jobIdentifier": execution});
    match http
        .request_json(ETL, Method::POST, "/etl/jobStatus", &[], Some(body))
        .await
    {
        Ok(reply) => reason_of(&reply),
        Err(err) => Reason::Unavailable(readable_error(&format!("{err:#}"))),
    }
}

fn reason_of(reply: &Value) -> Reason {
    StatusRecord::from_reply(reply)
        .and_then(|record| record.output)
        .and_then(|output| {
            output
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .map(|line| line.chars().take(REASON_CHARS).collect())
        })
        .map_or(Reason::NotRecorded, Reason::Recorded)
}

async fn open_runs(http: &ApiClient, now_ms: i64) -> Result<Runs> {
    let query = runs_query(&[
        ("workloadType", "job".into()),
        ("from", iso8601_utc(now_ms - OPEN_RUNS_WINDOW_MS)),
        ("limit", RUNS_PAGE.to_string()),
    ]);
    let answer = http
        .request_json(ETL, Method::GET, CONSUMPTION, &query, None)
        .await?;
    Ok(tiles::runs(&answer, now_ms))
}

async fn agent_versions(http: &ApiClient) -> Result<Vec<Approval>> {
    let query = runs_query(&[
        ("status", "waiting_for_approval".into()),
        ("limit", tiles::APPROVALS_PAGE.to_string()),
    ]);
    let answer = http
        .request_json(AGENTS, Method::GET, "/v1/approval-requests", &query, None)
        .await?;
    Ok(tiles::approvals(&answer))
}

async fn paused_agent_runs(http: &ApiClient) -> Result<PausedRuns> {
    let query = runs_query(&[("limit", PAUSED_RUNS_PAGE.to_string())]);
    let answer = http
        .request_json(AGENTS, Method::GET, "/v1/tool-approvals", &query, None)
        .await?;
    let items = answer["items"].as_array().cloned().unwrap_or_default();
    let total = answer["total"].as_u64().unwrap_or(items.len() as u64);
    Ok(PausedRuns { items, total })
}

async fn chat_approvals(http: &ApiClient) -> Result<ChatApprovals> {
    let query = runs_query(&[("assistantMode", "chat".into())]);
    let list = http
        .request_json(CHAT, Method::GET, "/chat/conversations", &query, None)
        .await?;
    let mut newest = parse_conversations(&list);
    newest.truncate(CHATS_CHECKED);
    let reads: Vec<_> = newest
        .iter()
        .map(|chat| {
            let http = http.clone();
            let path = format!("/chat/conversations/{}", seg(&chat.id));
            async move { http.request_json(CHAT, Method::GET, &path, &[], None).await }
        })
        .collect();
    let details = parallel(reads).await?;
    let mut pending = Vec::new();
    for (chat, detail) in newest.iter().zip(details) {
        let detail = detail?;
        if detail["pendingApproval"].is_object() {
            pending.push(ChatPending {
                conversation_id: chat.id.clone(),
                name: chat.title.clone(),
                approval: detail["pendingApproval"].clone(),
            });
        }
    }
    Ok(ChatApprovals {
        checked: newest.len(),
        pending,
    })
}

async fn month_usage(http: &ApiClient, now_ms: i64) -> Result<Value> {
    let query = runs_query(&[("from", month_start(now_ms))]);
    http.request_json(CHAT, Method::GET, "/chat/metrics", &query, None)
        .await
}

/// `2026-11-01T00:00:00Z` for the UTC month `epoch_ms` falls in.
fn month_start(epoch_ms: i64) -> String {
    format!("{}-01T00:00:00Z", &iso8601_utc(epoch_ms)[..7])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reason_is_the_first_line_the_run_recorded() {
        let reply = json!({"success": true, "data": [{"jobstatus": "Failed",
            "output": "\n  AnalysisException: table not found\n  at line 3", "startedat": "x"}]});
        assert_eq!(
            reason_of(&reply),
            Reason::Recorded("AnalysisException: table not found".into())
        );
        let empty = json!({"success": true, "data": [{"jobstatus": "Failed", "output": null}]});
        assert_eq!(reason_of(&empty), Reason::NotRecorded);
    }

    #[test]
    fn the_month_starts_on_the_first_utc() {
        let at = crate::tui::format::epoch_ms(&json!("2026-11-18T10:00:00Z")).unwrap();
        assert_eq!(month_start(at), "2026-11-01T00:00:00Z");
    }
}
