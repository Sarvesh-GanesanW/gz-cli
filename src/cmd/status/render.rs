//! `gz status` as a panel, and as JSON with everything gathered. Pure, so
//! every section and every failure is tested without a server.

use serde_json::{json, Value};

use super::sessions::{NotebookScan, SqlSession, SQL_SESSIONS_SCOPE};
use super::{
    execution_id, ChatApprovals, FailedRuns, PausedRuns, Reason, Report, Section, Since,
    CHATS_CHECKED,
};
use crate::cmd::agents::paused_message;
use crate::cmd::update;
use crate::cmd::usage::budget_rows;
use crate::cmd::window::span;
use crate::code_ui::panel::{Panel, Row};
use crate::spend::budget::BudgetStatus;
use crate::tui::format::{date_time, relative_time};
use crate::tui::views::home::tiles::{self, Approval, JobRun, Runs};

/// Items listed per section; the heading carries the full count.
const SHOWN: usize = 10;
/// Stuck runs named before "and N more".
const STUCK_NAMED: usize = 3;

pub fn panel(report: &Report) -> Panel {
    let mut aside = report.site.clone().unwrap_or_default();
    if !aside.is_empty() {
        aside.push_str(" · ");
    }
    aside.push_str(&since_text(report.since));
    let mut panel = Panel::new("Status").aside(aside);
    let (heading, rows) = failed_section(&report.failed, report.now_ms);
    panel = panel.section(Some(&heading), rows);
    let (heading, rows) = open_section(&report.open, report.now_ms);
    panel = panel.section(Some(&heading), rows);
    panel = panel.section(Some("Waiting for you"), waiting_rows(report));
    panel = panel.section(
        Some("Compute left running · costs money while it runs"),
        compute_rows(&report.sql_session, &report.notebooks, report.now_ms),
    );
    panel = panel.section(Some("AI usage this month"), usage_rows(&report.usage));
    panel.section(Some("gz"), release_rows(&report.latest_release))
}

fn since_text(since: Since) -> String {
    let at = date_time(since.at_ms());
    match since {
        Since::LastLook(_) => format!("since your last look, {at}"),
        Since::Flag(_) => format!("since {at}"),
        Since::FirstLook(_) => format!("since {at} (the last day; first look)"),
    }
}

fn failure(err: &str) -> Row {
    Row::note(format!("could not load: {err}"))
}

fn failed_section(failed: &Section<FailedRuns>, now_ms: i64) -> (String, Vec<Row>) {
    let failed = match failed {
        Ok(failed) => failed,
        Err(err) => return ("Failed job runs".into(), vec![failure(err)]),
    };
    let heading = count_heading("Failed job runs", failed.total, failed.runs.len());
    if failed.runs.is_empty() {
        return (heading, vec![Row::note("none")]);
    }
    let mut rows = Vec::new();
    for failure in failed.runs.iter().take(SHOWN) {
        rows.push(Row::entry(
            &failure.run.job,
            run_detail(&failure.run, now_ms),
        ));
        rows.extend(reason_row(failure, &failure.run.row));
    }
    if let Some(first) = failed.runs.first() {
        rows.push(Row::note(format!(
            "fix one with gz code: {}",
            fix_command(&first.run.row)
        )));
    }
    (heading, rows)
}

/// "Failed job runs · 68" or "· 68, newest 10 shown".
fn count_heading(title: &str, total: u64, listed: usize) -> String {
    let shown = listed.min(SHOWN) as u64;
    if total > shown {
        format!("{title} · {total}, newest {shown} shown")
    } else {
        format!("{title} · {total}")
    }
}

fn run_detail(run: &JobRun, now_ms: i64) -> String {
    let mut parts = Vec::new();
    if !run.project.is_empty() {
        parts.push(run.project.clone());
    }
    if let Some(at) = run.ended_ms.or(run.started_ms) {
        parts.push(relative_time(at, now_ms));
    }
    if let Some(by) = run.row["startedBy"].as_str() {
        parts.push(format!("by {by}"));
    }
    parts.extend(run_label(&run.row));
    parts.join(" · ")
}

fn run_label(row: &Value) -> Option<String> {
    let id = execution_id(row);
    (!id.is_empty()).then(|| format!("run {id}"))
}

