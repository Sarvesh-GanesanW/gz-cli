//! What each home tile shows, read from its endpoint's answer. Pure, so the
//! odd corners of each API (stale "running" rows, money as decimal strings,
//! no totals) are tested without a terminal.

use serde_json::Value;

use crate::tui::chat::{self, Conversation};
use crate::tui::fields;
use crate::tui::format::{self, text_of};

/// Runs "running" for longer than this are probably stuck: stage keeps
/// rows the reaper never settled, weeks old. Streaming jobs can run for
/// days, so they are folded into a dim line rather than hidden.
pub const STUCK_AFTER_MS: i64 = 24 * 60 * 60 * 1000;
/// The approval queue pages at 100 and sends no total.
pub const APPROVALS_PAGE: usize = 100;
/// Conversations shown; the list has no limit and no timestamps.
pub const RECENT_CHATS: usize = 6;

/// One job run from `/etl/consumption-metrics`.
#[derive(Debug, Clone, PartialEq)]
pub struct JobRun {
    pub job: String,
    pub project: String,
    pub started_ms: Option<i64>,
    pub ended_ms: Option<i64>,
    /// The raw row, for following links by its fields.
    pub row: Value,
}

/// Failed and open job runs that overlap the last 24 hours.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Runs {
    /// Newest first, as the server orders them.
    pub failed: Vec<JobRun>,
    /// Started within [`STUCK_AFTER_MS`].
    pub running: Vec<JobRun>,
    /// "Running" for longer than that.
    pub stuck: Vec<JobRun>,
    /// More runs matched than one page holds, so counts are lower bounds.
    pub truncated: bool,
}

