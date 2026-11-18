//! The audit's findings: rules over the agent builder's audit records.
//!
//! Every rule reads data the platform records; nothing is guessed from
//! names or instructions. Approval facts come from the tool catalog the
//! agent builder enforces (`requiresApproval` already includes the tools a
//! reviewer always approves), and MCP tools can never be put behind
//! approval: the agent builder only accepts native tools there.

use std::collections::HashMap;

use clap::ValueEnum;
use serde::Serialize;

use super::model::{AgentRecord, McpStatus, OwnerStatus, Tool};
use crate::tui::format::{epoch_ms, relative_time};

const MS_PER_DAY: i64 = 86_400_000;
/// Data classifications from the governance profile that count as sensitive.
const SENSITIVE_CLASSIFICATIONS: [&str; 2] = ["confidential", "restricted"];
/// Failure share of finished runs that makes a finding, and the worse one.
const FAILING_SHARE: f64 = 0.2;
const MOSTLY_FAILING_SHARE: f64 = 0.5;
/// A mostly-failing agent is only high severity after this many failures.
const MOSTLY_FAILING_MIN_RUNS: u64 = 3;
/// Spend over this multiple of the median spending agent is an outlier...
const SPEND_OUTLIER_FACTOR: f64 = 3.0;
/// ...if it is also over this many dollars,
const SPEND_OUTLIER_FLOOR_USD: f64 = 1.0;
/// ...and enough agents spent anything for a median to mean something.
const SPEND_OUTLIER_MIN_SPENDERS: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Low,
    Medium,
    High,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, ValueEnum, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FindingKind {
    /// A tool that changes data runs without asking.
    WriteWithoutApproval,
    /// MCP tools, which can't be put behind approval.
    McpUnreviewed,
    /// An MCP connection the agent names is gone or isn't MCP.
    MissingConnection,
    /// Reads site or sensitive data and can send data out without review.
    SensitiveOutbound,
    NoOwner,
    /// The owner left the user pool or is disabled.
    OwnerGone,
    /// No run within the window.
    Unused,
    FailingRuns,
    SpendOutlier,
    /// A sub-agent or team member can use what its parent can't.
    SubAgentBroader,
    /// A sub-agent or team member points to a deleted agent.
    MissingSubAgent,
}

impl FindingKind {
    pub fn label(self) -> String {
        self.to_possible_value()
            .map(|value| value.get_name().to_string())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Finding {
    pub agent_id: String,
    pub agent: String,
    pub finding: FindingKind,
    pub severity: Severity,
    pub reason: String,
}

/// What the rules need besides the records.
#[derive(Debug, Clone, Copy)]
pub struct AuditClock {
    pub now_ms: i64,
    /// The run window the records were read for.
    pub window_days: u32,
}

/// Every finding for every agent, worst first.
pub fn find_all(agents: &[AgentRecord], clock: AuditClock) -> Vec<Finding> {
    let by_id: HashMap<&str, &AgentRecord> = agents.iter().map(|a| (a.id.as_str(), a)).collect();
    let spend_threshold = spend_outlier_threshold(agents);
    let mut findings: Vec<Finding> = agents
        .iter()
        .flat_map(|agent| {
            let mut found = Vec::new();
            found.extend(write_without_approval(agent));
            found.extend(mcp_connections(agent));
            found.extend(sensitive_outbound(agent));
            found.extend(ownership(agent));
            found.extend(unused(agent, clock));
            found.extend(failing_runs(agent, clock));
            found.extend(spend_outlier(agent, spend_threshold, clock));
            found.extend(delegation(agent, &by_id));
            found
                .into_iter()
                .map(|(finding, severity, reason)| Finding {
                    agent_id: agent.id.clone(),
                    agent: agent.name.clone(),
                    finding,
                    severity,
                    reason,
                })
                .collect::<Vec<_>>()
        })
        .collect();
    findings.sort_by(|a, b| b.severity.cmp(&a.severity).then(a.agent.cmp(&b.agent)));
    findings
}

type Found = (FindingKind, Severity, String);

fn names<'a>(tools: impl Iterator<Item = &'a Tool>) -> Vec<&'a str> {
    tools.map(|tool| tool.name.as_str()).collect()
}

