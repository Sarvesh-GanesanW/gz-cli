//! `gz audit agents`: an administrator's view of every agent on the site,
//! with findings a CI job can fail on.
//!
//! Endpoint (gz-agent-builder-provider-api): `GET /v1/admin/agents/audit`
//! with `from`, `limit` (≤ 100) and `offset` → `{items, runsFrom, limit,
//! offset}`. The agent builder answers only members of the Admin group (its
//! `_require_admin`, read from the verified Cognito token) with 403
//! `AdminRequired`; gz passes that refusal on and never decides admin-ness
//! itself.

mod args;
mod findings;
mod model;
mod report;

pub use args::{AuditAction, AuditArgs};

use anyhow::{Context, Result};
use reqwest::Method;
use serde_json::{json, Value};

use crate::cmd::code_report::ReportedExit;
use crate::cmd::Runtime;
use crate::output::{self, OutputFormat};
use crate::tui::format::{iso8601_utc, system_now_ms};
use args::AgentsAuditArgs;
use findings::{AuditClock, Finding};
use model::{AgentRecord, OwnerStatus};

const AGENTS: &str = "agents";
const AUDIT_PATH: &str = "/v1/admin/agents/audit";
/// The route's largest page.
const PAGE_SIZE: usize = 100;
const MS_PER_DAY: i64 = 86_400_000;
/// Exit code when `--fail-on` finds something; 1 stays "gz itself failed".
pub const FINDINGS_EXIT_CODE: i32 = 2;

pub async fn run(rt: &Runtime, action: &AuditAction) -> Result<()> {
    match action {
        AuditAction::Agents(args) => agents(rt, args).await,
    }
}

async fn agents(rt: &Runtime, args: &AgentsAuditArgs) -> Result<()> {
    let now_ms = system_now_ms();
    let from = iso8601_utc(now_ms - i64::from(args.days) * MS_PER_DAY);
    let (raw, runs_from) = fetch_all(rt, &from).await?;
    let records = raw
        .iter()
        .map(|value| serde_json::from_value::<AgentRecord>(value.clone()))
        .collect::<Result<Vec<_>, _>>()
        .context("the agent builder's audit reply has an unexpected shape")?;
    let clock = AuditClock {
        now_ms,
        window_days: args.days,
    };
    let all_findings = findings::find_all(&records, clock);
    let selection = Selection::new(args, &records, &all_findings);

    print(rt, args, &records, &raw, &selection, &runs_from, now_ms)?;
    fail_on(args, &selection.findings)
}

/// Every page of the audit route.
async fn fetch_all(rt: &Runtime, from: &str) -> Result<(Vec<Value>, String)> {
    let mut items = Vec::new();
    let mut runs_from = from.to_string();
    loop {
        let query = vec![
            ("from".to_string(), from.to_string()),
            ("limit".to_string(), PAGE_SIZE.to_string()),
            ("offset".to_string(), items.len().to_string()),
        ];
        let page = rt
            .http
            .request_json(AGENTS, Method::GET, AUDIT_PATH, &query, None)
            .await
            .map_err(explain_refusal)?;
        let batch = page["items"]
            .as_array()
            .context("the agent builder's audit reply has no items list")?;
        if let Some(server_from) = page["runsFrom"].as_str() {
            runs_from = server_from.to_string();
        }
        let is_last = batch.len() < PAGE_SIZE;
        items.extend(batch.iter().cloned());
        if is_last {
            return Ok((items, runs_from));
        }
    }
}

/// Turns the agent builder's refusals into what to do about them.
fn explain_refusal(err: anyhow::Error) -> anyhow::Error {
    let text = format!("{err:#}");
    if text.contains("HTTP 403") {
        let reason = crate::http::readable_error(&text);
        return anyhow::anyhow!(
            "gz audit agents is for site administrators, and the agent builder refused this account ({reason}). Ask an administrator to add you to the site's Admin group, then run `gz auth login` again"
        );
    }
    if text.contains("HTTP 404") && text.ends_with(": Not Found") {
        return anyhow::anyhow!(
            "this site's agent builder has no audit route yet: it needs the gz-agent-builder-provider-api update with GET {AUDIT_PATH} deployed"
        );
    }
    err
}

/// The agents and findings the filters keep.
struct Selection<'a> {
    agents: Vec<usize>,
    findings: Vec<&'a Finding>,
}

