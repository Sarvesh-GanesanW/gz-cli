//! Agents calling agents (ORCHESTRATION_CONTRACT.md Phase 2): editing an
//! agent's `subAgents`, and showing the `subagent_*` events its runs emit
//! nested under the parent's answer.

use anyhow::{bail, Result};
use owo_colors::OwoColorize;
use regex::Regex;
use serde_json::{json, Value};
use std::io::Write;

use crate::cmd::flow_run::{compact, format_duration};
use crate::cmd::stream::{self, Outcome, RunEvent};
use crate::cmd::Runtime;
use crate::output::OutputFormat;
use crate::sse::{Event, Flow};

/// Most sub-agents one agent may have.
pub const MAX_SUB_AGENTS: usize = 8;

/// Aliases become tool names (`call_<alias>`), so they follow the agent
/// builder's key rule.
const ALIAS_PATTERN: &str = r"^[a-z][a-z0-9_]{0,39}$";

/// The agent's sub-agent list with `entry` appended, or why it can't be.
pub fn add_sub_agent(current: &[Value], parent: &str, entry: Value) -> Result<Vec<Value>> {
    let alias = entry["alias"].as_str().unwrap_or_default();
    let child = entry["agentId"].as_str().unwrap_or_default();
    let pattern = Regex::new(ALIAS_PATTERN).expect("valid alias pattern");
    if !pattern.is_match(alias) {
        bail!("alias '{alias}' must be lowercase letters, digits, and _ (it becomes the tool call_{alias})");
    }
    if child == parent {
        bail!("an agent can't be its own sub-agent");
    }
    if current.iter().any(|s| s["alias"].as_str() == Some(alias)) {
        bail!("this agent already has a sub-agent called {alias}");
    }
    if current.iter().any(|s| s["agentId"].as_str() == Some(child)) {
        bail!("agent {child} is already a sub-agent of this agent");
    }
    if current.len() >= MAX_SUB_AGENTS {
        bail!("an agent can have at most {MAX_SUB_AGENTS} sub-agents");
    }
    let mut next = current.to_vec();
    next.push(entry);
    Ok(next)
}

/// The list without the sub-agent named by alias (or agent id).
pub fn remove_sub_agent(current: &[Value], key: &str) -> Result<Vec<Value>> {
    let matches =
        |s: &Value| s["alias"].as_str() == Some(key) || s["agentId"].as_str() == Some(key);
    if !current.iter().any(matches) {
        let known: Vec<&str> = current.iter().filter_map(|s| s["alias"].as_str()).collect();
        bail!(
            "no sub-agent called {key}{}",
            if known.is_empty() {
                String::new()
            } else {
                format!(" (have: {})", known.join(", "))
            }
        );
    }
    Ok(current.iter().filter(|s| !matches(s)).cloned().collect())
}

pub fn new_entry(child: &str, alias: &str, description: Option<&str>) -> Value {
    let mut entry = json!({"agentId": child, "alias": alias});
    if let Some(description) = description {
        entry["description"] = json!(description);
    }
    entry
}

/// One sub-agent event, decoded (the contract's `subagent_*` types).
#[derive(Debug, Clone, PartialEq)]
pub enum SubagentEvent {
    Started {
        id: String,
        name: String,
        profile: Option<String>,
    },
    Tool {
        id: String,
        tool: String,
        started: bool,
    },
    Finished {
        id: String,
        status: String,
        summary: String,
        input_tokens: u64,
        output_tokens: u64,
        duration_ms: Option<u64>,
    },
}

impl SubagentEvent {
    /// `None` for anything that isn't a sub-agent event (including
    /// `subagent_text`, which is progress only).
    pub fn parse(event: &Event) -> Option<Self> {
        let data = event.json()?;
        let kind = data["type"].as_str().or(event.event.as_deref())?;
        let text = |key: &str| data[key].as_str().map(str::to_string);
        let id = text("subagentId").unwrap_or_default();
        Some(match kind {
            "subagent_started" => Self::Started {
                id,
                name: text("name").unwrap_or_else(|| "sub-agent".to_string()),
                profile: text("profile"),
            },
            "subagent_tool" => Self::Tool {
                id,
                tool: text("toolName").unwrap_or_else(|| "tool".to_string()),
                started: data["status"].as_str().is_none_or(|s| s == "started"),
            },
            "subagent_finished" => Self::Finished {
                id,
                status: text("status").unwrap_or_else(|| "completed".to_string()),
                summary: text("summary").unwrap_or_default(),
                input_tokens: data["usage"]["inputTokens"].as_u64().unwrap_or(0),
                output_tokens: data["usage"]["outputTokens"].as_u64().unwrap_or(0),
                duration_ms: data["durationMs"].as_u64(),
            },
            _ => return None,
        })
    }
}

