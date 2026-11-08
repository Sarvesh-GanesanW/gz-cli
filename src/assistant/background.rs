//! Background subagents, like Claude Code's: with `backgroundSubagents` the
//! chat's `task` tool only asks gz for a subagent and returns at once, so
//! the main agent stays free to talk with the user. gz runs each subagent
//! as a chat run of its own (`subagentProfile`, see [`Session::into_subagent`]),
//! shows its progress the way inline subagents show theirs, and ends with a
//! [`SubagentReport`] for the main agent.

use std::time::Instant;

use crate::assistant::{Approver, AssistantEvent, Session, TurnEnd};
use crate::subagents::{
    Status, SubagentEvent, SubagentReport, SubagentRequest, SubagentUsage, ToolState,
};

/// Runs `request` on `session` to its end. `emit` gets the subagent's
/// progress as main-page events, then its finish and its report.
pub async fn run(
    mut session: Session,
    request: SubagentRequest,
    approver: &dyn Approver,
    emit: &mut (dyn FnMut(AssistantEvent) + Send),
) {
    let mut relay = Relay::new(request);
    emit(relay.started());
    let prompt = relay.request.prompt.clone();
    let end = {
        let mut on_event =
            |event: AssistantEvent| relay.relay(event).into_iter().for_each(&mut *emit);
        session.send(&prompt, approver, &mut on_event).await
    };
    let end = end.unwrap_or_else(|err| TurnEnd::Failed(format!("{err:#}")));
    relay.finish(&end).into_iter().for_each(emit);
}

/// Turns one subagent session's events into what the main page shows.
pub struct Relay {
    request: SubagentRequest,
    started: Instant,
    /// The answer since the last tool call; the report once the run ends.
    answer: String,
    usage: SubagentUsage,
}

impl Relay {
    pub fn new(request: SubagentRequest) -> Self {
        Self {
            request,
            started: Instant::now(),
            answer: String::new(),
            usage: SubagentUsage::default(),
        }
    }

    pub fn started(&self) -> AssistantEvent {
        AssistantEvent::Subagent(SubagentEvent::Started {
            id: self.request.id.clone(),
            name: self.request.name.clone(),
            profile: Some(self.request.profile.clone()),
            agent_id: None,
            task: Some(self.request.prompt.clone()),
        })
    }

    pub fn relay(&mut self, event: AssistantEvent) -> Vec<AssistantEvent> {
        let id = self.request.id.clone();
        let tool = |tool: &str, state| {
            AssistantEvent::Subagent(SubagentEvent::Tool {
                id: id.clone(),
                tool: tool.to_string(),
                state,
            })
        };
        let on_page = |event| AssistantEvent::ForSubagent {
            id: id.clone(),
            event: Box::new(event),
        };
        match event {
            AssistantEvent::Text(text) if !text.is_empty() => {
                self.answer.push_str(&text);
                vec![AssistantEvent::Subagent(SubagentEvent::Text {
                    id: id.clone(),
                    text,
                })]
            }
            // Server tools report only that they started.
            AssistantEvent::ToolStarted {
                ref name,
                local: false,
                ..
            } => {
                self.answer.clear();
                vec![
                    tool(name, ToolState::Started),
                    tool(name, ToolState::Finished),
                ]
            }
            AssistantEvent::ToolStarted { ref name, .. } => {
                self.answer.clear();
                vec![tool(name, ToolState::Started), on_page(event.clone())]
            }
            AssistantEvent::ToolFinished {
                ref name, is_error, ..
            } => {
                let state = if is_error {
                    ToolState::Failed
                } else {
                    ToolState::Finished
                };
                vec![tool(name, state), on_page(event.clone())]
            }
            AssistantEvent::Usage(ref usage) => {
                self.add_usage(usage);
                vec![on_page(event.clone())]
            }
            AssistantEvent::Notice(_) => vec![on_page(event)],
            // A subagent's run delegates nothing, takes no notes, and ends
            // through `finish`.
            _ => Vec::new(),
        }
    }

