//! Follows a chat or agent run's event stream and prints the answer live.
//! Both services send `llm_chunk` events carrying answer text, tool events
//! while they work, a terminal status, and `[DONE]`. Long runs ask the
//! client to reconnect, which resumes after the last event id.

use anyhow::Result;
use owo_colors::OwoColorize;
use reqwest::Method;
use serde_json::Value;
use std::io::Write;

use crate::cmd::Runtime;
use crate::output::OutputFormat;
use crate::sse::{self, Event, Flow};

/// How a run ended.
#[derive(Debug, Default)]
pub struct Outcome {
    pub status: Option<String>,
    pub error: Option<String>,
    /// Set when the run paused for a human decision.
    pub interrupt: Option<Value>,
}

const TERMINAL: &[&str] = &[
    "completed",
    "failed",
    "cancelled",
    "interrupted",
    "timed_out",
];

pub async fn follow(rt: &Runtime, service: &str, events_path: &str) -> Result<Outcome> {
    let mut printer = Printer::new(rt.format);
    let mut last_id: Option<String> = None;
    loop {
        let query: Vec<(String, String)> = last_id
            .iter()
            .map(|id| ("after".to_string(), id.clone()))
            .collect();
        let response = rt
            .http
            .stream(service, Method::GET, events_path, &query, None)
            .await?;
        let mut reconnect = false;
        last_id = sse::read(response, last_id, |event| {
            if is_reconnect(event) {
                reconnect = true;
                return Ok(Flow::Stop);
            }
            printer.handle(event)
        })
        .await?;
        if !reconnect {
            break;
        }
    }
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

fn is_reconnect(event: &Event) -> bool {
    event.event.as_deref() == Some("reconnect")
        || event.json().is_some_and(|d| d["type"] == "reconnect")
}

struct Printer {
    format: OutputFormat,
    mid_line: bool,
    outcome: Outcome,
}

impl Printer {
    fn new(format: OutputFormat) -> Self {
        Self {
            format,
            mid_line: false,
            outcome: Outcome::default(),
        }
    }

    fn handle(&mut self, event: &Event) -> Result<Flow> {
        if event.is_done() {
            return Ok(Flow::Stop);
        }
        let data = event.json().unwrap_or(Value::Null);
        let kind = event_type(event, &data);
        if self.format != OutputFormat::Table {
            let mut line = data.clone();
            if line.is_object() && line.get("type").is_none() {
                line["type"] = Value::String(kind.clone());
            }
            println!("{}", serde_json::to_string(&line)?);
        }
        match kind.as_str() {
            "llm_chunk" => self.text(data["text"].as_str().unwrap_or_default()),
            "tool_use" | "tool" => {
                let name = data["toolName"]
                    .as_str()
                    .or(data["name"].as_str())
                    .unwrap_or("tool");
                self.note(&format!("⚙ {name}"));
            }
            "tool_error" => self.note(&format!("⚠ tool failed: {}", message(&data))),
            "warning" | "system_warning" => self.note(&format!("⚠ {}", message(&data))),
            "interrupt" => self.outcome.interrupt = Some(data.clone()),
            "error" | "failed" => self.outcome.error = Some(message(&data)),
            "cancelled" | "stopped" => self.outcome.status = Some("cancelled".to_string()),
            "run_status" | "run_terminal" | "completed" | "end" => {
                if let Some(status) = data["status"].as_str() {
                    self.outcome.status = Some(status.to_string());
                    if status == "interrupted" && self.outcome.interrupt.is_none() {
                        self.outcome.interrupt = Some(data.clone());
                    }
                    if TERMINAL.contains(&status) && status != "completed" {
                        return Ok(Flow::Stop);
                    }
                }
            }
            _ => {}
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

fn message(data: &Value) -> String {
    ["message", "detail", "error", "text"]
        .iter()
        .find_map(|k| data[k].as_str())
        .map(str::to_string)
        .unwrap_or_else(|| data.to_string())
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

    #[test]
    fn records_errors_interrupts_and_status() {
        let mut p = Printer::new(OutputFormat::Yaml);
        p.handle(&event(r#"{"type":"error","message":"boom"}"#))
            .unwrap();
        assert_eq!(p.outcome.error.as_deref(), Some("boom"));
        p.handle(&event(
            r#"{"type":"run_status","status":"interrupted","interruptId":"i1"}"#,
        ))
        .unwrap();
        assert_eq!(p.outcome.status.as_deref(), Some("interrupted"));
        assert_eq!(p.outcome.interrupt.as_ref().unwrap()["interruptId"], "i1");
    }

    #[test]
    fn named_events_and_reconnects_are_recognised() {
        let named = Event {
            id: None,
            event: Some("reconnect".into()),
            data: "{}".into(),
        };
        assert!(is_reconnect(&named));
        assert!(is_reconnect(&event(r#"{"type":"reconnect"}"#)));
        assert!(!is_reconnect(&event(r#"{"type":"llm_chunk"}"#)));
        let agent_chunk = Event {
            id: None,
            event: Some("llm_chunk".into()),
            data: r#"{"text":"hi"}"#.into(),
        };
        assert_eq!(
            event_type(&agent_chunk, &agent_chunk.json().unwrap()),
            "llm_chunk"
        );
    }
}