fn write_without_approval(agent: &AgentRecord) -> Vec<Found> {
    let mut found = Vec::new();
    let unreviewed: Vec<&Tool> = agent
        .tools
        .iter()
        .filter(|t| t.writes_unreviewed())
        .collect();
    if !unreviewed.is_empty() {
        let severity = if unreviewed.iter().any(|tool| tool.is_elevated) {
            Severity::High
        } else {
            Severity::Medium
        };
        found.push((
            FindingKind::WriteWithoutApproval,
            severity,
            format!(
                "changes data without asking: {}",
                names(unreviewed.into_iter()).join(", ")
            ),
        ));
    }
    let etl_skips = names(agent.tools.iter().filter(|t| t.skips_approval_in_etl_jobs));
    if !etl_skips.is_empty() {
        found.push((
            FindingKind::WriteWithoutApproval,
            Severity::Low,
            format!(
                "in ETL jobs runs without a reviewer: {}",
                etl_skips.join(", ")
            ),
        ));
    }
    found
}

fn mcp_connections(agent: &AgentRecord) -> Vec<Found> {
    agent
        .mcp_connections
        .iter()
        .map(|mcp| {
            let name = &mcp.name;
            match mcp.status {
                McpStatus::Ok => {
                    let host = mcp.server_host.as_deref().unwrap_or("unknown host");
                    let scope = match &mcp.allowed_tools {
                        None => "every tool the server offers".to_string(),
                        Some(tools) => format!("its tools {}", tools.join(", ")),
                    };
                    (
                        FindingKind::McpUnreviewed,
                        Severity::Medium,
                        format!(
                            "MCP connection {name} ({host}): {scope} run without approval; MCP tools can't require it"
                        ),
                    )
                }
                McpStatus::Missing => missing_connection(name, "no longer exists"),
                McpStatus::NotMcp => missing_connection(name, "is not an MCP connection"),
                McpStatus::Invalid => missing_connection(name, "has an unreadable configuration"),
            }
        })
        .collect()
}

fn missing_connection(name: &str, problem: &str) -> Found {
    (
        FindingKind::MissingConnection,
        Severity::Medium,
        format!("MCP connection {name} {problem}; the agent's runs fail until it is fixed"),
    )
}

/// Ways out of the site that no reviewer sees: web tools without approval
/// and every working MCP connection.
fn unreviewed_outbound(agent: &AgentRecord) -> Vec<String> {
    let tools = agent
        .tools
        .iter()
        .filter(|tool| tool.is_outbound() && !tool.requires_approval)
        .map(|tool| tool.name.clone());
    let mcp = agent
        .mcp_connections
        .iter()
        .filter(|mcp| mcp.status == McpStatus::Ok)
        .map(|mcp| format!("MCP {}", mcp.name));
    tools.chain(mcp).collect()
}

fn sensitive_outbound(agent: &AgentRecord) -> Option<Found> {
    let outbound = unreviewed_outbound(agent);
    if outbound.is_empty() {
        return None;
    }
    let outbound = outbound.join(", ");
    if let Some(class) = agent
        .data_classification
        .as_deref()
        .filter(|class| SENSITIVE_CLASSIFICATIONS.contains(class))
    {
        return Some((
            FindingKind::SensitiveOutbound,
            Severity::High,
            format!("handles {class} data and can send data out without review: {outbound}"),
        ));
    }
    let readers = names(agent.tools.iter().filter(|tool| tool.reads_site_data()));
    (!readers.is_empty()).then(|| {
        (
            FindingKind::SensitiveOutbound,
            Severity::Medium,
            format!(
                "reads site data ({}) and can send data out without review: {outbound}",
                readers.join(", ")
            ),
        )
    })
}