/// Names of started sub-agents, so later events can say who they're about.
#[derive(Debug, Default)]
pub struct Names(Vec<(String, String)>);

impl Names {
    fn of(&self, id: &str) -> String {
        self.0
            .iter()
            .find(|(known, _)| known == id)
            .map(|(_, name)| name.clone())
            .unwrap_or_else(|| id.to_string())
    }
}

/// The nested line an event prints under the parent's answer.
pub fn line(names: &mut Names, event: &SubagentEvent) -> Option<String> {
    match event {
        SubagentEvent::Started { id, name, profile } => {
            names.0.push((id.clone(), name.clone()));
            Some(match profile {
                Some(profile) => format!("  ● {name} · {profile}"),
                None => format!("  ● {name}"),
            })
        }
        SubagentEvent::Tool { id, tool, started } => {
            started.then(|| format!("    ⎿  {} · {tool}", names.of(id)))
        }
        SubagentEvent::Finished {
            id,
            status,
            summary,
            input_tokens,
            output_tokens,
            duration_ms,
        } => {
            let symbol = match status.as_str() {
                "completed" => "✓",
                "cancelled" => "■",
                _ => "✗",
            };
            let mut parts = vec![format!("  {symbol} {}", names.of(id))];
            if let Some(ms) = duration_ms {
                parts.push(format_duration(*ms));
            }
            if input_tokens + output_tokens > 0 {
                parts.push(format!(
                    "↑{} ↓{}",
                    compact(*input_tokens),
                    compact(*output_tokens)
                ));
            }
            let first = summary.lines().next().unwrap_or_default().trim();
            if !first.is_empty() {
                parts.push(first.to_string());
            }
            Some(parts.join(" · "))
        }
    }
}

/// Streams an agent run like `stream::follow`, plus nested sub-agent lines.
pub async fn follow(rt: &Runtime, service: &str, events_path: &str) -> Result<Outcome> {
    let mut printer = Printer::new(rt.format);
    let mut last_id = None;
    stream::read_run(
        &rt.http,
        service,
        events_path,
        &mut last_id,
        |event, parsed| printer.handle(event, parsed),
    )
    .await?;
    printer.finish();
    Ok(printer.outcome)
}

struct Printer {
    format: OutputFormat,
    names: Names,
    mid_line: bool,
    outcome: Outcome,
}

impl Printer {
    fn new(format: OutputFormat) -> Self {
        Self {
            format,
            names: Names::default(),
            mid_line: false,
            outcome: Outcome::default(),
        }
    }

    fn handle(&mut self, event: &Event, parsed: RunEvent) -> Result<Flow> {
        if self.format != OutputFormat::Table {
            if let Some(line) = event.json() {
                println!("{}", serde_json::to_string(&line)?);
            }
        }
        if let Some(sub) = SubagentEvent::parse(event) {
            if let Some(text) = line(&mut self.names, &sub) {
                self.note(&text);
            }
            return Ok(Flow::Continue);
        }
        match parsed {
            RunEvent::Text(text) => self.text(&text),
            RunEvent::ServerTool(name) => self.note(&format!("⚙ {name}")),
            RunEvent::ToolError(why) => self.note(&format!("⚠ tool failed: {why}")),
            RunEvent::Warning(why) => self.note(&format!("⚠ {why}")),
            RunEvent::Approval(data) | RunEvent::ClientToolRequest(data) => {
                self.outcome.interrupt = Some(data)
            }
            RunEvent::Error(why) => self.outcome.error = Some(why),
            RunEvent::Cancelled => self.outcome.status = Some("cancelled".to_string()),
            RunEvent::Status(status) => {
                let stop = RunEvent::is_terminal_status(&status) && status != "completed";
                self.outcome.status = Some(status);
                if stop {
                    return Ok(Flow::Stop);
                }
            }
            RunEvent::Usage(_) | RunEvent::Reconnect | RunEvent::Done | RunEvent::Other => {}
        }
        Ok(Flow::Continue)
    }

    fn text(&mut self, text: &str) {
        if self.format != OutputFormat::Table || text.is_empty() {
            return;
        }
        print!("{text}");
        std::io::stdout().flush().ok();
        self.mid_line = !text.ends_with('\n');
    }

    fn note(&mut self, note: &str) {
        if self.format != OutputFormat::Table {
            return;
        }
        if self.mid_line {
            println!();
            self.mid_line = false;
        }
        eprintln!("{}", note.dimmed());
    }