pub fn runs(answer: &Value, now_ms: i64) -> Runs {
    let items = answer["items"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let total = answer["pagination"]["total"].as_u64().unwrap_or(0);
    let mut runs = Runs {
        truncated: total > items.len() as u64,
        ..Runs::default()
    };
    for row in items {
        let run = job_run(row);
        match status(row).as_str() {
            "failed" => runs.failed.push(run),
            "running" | "queued" => {
                let is_stuck = run
                    .started_ms
                    .is_some_and(|started| now_ms - started > STUCK_AFTER_MS);
                if is_stuck {
                    runs.stuck.push(run);
                } else {
                    runs.running.push(run);
                }
            }
            _ => {}
        }
    }
    runs
}

fn status(row: &Value) -> String {
    row["status"].as_str().unwrap_or_default().to_lowercase()
}

pub fn job_run(row: &Value) -> JobRun {
    let time = |key: &str| row.get(key).and_then(format::epoch_ms);
    JobRun {
        job: text_or(row, &["jobName", "datasetName"], "Unnamed job"),
        project: text_or(row, &["projectName"], ""),
        started_ms: time("startedAt"),
        ended_ms: time("completedAt"),
        row: row.clone(),
    }
}

/// An agent version waiting for the caller's review.
#[derive(Debug, Clone, PartialEq)]
pub struct Approval {
    pub agent: String,
    pub version: Option<i64>,
    pub requested_by: String,
    pub requested_ms: Option<i64>,
    pub row: Value,
}

pub fn approvals(answer: &Value) -> Vec<Approval> {
    answer["items"]
        .as_array()
        .into_iter()
        .flatten()
        .map(approval)
        .collect()
}

pub fn approval(row: &Value) -> Approval {
    Approval {
        agent: text_or(row, &["definitionSnapshot.name", "agentId"], "An agent"),
        version: row["agentVersion"].as_i64(),
        requested_by: text_or(row, &["requestedBy"], ""),
        requested_ms: row.get("requestedAt").and_then(format::epoch_ms),
        row: row.clone(),
    }
}

/// "5" or, for a full page, "100+".
pub fn approval_count(count: usize) -> String {
    if count >= APPROVALS_PAGE {
        format!("{APPROVALS_PAGE}+")
    } else {
        count.to_string()
    }
}

/// The chat budget: a rolling window that counts only spend tied to the
/// caller's budget policy, so it is never compared with month spend.
#[derive(Debug, Clone, PartialEq)]
pub enum Budget {
    NoLimit {
        window_days: Option<u64>,
    },
    Limit {
        used_usd: f64,
        limit_usd: f64,
        window_days: Option<u64>,
        exhausted: bool,
    },
    Unknown,
}

/// AI spend this calendar month (UTC) from `/chat/metrics`.
#[derive(Debug, Clone, PartialEq)]
pub struct Spend {
    /// The caller's own chat spend (`userMetrics` row for `username`).
    pub mine: Option<f64>,
    /// The whole site's spend, which includes agent-builder runs.
    pub site: Option<f64>,
    /// The agent-builder part of `site`.
    pub agents: Option<f64>,
    pub budget: Budget,
}

pub fn spend(answer: &Value) -> Spend {
    let me = answer["username"].as_str().unwrap_or_default();
    let cost_of = |list: &str, key: &str, value: &str| {
        answer[list]
            .as_array()
            .into_iter()
            .flatten()
            .find(|row| row[key].as_str() == Some(value))
            .and_then(|row| money(&row["totalCostUsd"]))
    };
    Spend {
        mine: cost_of("userMetrics", "username", me).or_else(|| {
            // A user with no requests this month has no row: they spent nothing.
            answer["userMetrics"].is_array().then_some(0.0)
        }),
        site: money(&answer["costUsd"]["total"]),
        agents: cost_of("agentMetrics", "agentType", "agent_builder"),
        budget: budget(&answer["budget"]),
    }
}

pub fn budget(value: &Value) -> Budget {
    if !value.is_object() {
        return Budget::Unknown;
    }
    let window_days = value["windowDays"].as_u64();
    match value["limitCents"].as_f64() {
        None => Budget::NoLimit { window_days },
        Some(limit_cents) => Budget::Limit {
            used_usd: money(&value["usedUsd"])
                .or_else(|| value["usedCents"].as_f64().map(|c| c / 100.0))
                .unwrap_or(0.0),
            limit_usd: limit_cents / 100.0,
            window_days,
            exhausted: value["exhausted"].as_bool() == Some(true),
        },
    }
}

/// Chat and agents send money as decimal strings, ETL as floats.
fn money(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// "$24.68", "<$0.01" or "—".
pub fn usd(amount: Option<f64>) -> String {
    match amount {
        Some(n) if n > 0.0 && n < 0.01 => "<$0.01".to_string(),
        Some(n) => format!("${n:.2}"),
        None => format::MISSING.to_string(),
    }
}

/// The newest conversations, in the server's order (newest created first;
/// the list carries no times).
pub fn recent_chats(answer: &Value) -> Vec<Conversation> {
    let mut chats = chat::parse_conversations(answer);
    chats.truncate(RECENT_CHATS);
    chats
}

fn text_or(row: &Value, path: &[&str], fallback: &str) -> String {
    fields::lookup_any(row, path)
        .filter(|v| !v.is_null())
        .map(text_of)
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use crate::tui::app::test_support::TEST_NOW_MS;

    const HOUR: i64 = 60 * 60 * 1000;

    fn at(ms: i64) -> String {
        format::iso8601_utc(ms)
    }

    #[test]
    fn runs_split_into_failed_running_and_stuck() {
        let answer = json!({
            "items": [
                {"status": "running", "jobName": "fresh", "startedAt": at(TEST_NOW_MS - HOUR)},
                {"status": "failed", "jobName": "ChallanReceiptBulk", "projectName": "Invoice-Extractor",
                 "startedAt": at(TEST_NOW_MS - 2 * HOUR), "completedAt": at(TEST_NOW_MS - 2 * HOUR + 7_000)},
                {"status": "succeeded", "jobName": "fine"},
                {"status": "running", "jobName": "memTest", "startedAt": "2026-10-01T17:16:37.706409+00:00"},
                {"status": "queued", "jobName": "waiting"}
            ],
            "pagination": {"limit": 100, "offset": 0, "total": 5}
        });
        let runs = runs(&answer, TEST_NOW_MS);
        assert_eq!(runs.failed.len(), 1);
        assert_eq!(runs.failed[0].project, "Invoice-Extractor");
        assert_eq!(
            runs.failed[0].ended_ms,
            Some(TEST_NOW_MS - 2 * HOUR + 7_000)
        );
        let running: Vec<&str> = runs.running.iter().map(|r| r.job.as_str()).collect();
        assert_eq!(
            running,
            ["fresh", "waiting"],
            "a run with no start isn't stuck"
        );
        assert_eq!(runs.stuck[0].job, "memTest");
        assert!(!runs.truncated);
    }

    #[test]
    fn a_full_page_of_runs_is_marked_truncated() {
        let answer = json!({"items": [{"status": "failed"}], "pagination": {"total": 250}});
        let runs = runs(&answer, TEST_NOW_MS);
        assert!(runs.truncated);
        assert_eq!(runs.failed[0].job, "Unnamed job");
    }

    #[test]
    fn approvals_name_the_agent_and_version() {
        let answer = json!({"items": [{
            "id": 271, "agentId": "98af", "agentVersion": 3, "requestedBy": "sarvesh",
            "requestedAt": "2026-11-08T04:46:18.193961Z",
            "definitionSnapshot": {"name": "rlo-pair-judge"}
        }], "limit": 100, "offset": 0});
        let list = approvals(&answer);
        assert_eq!(list[0].agent, "rlo-pair-judge");
        assert_eq!(list[0].version, Some(3));
        assert!(list[0].requested_ms.is_some());
        assert_eq!(approval_count(5), "5");
        assert_eq!(approval_count(100), "100+");
    }

    #[test]
    fn spend_keeps_my_site_and_agent_figures_apart() {
        let answer = json!({
            "username": "sagatest",
            "costUsd": {"total": "24.684345973200000000"},
            "userMetrics": [
                {"username": "sarvesh", "totalCostUsd": "1.83"},
                {"username": "sagatest", "totalCostUsd": "6.864986640000000000"}
            ],
            "agentMetrics": [{"agentType": "agent_builder", "totalCostUsd": "12.839"}],
            "budget": {"limitCents": null, "usedCents": 0, "usedUsd": "0.0", "windowDays": 30}
        });
        let spend = spend(&answer);
        assert_eq!(usd(spend.mine), "$6.86");
        assert_eq!(usd(spend.site), "$24.68");
        assert_eq!(usd(spend.agents), "$12.84");
        assert_eq!(
            spend.budget,
            Budget::NoLimit {
                window_days: Some(30)
            }
        );
    }

    #[test]
    fn spend_without_my_row_is_zero_and_limits_read_in_dollars() {
        let answer = json!({
            "username": "new-user", "userMetrics": [], "costUsd": {"total": "0.004"},
            "budget": {"limitCents": 2500, "usedUsd": "26.10", "windowDays": 30, "exhausted": true}
        });
        let spend = spend(&answer);
        assert_eq!(spend.mine, Some(0.0));
        assert_eq!(usd(spend.site), "<$0.01");
        assert_eq!(usd(spend.agents), "—");
        assert_eq!(
            spend.budget,
            Budget::Limit {
                used_usd: 26.10,
                limit_usd: 25.0,
                window_days: Some(30),
                exhausted: true
            }
        );
    }

    #[test]
    fn recent_chats_keep_server_order_and_cap() {
        let conversations: Vec<Value> = (0..10)
            .map(|i| json!({"conversationId": format!("c{i}"), "conversationName": format!("chat {i}")}))
            .collect();
        let chats = recent_chats(&json!({ "conversations": conversations }));
        assert_eq!(chats.len(), RECENT_CHATS);
        assert_eq!(chats[0].id, "c0");
    }
}