fn ownership(agent: &AgentRecord) -> Option<Found> {
    let owner = agent.created_by.as_deref().filter(|name| !name.is_empty());
    match (agent.owner_status, owner) {
        (OwnerStatus::None, _) | (_, None) => Some((
            FindingKind::NoOwner,
            Severity::High,
            "has no owner".to_string(),
        )),
        (OwnerStatus::Missing, Some(owner)) => Some((
            FindingKind::OwnerGone,
            Severity::High,
            format!("owner {owner} no longer exists in the user pool"),
        )),
        (OwnerStatus::Disabled, Some(owner)) => Some((
            FindingKind::OwnerGone,
            Severity::Medium,
            format!("owner {owner} is disabled"),
        )),
        (OwnerStatus::Active | OwnerStatus::Unknown, Some(_)) => None,
    }
}

fn unused(agent: &AgentRecord, clock: AuditClock) -> Option<Found> {
    let window_ms = i64::from(clock.window_days) * MS_PER_DAY;
    let reason = match agent.runs.last_run_at.as_deref().and_then(timestamp_ms) {
        Some(last) if clock.now_ms - last >= window_ms => {
            format!("last run {}", relative_time(last, clock.now_ms))
        }
        Some(_) => return None,
        None => {
            let created = agent.created_at.as_deref().and_then(timestamp_ms)?;
            if clock.now_ms - created < window_ms {
                return None;
            }
            format!(
                "never run; created {}",
                relative_time(created, clock.now_ms)
            )
        }
    };
    Some((FindingKind::Unused, Severity::Low, reason))
}

fn timestamp_ms(text: &str) -> Option<i64> {
    epoch_ms(&serde_json::Value::String(text.to_string()))
}

fn failing_runs(agent: &AgentRecord, clock: AuditClock) -> Option<Found> {
    let failed = agent.runs.failed_count;
    let finished = agent.runs.finished();
    if failed == 0 || finished == 0 {
        return None;
    }
    let share = failed as f64 / finished as f64;
    let severity = if share >= MOSTLY_FAILING_SHARE && failed >= MOSTLY_FAILING_MIN_RUNS {
        Severity::High
    } else if share >= FAILING_SHARE {
        Severity::Medium
    } else {
        return None;
    };
    Some((
        FindingKind::FailingRuns,
        severity,
        format!(
            "{failed} of {finished} finished runs failed in the last {} days",
            clock.window_days
        ),
    ))
}

/// The spend above which an agent is an outlier, when there are enough
/// spending agents to compare with.
fn spend_outlier_threshold(agents: &[AgentRecord]) -> Option<f64> {
    let mut spends: Vec<f64> = agents
        .iter()
        .map(|agent| agent.runs.cost_usd)
        .filter(|cost| *cost > 0.0)
        .collect();
    if spends.len() < SPEND_OUTLIER_MIN_SPENDERS {
        return None;
    }
    spends.sort_by(f64::total_cmp);
    let middle = spends.len() / 2;
    let median = if spends.len() % 2 == 0 {
        (spends[middle - 1] + spends[middle]) / 2.0
    } else {
        spends[middle]
    };
    Some((median * SPEND_OUTLIER_FACTOR).max(SPEND_OUTLIER_FLOOR_USD))
}

fn spend_outlier(agent: &AgentRecord, threshold: Option<f64>, clock: AuditClock) -> Option<Found> {
    let threshold = threshold?;
    let cost = agent.runs.cost_usd;
    (cost > threshold).then(|| {
        (
            FindingKind::SpendOutlier,
            Severity::Medium,
            format!(
                "spent ${cost:.2} in the last {} days, over ${threshold:.2} ({SPEND_OUTLIER_FACTOR}x the median spending agent)",
                clock.window_days
            ),
        )
    })
}

fn delegation(agent: &AgentRecord, by_id: &HashMap<&str, &AgentRecord>) -> Vec<Found> {
    agent
        .delegates()
        .filter_map(|delegate| {
            let Some(child) = by_id.get(delegate.agent_id.as_str()) else {
                return Some((
                    FindingKind::MissingSubAgent,
                    Severity::Medium,
                    format!(
                        "sub-agent {} points to agent {}, which no longer exists",
                        delegate.alias, delegate.agent_id
                    ),
                ));
            };
            broader_child(agent, child, &delegate.alias)
        })
        .collect()
}

