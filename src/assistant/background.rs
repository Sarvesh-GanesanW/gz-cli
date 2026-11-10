//! Background subagents: with `backgroundSubagents` the
//! chat's `task` tool only asks gz for a subagent and returns at once, so
//! the main agent stays free to talk with the user. gz runs each subagent
//! as a chat run of its own (`subagentProfile`, see [`Session::into_subagent`]),
//! shows its progress the way inline subagents show theirs, and ends with a
//! [`SubagentReport`] for the main agent. Each runs on the model, effort and
//! context size the request names (the user's `/subagents` settings or the
//! main agent's choice), else on the chat's own.

use std::time::Instant;

use crate::assistant::models::{Catalog, GroundzeroMode, Model, Selection};
use crate::assistant::{Approver, AssistantEvent, Session, TurnEnd};
use crate::subagents::{
    RunsOn, Status, SubagentEvent, SubagentReport, SubagentRequest, SubagentUsage, ToolState,
};

/// A background subagent and what its run is started with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundRun {
    pub request: SubagentRequest,
    /// `None` leaves the model to the server's default.
    pub model: Option<Selection>,
    /// Its `autoCompactTokens`; `None` is the server's default.
    pub auto_compact: Option<u64>,
}

impl BackgroundRun {
    /// Resolves what `request` runs on. The chat's own choices (`main`,
    /// `main_auto_compact`) fill in whatever the request leaves unset.
    pub fn plan(
        request: SubagentRequest,
        catalog: Option<&Catalog>,
        main: Option<&Selection>,
        main_auto_compact: Option<u64>,
    ) -> Self {
        let main = main
            .cloned()
            .or_else(|| catalog.and_then(Catalog::default_selection));
        let model = match &request.model {
            Some(mode) => Some(requested_mode(
                mode,
                request.reasoning_effort.as_deref(),
                catalog,
                main.as_ref(),
            )),
            None => main.map(|mut selection| {
                // Subagents on a connection keep the chat's effort, and the
                // server sends none for one.
                if let (Some(effort), Model::Groundzero(_)) =
                    (&request.reasoning_effort, &selection.model)
                {
                    selection.effort = Some(effort.clone());
                }
                selection
            }),
        };
        let auto_compact = request.context_tokens.or(main_auto_compact);
        Self {
            request,
            model,
            auto_compact,
        }
    }

    /// What the subagent's rows show about its model.
    pub fn runs_on(&self) -> RunsOn {
        let (model, label, effort) = match &self.model {
            Some(selection) => (
                Some(selection.model.key()),
                Some(selection.model.label()),
                selection.effort.clone(),
            ),
            None => (None, None, None),
        };
        RunsOn {
            model,
            label,
            effort,
            context_tokens: self.auto_compact,
        }
    }
}

/// The mode the server named, with its effort, else the chat's effort when
/// that mode offers it. A mode missing from the list is still used as
/// named: the server just chose it.
fn requested_mode(
    mode: &str,
    effort: Option<&str>,
    catalog: Option<&Catalog>,
    main: Option<&Selection>,
) -> Selection {
    let fallback_effort = main.and_then(|m| m.effort.as_deref());
    let listed = catalog.and_then(|c| {
        c.modes
            .iter()
            .find(|m| m.value == mode)
            .map(|m| (Model::Groundzero(m.clone()), m.efforts.clone()))
    });
    match listed {
        Some((model, efforts)) => Selection {
            effort: effort
                .or(fallback_effort.filter(|e| efforts.iter().any(|x| x == e)))
                .map(str::to_string),
            model,
        },
        None => Selection {
            model: Model::Groundzero(GroundzeroMode {
                value: mode.to_string(),
                label: mode.to_string(),
                efforts: Vec::new(),
                context_window: None,
            }),
            effort: effort.map(str::to_string),
        },
    }
}

/// Runs `plan` on `session` (already set to its model) to its end. `emit`
/// gets the subagent's progress as main-page events, then its finish and
/// its report.
pub async fn run(
    mut session: Session,
    plan: BackgroundRun,
    approver: &dyn Approver,
    emit: &mut (dyn FnMut(AssistantEvent) + Send),
) {
    let runs_on = plan.runs_on();
    let mut relay = Relay::new(plan.request, runs_on);
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
    runs_on: RunsOn,
    started: Instant,
    /// The answer since the last tool call; the report once the run ends.
    answer: String,
    usage: SubagentUsage,
}

