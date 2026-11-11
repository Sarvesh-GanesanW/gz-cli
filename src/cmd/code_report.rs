//! The report script mode prints for one `gz code "<prompt>" -o json|yaml`
//! run, built from the run's events and its approvers' decisions, and the
//! exit code that goes with it.

use std::collections::VecDeque;
use std::time::Instant;

use serde::Serialize;

use crate::assistant::{AssistantEvent, TurnEnd};
use crate::cmd::code_unattended::{Decided, DecisionLog};
use crate::cmd::stream::{TokenCounts, Usage};
use crate::output::OutputFormat;
use crate::spend::caps::CapReached;
use crate::subagents::{Status, SubagentEvent};

/// Exit codes, for scripts to branch on.
pub mod exit {
    pub const COMPLETED: i32 = 0;
    pub const FAILED: i32 = 1;
    /// Same as a one-shot run refused before sending (`CapReached`).
    pub const CAP_REACHED: i32 = crate::spend::caps::CapReached::EXIT_CODE;
    /// The run finished, but at least one action was refused because it
    /// needed a person's approval (or auto mode's review blocked it).
    pub const NEEDED_APPROVAL: i32 = 4;
    /// Stopped with Ctrl+C, as shells report SIGINT.
    pub const INTERRUPTED: i32 = 130;
}

/// What the decision word is for a tool the server ran on its own, which
/// gz was not asked about.
const SERVER: &str = "server";
/// A local tool that ran without a prompt (saved rule, `--yes-reads`, this
/// chat's grant, or a tool that needs none); the audit log's word.
const PREAPPROVED: &str = "preapproved";
const MICROS: f64 = 1_000_000.0;

/// Ends `gz code` with `code` once the report is printed; main exits with
/// it instead of printing an error (the report already holds it).
#[derive(Debug)]
pub struct ReportedExit(pub i32);

impl std::fmt::Display for ReportedExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "gz code ended with exit code {}", self.0)
    }
}