fn reason_row(failure: &super::FailedRun, row: &Value) -> Option<Row> {
    let text = match &failure.reason {
        Reason::Recorded(reason) => format!("  reason: {reason}"),
        Reason::NotRecorded => format!(
            "  no reason recorded; read the logs: gz jobs logs --job {}",
            execution_id(row)
        ),
        Reason::Unavailable(err) => format!("  reason not loaded: {err}"),
        Reason::NotAsked => return None,
    };
    Some(Row::note(text))
}

fn fix_command(row: &Value) -> String {
    format!(
        "gz fix {} --project {} --run {}",
        row["jobId"],
        row["projectId"],
        execution_id(row)
    )
}

fn open_section(open: &Section<Runs>, now_ms: i64) -> (String, Vec<Row>) {
    let runs = match open {
        Ok(runs) => runs,
        Err(err) => return ("Job runs in progress".into(), vec![failure(err)]),
    };
    let total = runs.running.len() as u64;
    let mut heading = count_heading("Job runs in progress", total, runs.running.len());
    if runs.truncated {
        heading.push_str(" (one page; there may be more)");
    }
    let mut rows: Vec<Row> = runs
        .running
        .iter()
        .take(SHOWN)
        .map(|run| Row::entry(&run.job, in_progress_detail(run, now_ms)))
        .collect();
    if !runs.stuck.is_empty() {
        rows.push(Row::note(stuck_note(&runs.stuck)));
    }
    if rows.is_empty() {
        rows.push(Row::note("none"));
    }
    (heading, rows)
}

fn in_progress_detail(run: &JobRun, now_ms: i64) -> String {
    let mut parts = Vec::new();
    if !run.project.is_empty() {
        parts.push(run.project.clone());
    }
    let status = run.row["status"].as_str().unwrap_or("running");
    match run.started_ms {
        Some(started) => parts.push(format!("{status} for {}", span(now_ms - started))),
        None => parts.push(format!("{status}, not started yet")),
    }
    parts.extend(run_label(&run.row));
    parts.join(" · ")
}

fn stuck_note(stuck: &[JobRun]) -> String {
    let mut names: Vec<&str> = stuck
        .iter()
        .take(STUCK_NAMED)
        .map(|r| r.job.as_str())
        .collect();
    names.dedup();
    let mut text = format!(
        "{} more say running but started over a day ago, probably stuck: {}",
        stuck.len(),
        names.join(", ")
    );
    if stuck.len() > STUCK_NAMED {
        text.push_str(&format!(" and {} more", stuck.len() - STUCK_NAMED));
    }
    text
}

fn waiting_rows(report: &Report) -> Vec<Row> {
    let mut rows = Vec::new();
    match &report.agent_versions {
        Ok(list) => rows.extend(
            list.iter()
                .take(SHOWN)
                .map(|a| version_row(a, report.now_ms)),
        ),
        Err(err) => rows.push(Row::note(format!(
            "agent versions to review could not load: {err}"
        ))),
    }
    match &report.paused_agent_runs {
        Ok(paused) => rows.extend(paused_rows(paused, report.now_ms)),
        Err(err) => rows.push(Row::note(format!(
            "paused agent runs could not load: {err}"
        ))),
    }
    match &report.chats {
        Ok(chats) => rows.extend(chat_rows(chats)),
        Err(err) => rows.push(Row::note(format!("paused chats could not load: {err}"))),
    }
    let nothing = matches!(&report.agent_versions, Ok(list) if list.is_empty())
        && matches!(&report.paused_agent_runs, Ok(p) if p.items.is_empty())
        && matches!(&report.chats, Ok(c) if c.pending.is_empty());
    if nothing {
        rows.insert(0, Row::note("nothing"));
    }
    if report.chats.is_ok() {
        rows.push(Row::note(format!(
            "chats: only your {CHATS_CHECKED} newest are checked; the chat service has no list of paused chats"
        )));
    }
    rows
}