fn broader_child(parent: &AgentRecord, child: &AgentRecord, alias: &str) -> Option<Found> {
    let extra_tools: Vec<&Tool> = child
        .tools
        .iter()
        .filter(|t| !parent.has_tool(&t.name))
        .collect();
    let extra_mcp: Vec<String> = child
        .mcp_connections
        .iter()
        .filter(|mcp| !parent.has_mcp_connection(&mcp.name))
        .map(|mcp| format!("MCP {}", mcp.name))
        .collect();
    if extra_tools.is_empty() && extra_mcp.is_empty() {
        return None;
    }
    let risky = !extra_mcp.is_empty()
        || extra_tools
            .iter()
            .any(|tool| tool.writes_unreviewed() || tool.is_outbound());
    let severity = if risky {
        Severity::High
    } else {
        Severity::Medium
    };
    let extras: Vec<String> = extra_tools
        .iter()
        .map(|tool| tool.name.clone())
        .chain(extra_mcp)
        .collect();
    Some((
        FindingKind::SubAgentBroader,
        severity,
        format!(
            "sub-agent {alias} ({}) can use what {} can't: {}",
            child.name,
            parent.name,
            extras.join(", ")
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    const NOW: &str = "2026-11-18T00:00:00Z";

    fn clock() -> AuditClock {
        AuditClock {
            now_ms: timestamp_ms(NOW).unwrap(),
            window_days: 30,
        }
    }

    fn tool(name: &str, category: &str, mutating: bool, elevated: bool, approval: bool) -> Value {
        json!({
            "name": name, "kind": "builtIn", "category": category,
            "isMutating": mutating, "isElevated": elevated,
            "requiresApproval": approval, "skipsApprovalInEtlJobs": false,
        })
    }

    fn agent(id: &str, overrides: Value) -> AgentRecord {
        let mut record = json!({
            "id": id, "name": format!("agent-{id}"), "createdBy": "alice",
            "ownerStatus": "active", "createdAt": "2026-11-01T00:00:00+00:00",
            "dataClassification": null,
            "model": {"modelId": "m", "provider": "GZ", "connectionName": null},
            "tools": [], "mcpConnections": [], "subAgents": [], "team": null,
            "limits": {"maxRunTreeSpendUsd": null, "quotaLimitCents": null, "quotaSpendUsd": null},
            "runs": {
                "count": 1, "childRunCount": 0, "completedCount": 1, "failedCount": 0,
                "cancelledCount": 0, "interruptedCount": 0, "runningCount": 0,
                "costUsd": "0", "unpricedRunCount": 0,
                "lastRunAt": "2026-11-17T00:00:00+00:00", "lastRunStatus": "completed",
            },
        });
        merge(&mut record, overrides);
        serde_json::from_value(record).unwrap()
    }

    fn merge(target: &mut Value, patch: Value) {
        match (target, patch) {
            (Value::Object(target), Value::Object(patch)) => {
                for (key, value) in patch {
                    merge(target.entry(key).or_insert(Value::Null), value);
                }
            }
            (target, patch) => *target = patch,
        }
    }

    fn kinds(findings: &[Finding]) -> Vec<(FindingKind, Severity)> {
        findings.iter().map(|f| (f.finding, f.severity)).collect()
    }

    #[test]
    fn a_tidy_agent_has_no_findings() {
        let tidy = agent(
            "a",
            json!({"tools": [tool("get_datasets", "datasets", false, false, false)]}),
        );
        assert!(find_all(&[tidy], clock()).is_empty());
    }

    #[test]
    fn unreviewed_writes_are_high_when_elevated_and_medium_otherwise() {
        let elevated = agent(
            "a",
            json!({"tools": [
                tool("submit_etl_job", "etl", true, true, false),
                tool("save_dataset", "datasets", true, true, true),
            ]}),
        );
        let plain = agent(
            "b",
            json!({"tools": [tool("create_filter", "filter", true, false, false)]}),
        );

        let found = find_all(&[elevated, plain], clock());

        assert_eq!(
            kinds(&found),
            vec![
                (FindingKind::WriteWithoutApproval, Severity::High),
                (FindingKind::WriteWithoutApproval, Severity::Medium),
            ]
        );
        assert_eq!(
            found[0].reason,
            "changes data without asking: submit_etl_job"
        );
    }

    #[test]
    fn tools_etl_jobs_run_without_a_reviewer_are_a_low_finding() {
        let mut submit = tool("submit_etl_job", "etl", true, true, true);
        submit["skipsApprovalInEtlJobs"] = json!(true);
        let found = find_all(&[agent("a", json!({"tools": [submit]}))], clock());

        assert_eq!(
            kinds(&found),
            vec![(FindingKind::WriteWithoutApproval, Severity::Low)]
        );
        assert!(found[0].reason.contains("ETL jobs"));
    }

    #[test]
    fn mcp_connections_are_unreviewed_and_broken_ones_are_named() {
        let found = find_all(
            &[agent(
                "a",
                json!({"mcpConnections": [
                    {"name": "Search", "status": "ok", "serverHost": "mcp.example.com", "allowedTools": null},
                    {"name": "Old", "status": "missing", "serverHost": null, "allowedTools": null},
                ]}),
            )],
            clock(),
        );

        assert_eq!(
            kinds(&found),
            vec![
                (FindingKind::McpUnreviewed, Severity::Medium),
                (FindingKind::MissingConnection, Severity::Medium),
            ]
        );
        assert!(found[0].reason.contains("mcp.example.com"));
        assert!(found[0].reason.contains("every tool the server offers"));
    }

    #[test]
    fn site_data_readers_with_an_unreviewed_way_out_are_flagged() {
        let reader = agent(
            "a",
            json!({"tools": [
                tool("query_data_provider", "data_provider", false, false, true),
                tool("web_fetch", "web", false, false, false),
            ]}),
        );
        let reviewed_out = agent(
            "b",
            json!({"tools": [
                tool("get_datasets", "datasets", false, false, false),
                tool("send_webhook_notification", "web", true, false, true),
            ]}),
        );

        let found = find_all(&[reader, reviewed_out], clock());

        assert_eq!(
            kinds(&found),
            vec![(FindingKind::SensitiveOutbound, Severity::Medium)]
        );
        assert_eq!(
            found[0].reason,
            "reads site data (query_data_provider) and can send data out without review: web_fetch"
        );
    }

    #[test]
    fn classified_sensitive_agents_with_mcp_access_are_high() {
        let found = find_all(
            &[agent(
                "a",
                json!({
                    "dataClassification": "restricted",
                    "mcpConnections": [{"name": "Search", "status": "ok", "serverHost": "h", "allowedTools": ["q"]}],
                }),
            )],
            clock(),
        );

        assert!(kinds(&found).contains(&(FindingKind::SensitiveOutbound, Severity::High)));
    }

    #[test]
    fn owners_that_are_absent_gone_or_disabled_are_flagged() {
        let found = find_all(
            &[
                agent("a", json!({"createdBy": null, "ownerStatus": "none"})),
                agent("b", json!({"createdBy": "bob", "ownerStatus": "missing"})),
                agent(
                    "c",
                    json!({"createdBy": "carol", "ownerStatus": "disabled"}),
                ),
                agent("d", json!({"createdBy": "dave", "ownerStatus": "unknown"})),
            ],
            clock(),
        );

        assert_eq!(
            kinds(&found),
            vec![
                (FindingKind::NoOwner, Severity::High),
                (FindingKind::OwnerGone, Severity::High),
                (FindingKind::OwnerGone, Severity::Medium),
            ]
        );
        assert_eq!(
            found[1].reason,
            "owner bob no longer exists in the user pool"
        );
    }

    #[test]
    fn agents_without_a_run_in_the_window_are_unused() {
        let stale = agent(
            "a",
            json!({"runs": {"lastRunAt": "2026-09-01T00:00:00+00:00"}}),
        );
        let never = agent(
            "b",
            json!({
                "createdAt": "2026-08-01T00:00:00Z", "runs": {"lastRunAt": null, "count": 0},
            }),
        );
        let new = agent("c", json!({"runs": {"lastRunAt": null, "count": 0}}));

        let found = find_all(&[stale, never, new], clock());

        assert_eq!(
            kinds(&found),
            vec![
                (FindingKind::Unused, Severity::Low),
                (FindingKind::Unused, Severity::Low)
            ]
        );
        assert_eq!(found[0].reason, "last run 2mo ago");
        assert_eq!(found[1].reason, "never run; created 3mo ago");
    }

    #[test]
    fn failing_runs_scale_with_their_share() {
        let runs = |completed: u64, failed: u64| json!({"runs": {"completedCount": completed, "failedCount": failed, "count": completed + failed}});
        let found = find_all(
            &[
                agent("a", runs(1, 3)),
                agent("b", runs(3, 1)),
                agent("c", runs(9, 1)),
            ],
            clock(),
        );

        assert_eq!(
            kinds(&found),
            vec![
                (FindingKind::FailingRuns, Severity::High),
                (FindingKind::FailingRuns, Severity::Medium)
            ]
        );
        assert_eq!(
            found[0].reason,
            "3 of 4 finished runs failed in the last 30 days"
        );
    }

    #[test]
    fn spend_far_above_the_median_is_an_outlier() {
        let spend = |cost: &str| json!({"runs": {"costUsd": cost}});
        let found = find_all(
            &[
                agent("a", spend("1.00")),
                agent("b", spend("1.20")),
                agent("c", spend("0.90")),
                agent("d", spend("9.50")),
                agent("e", spend("0")),
            ],
            clock(),
        );

        assert_eq!(
            kinds(&found),
            vec![(FindingKind::SpendOutlier, Severity::Medium)]
        );
        assert_eq!(found[0].agent, "agent-d");
        assert!(found[0]
            .reason
            .starts_with("spent $9.50 in the last 30 days, over $3.30"));
    }

    #[test]
    fn too_few_spenders_make_no_outlier() {
        let spend = |cost: &str| json!({"runs": {"costUsd": cost}});
        let found = find_all(
            &[agent("a", spend("0.01")), agent("b", spend("50"))],
            clock(),
        );
        assert!(found.is_empty());
    }

    #[test]
    fn sub_agents_and_team_members_with_more_access_than_the_parent_are_flagged() {
        let parent = agent(
            "p",
            json!({
                "tools": [tool("get_datasets", "datasets", false, false, false)],
                "subAgents": [{"agentId": "w", "alias": "writer"}, {"agentId": "gone", "alias": "lost"}],
                "team": {"mode": "orchestrator", "members": [{"agentId": "r", "alias": "reader"}]},
            }),
        );
        let writer = agent(
            "w",
            json!({"tools": [
                tool("get_datasets", "datasets", false, false, false),
                tool("save_dataset", "datasets", true, true, true),
                tool("web_fetch", "web", false, false, false),
            ]}),
        );
        let reader = agent(
            "r",
            json!({"tools": [tool("get_distinct_values", "data_provider", false, false, false)]}),
        );

        let found: Vec<Finding> = find_all(&[parent, writer, reader], clock())
            .into_iter()
            .filter(|f| f.agent == "agent-p")
            .collect();

        assert_eq!(
            kinds(&found),
            vec![
                (FindingKind::SubAgentBroader, Severity::High),
                (FindingKind::MissingSubAgent, Severity::Medium),
                (FindingKind::SubAgentBroader, Severity::Medium),
            ]
        );
        assert_eq!(
            found[0].reason,
            "sub-agent writer (agent-w) can use what agent-p can't: save_dataset, web_fetch"
        );
    }

    #[test]
    fn finding_labels_match_the_flag_values() {
        assert_eq!(
            FindingKind::WriteWithoutApproval.label(),
            "write-without-approval"
        );
        assert_eq!(
            serde_json::to_value(FindingKind::SubAgentBroader).unwrap(),
            json!("sub-agent-broader")
        );
    }
}