    fn add_usage(&mut self, usage: &crate::cmd::stream::Usage) {
        if let Some(tokens) = &usage.tokens {
            self.usage.input_tokens += tokens.prompt();
            self.usage.output_tokens += tokens.output;
        }
        if let Some(cost) = usage.cost_usd {
            *self.usage.cost_usd.get_or_insert(0.0) += cost;
        }
    }

    /// The subagent's finish and its report, once its run ended with `end`.
    pub fn finish(self, end: &TurnEnd) -> [AssistantEvent; 2] {
        let (status, text) = match end {
            TurnEnd::Completed => (Status::Completed, self.answer.trim().to_string()),
            TurnEnd::Cancelled => (Status::Cancelled, "Stopped by the user.".to_string()),
            TurnEnd::Failed(why) => (Status::Failed, format!("The subagent failed: {why}")),
        };
        let finished = SubagentEvent::Finished {
            id: self.request.id.clone(),
            status,
            summary: text.clone(),
            usage: self.usage,
            duration: Some(self.started.elapsed()),
        };
        let report = SubagentReport {
            id: self.request.id,
            name: self.request.name,
            status,
            text,
        };
        [
            AssistantEvent::Subagent(finished),
            AssistantEvent::SubagentReport(report),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::stream::{TokenCounts, Usage};

    fn relay() -> Relay {
        Relay::new(SubagentRequest {
            id: "sa_1".into(),
            name: "Check nightly-etl".into(),
            profile: "data_engineer".into(),
            prompt: "Why do the runs fail?".into(),
        })
    }

    #[test]
    fn the_report_is_the_answer_after_the_last_tool() {
        let mut relay = relay();
        relay.relay(AssistantEvent::Text("Let me look.".into()));
        let shown = relay.relay(AssistantEvent::ToolStarted {
            name: "run_command".into(),
            summary: "Run make logs".into(),
            local: true,
        });
        assert!(matches!(
            &shown[..],
            [
                AssistantEvent::Subagent(SubagentEvent::Tool { state: ToolState::Started, .. }),
                AssistantEvent::ForSubagent { id, .. },
            ] if id == "sa_1"
        ));
        relay.relay(AssistantEvent::Text("Two runs timed out.".into()));
        let [finished, report] = relay.finish(&TurnEnd::Completed);
        assert!(matches!(
            finished,
            AssistantEvent::Subagent(SubagentEvent::Finished {
                status: Status::Completed,
                ..
            })
        ));
        let AssistantEvent::SubagentReport(report) = report else {
            panic!("no report");
        };
        assert_eq!(report.text, "Two runs timed out.");
        assert_eq!(report.name, "Check nightly-etl");
    }

    #[test]
    fn usage_adds_up_and_failures_say_why() {
        let mut relay = relay();
        for cost in [0.01, 0.02] {
            relay.relay(AssistantEvent::Usage(Usage {
                tokens: Some(TokenCounts {
                    input: 10,
                    output: 5,
                    cache_read: 90,
                    cache_write: 0,
                }),
                cost_usd: Some(cost),
                latency_ms: None,
                time_to_first_token_ms: None,
                output_tokens_per_second: None,
                reasoning_tokens: None,
                subagents: Vec::new(),
            }));
        }
        let [finished, report] = relay.finish(&TurnEnd::Failed("budget exceeded".into()));
        let AssistantEvent::Subagent(SubagentEvent::Finished { usage, .. }) = finished else {
            panic!("no finish");
        };
        assert_eq!((usage.input_tokens, usage.output_tokens), (200, 10));
        assert!((usage.cost_usd.unwrap() - 0.03).abs() < 1e-9);
        assert!(matches!(report, AssistantEvent::SubagentReport(r)
            if r.status == Status::Failed && r.text.contains("budget exceeded")));
    }
}