impl Relay {
    pub fn new(request: SubagentRequest, runs_on: RunsOn) -> Self {
        Self {
            request,
            runs_on,
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
            runs_on: self.runs_on.clone(),
            parent: None,
        })
    }

    pub fn relay(&mut self, event: AssistantEvent) -> Vec<AssistantEvent> {
        let id = self.request.id.clone();
        let tool = |tool: &str, state| {
            AssistantEvent::Subagent(SubagentEvent::Tool {
                id: id.clone(),
                tool: tool.to_string(),
                state,
                tool_use_id: None,
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

    fn request() -> SubagentRequest {
        SubagentRequest {
            id: "sa_1".into(),
            name: "Check nightly-etl".into(),
            profile: "data_engineer".into(),
            prompt: "Why do the runs fail?".into(),
            model: None,
            reasoning_effort: None,
            context_tokens: None,
        }
    }

    fn relay() -> Relay {
        Relay::new(request(), RunsOn::default())
    }

    fn catalog() -> Catalog {
        Catalog {
            modes: crate::assistant::models::modes_from(&serde_json::json!({"groundzeroModes": [
                {"value": "opus", "label": "Claude Opus 5", "reasoningEfforts": ["low", "high"]},
                {"value": "haiku55", "label": "Claude Haiku 5.5", "reasoningEfforts": ["low"]},
            ]})),
            default_mode: Some("opus".into()),
            default_effort: Some("high".into()),
            ..Catalog::default()
        }
    }

    #[test]
    fn a_background_run_uses_the_requests_model_effort_and_context() {
        let catalog = catalog();
        let main = catalog.resolve("opus", Some("high"));
        let asked = SubagentRequest {
            model: Some("haiku55".into()),
            reasoning_effort: Some("low".into()),
            context_tokens: Some(200_000),
            ..request()
        };
        let plan = BackgroundRun::plan(asked, Some(&catalog), main.as_ref(), Some(500_000));
        let mut body = serde_json::json!({});
        plan.model.as_ref().unwrap().apply_to(&mut body);
        assert_eq!(
            body,
            serde_json::json!({"groundzeroMode": "haiku55", "reasoningEffort": "low"})
        );
        assert_eq!(plan.auto_compact, Some(200_000));
        let shown = plan.runs_on();
        assert_eq!(shown.describe().as_deref(), Some("Claude Haiku 5.5 · 200K"));
    }

    #[test]
    fn a_background_run_falls_back_to_the_chats_choices() {
        let catalog = catalog();
        let main = catalog.resolve("opus", Some("high"));
        let plan = BackgroundRun::plan(request(), Some(&catalog), main.as_ref(), Some(300_000));
        assert_eq!(plan.model, main, "the chat's model and effort");
        assert_eq!(plan.auto_compact, Some(300_000));
        // Only a model named: the chat's effort, unless that model lacks it.
        let haiku = SubagentRequest {
            model: Some("haiku55".into()),
            ..request()
        };
        let plan = BackgroundRun::plan(haiku, Some(&catalog), main.as_ref(), None);
        assert_eq!(plan.model.unwrap().effort, None, "haiku has no `high`");
        let main_low = catalog.resolve("opus", Some("low"));
        let haiku = SubagentRequest {
            model: Some("haiku55".into()),
            ..request()
        };
        let plan = BackgroundRun::plan(haiku, Some(&catalog), main_low.as_ref(), None);
        assert_eq!(plan.model.unwrap().effort.as_deref(), Some("low"));
        // Only an effort named: the chat's model with it.
        let effort = SubagentRequest {
            reasoning_effort: Some("low".into()),
            ..request()
        };
        let plan = BackgroundRun::plan(effort, Some(&catalog), None, None);
        let model = plan.model.unwrap();
        assert_eq!(
            (model.model.key().as_str(), model.effort.as_deref()),
            ("opus", Some("low")),
            "the server default model when the chat has none chosen"
        );
        // Nothing known at all: the server decides, as before.
        let plan = BackgroundRun::plan(request(), None, None, None);
        assert_eq!((plan.model, plan.auto_compact), (None, None));
    }

    #[test]
    fn a_mode_missing_from_the_list_is_still_sent() {
        let asked = SubagentRequest {
            model: Some("newmode".into()),
            reasoning_effort: Some("max".into()),
            ..request()
        };
        let plan = BackgroundRun::plan(asked, None, None, None);
        let mut body = serde_json::json!({});
        plan.model.unwrap().apply_to(&mut body);
        assert_eq!(
            body,
            serde_json::json!({"groundzeroMode": "newmode", "reasoningEffort": "max"})
        );
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
