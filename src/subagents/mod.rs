//! Subagents: helper agents a chat run (the `task` tool) or an
//! agent-builder workflow starts while the main agent waits. Both services
//! stream the same events (`subagent_*`, `node_*`, `handoff`; see
//! ORCHESTRATION_CONTRACT.md), so every front end decodes them with
//! [`SubagentEvent::parse`], tracks them on a [`Board`], and draws them with
//! [`render`].

pub mod render;

use std::time::{Duration, Instant};

use serde_json::Value;

/// How a subagent's tool call is going.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolState {
    Started,
    Finished,
    Failed,
}

impl ToolState {
    fn parse(text: Option<&str>) -> Self {
        match text {
            Some("finished") => Self::Finished,
            Some("failed") => Self::Failed,
            _ => Self::Started,
        }
    }
}

/// Where a subagent (or workflow node) stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Running,
    Completed,
    Failed,
    Cancelled,
    /// A workflow node whose condition didn't match.
    Skipped,
}

impl Status {
    fn parse(text: Option<&str>) -> Self {
        match text {
            Some("completed") => Self::Completed,
            Some("failed") => Self::Failed,
            Some("cancelled") => Self::Cancelled,
            Some("skipped") => Self::Skipped,
            _ => Self::Running,
        }
    }

    pub fn is_done(self) -> bool {
        self != Self::Running
    }

    pub fn word(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Skipped => "skipped",
        }
    }
}

/// Tokens and spend one subagent reported.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SubagentUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: Option<f64>,
}

impl SubagentUsage {
    pub fn from_value(data: &Value) -> Self {
        let cost_usd = match &data["costUsd"] {
            Value::String(text) => text.parse().ok(),
            Value::Number(n) => n.as_f64(),
            _ => None,
        };
        Self {
            input_tokens: data["inputTokens"].as_u64().unwrap_or(0),
            output_tokens: data["outputTokens"].as_u64().unwrap_or(0),
            cost_usd,
        }
    }
}

/// One orchestration event, decoded.
#[derive(Debug, Clone, PartialEq)]
pub enum SubagentEvent {
    Started {
        id: String,
        name: String,
        profile: Option<String>,
        /// Set when the subagent is a builder agent (agents calling agents,
        /// workflow nodes).
        agent_id: Option<String>,
    },
    Tool {
        id: String,
        tool: String,
        state: ToolState,
    },
    /// Progress text from the subagent; never part of the main answer.
    Text { id: String, text: String },
    Finished {
        id: String,
        status: Status,
        summary: String,
        usage: SubagentUsage,
        duration: Option<Duration>,
    },
    NodeStarted {
        node_id: String,
        id: Option<String>,
        name: String,
    },
    NodeFinished {
        node_id: String,
        id: Option<String>,
        status: Status,
    },
    Handoff {
        from: String,
        to: String,
        reason: Option<String>,
    },
}