    fn finish(&mut self) {
        if self.format == OutputFormat::Table && self.mid_line {
            println!();
        }
    }
}

/// Rows for a run's `children` (child runs started by sub-agents).
pub fn children_rows(run: &Value) -> Vec<Value> {
    run["children"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|child| {
            json!({
                "runId": child["runId"],
                "agentId": child["agentId"],
                "status": child["status"],
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(data: Value) -> Event {
        Event {
            id: Some("1".into()),
            event: None,
            data: data.to_string(),
        }
    }

    #[test]
    fn adds_sub_agents_with_checks() {
        let current = vec![new_entry("child-a", "billing", Some("amounts"))];
        let next =
            add_sub_agent(&current, "parent", new_entry("child-b", "refunds", None)).unwrap();
        assert_eq!(next.len(), 2);
        assert_eq!(next[1], json!({"agentId": "child-b", "alias": "refunds"}));
        let err = |entry: Value| {
            add_sub_agent(&current, "parent", entry)
                .unwrap_err()
                .to_string()
        };
        assert!(err(new_entry("child-b", "Bad-Alias", None)).contains("lowercase"));
        assert!(err(new_entry("parent", "me", None)).contains("its own"));
        assert!(err(new_entry("child-c", "billing", None)).contains("already has"));
        assert!(err(new_entry("child-a", "other", None)).contains("already a sub-agent"));
        let full: Vec<Value> = (0..MAX_SUB_AGENTS)
            .map(|i| new_entry(&format!("c{i}"), &format!("a{i}"), None))
            .collect();
        assert!(add_sub_agent(&full, "p", new_entry("x", "x", None))
            .unwrap_err()
            .to_string()
            .contains("at most"));
    }

    #[test]
    fn removes_by_alias_or_agent_id() {
        let current = vec![
            new_entry("child-a", "billing", None),
            new_entry("child-b", "refunds", None),
        ];
        assert_eq!(remove_sub_agent(&current, "billing").unwrap().len(), 1);
        assert_eq!(
            remove_sub_agent(&current, "child-b").unwrap()[0]["alias"],
            "billing"
        );
        let err = remove_sub_agent(&current, "nope").unwrap_err().to_string();
        assert!(err.contains("have: billing, refunds"), "{err}");
    }

    #[test]
    fn sub_agent_events_print_nested_lines() {
        let mut names = Names::default();
        let started = SubagentEvent::parse(&event(
            json!({"type": "subagent_started", "subagentId": "sa_1",
            "name": "billing_expert", "agentId": "u-1"}),
        ))
        .unwrap();
        assert_eq!(
            line(&mut names, &started).as_deref(),
            Some("  ● billing_expert")
        );
        let tool = SubagentEvent::parse(&event(
            json!({"type": "subagent_tool", "subagentId": "sa_1",
            "toolName": "run_sql", "status": "started"}),
        ))
        .unwrap();
        assert_eq!(
            line(&mut names, &tool).as_deref(),
            Some("    ⎿  billing_expert · run_sql")
        );
        let finished_tool = SubagentEvent::parse(&event(
            json!({"type": "subagent_tool", "subagentId": "sa_1",
            "toolName": "run_sql", "status": "finished"}),
        ))
        .unwrap();
        assert!(line(&mut names, &finished_tool).is_none());
        let done = SubagentEvent::parse(&event(
            json!({"type": "subagent_finished", "subagentId": "sa_1",
            "status": "completed", "summary": "Refund approved\nmore", "durationMs": 900,
            "usage": {"inputTokens": 1200, "outputTokens": 40}}),
        ))
        .unwrap();
        assert_eq!(
            line(&mut names, &done).as_deref(),
            Some("  ✓ billing_expert · 900ms · ↑1.2k ↓40 · Refund approved")
        );
        assert!(SubagentEvent::parse(&event(
            json!({"type": "subagent_text", "subagentId": "sa_1", "text": "x"})
        ))
        .is_none());
        assert!(SubagentEvent::parse(&event(json!({"type": "llm_chunk", "text": "x"}))).is_none());
    }

    #[test]
    fn children_rows_keep_the_contract_fields() {
        let rows = children_rows(
            &json!({"children": [{"runId": "r2", "agentId": "u-1", "status": "completed", "extra": 1}]}),
        );
        assert_eq!(
            rows,
            vec![json!({"runId": "r2", "agentId": "u-1", "status": "completed"})]
        );
        assert!(children_rows(&json!({})).is_empty());
    }
}