impl std::error::Error for ReportedExit {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    Completed,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub tool: String,
    pub summary: String,
    pub decision: String,
    pub is_error: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip)]
    refused: bool,
    /// Started and not yet finished, so a finish finds it.
    #[serde(skip)]
    open: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SubagentRun {
    pub name: String,
    pub status: &'static str,
    #[serde(skip)]
    id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageTotals {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_write_input_tokens: Option<u64>,
    pub cost_usd: Option<f64>,
}

impl UsageTotals {
    /// Adds one answer's figures; a figure stays `None` until some answer
    /// reports it.
    fn add(&mut self, usage: &Usage) {
        if let Some(tokens) = &usage.tokens {
            let mut sum = TokenCounts {
                input: self.input_tokens.unwrap_or(0),
                output: self.output_tokens.unwrap_or(0),
                cache_read: self.cache_read_input_tokens.unwrap_or(0),
                cache_write: self.cache_write_input_tokens.unwrap_or(0),
            };
            sum.add(tokens);
            self.input_tokens = Some(sum.input);
            self.output_tokens = Some(sum.output);
            self.cache_read_input_tokens = Some(sum.cache_read);
            self.cache_write_input_tokens = Some(sum.cache_write);
        }
        if let Some(cost) = usage.cost_usd {
            let sum = self.cost_usd.unwrap_or(0.0) + cost;
            // The server prices to the micro-dollar; summing floats must not
            // print 0.030000000000000002.
            self.cost_usd = Some((sum * MICROS).round() / MICROS);
        }
    }
}

/// The printed object. Field names match gz's other JSON (camelCase).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub conversation_id: String,
    pub model: Option<String>,
    pub answer: String,
    pub tool_calls: Vec<ToolCall>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub subagents: Vec<SubagentRun>,
    pub usage: UsageTotals,
    pub duration_ms: u64,
    pub status: RunStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Collects one run as it happens.
pub struct Report {
    conversation_id: String,
    model: Option<String>,
    started: Instant,
    decisions: DecisionLog,
    answer: String,
    /// A tool ran since the answer text last grew: new text starts a new
    /// answer, since the final answer is what follows the last tool.
    tool_since_text: bool,
    tool_calls: Vec<ToolCall>,
    /// Allowed local actions waiting for the event that shows them running.
    allowed_local: VecDeque<Decided>,
    /// Allowed Groundzero actions; the server shows them running, if at all,
    /// under a display name, so each is matched to the next server tool.
    allowed_server: VecDeque<Decided>,
    subagents: Vec<SubagentRun>,
    usage: UsageTotals,
}

impl Report {
    pub fn new(conversation_id: &str, model: Option<String>, decisions: DecisionLog) -> Self {
        Self {
            conversation_id: conversation_id.to_string(),
            model,
            started: Instant::now(),
            decisions,
            answer: String::new(),
            tool_since_text: false,
            tool_calls: Vec::new(),
            allowed_local: VecDeque::new(),
            allowed_server: VecDeque::new(),
            subagents: Vec::new(),
            usage: UsageTotals::default(),
        }
    }

    /// Where this run's approvers note their decisions.
    pub fn decisions(&self) -> DecisionLog {
        self.decisions.clone()
    }

    pub fn set_model(&mut self, model: Option<String>) {
        self.model = model;
    }

    /// Takes in one event. Decisions are made before the events that show
    /// their outcome, so they are read first.
    pub fn observe(&mut self, event: &AssistantEvent) {
        self.take_decisions();
        match event {
            AssistantEvent::Text(text) => self.answer_text(text),
            AssistantEvent::ToolStarted {
                name,
                summary,
                local,
            } => self.tool_started(name, summary, *local),
            AssistantEvent::ToolFinished {
                name,
                summary,
                is_error,
                ..
            } => self.tool_finished(name, summary, *is_error),
            AssistantEvent::Subagent(event) => self.subagent(event),
            AssistantEvent::Usage(usage) => self.usage.add(usage),
            // A subagent's text is its own progress, never the answer.
            AssistantEvent::ForSubagent { event, .. }
                if !matches!(**event, AssistantEvent::Text(_)) =>
            {
                self.observe(event)
            }
            _ => {}
        }
    }

    fn take_decisions(&mut self) {
        for decided in self.decisions.drain() {
            if decided.refused {
                self.tool_calls.push(refused_call(decided));
            } else if decided.server_action {
                self.allowed_server.push_back(decided);
            } else {
                self.allowed_local.push_back(decided);
            }
        }
    }

    fn answer_text(&mut self, text: &str) {
        if std::mem::take(&mut self.tool_since_text) {
            self.answer.clear();
        }
        self.answer.push_str(text);
    }

    fn tool_started(&mut self, name: &str, summary: &str, local: bool) {
        self.tool_since_text = true;
        let waiting = if local {
            &mut self.allowed_local
        } else {
            &mut self.allowed_server
        };
        let decision = match waiting.pop_front() {
            Some(decided) => decided.decision.to_string(),
            None if local => PREAPPROVED.to_string(),
            None => SERVER.to_string(),
        };
        self.tool_calls.push(ToolCall {
            tool: name.to_string(),
            summary: summary.to_string(),
            decision,
            is_error: false,
            reason: None,
            refused: false,
            open: local,
        });
    }

    fn tool_finished(&mut self, name: &str, summary: &str, is_error: bool) {
        let started = self
            .tool_calls
            .iter_mut()
            .rev()
            .find(|call| call.open && call.tool == name && call.summary == summary);
        if let Some(call) = started {
            call.open = false;
            call.is_error = is_error;
        }
    }

    fn subagent(&mut self, event: &SubagentEvent) {
        match event {
            SubagentEvent::Started { id, name, .. } => self.subagents.push(SubagentRun {
                name: name.clone(),
                status: status_word(Status::Running),
                id: id.clone(),
            }),
            SubagentEvent::Interrupted { id } => self.set_subagent(id, Status::Waiting),
            SubagentEvent::Finished { id, status, .. } => self.set_subagent(id, *status),
            _ => {}
        }
    }

    fn set_subagent(&mut self, id: &str, status: Status) {
        if let Some(run) = self.subagents.iter_mut().find(|run| run.id == id) {
            run.status = status_word(status);
        }
    }

    /// The finished report: `end` is how the turn ended, or what stopped it
    /// from running.
    pub fn finish(mut self, end: &anyhow::Result<TurnEnd>) -> Summary {
        self.take_decisions();
        // Approved Groundzero actions the server never showed running.
        let unshown = self.allowed_server.drain(..).map(|decided| ToolCall {
            tool: decided.tool,
            summary: decided.summary,
            decision: decided.decision.to_string(),
            is_error: false,
            reason: None,
            refused: false,
            open: false,
        });
        self.tool_calls.extend(unshown);
        let (status, error) = match end {
            Ok(TurnEnd::Completed) => (RunStatus::Completed, None),
            Ok(TurnEnd::Cancelled) => (RunStatus::Interrupted, None),
            Ok(TurnEnd::Failed(why)) => (RunStatus::Failed, Some(why.clone())),
            Err(err) => (RunStatus::Failed, Some(format!("{err:#}"))),
        };
        Summary {
            conversation_id: self.conversation_id,
            model: self.model,
            answer: self.answer.trim().to_string(),
            tool_calls: self.tool_calls,
            subagents: self.subagents,
            usage: self.usage,
            duration_ms: u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            status,
            error,
        }
    }
}

fn refused_call(decided: Decided) -> ToolCall {
    ToolCall {
        tool: decided.tool,
        summary: decided.summary,
        decision: decided.decision.to_string(),
        is_error: true,
        reason: decided.reason,
        refused: true,
        open: false,
    }
}

fn status_word(status: Status) -> &'static str {
    match status {
        Status::Running => "running",
        Status::Completed => "completed",
        Status::Failed => "failed",
        Status::Cancelled => "cancelled",
        Status::Skipped => "skipped",
        Status::Waiting => "waiting",
    }
}