fn text(data: &Value, key: &str) -> Option<String> {
    data[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

impl SubagentEvent {
    /// Decodes an event of type `kind`; `None` for anything else.
    pub fn parse(kind: &str, data: &Value) -> Option<Self> {
        let id = || text(data, "subagentId");
        Some(match kind {
            "subagent_started" => Self::Started {
                id: id()?,
                name: text(data, "name").unwrap_or_else(|| "Task".to_string()),
                profile: text(data, "profile"),
                agent_id: text(data, "agentId"),
            },
            "subagent_tool" => Self::Tool {
                id: id()?,
                tool: text(data, "toolName").unwrap_or_else(|| "tool".to_string()),
                state: ToolState::parse(data["status"].as_str()),
            },
            "subagent_text" => Self::Text {
                id: id()?,
                text: data["text"].as_str().unwrap_or_default().to_string(),
            },
            "subagent_finished" => Self::Finished {
                id: id()?,
                status: match Status::parse(data["status"].as_str()) {
                    // A finish without a recognised status still finished.
                    Status::Running => Status::Completed,
                    status => status,
                },
                summary: data["summary"].as_str().unwrap_or_default().to_string(),
                usage: SubagentUsage::from_value(&data["usage"]),
                duration: data["durationMs"].as_u64().map(Duration::from_millis),
            },
            "node_started" => Self::NodeStarted {
                node_id: text(data, "nodeId")?,
                id: id(),
                name: text(data, "name").unwrap_or_default(),
            },
            "node_finished" => Self::NodeFinished {
                node_id: text(data, "nodeId")?,
                id: id(),
                status: Status::parse(data["status"].as_str()),
            },
            "handoff" => Self::Handoff {
                from: text(data, "fromNodeId")?,
                to: text(data, "toNodeId")?,
                reason: text(data, "reason"),
            },
            _ => return None,
        })
    }
}

/// The subagent an interrupt or client-tool request came from, if any.
pub fn asked_by(data: &Value) -> Option<String> {
    text(data, "subagentId")
}

/// Per-subagent figures from a run's `response_metrics.subagents`.
pub fn usage_by_subagent(metrics: &Value) -> Vec<(String, SubagentUsage)> {
    metrics["subagents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| Some((text(row, "subagentId")?, SubagentUsage::from_value(row))))
        .collect()
}

/// One tool call a subagent made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub name: String,
    pub state: ToolState,
}

/// Everything known about one subagent.
#[derive(Debug, Clone, PartialEq)]
pub struct Subagent {
    pub id: String,
    pub name: String,
    pub profile: Option<String>,
    pub agent_id: Option<String>,
    /// The workflow node it runs, for workflow runs.
    pub node_id: Option<String>,
    pub status: Status,
    pub tools: Vec<ToolCall>,
    /// Progress text, shown only in the expanded transcript.
    pub text: String,
    pub summary: String,
    pub usage: SubagentUsage,
    pub started: Instant,
    /// Server-reported run time once finished; live elapsed before that.
    pub duration: Option<Duration>,
}

impl Subagent {
    fn new(id: &str, name: &str, now: Instant) -> Self {
        Self {
            id: id.to_string(),
            name: name.to_string(),
            profile: None,
            agent_id: None,
            node_id: None,
            status: Status::Running,
            tools: Vec::new(),
            text: String::new(),
            summary: String::new(),
            usage: SubagentUsage::default(),
            started: now,
            duration: None,
        }
    }

    pub fn elapsed(&self, now: Instant) -> Duration {
        self.duration
            .unwrap_or_else(|| now.saturating_duration_since(self.started))
    }

    /// The tool it's running now, if any.
    pub fn current_tool(&self) -> Option<&str> {
        self.tools
            .iter()
            .rev()
            .find(|t| t.state == ToolState::Started)
            .map(|t| t.name.as_str())
    }
}

/// A swarm handoff between workflow nodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handoff {
    pub from: String,
    pub to: String,
    pub reason: Option<String>,
}

/// The subagents of one turn or workflow run, in start order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Board {
    pub agents: Vec<Subagent>,
    pub handoffs: Vec<Handoff>,
}

impl Board {
    pub fn get(&self, id: &str) -> Option<&Subagent> {
        self.agents.iter().find(|a| a.id == id)
    }

    /// The display name of `id`, falling back to the id itself.
    pub fn name_of(&self, id: &str) -> String {
        self.get(id)
            .map(|a| a.name.clone())
            .unwrap_or_else(|| id.to_string())
    }

    pub fn running(&self) -> impl Iterator<Item = &Subagent> {
        self.agents.iter().filter(|a| !a.status.is_done())
    }

    fn entry(&mut self, id: &str, name: &str, now: Instant) -> &mut Subagent {
        let index = match self.agents.iter().position(|a| a.id == id) {
            Some(i) => i,
            None => {
                self.agents.push(Subagent::new(id, name, now));
                self.agents.len() - 1
            }
        };
        &mut self.agents[index]
    }

