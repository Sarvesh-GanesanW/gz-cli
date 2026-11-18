//! The audit's table rows and JSON document.

use serde_json::{json, Map, Value};

use super::findings::{Finding, Severity};
use super::model::{AgentRecord, McpStatus, OwnerStatus, ToolKind};
use crate::tui::format::{date_time, epoch_ms, relative_time};

/// Most telling first: narrow terminals hide columns from the end.
pub const AGENT_COLUMNS: [&str; 14] = [
    "name",
    "owner",
    "findings",
    "runs",
    "tools",
    "approval",
    "mcp",
    "delegates",
    "model",
    "connections",
    "limits",
    "created",
    "changed",
    "id",
];

pub const FINDING_COLUMNS: [&str; 4] = ["severity", "agent", "finding", "reason"];

/// One agent's table row; `raw` is its server record, for dates and names.
pub fn agent_row(agent: &AgentRecord, raw: &Value, findings: &[&Finding], now_ms: i64) -> Value {
    json!({
        "name": agent.name,
        "id": agent.id,
        "owner": owner_cell(agent),
        "created": date_cell(agent.created_at.as_deref()),
        "changed": date_cell(raw["updatedAt"].as_str()),
        "model": model_cell(agent),
        "tools": tools_cell(agent),
        "mcp": mcp_cell(agent),
        "connections": connections_cell(agent, raw),
        "delegates": delegates_cell(agent),
        "approval": approval_cell(agent),
        "limits": limits_cell(agent),
        "runs": runs_cell(agent, now_ms),
        "findings": findings_cell(findings),
    })
}

pub fn finding_row(finding: &Finding) -> Value {
    json!({
        "severity": finding.severity.label(),
        "agent": finding.agent,
        "finding": finding.finding.label(),
        "reason": finding.reason,
    })
}

/// The `-o json` document: every server record with its findings attached.
pub fn document(
    raw_agents: &[&Value],
    findings: &[&Finding],
    runs_from: &str,
    window_days: u32,
) -> Value {
    let agents: Vec<Value> = raw_agents
        .iter()
        .map(|raw| {
            let mut record = raw.as_object().cloned().unwrap_or_else(Map::new);
            let own: Vec<&&Finding> = findings
                .iter()
                .filter(|f| raw["id"].as_str() == Some(f.agent_id.as_str()))
                .collect();
            record.insert("findings".to_string(), json!(own));
            Value::Object(record)
        })
        .collect();
    json!({
        "runsFrom": runs_from,
        "windowDays": window_days,
        "summary": summary(agents.len(), findings),
        "agents": agents,
        "findings": findings,
    })
}

fn summary(agent_count: usize, findings: &[&Finding]) -> Value {
    let count = |severity: Severity| findings.iter().filter(|f| f.severity == severity).count();
    json!({
        "agents": agent_count,
        "high": count(Severity::High),
        "medium": count(Severity::Medium),
        "low": count(Severity::Low),
    })
}

/// "3 agents · 1 high, 2 medium, 0 low".
pub fn summary_line(agent_count: usize, findings: &[&Finding]) -> String {
    let counts = summary(agent_count, findings);
    format!(
        "{} agent{} · {} high, {} medium, {} low",
        agent_count,
        if agent_count == 1 { "" } else { "s" },
        counts["high"],
        counts["medium"],
        counts["low"]
    )
}

fn owner_cell(agent: &AgentRecord) -> String {
    let owner = agent.created_by.as_deref().unwrap_or("none");
    match agent.owner_status {
        OwnerStatus::Active => owner.to_string(),
        OwnerStatus::None => "none".to_string(),
        OwnerStatus::Missing => format!("{owner} (gone)"),
        OwnerStatus::Disabled => format!("{owner} (disabled)"),
        OwnerStatus::Unknown => format!("{owner} (unchecked)"),
    }
}

fn date_cell(text: Option<&str>) -> Value {
    text.and_then(|t| epoch_ms(&json!(t)))
        .map_or(Value::Null, |ms| json!(date_time(ms)))
}

fn model_cell(agent: &AgentRecord) -> String {
    let model = agent.model.model_id.as_deref().unwrap_or("default");
    match agent.model.connection_name.as_deref() {
        Some(connection) => format!(
            "{model} via {} connection {connection}",
            agent.model.provider
        ),
        None => model.to_string(),
    }
}

fn tools_cell(agent: &AgentRecord) -> Value {
    let built_in = agent
        .tools
        .iter()
        .filter(|t| t.kind != ToolKind::Client)
        .count();
    let client: Vec<&str> = agent
        .tools
        .iter()
        .filter(|t| t.kind == ToolKind::Client)
        .map(|t| t.name.as_str())
        .collect();
    let writes = agent.tools.iter().filter(|t| t.is_mutating).count();
    if built_in == 0 && client.is_empty() {
        return Value::Null;
    }
    let mut parts = vec![format!("{built_in} built-in ({writes} write)")];
    if !client.is_empty() {
        parts.push(format!("client: {}", client.join(", ")));
    }
    json!(parts.join(", "))
}