impl<'a> Selection<'a> {
    fn new(args: &AgentsAuditArgs, records: &[AgentRecord], all: &'a [Finding]) -> Self {
        let owned_by = |record: &AgentRecord| {
            args.owner
                .as_deref()
                .is_none_or(|owner| record.created_by.as_deref() == Some(owner))
        };
        let findings: Vec<&Finding> = all
            .iter()
            .filter(|f| args.finding.is_empty() || args.finding.contains(&f.finding))
            .filter(|f| args.severity.is_none_or(|minimum| f.severity >= minimum))
            .filter(|f| records.iter().any(|r| r.id == f.agent_id && owned_by(r)))
            .collect();
        let narrowed = !args.finding.is_empty() || args.severity.is_some();
        let agents = records
            .iter()
            .enumerate()
            .filter(|(_, record)| owned_by(record))
            .filter(|(_, record)| !narrowed || findings.iter().any(|f| f.agent_id == record.id))
            .map(|(index, _)| index)
            .collect();
        Self { agents, findings }
    }
}

fn print(
    rt: &Runtime,
    args: &AgentsAuditArgs,
    records: &[AgentRecord],
    raw: &[Value],
    selection: &Selection,
    runs_from: &str,
    now_ms: i64,
) -> Result<()> {
    match rt.format {
        OutputFormat::Table => print_tables(records, raw, selection, args.days, now_ms),
        OutputFormat::Csv => {
            let rows: Vec<Value> = selection
                .findings
                .iter()
                .map(|f| report::finding_row(f))
                .collect();
            output::print_value(&Value::Array(rows), rt.format, None)
        }
        OutputFormat::Json | OutputFormat::Yaml => {
            let shown: Vec<&Value> = selection.agents.iter().map(|&i| &raw[i]).collect();
            let document = report::document(&shown, &selection.findings, runs_from, args.days);
            output::print_value(&document, rt.format, None)
        }
    }
}

fn print_tables(
    records: &[AgentRecord],
    raw: &[Value],
    selection: &Selection,
    days: u32,
    now_ms: i64,
) -> Result<()> {
    if selection.agents.is_empty() {
        output::info("no agents match these filters");
        return Ok(());
    }
    let rows: Vec<Value> = selection
        .agents
        .iter()
        .map(|&index| {
            let record = &records[index];
            let own: Vec<&Finding> = selection
                .findings
                .iter()
                .copied()
                .filter(|f| f.agent_id == record.id)
                .collect();
            report::agent_row(record, &raw[index], &own, now_ms)
        })
        .collect();
    output::print_with_columns(
        &Value::Array(rows),
        OutputFormat::Table,
        None,
        &report::AGENT_COLUMNS,
    )?;
    if !selection.findings.is_empty() {
        println!();
        let rows: Vec<Value> = selection
            .findings
            .iter()
            .map(|f| report::finding_row(f))
            .collect();
        output::print_with_columns(
            &json!(rows),
            OutputFormat::Table,
            None,
            &report::FINDING_COLUMNS,
        )?;
    }
    output::info(&format!(
        "{} · runs and cost cover the last {days} days",
        report::summary_line(selection.agents.len(), &selection.findings)
    ));
    for note in notes(records, selection) {
        output::warn(&note);
    }
    Ok(())
}

/// What the audit could not check, so a clean result isn't over-read.
fn notes(records: &[AgentRecord], selection: &Selection) -> Vec<String> {
    let shown = || selection.agents.iter().map(|&i| &records[i]);
    let mut notes = Vec::new();
    let unchecked = shown()
        .filter(|r| r.owner_status == OwnerStatus::Unknown)
        .count();
    if unchecked > 0 {
        notes.push(format!(
            "the agent builder could not look up {unchecked} owner(s) in the user pool, so missing owners may not be flagged"
        ));
    }
    let unpriced: u64 = shown().map(|r| r.runs.unpriced_run_count).sum();
    if unpriced > 0 {
        notes.push(format!(
            "{unpriced} run(s) have no recorded price; their cost is not in the totals or the spend check"
        ));
    }
    notes
}

fn fail_on(args: &AgentsAuditArgs, findings: &[&Finding]) -> Result<()> {
    let Some(threshold) = args.fail_on else {
        return Ok(());
    };
    let failing = findings.iter().filter(|f| f.severity >= threshold).count();
    if failing == 0 {
        return Ok(());
    }
    output::warn(&format!(
        "{failing} finding(s) at {} severity or above (--fail-on {})",
        threshold.label(),
        threshold.label()
    ));
    Err(ReportedExit(FINDINGS_EXIT_CODE).into())
}