    /// Records `event`. Returns the subagent it finished, if it finished one,
    /// so front ends can print it once.
    pub fn apply(&mut self, event: &SubagentEvent, now: Instant) -> Option<Subagent> {
        match event {
            SubagentEvent::Started {
                id,
                name,
                profile,
                agent_id,
            } => {
                let agent = self.entry(id, name, now);
                agent.name = name.clone();
                agent.profile = profile.clone();
                agent.agent_id = agent_id.clone();
                None
            }
            SubagentEvent::Tool { id, tool, state } => {
                let agent = self.entry(id, id, now);
                match agent
                    .tools
                    .iter_mut()
                    .rev()
                    .find(|t| t.name == *tool && t.state == ToolState::Started)
                {
                    Some(call) if *state != ToolState::Started => call.state = *state,
                    _ => agent.tools.push(ToolCall {
                        name: tool.clone(),
                        state: *state,
                    }),
                }
                None
            }
            SubagentEvent::Text { id, text } => {
                self.entry(id, id, now).text.push_str(text);
                None
            }
            SubagentEvent::Finished {
                id,
                status,
                summary,
                usage,
                duration,
            } => {
                let agent = self.entry(id, id, now);
                agent.status = *status;
                agent.summary = summary.clone();
                agent.usage = *usage;
                agent.duration =
                    Some(duration.unwrap_or_else(|| now.saturating_duration_since(agent.started)));
                Some(agent.clone())
            }
            SubagentEvent::NodeStarted { node_id, id, name } => {
                let key = id.clone().unwrap_or_else(|| format!("node:{node_id}"));
                let label = if name.is_empty() { node_id } else { name };
                let agent = self.entry(&key, label, now);
                agent.node_id = Some(node_id.clone());
                None
            }
            SubagentEvent::NodeFinished {
                node_id,
                id,
                status,
            } => {
                let key = id.clone().unwrap_or_else(|| format!("node:{node_id}"));
                let agent = self.entry(&key, node_id, now);
                agent.node_id = Some(node_id.clone());
                // The node's own subagent_finished may already have said more.
                if agent.status.is_done() {
                    return None;
                }
                agent.status = *status;
                agent.duration = Some(now.saturating_duration_since(agent.started));
                Some(agent.clone())
            }
            SubagentEvent::Handoff { from, to, reason } => {
                self.handoffs.push(Handoff {
                    from: from.clone(),
                    to: to.clone(),
                    reason: reason.clone(),
                });
                None
            }
        }
    }