fn mcp_cell(agent: &AgentRecord) -> Value {
    if agent.mcp_connections.is_empty() {
        return Value::Null;
    }
    let cells: Vec<String> = agent
        .mcp_connections
        .iter()
        .map(|mcp| {
            let scope = match (mcp.status, &mcp.allowed_tools) {
                (McpStatus::Ok, None) => "all tools".to_string(),
                (McpStatus::Ok, Some(tools)) => format!("{} tools", tools.len()),
                (McpStatus::Missing, _) => "missing".to_string(),
                (McpStatus::NotMcp, _) => "not MCP".to_string(),
                (McpStatus::Invalid, _) => "invalid".to_string(),
            };
            format!("{}: {scope}", mcp.name)
        })
        .collect();
    json!(cells.join(", "))
}

/// Connections the definition names, plus a reminder that data tools reach
/// whatever the person running the agent can reach.
fn connections_cell(agent: &AgentRecord, raw: &Value) -> Value {
    let mut parts: Vec<String> = raw["dataConnections"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| {
            Some(format!(
                "{} {}",
                connection_use(c["use"].as_str()?),
                c["name"].as_str()?
            ))
        })
        .collect();
    if agent.tools.iter().any(|t| t.reads_site_data()) {
        parts.push("+ runner's connections via data tools".to_string());
    }
    if parts.is_empty() {
        Value::Null
    } else {
        json!(parts.join(", "))
    }
}

fn connection_use(code: &str) -> &str {
    match code {
        "history" => "chat history",
        "pdfParser" => "PDF parser",
        other => other,
    }
}

fn delegates_cell(agent: &AgentRecord) -> Value {
    let aliases = |members: &[super::model::Delegate]| {
        members
            .iter()
            .map(|d| d.alias.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    match (&agent.team, agent.sub_agents.is_empty()) {
        (Some(team), _) => json!(format!("team {}: {}", team.mode, aliases(&team.members))),
        (None, false) => json!(format!("sub-agents: {}", aliases(&agent.sub_agents))),
        (None, true) => Value::Null,
    }
}

fn approval_cell(agent: &AgentRecord) -> Value {
    let asks: Vec<&str> = agent
        .tools
        .iter()
        .filter(|t| t.requires_approval)
        .map(|t| t.name.as_str())
        .collect();
    let unreviewed = agent.tools.iter().filter(|t| t.writes_unreviewed()).count();
    if asks.is_empty() && unreviewed == 0 {
        return Value::Null;
    }
    json!(format!(
        "asks for {} tool{}, {} write{} without asking",
        asks.len(),
        if asks.len() == 1 { "" } else { "s" },
        unreviewed,
        if unreviewed == 1 { "" } else { "s" }
    ))
}

fn limits_cell(agent: &AgentRecord) -> Value {
    let limits = &agent.limits;
    let mut parts = Vec::new();
    if let Some(per_run) = limits.max_run_tree_spend_usd {
        parts.push(format!("${per_run:.2}/run"));
    }
    if let Some(cents) = limits.quota_limit_cents {
        let used = limits.quota_spend_usd.unwrap_or(0.0);
        parts.push(format!(
            "quota ${:.2} (${used:.2} used)",
            cents as f64 / 100.0
        ));
    }
    if parts.is_empty() {
        Value::Null
    } else {
        json!(parts.join(", "))
    }
}

fn runs_cell(agent: &AgentRecord, now_ms: i64) -> String {
    let runs = &agent.runs;
    let mut text = format!(
        "{} ({} failed), ${:.2}",
        runs.count, runs.failed_count, runs.cost_usd
    );
    if runs.child_run_count > 0 {
        text.push_str(&format!(" + {} as a sub-agent", runs.child_run_count));
    }
    if runs.unpriced_run_count > 0 {
        text.push_str(&format!(" + {} unpriced", runs.unpriced_run_count));
    }
    let last = runs
        .last_run_at
        .as_deref()
        .and_then(|t| epoch_ms(&json!(t)))
        .map_or_else(
            || "never run".to_string(),
            |ms| format!("last {}", relative_time(ms, now_ms)),
        );
    format!("{text}, {last}")
}

fn findings_cell(findings: &[&Finding]) -> Value {
    if findings.is_empty() {
        return Value::Null;
    }
    let parts: Vec<String> = [Severity::High, Severity::Medium, Severity::Low]
        .into_iter()
        .filter_map(|severity| {
            let count = findings.iter().filter(|f| f.severity == severity).count();
            (count > 0).then(|| format!("{count} {}", severity.label()))
        })
        .collect();
    json!(parts.join(", "))
}