impl Summary {
    /// See [`exit`]: a failure outranks a refusal.
    pub fn exit_code(&self, end: &anyhow::Result<TurnEnd>) -> i32 {
        if let Err(err) = end {
            if err.downcast_ref::<CapReached>().is_some() {
                return exit::CAP_REACHED;
            }
        }
        match self.status {
            RunStatus::Failed => exit::FAILED,
            RunStatus::Interrupted => exit::INTERRUPTED,
            RunStatus::Completed if self.tool_calls.iter().any(|call| call.refused) => {
                exit::NEEDED_APPROVAL
            }
            RunStatus::Completed => exit::COMPLETED,
        }
    }

    /// The report as `format` prints it; script mode takes json or yaml only.
    pub fn render(&self, format: OutputFormat) -> anyhow::Result<String> {
        Ok(match format {
            OutputFormat::Yaml => serde_yaml::to_string(self)?,
            OutputFormat::Json | OutputFormat::Table | OutputFormat::Csv => {
                serde_json::to_string_pretty(self)?
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant::tools::Access;
    use crate::assistant::{Approver, PermissionRequest};
    use crate::cmd::code_unattended::{Nobody, Recording};

    fn started(name: &str, summary: &str, local: bool) -> AssistantEvent {
        AssistantEvent::ToolStarted {
            name: name.into(),
            summary: summary.into(),
            local,
        }
    }

    fn finished(name: &str, summary: &str, is_error: bool) -> AssistantEvent {
        AssistantEvent::ToolFinished {
            name: name.into(),
            summary: summary.into(),
            is_error,
            detail: String::new(),
            output: String::new(),
            diff: None,
        }
    }

    fn report() -> (Report, DecisionLog) {
        let log = DecisionLog::default();
        (Report::new("c1", Some("opus".into()), log.clone()), log)
    }

    #[test]
    fn the_answer_is_the_text_after_the_last_tool() {
        let (mut report, _) = report();
        for event in [
            AssistantEvent::Text("Let me look.".into()),
            started("read_file", "read_file a.txt", true),
            finished("read_file", "read_file a.txt", false),
            AssistantEvent::Text("It says ".into()),
            AssistantEvent::Text("hi.\n".into()),
        ] {
            report.observe(&event);
        }
        let summary = report.finish(&Ok(TurnEnd::Completed));
        assert_eq!(summary.answer, "It says hi.");
        assert_eq!(summary.tool_calls[0].decision, PREAPPROVED);
        assert_eq!(summary.exit_code(&Ok(TurnEnd::Completed)), exit::COMPLETED);
    }

    #[tokio::test]
    async fn a_refusal_for_lack_of_approval_is_listed_and_exits_non_zero() {
        let (mut report, log) = report();
        let approver = Recording::new(Box::new(Nobody), log);
        let request = PermissionRequest {
            access: Access::Command,
            summary: "run_command cargo test".into(),
            preview: None,
            chat_scope: String::new(),
            always_scope: None,
            action: None,
            asked_by: None,
            target: None,
            note: None,
        };
        approver.decide(&request).await;
        report.observe(&AssistantEvent::Notice("blocked".into()));
        report.observe(&AssistantEvent::Text("Couldn't run the tests.".into()));
        let end = Ok(TurnEnd::Completed);
        let summary = report.finish(&end);
        let call = &summary.tool_calls[0];
        assert_eq!(
            (call.tool.as_str(), call.decision.as_str(), call.is_error),
            ("run_command", "blocked", true)
        );
        assert!(call
            .reason
            .as_deref()
            .unwrap()
            .contains("needs the user's approval"));
        assert_eq!(summary.exit_code(&end), exit::NEEDED_APPROVAL);
    }

    #[test]
    fn subagents_usage_and_errors_add_up() {
        let (mut report, _) = report();
        let metrics = |input, cost| {
            AssistantEvent::Usage(Usage::from_event(&serde_json::json!({
                "inputTokens": input, "outputTokens": 5, "costUsd": cost
            })))
        };
        report.observe(&AssistantEvent::Subagent(SubagentEvent::Started {
            id: "s1".into(),
            name: "Check logs".into(),
            profile: None,
            agent_id: None,
            task: None,
            runs_on: Default::default(),
            parent: None,
        }));
        report.observe(&AssistantEvent::ForSubagent {
            id: "s1".into(),
            event: Box::new(started("run_command", "run_command ls", true)),
        });
        report.observe(&metrics(10, "0.01"));
        report.observe(&metrics(20, "0.02"));
        report.observe(&started("Search docs", "Search docs", false));
        let end = Ok(TurnEnd::Failed("model unavailable".into()));
        let summary = report.finish(&end);
        assert_eq!(summary.subagents[0].status, "running");
        assert_eq!(summary.tool_calls[1].decision, SERVER);
        assert_eq!(summary.usage.input_tokens, Some(30));
        assert_eq!(summary.usage.output_tokens, Some(10));
        assert_eq!(summary.usage.cost_usd, Some(0.03));
        assert_eq!(summary.status, RunStatus::Failed);
        assert_eq!(summary.error.as_deref(), Some("model unavailable"));
        assert_eq!(summary.exit_code(&end), exit::FAILED);
        let json: serde_json::Value =
            serde_json::from_str(&summary.render(OutputFormat::Json).unwrap()).unwrap();
        assert_eq!(json["conversationId"], "c1");
        assert_eq!(json["usage"]["costUsd"], 0.03);
        assert_eq!(json["toolCalls"][0]["isError"], false);
        let yaml: serde_json::Value =
            serde_yaml::from_str(&summary.render(OutputFormat::Yaml).unwrap()).unwrap();
        assert_eq!(yaml, json);
    }
}
