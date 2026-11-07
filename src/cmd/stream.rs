//! Chat and agent run streams. Both services send `llm_chunk` events
//! carrying answer text, tool events while they work, a terminal status,
//! and `[DONE]`. Long runs ask the client to reconnect, which resumes after
//! the last event id.
//!
//! [`RunEvent::parse`] gives every consumer one typed view of an event;
//! [`read_run`] handles the transport; [`follow`] prints a run for the CLI.

use anyhow::Result;
use owo_colors::OwoColorize;
use reqwest::Method;
use serde_json::Value;
use std::io::Write;

use crate::cmd::{new_uuid, Runtime};
use crate::http::ApiClient;
use crate::output::OutputFormat;
use crate::sse::{self, Event, Flow};
use crate::subagents::{self, SubagentEvent, SubagentUsage};

const TERMINAL: &[&str] = &[
    "completed",
    "error",
    "failed",
    "cancelled",
    "interrupted",
    "timed_out",
];

/// Token counts a run reports for one answer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenCounts {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl TokenCounts {
    /// Everything the model read: fresh input plus cached prompt tokens.
    pub fn prompt(&self) -> u64 {
        self.input + self.cache_read + self.cache_write
    }

    pub fn add(&mut self, other: &TokenCounts) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
    }
}

/// The `response_metrics` event a chat run emits once an answer is done
/// (gz-chat-eks agent/generate.py, forwarded by run_execution.py).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Usage {
    /// `None` when the provider didn't report tokens
    /// (`tokenUsageStatus: "unavailable"`).
    pub tokens: Option<TokenCounts>,
    /// Finalized spend in USD, when the site has a budget.
    pub cost_usd: Option<f64>,
    pub latency_ms: Option<u64>,
    /// Server-measured generation speed (gz-chat-eks utils/generation_speed.py).
    pub time_to_first_token_ms: Option<u64>,
    pub output_tokens_per_second: Option<f64>,
    pub reasoning_tokens: Option<u64>,
    /// The run's subagents' shares of the totals above.
    pub subagents: Vec<(String, SubagentUsage)>,
}

impl Usage {
    pub fn from_event(data: &Value) -> Self {
        let count = |key: &str| data[key].as_u64();
        let reported = data["tokenUsageStatus"].as_str() != Some("unavailable");
        let fields = [
            count("inputTokens"),
            count("outputTokens"),
            count("cacheReadInputTokens"),
            count("cacheWriteInputTokens"),
        ];
        let tokens = (reported && fields.iter().any(Option::is_some)).then(|| TokenCounts {
            input: fields[0].unwrap_or(0),
            output: fields[1].unwrap_or(0),
            cache_read: fields[2].unwrap_or(0),
            cache_write: fields[3].unwrap_or(0),
        });
        // The server sends money as a decimal string ("0.0123").
        let cost_usd = match &data["costUsd"] {
            Value::String(text) => text.parse().ok(),
            Value::Number(n) => n.as_f64(),
            _ => None,
        };
        Self {
            tokens,
            cost_usd,
            latency_ms: count("latencyMs"),
            time_to_first_token_ms: count("timeToFirstTokenMs"),
            output_tokens_per_second: data["outputTokensPerSecond"].as_f64(),
            reasoning_tokens: count("reasoningTokens"),
            subagents: subagents::usage_by_subagent(data),
        }
    }
}

/// One run event, decoded.
#[derive(Debug, Clone, PartialEq)]
pub enum RunEvent {
    /// A piece of the answer.
    Text(String),
    /// A tool the server runs itself started.
    ServerTool(String),
    ToolError(String),
    Warning(String),
    /// The run asks the user to approve or deny a server-side action.
    Approval(Value),
    /// The run asks the client to execute one of its declared tools.
    ClientToolRequest(Value),
    Error(String),
    /// The run reached `status` (`completed`, `interrupted`, …).
    Status(String),
    /// Token and cost figures for the answer just finished.
    Usage(Usage),
    /// A subagent (or workflow node) started, worked, or finished.
    Subagent(SubagentEvent),
    Cancelled,
    Reconnect,
    Done,
    Other,
}