fn version_row(approval: &Approval, now_ms: i64) -> Row {
    let version = approval
        .version
        .map(|v| format!(" v{v}"))
        .unwrap_or_default();
    let mut detail = "agent version to review".to_string();
    if !approval.requested_by.is_empty() {
        detail.push_str(&format!(" · from {}", approval.requested_by));
    }
    if let Some(at) = approval.requested_ms {
        detail.push_str(&format!(" · {}", relative_time(at, now_ms)));
    }
    detail.push_str(&format!(
        " · gz agents decide {} --data '{{\"decision\":\"approved\"}}'",
        approval.row["id"]
    ));
    Row::entry(&format!("{}{version}", approval.agent), detail)
}

fn paused_rows(paused: &PausedRuns, now_ms: i64) -> Vec<Row> {
    let mut rows = Vec::new();
    for item in paused.items.iter().take(SHOWN) {
        let agent = item["agentName"].as_str().unwrap_or("An agent");
        let mut detail = "agent run paused for your approval".to_string();
        if let Some(by) = item["startedBy"].as_str() {
            detail.push_str(&format!(" · started by {by}"));
        }
        if let Some(at) = item
            .get("requestedAt")
            .and_then(crate::tui::format::epoch_ms)
        {
            detail.push_str(&format!(" · {}", relative_time(at, now_ms)));
        }
        rows.push(Row::entry(agent, detail));
        let agent_id = item["agentId"].as_str().unwrap_or_default();
        let run_id = text_of(&item["runId"]);
        let message = paused_message(agent_id, &run_id, item);
        if let Some(command) = message.lines().find(|l| l.starts_with("approve:")) {
            rows.push(Row::note(format!("  {command}")));
        }
    }
    if paused.total as usize > paused.items.len().min(SHOWN) {
        rows.push(Row::note(format!(
            "{} paused agent runs in all; see them in the web app's agent approvals",
            paused.total
        )));
    }
    rows
}

fn chat_rows(chats: &ChatApprovals) -> Vec<Row> {
    chats
        .pending
        .iter()
        .map(|chat| {
            let approval = &chat.approval;
            let what = approval["interruptName"].as_str().unwrap_or("an action");
            let run = text_of(&approval["runId"]);
            let interrupt = text_of(&approval["interruptId"]);
            Row::entry(
                &chat.name,
                format!("chat waiting to {what} · gz chat approve {run} --interrupt {interrupt}"),
            )
        })
        .collect()
}