    /// Folds `response_metrics.subagents` figures into the board, which win
    /// over what the finish events carried.
    pub fn record_usage(&mut self, rows: &[(String, SubagentUsage)]) {
        for (id, usage) in rows {
            if let Some(agent) = self.agents.iter_mut().find(|a| a.id == *id) {
                agent.usage = *usage;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(value: Value) -> SubagentEvent {
        let kind = value["type"].as_str().unwrap().to_string();
        SubagentEvent::parse(&kind, &value).unwrap()
    }

    #[test]
    fn parses_every_event_kind() {
        assert_eq!(
            parse(
                json!({"type":"subagent_started","subagentId":"sa_1","name":"Check logs","profile":"data_engineer","agentId":null})
            ),
            SubagentEvent::Started {
                id: "sa_1".into(),
                name: "Check logs".into(),
                profile: Some("data_engineer".into()),
                agent_id: None,
            }
        );
        assert_eq!(
            parse(
                json!({"type":"subagent_tool","subagentId":"sa_1","toolName":"get_job_logs","status":"failed"})
            ),
            SubagentEvent::Tool {
                id: "sa_1".into(),
                tool: "get_job_logs".into(),
                state: ToolState::Failed,
            }
        );
        let finished = parse(
            json!({"type":"subagent_finished","subagentId":"sa_1","status":"completed",
            "summary":"Job 4 failed on a null key","usage":{"inputTokens":900,"outputTokens":120,"costUsd":"0.0021"},"durationMs":12000}),
        );
        assert_eq!(
            finished,
            SubagentEvent::Finished {
                id: "sa_1".into(),
                status: Status::Completed,
                summary: "Job 4 failed on a null key".into(),
                usage: SubagentUsage {
                    input_tokens: 900,
                    output_tokens: 120,
                    cost_usd: Some(0.0021)
                },
                duration: Some(Duration::from_secs(12)),
            }
        );
        assert!(matches!(
            parse(json!({"type":"node_finished","nodeId":"extract","status":"skipped"})),
            SubagentEvent::NodeFinished {
                status: Status::Skipped,
                ..
            }
        ));
        assert!(matches!(
            parse(
                json!({"type":"handoff","fromNodeId":"triage","toNodeId":"billing","reason":"refund"})
            ),
            SubagentEvent::Handoff { .. }
        ));
        assert!(SubagentEvent::parse("llm_chunk", &json!({})).is_none());
        assert!(
            SubagentEvent::parse("subagent_tool", &json!({"toolName":"x"})).is_none(),
            "events without a subagent id are dropped"
        );
    }

    #[test]
    fn board_tracks_parallel_subagents() {
        let now = Instant::now();
        let mut board = Board::default();
        for (id, name) in [("a", "Logs"), ("b", "Schema")] {
            board.apply(
                &SubagentEvent::Started {
                    id: id.into(),
                    name: name.into(),
                    profile: Some("analyst".into()),
                    agent_id: None,
                },
                now,
            );
        }
        board.apply(
            &SubagentEvent::Tool {
                id: "a".into(),
                tool: "get_job_logs".into(),
                state: ToolState::Started,
            },
            now,
        );
        assert_eq!(board.running().count(), 2);
        assert_eq!(board.get("a").unwrap().current_tool(), Some("get_job_logs"));
        board.apply(
            &SubagentEvent::Tool {
                id: "a".into(),
                tool: "get_job_logs".into(),
                state: ToolState::Finished,
            },
            now,
        );
        assert_eq!(
            board.get("a").unwrap().tools.len(),
            1,
            "finish updates the call"
        );
        let done = board
            .apply(
                &SubagentEvent::Finished {
                    id: "b".into(),
                    status: Status::Failed,
                    summary: "no access".into(),
                    usage: SubagentUsage::default(),
                    duration: None,
                },
                now + Duration::from_secs(3),
            )
            .unwrap();
        assert_eq!(done.status, Status::Failed);
        assert_eq!(done.duration, Some(Duration::from_secs(3)));
        assert_eq!(board.running().count(), 1);
        assert_eq!(board.name_of("b"), "Schema");
    }

    #[test]
    fn workflow_nodes_reuse_their_subagent() {
        let now = Instant::now();
        let mut board = Board::default();
        board.apply(
            &SubagentEvent::NodeStarted {
                node_id: "extract".into(),
                id: Some("sa_9".into()),
                name: "Extractor".into(),
            },
            now,
        );
        board.apply(
            &SubagentEvent::Started {
                id: "sa_9".into(),
                name: "Extractor".into(),
                profile: None,
                agent_id: Some("agent-1".into()),
            },
            now,
        );
        assert_eq!(board.agents.len(), 1);
        assert_eq!(board.agents[0].node_id.as_deref(), Some("extract"));
        let finished = board.apply(
            &SubagentEvent::NodeFinished {
                node_id: "extract".into(),
                id: Some("sa_9".into()),
                status: Status::Completed,
            },
            now,
        );
        assert!(finished.is_some());
    }

    #[test]
    fn metrics_usage_wins_and_interrupts_name_their_subagent() {
        let mut board = Board::default();
        board.apply(
            &SubagentEvent::Started {
                id: "sa_1".into(),
                name: "Logs".into(),
                profile: None,
                agent_id: None,
            },
            Instant::now(),
        );
        let rows = usage_by_subagent(
            &json!({"subagents":[{"subagentId":"sa_1","inputTokens":10,"outputTokens":5,"costUsd":0.5}]}),
        );
        board.record_usage(&rows);
        assert_eq!(board.agents[0].usage.output_tokens, 5);
        assert_eq!(
            asked_by(&json!({"subagentId":"sa_1"})).as_deref(),
            Some("sa_1")
        );
        assert_eq!(asked_by(&json!({})), None);
    }
}