impl RunEvent {
    pub fn parse(event: &Event) -> Self {
        if event.is_done() {
            return Self::Done;
        }
        let data = event.json().unwrap_or(Value::Null);
        let kind = event_type(event, &data);
        if let Some(subagent) = SubagentEvent::parse(&kind, &data) {
            return Self::Subagent(subagent);
        }
        match kind.as_str() {
            "llm_chunk" => Self::Text(data["text"].as_str().unwrap_or_default().to_string()),
            "tool_use" | "tool" => Self::ServerTool(
                data["toolName"]
                    .as_str()
                    .or(data["name"].as_str())
                    .unwrap_or("tool")
                    .to_string(),
            ),
            "tool_error" => Self::ToolError(message(&data)),
            "warning" | "system_warning" => Self::Warning(message(&data)),
            "interrupt" => Self::Approval(data),
            "client_tool_request" => Self::ClientToolRequest(data),
            "error" | "failed" => Self::Error(message(&data)),
            "cancelled" | "stopped" => Self::Cancelled,
            "reconnect" => Self::Reconnect,
            "response_metrics" => Self::Usage(Usage::from_event(&data)),
            "run_status" | "run_terminal" | "completed" | "end" => match data["status"].as_str() {
                Some(status) => match failure_reason(status, data["error"].as_str()) {
                    Some(reason) => Self::Error(reason),
                    None => Self::Status(status.to_string()),
                },
                None => Self::Other,
            },
            _ => Self::Other,
        }
    }

    /// True for statuses after which no more events will come.
    pub fn is_terminal_status(status: &str) -> bool {
        TERMINAL.contains(&status)
    }
}

/// Streams `events_path`, reconnecting when the server asks, until the run
/// finishes or `on_event` stops it. `last_id` is updated in place so a later
/// call (after a resume) continues where this one stopped.
pub async fn read_run(
    http: &ApiClient,
    service: &str,
    events_path: &str,
    last_id: &mut Option<String>,
    mut on_event: impl FnMut(&Event, RunEvent) -> Result<Flow>,
) -> Result<()> {
    loop {
        let query: Vec<(String, String)> = last_id
            .iter()
            .map(|id| ("after".to_string(), id.clone()))
            .collect();
        let response = http
            .stream(service, Method::GET, events_path, &query, None)
            .await?;
        let mut reconnect = false;
        *last_id = sse::read(response, last_id.take(), |event| {
            match RunEvent::parse(event) {
                RunEvent::Reconnect => {
                    reconnect = true;
                    Ok(Flow::Stop)
                }
                RunEvent::Done => Ok(Flow::Stop),
                parsed => on_event(event, parsed),
            }
        })
        .await?;
        if !reconnect {
            return Ok(());
        }
    }
}

/// The parts of a chat approval interrupt a client shows and answers.
/// The server sends snake_case (`interrupt_id`, `action_preview.*`); the
/// camelCase fallbacks keep older payloads working.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approval {
    pub interrupt_id: String,
    pub tool: String,
    pub summary: Option<String>,
    pub preview: Option<String>,
}

impl Approval {
    pub fn from_event(data: &Value) -> Self {
        let action = &data["action_preview"];
        let text = |v: &Value| v.as_str().map(str::to_string);
        let preview = match &action["input_preview"] {
            Value::Null => None,
            Value::String(s) => Some(s.clone()),
            other => serde_json::to_string_pretty(other).ok(),
        };
        Self {
            interrupt_id: text(&data["interrupt_id"])
                .or_else(|| text(&data["interruptId"]))
                .or_else(|| text(&data["id"]))
                .unwrap_or_default(),
            tool: text(&action["tool_name"])
                .or_else(|| text(&data["toolName"]))
                .unwrap_or_else(|| "an action".to_string()),
            summary: text(&action["summary"]).or_else(|| text(&data["message"])),
            preview,
        }
    }
}

/// Statuses that mean the run failed, and a readable reason.
pub fn failure_reason(status: &str, error: Option<&str>) -> Option<String> {
    if !matches!(status, "failed" | "timed_out" | "error") {
        return None;
    }
    Some(match error {
        Some("client_tool_timeout") => {
            "the run waited too long for local tool results (15 min); ask again".to_string()
        }
        Some(detail) if !detail.is_empty() => detail.to_string(),
        _ => format!("the run ended with status {status}"),
    })
}

/// Headers the web app sends with every chat run start/resume: an
/// idempotency key and the refresh token for tools that outlive the ID token.
pub async fn run_headers(http: &ApiClient) -> Vec<(&'static str, String)> {
    let mut headers = vec![("X-Request-ID", new_uuid())];
    if let Some(refresh) = http.refresh_token().await {
        headers.push(("X-Refresh-Token", refresh));
    }
    headers
}

/// How a printed run ended.
#[derive(Debug, Default)]
pub struct Outcome {
    pub status: Option<String>,
    pub error: Option<String>,
    /// Set when the run paused for a human decision.
    pub interrupt: Option<Value>,
    /// The subagent that raised `interrupt`, by name, when one did.
    pub asked_by: Option<String>,
}

