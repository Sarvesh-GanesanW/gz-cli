//! Background subagents: the chat hands a subagent to
//! gz and the main agent is free at once. The subagent keeps running across
//! turns (live under the input box, on its own page), and when it ends its
//! report goes to the main agent: into the running turn as a note when one
//! is running, else as a turn of its own. `/subagents stop` stops them.
//! With `/subagents parallel N`, requests beyond N wait in a queue and
//! start as running ones finish.

use crate::assistant::background::BackgroundRun;
use crate::assistant::jobs::Finished;
use crate::assistant::steering::MAX_NOTE_CHARS;
use crate::code_ui::items::Item;
use crate::subagents::{render as subagent_render, Status, SubagentReport, SubagentRequest};

use super::{Effect, UiState};

impl UiState {
    /// The chat asked for a background subagent: it runs on the model the
    /// request names (else the chat's), now or once a slot frees up.
    pub(super) fn subagent_requested(&mut self, request: SubagentRequest) -> Vec<Effect> {
        let mut plan = BackgroundRun::plan(
            request,
            self.catalog.as_ref(),
            self.model.as_ref(),
            self.auto_compact,
        );
        // With no size of its own it follows the user's price choice for the
        // model it runs on, which may not be the chat's.
        plan.auto_compact = plan
            .auto_compact
            .or_else(|| self.standard_price_threshold(plan.model.as_ref()));
        self.queued.push_back(plan);
        self.start_queued()
    }

    /// Starts queued subagents while `/subagents parallel` leaves room.
    pub(super) fn start_queued(&mut self) -> Vec<Effect> {
        let mut effects = Vec::new();
        while self.has_free_slot() {
            if !self.may_start_runs() {
                // Past a cap they wait for the user's choice.
                if !self.queued.is_empty() {
                    self.offer_cap_choice();
                }
                break;
            }
            let Some(plan) = self.queued.pop_front() else {
                break;
            };
            self.background.insert(plan.request.id.clone());
            effects.push(Effect::StartSubagent(plan));
        }
        effects
    }

    fn has_free_slot(&self) -> bool {
        self.subagent_settings
            .max_concurrent
            .is_none_or(|limit| self.background.len() < limit as usize)
    }

    /// Forgets the subagents still waiting to start; returns how many.
    pub(super) fn drop_queued(&mut self) -> usize {
        let count = self.queued.len();
        self.queued.clear();
        count
    }

    /// `Task(name · profile · model)` for each queued subagent, for the
    /// live region and `/subagents`.
    pub fn queued_targets(&self) -> Vec<String> {
        self.queued
            .iter()
            .map(|plan| {
                subagent_render::describe(
                    &plan.request.name,
                    Some(&plan.request.profile),
                    &plan.runs_on(),
                )
            })
            .collect()
    }

    /// Whether `id` is a background subagent still running.
    pub(super) fn is_background(&self, id: &str) -> bool {
        self.background.contains(id)
    }

    /// A background subagent ended: its report goes to the main agent. One
    /// the user stopped reports nothing, so stopping never starts a turn.
    pub(super) fn subagent_reported(&mut self, report: SubagentReport) -> Vec<Effect> {
        self.background.remove(&report.id);
        let mut effects = self.start_queued();
        if report.status == Status::Cancelled {
            return effects;
        }
        effects.extend(self.tell_main_agent(report.message()));
        effects
    }

    /// A background command ended: the main agent hears about it the way
    /// it hears subagent reports.
    pub fn command_finished(&mut self, finished: Finished) -> Vec<Effect> {
        self.record(Item::Notice(format!(
            "⎿  Background command {} finished ({}): {}",
            finished.id,
            exit_words(&finished.exit),
            finished.command
        )));
        if !finished.git_reverts.is_empty() {
            self.record(Item::Notice(finished.git_reverts.user_notice()));
        }
        self.tell_main_agent(finished.message())
    }

    /// Hands `message` to the main agent: into the running turn as a note
    /// when it can take one, else as a turn of its own.
    fn tell_main_agent(&mut self, message: String) -> Vec<Effect> {
        let fits_a_note = !self.compacting && message.chars().count() <= MAX_NOTE_CHARS;
        match self.live_steering() {
            Some(steering) if self.is_busy() && fits_a_note => {
                steering.queue(&message);
                vec![Effect::Steer]
            }
            _ => {
                self.reports.push_back(message);
                if !self.is_busy() && !self.compacting {
                    self.next_message()
                } else {
                    Vec::new()
                }
            }
        }
    }