fn text_of(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

pub fn compute_rows(
    sql: &Section<Option<SqlSession>>,
    notebooks: &Section<NotebookScan>,
    now_ms: i64,
) -> Vec<Row> {
    let mut rows = Vec::new();
    match sql {
        Ok(Some(session)) => rows.push(sql_row(session, now_ms)),
        Ok(None) => rows.push(Row::note("no SQL session kept by gz")),
        Err(err) => rows.push(Row::note(format!(
            "SQL session could not be checked: {err}"
        ))),
    }
    rows.push(Row::note(format!("  {SQL_SESSIONS_SCOPE}")));
    match notebooks {
        Ok(scan) => rows.extend(notebook_rows(scan)),
        Err(err) => rows.push(Row::note(format!("notebooks could not load: {err}"))),
    }
    rows
}

fn sql_row(session: &SqlSession, now_ms: i64) -> Row {
    let mut parts = vec![session.state.clone()];
    if let Some(warehouse) = &session.warehouse {
        parts.push(format!("warehouse {warehouse}"));
    }
    if let Some(compute) = &session.compute {
        parts.push(format!("compute {compute}"));
    }
    parts.push(match session.started_ms {
        Some(at) => format!("up {}", span(now_ms - at)),
        None => "start time not recorded".into(),
    });
    parts.push("stop: gz sql stop".into());
    Row::entry(
        &format!("SQL session {}", session.session_id),
        parts.join(" · "),
    )
}

fn notebook_rows(scan: &NotebookScan) -> Vec<Row> {
    let mut rows: Vec<Row> = scan
        .running
        .iter()
        .map(|notebook| {
            let name = notebook
                .name
                .clone()
                .or_else(|| {
                    notebook
                        .notebook_id
                        .as_ref()
                        .map(|id| format!("notebook {id}"))
                })
                .unwrap_or_else(|| "a notebook".into());
            let mut detail = format!("{} · project {}", notebook.status, notebook.project);
            if let Some(runtime) = &notebook.runtime {
                detail.push_str(&format!(" · {runtime}"));
            }
            Row::entry(&name, detail)
        })
        .collect();
    if rows.is_empty() {
        rows.push(Row::note("no notebooks of yours running"));
    } else {
        rows.push(Row::note(
            "  the notebook service records no start time, so how long they have run isn't known",
        ));
    }
    if !scan.unchecked_projects.is_empty() {
        rows.push(Row::note(format!(
            "  notebooks not checked in: {}",
            scan.unchecked_projects.join(", ")
        )));
    }
    rows
}

fn usage_rows(usage: &Section<Value>) -> Vec<Row> {
    let answer = match usage {
        Ok(answer) => answer,
        Err(err) => return vec![failure(err)],
    };
    let spend = tiles::spend(answer);
    let mut rows = vec![
        Row::field("you", tiles::usd(spend.mine)),
        Row::field("whole site", tiles::usd(spend.site)),
    ];
    rows.extend(match BudgetStatus::from_json(&answer["budget"]) {
        Some(budget) => budget_rows(&budget),
        None => vec![Row::note("the server sent no budget")],
    });
    rows
}

fn release_rows(latest: &Section<String>) -> Vec<Row> {
    let current = env!("CARGO_PKG_VERSION");
    let text = match latest {
        Ok(tag) if update::is_newer(tag) => {
            format!("{tag} is out (you have {current}) · gz update")
        }
        Ok(_) => format!("{current} is the latest release"),
        Err(err) => format!("could not check for a newer release: {err}"),
    };
    vec![Row::note(text)]
}

/// Everything gathered, each section as `{…}` or `{"error": "…"}`.
pub fn to_json(report: &Report) -> Value {
    json!({
        "site": report.site,
        "since": crate::tui::format::iso8601_utc(report.since.at_ms()),
        "sinceFrom": match report.since {
            Since::LastLook(_) => "lastLook",
            Since::Flag(_) => "flag",
            Since::FirstLook(_) => "firstLook",
        },
        "failedRuns": section_json(&report.failed, failed_json),
        "runsInProgress": section_json(&report.open, |runs| json!({
            "running": runs.running.iter().map(|r| r.row.clone()).collect::<Vec<_>>(),
            "probablyStuck": runs.stuck.iter().map(|r| r.row.clone()).collect::<Vec<_>>(),
            "truncated": runs.truncated,
        })),
        "agentVersionsToReview": section_json(&report.agent_versions, |list| {
            json!(list.iter().map(|a| a.row.clone()).collect::<Vec<_>>())
        }),
        "pausedAgentRuns": section_json(&report.paused_agent_runs, |p| json!({
            "total": p.total, "items": p.items,
        })),
        "pausedChats": section_json(&report.chats, |c| json!({
            "checked": c.checked,
            "pending": c.pending.iter().map(|p| json!({
                "conversationId": p.conversation_id,
                "name": p.name,
                "pendingApproval": p.approval,
            })).collect::<Vec<_>>(),
        })),
        "sqlSession": section_json(&report.sql_session, |s| json!({
            "kept": s, "scope": SQL_SESSIONS_SCOPE,
        })),
        "notebooks": section_json(&report.notebooks, |scan| json!(scan)),
        "aiUsage": section_json(&report.usage, |usage| usage.clone()),
        "release": section_json(&report.latest_release, |tag| json!({
            "current": env!("CARGO_PKG_VERSION"),
            "latest": tag,
            "newer": update::is_newer(tag),
        })),
    })
}

pub(crate) fn section_json<T>(section: &Section<T>, to_json: impl Fn(&T) -> Value) -> Value {
    match section {
        Ok(data) => to_json(data),
        Err(err) => json!({"error": err}),
    }
}

fn failed_json(failed: &FailedRuns) -> Value {
    let runs: Vec<Value> = failed
        .runs
        .iter()
        .map(|failure| {
            let mut row = failure.run.row.clone();
            row["reason"] = match &failure.reason {
                Reason::Recorded(text) => json!({"recorded": text}),
                Reason::NotRecorded => json!({"recorded": null}),
                Reason::Unavailable(err) => json!({"error": err}),
                Reason::NotAsked => json!({"lookedUp": false}),
            };
            row
        })
        .collect();
    json!({"total": failed.total, "runs": runs})
}

#[cfg(test)]
mod tests;