/// Prints a run as it streams (text to stdout, tool notes to stderr; NDJSON
/// in `-o json`).
pub async fn follow(rt: &Runtime, service: &str, events_path: &str) -> Result<Outcome> {
    let mut printer = Printer::new(rt.format);
    let mut last_id = None;
    read_run(
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

fn event_type(event: &Event, data: &Value) -> String {
    data["type"]
        .as_str()
        .or(event.event.as_deref())
        .unwrap_or("message")
        .to_string()
}

pub fn message(data: &Value) -> String {
    ["message", "detail", "error", "text"]
        .iter()
        .find_map(|k| data[k].as_str())
        .map(str::to_string)
        .unwrap_or_else(|| data.to_string())
}

struct Printer {
    format: OutputFormat,
    mid_line: bool,
    outcome: Outcome,
    subagents: subagents::Board,
}

impl Printer {
    fn new(format: OutputFormat) -> Self {
        Self {
            format,
            mid_line: false,
            outcome: Outcome::default(),
            subagents: subagents::Board::default(),
        }
    }

    fn handle(&mut self, event: &Event, parsed: RunEvent) -> Result<Flow> {
        if self.format != OutputFormat::Table {
            let data = event.json().unwrap_or(Value::Null);
            let mut line = data.clone();
            if line.is_object() && line.get("type").is_none() {
                line["type"] = Value::String(event_type(event, &data));
            }
            println!("{}", serde_json::to_string(&line)?);
        }
        match parsed {
            RunEvent::Text(text) => self.text(&text),
            RunEvent::ServerTool(name) => self.note(&format!("⚙ {name}")),
            RunEvent::ToolError(why) => self.note(&format!("⚠ tool failed: {why}")),
            RunEvent::Warning(why) => self.note(&format!("⚠ {why}")),
            RunEvent::Approval(data) | RunEvent::ClientToolRequest(data) => {
                self.outcome.asked_by =
                    subagents::asked_by(&data).map(|id| self.subagents.name_of(&id));
                self.outcome.interrupt = Some(data)
            }
            RunEvent::Subagent(event) => self.subagent(&event),
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

    /// One line when a subagent starts and one when it ends; their tool
    /// calls and progress stay out of the answer.
    fn subagent(&mut self, event: &SubagentEvent) {
        let now = std::time::Instant::now();
        let finished = self.subagents.apply(event, now);
        let line = match (event, finished) {
            (SubagentEvent::Handoff { from, to, reason }, _) => {
                subagents::render::handoff_line(from, to, reason.as_deref())
            }
            (_, Some(agent)) => subagents::render::compact_line(&agent, now),
            (SubagentEvent::Started { id, .. }, None) => match self.subagents.get(id) {
                Some(agent) => subagents::render::compact_line(agent, now),
                None => return,
            },
            _ => return,
        };
        self.note(&line);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn event(data: &str) -> Event {
        Event {
            id: Some("1".into()),
            event: None,
            data: data.into(),
        }
    }

    fn handle(p: &mut Printer, data: &str) {
        let e = event(data);
        let parsed = RunEvent::parse(&e);
        p.handle(&e, parsed).unwrap();
    }

    #[test]
    fn parses_response_metrics() {
        let parsed = RunEvent::parse(&event(
            r#"{"type":"response_metrics","inputTokens":1200,"outputTokens":340,"cacheReadInputTokens":5000,"cacheWriteInputTokens":0,"totalTokens":6540,"tokenUsageStatus":"reported","latencyMs":4200,"costUsd":"0.012300"}"#,
        ));
        let RunEvent::Usage(usage) = parsed else {
            panic!("not usage: {parsed:?}");
        };
        let tokens = usage.tokens.unwrap();
        assert_eq!(
            (tokens.input, tokens.output, tokens.prompt()),
            (1200, 340, 6200)
        );
        assert_eq!(usage.cost_usd, Some(0.0123));
        assert_eq!(usage.latency_ms, Some(4200));
        let timed = Usage::from_event(&serde_json::json!({
            "outputTokens": 10, "timeToFirstTokenMs": 850, "outputTokensPerSecond": 62.5,
            "reasoningTokens": 120
        }));
        assert_eq!(timed.time_to_first_token_ms, Some(850));
        assert_eq!(timed.output_tokens_per_second, Some(62.5));
        assert_eq!(timed.reasoning_tokens, Some(120));
        let unavailable = Usage::from_event(&serde_json::json!({
            "inputTokens": null, "tokenUsageStatus": "unavailable", "latencyMs": 10
        }));
        assert!(unavailable.tokens.is_none());
    }

    #[test]
    fn reads_snake_case_approvals() {
        let data = serde_json::json!({
            "type": "interrupt",
            "interrupt_id": "v1:approval:9",
            "message": "Run the job?",
            "action_preview": {"tool_name": "submit_job", "input_preview": {"jobId": 4}},
        });
        let approval = Approval::from_event(&data);
        assert_eq!(approval.interrupt_id, "v1:approval:9");
        assert_eq!(approval.tool, "submit_job");
        assert_eq!(approval.summary.as_deref(), Some("Run the job?"));
        assert!(approval.preview.unwrap().contains("\"jobId\": 4"));
        let legacy =
            Approval::from_event(&serde_json::json!({"interruptId": "i1", "toolName": "x"}));
        assert_eq!(
            (legacy.interrupt_id.as_str(), legacy.tool.as_str()),
            ("i1", "x")
        );
    }

    #[test]
    fn explains_failed_statuses() {
        assert!(failure_reason("completed", None).is_none());
        assert!(failure_reason("error", Some("client_tool_timeout"))
            .unwrap()
            .contains("15 min"));
        assert_eq!(
            failure_reason("failed", None).unwrap(),
            "the run ended with status failed"
        );
    }

    #[test]
    fn records_errors_interrupts_and_status() {
        let mut p = Printer::new(OutputFormat::Yaml);
        handle(&mut p, r#"{"type":"error","message":"boom"}"#);
        assert_eq!(p.outcome.error.as_deref(), Some("boom"));
        handle(&mut p, r#"{"type":"interrupt","interruptId":"i1"}"#);
        handle(&mut p, r#"{"type":"run_status","status":"interrupted"}"#);
        assert_eq!(p.outcome.status.as_deref(), Some("interrupted"));
        assert_eq!(p.outcome.interrupt.as_ref().unwrap()["interruptId"], "i1");
    }

    #[test]
    fn subagent_events_parse_and_attribute_interrupts() {
        assert!(matches!(
            RunEvent::parse(&event(
                r#"{"type":"subagent_started","subagentId":"sa_1","name":"Logs","profile":"data_engineer"}"#
            )),
            RunEvent::Subagent(SubagentEvent::Started { .. })
        ));
        let mut p = Printer::new(OutputFormat::Yaml);
        handle(
            &mut p,
            r#"{"type":"subagent_started","subagentId":"sa_1","name":"Logs"}"#,
        );
        handle(
            &mut p,
            r#"{"type":"interrupt","interrupt_id":"i1","subagentId":"sa_1"}"#,
        );
        assert_eq!(p.outcome.asked_by.as_deref(), Some("Logs"));
        let RunEvent::Usage(usage) = RunEvent::parse(&event(
            r#"{"type":"response_metrics","outputTokens":5,"subagents":[{"subagentId":"sa_1","inputTokens":3,"outputTokens":2}]}"#,
        )) else {
            panic!("not usage");
        };
        assert_eq!(usage.subagents[0].1.output_tokens, 2);
    }

    #[test]
    fn parses_each_event_kind() {
        assert_eq!(
            RunEvent::parse(&event(r#"{"type":"llm_chunk","text":"hi"}"#)),
            RunEvent::Text("hi".into())
        );
        assert!(matches!(
            RunEvent::parse(&event(
                r#"{"type":"client_tool_request","interruptId":"ct_1","toolName":"read_file","input":{}}"#
            )),
            RunEvent::ClientToolRequest(_)
        ));
        assert_eq!(
            RunEvent::parse(&event(r#"{"type":"run_status","status":"completed"}"#)),
            RunEvent::Status("completed".into())
        );
        assert_eq!(RunEvent::parse(&event("[DONE]")), RunEvent::Done);
    }

    #[test]
    fn named_events_and_reconnects_are_recognised() {
        let named = Event {
            id: None,
            event: Some("reconnect".into()),
            data: "{}".into(),
        };
        assert_eq!(RunEvent::parse(&named), RunEvent::Reconnect);
        assert_eq!(
            RunEvent::parse(&event(r#"{"type":"reconnect"}"#)),
            RunEvent::Reconnect
        );
        let agent_chunk = Event {
            id: None,
            event: Some("llm_chunk".into()),
            data: r#"{"text":"hi"}"#.into(),
        };
        assert_eq!(RunEvent::parse(&agent_chunk), RunEvent::Text("hi".into()));
    }
}