    /// Starts a turn that hands the main agent subagent reports, shown as a
    /// note rather than as something the user typed.
    pub fn start_report_turn(&mut self, message: &str) {
        let sources: Vec<String> = message
            .match_indices('<')
            .filter_map(|(at, _)| notice_source(&message[at..]))
            .collect();
        self.record(Item::Notice(format!(
            "⎿  gz reads the update from {}",
            sources.join(", ")
        )));
        self.begin_turn();
    }

    /// `/subagents stop`: stops every background subagent.
    pub(super) fn stop_background(&mut self) -> Vec<Effect> {
        let queued = self.drop_queued();
        if queued > 0 {
            self.record(Item::Notice(format!(
                "Dropped {queued} queued subagent{}",
                plural(queued)
            )));
        }
        if self.background.is_empty() {
            if queued == 0 {
                self.record(Item::Notice("No background subagents are running".into()));
            }
            return Vec::new();
        }
        let count = self.background.len();
        self.drop_prompts(|background| background.is_some());
        self.record(Item::Notice(format!(
            "Stopping {count} background subagent{}",
            plural(count)
        )));
        vec![Effect::StopSubagents]
    }
}

fn plural(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

/// How a message waiting for the model shows: a report by its subagent.
pub(super) fn waiting_text(text: &str) -> String {
    match notice_source(text) {
        Some(source) => format!("Update from {source}"),
        None => text.to_string(),
    }
}

/// Who a message for the main agent comes from when the user didn't type
/// it: a subagent (its report) or a background command (its end).
pub(super) fn notice_source(text: &str) -> Option<String> {
    SubagentReport::name_in(text)
        .or_else(|| Finished::id_in(text).map(|id| format!("command {id}")))
}

fn exit_words(exit: &str) -> String {
    match exit {
        "0" => "exit code 0".to_string(),
        "stopped" | "signal" | "failed" => exit.to_string(),
        code => format!("exit code {code}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant::steering::Steering;
    use crate::assistant::tools::Access;
    use crate::assistant::{AssistantEvent, TurnEnd};
    use crate::code_ui::approver::Ask;
    use crate::code_ui::state::test_support::*;
    use crate::code_ui::state::Waiting;
    use crate::subagents::SubagentEvent;
    use std::sync::{Arc, Mutex};

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

    fn report(status: Status) -> SubagentReport {
        SubagentReport {
            id: "sa_1".into(),
            name: "Check nightly-etl".into(),
            status,
            text: "Two runs timed out.".into(),
        }
    }

    fn started() -> AssistantEvent {
        AssistantEvent::Subagent(SubagentEvent::Started {
            id: "sa_1".into(),
            name: "Check nightly-etl".into(),
            profile: None,
            agent_id: None,
            task: None,
            runs_on: Default::default(),
            parent: None,
        })
    }

    fn with_steering(mut s: UiState) -> UiState {
        let resolved = crate::config::resolve(
            &crate::config::Config::default(),
            Some("background-test"),
            Some("http://127.0.0.1:9".into()),
            Some("stub-token".to_string()),
        );
        let http =
            crate::http::ApiClient::new(resolved, crate::output::OutputFormat::Json, true, 0, 5)
                .unwrap();
        s.steering = Some(Steering::new(http, Arc::new(Mutex::new(None))));
        s
    }

    #[test]
    fn a_background_subagent_outlives_the_turn_that_started_it() {
        let mut s = state();
        s.start_turn("why does nightly-etl fail?");
        let effects = s.apply(AssistantEvent::SubagentRequested(request()));
        assert!(matches!(&effects[..], [Effect::StartSubagent(plan)] if plan.request == request()));
        s.apply(started());
        s.apply(AssistantEvent::Done(TurnEnd::Completed));
        assert!(!s.is_busy(), "the main agent is free");
        assert_eq!(s.subagents.running().count(), 1, "still running");
        s.start_turn("meanwhile, list the tables");
        assert_eq!(s.subagents.running().count(), 1, "kept across turns");
    }

    #[test]
    fn a_report_while_idle_becomes_a_turn_shown_as_a_note() {
        let mut s = state();
        s.apply(AssistantEvent::SubagentRequested(request()));
        let effects = s.apply(AssistantEvent::SubagentReport(report(Status::Completed)));
        let [Effect::SendReports(message)] = &effects[..] else {
            panic!("no report turn: {effects:?}");
        };
        assert!(message.contains("Two runs timed out."));
        s.flush.clear();
        s.start_report_turn(message);
        assert!(s.is_busy());
        assert!(matches!(&s.flush[..], [Item::Notice(n)] if n.contains("Check nightly-etl")));
    }

    #[test]
    fn a_finished_background_command_reaches_the_main_agent() {
        let finished = Finished {
            id: "bg_1".into(),
            command: "npm test".into(),
            exit: "1".into(),
            git_reverts: Default::default(),
        };
        let mut idle = state();
        let effects = idle.command_finished(finished.clone());
        assert!(matches!(&effects[..], [Effect::SendReports(m)] if m.contains("bg_1")));
        assert!(matches!(idle.flush.last(), Some(Item::Notice(n)) if n.contains("exit code 1")));

        let mut busy = with_steering(with_catalog());
        busy.start_turn("go on");
        assert_eq!(busy.command_finished(finished), vec![Effect::Steer]);
        assert_eq!(
            busy.waiting(),
            vec![Waiting::Queued("Update from command bg_1".into())]
        );
    }

    #[test]
    fn a_report_while_busy_goes_into_the_running_turn() {
        let mut s = with_steering(with_catalog());
        s.start_turn("something else");
        s.apply(AssistantEvent::SubagentRequested(request()));
        let effects = s.apply(AssistantEvent::SubagentReport(report(Status::Completed)));
        assert_eq!(effects, vec![Effect::Steer]);
        assert_eq!(
            s.waiting(),
            vec![Waiting::Queued("Update from Check nightly-etl".into())]
        );
    }

    #[test]
    fn reports_that_miss_the_turn_follow_the_users_queued_message() {
        let mut s = state();
        s.start_turn("something else");
        s.apply(AssistantEvent::SubagentReport(report(Status::Completed)));
        s.queue.push_back("and then this".into());
        let effects = s.apply(AssistantEvent::Done(TurnEnd::Completed));
        assert_eq!(effects, vec![Effect::Send("and then this".into())]);
        s.start_turn("and then this");
        let effects = s.apply(AssistantEvent::Done(TurnEnd::Completed));
        assert!(matches!(&effects[..], [Effect::SendReports(m)] if m.contains("timed out")));
    }

    #[test]
    fn stopping_reports_nothing_and_drops_only_their_prompts() {
        let mut s = state();
        s.apply(AssistantEvent::SubagentRequested(request()));
        let (mut theirs, _rx1) = ask(Access::Command, "Run make logs", None);
        theirs.background = Some("sa_1".into());
        let (main, _rx2) = ask(Access::Write, "Edit README.md", None);
        s.open_prompt(theirs);
        s.open_prompt(main);
        assert_eq!(s.stop_background(), vec![Effect::StopSubagents]);
        let open = s.prompt.as_ref().map(|p| p.request.summary.clone());
        assert_eq!(
            open.as_deref(),
            Some("Edit README.md"),
            "the main prompt stays"
        );
        assert!(s
            .apply(AssistantEvent::SubagentReport(report(Status::Cancelled)))
            .is_empty());
        assert!(s.background.is_empty());
    }

    #[test]
    fn prompts_wait_their_turn_and_the_main_ones_end_with_the_turn() {
        let mut s = state();
        s.start_turn("go");
        let (theirs, _rx1) = ask(Access::Command, "Run make logs", None);
        let theirs = Ask {
            background: Some("sa_1".into()),
            ..theirs
        };
        let (main, _rx2) = ask(Access::Write, "Edit README.md", None);
        s.open_prompt(main);
        s.open_prompt(theirs);
        assert_eq!(s.prompt.as_ref().unwrap().request.summary, "Edit README.md");
        s.apply(AssistantEvent::Done(TurnEnd::Completed));
        assert_eq!(
            s.prompt.as_ref().unwrap().request.summary,
            "Run make logs",
            "the subagent's prompt outlives the main turn"
        );
    }
}
