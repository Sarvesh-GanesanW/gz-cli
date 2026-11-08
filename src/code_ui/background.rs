//! Starts and stops the background subagents the chat asks for (see
//! [`crate::assistant::background`]). Each runs as its own session on its
//! own task; its events join the main event stream and its permission
//! prompts the main prompt queue, so the UI shows them like any other.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;

use crate::assistant::background::{self, BackgroundRun};
use crate::assistant::permissions::Policy;
use crate::assistant::{AssistantEvent, Canceller, Session, TurnEnd};
use crate::code_ui::approver::{Ask, AutoReview, ChannelApprover, Mode};
use crate::http::ApiClient;

/// How long gz waits on exit for the chat to stop running subagents.
const STOP_ON_EXIT: Duration = Duration::from_secs(3);

pub struct Launcher {
    http: ApiClient,
    root: PathBuf,
    events: mpsc::UnboundedSender<AssistantEvent>,
    asks: mpsc::UnboundedSender<Ask>,
    mode: Arc<Mutex<Mode>>,
    auto: Option<AutoReview>,
    /// The main session's grants, which every subagent shares.
    policy: Policy,
    /// Every subagent started, to stop them; finished ones stop at once.
    running: Vec<Canceller>,
}

impl Launcher {
    pub fn new(
        http: ApiClient,
        root: PathBuf,
        events: mpsc::UnboundedSender<AssistantEvent>,
        asks: mpsc::UnboundedSender<Ask>,
        mode: Arc<Mutex<Mode>>,
        auto: Option<AutoReview>,
        policy: Policy,
    ) -> Self {
        Self {
            http,
            root,
            events,
            asks,
            mode,
            auto,
            policy,
            running: Vec::new(),
        }
    }

    /// Runs `plan` on the model, effort and context size it resolved.
    pub fn start(&mut self, plan: BackgroundRun) {
        let request = &plan.request;
        let mut session = match Session::new(self.http.clone(), &self.root) {
            Ok(session) => {
                session.into_subagent(&request.profile, &request.name, self.policy.shared())
            }
            Err(err) => return self.fail(plan.clone(), &format!("{err:#}")),
        };
        session.set_model(plan.model.clone());
        session.set_auto_compact(plan.auto_compact);
        self.running.push(session.canceller());
        let mut approver =
            ChannelApprover::new(self.asks.clone(), self.mode.clone()).for_background(&request.id);
        if let Some(auto) = self.auto.clone() {
            approver = approver.with_auto(auto);
        }
        let events = self.events.clone();
        tokio::spawn(async move {
            let mut emit = |event| {
                events.send(event).ok();
            };
            background::run(session, plan, &approver, &mut emit).await;
        });
    }

    /// A subagent that could not start fails at once, so the main agent
    /// hears about it.
    fn fail(&self, plan: BackgroundRun, why: &str) {
        let text = format!("it could not start: {why}");
        let runs_on = plan.runs_on();
        let relay = background::Relay::new(plan.request, runs_on);
        self.events.send(relay.started()).ok();
        for event in relay.finish(&TurnEnd::Failed(text)) {
            self.events.send(event).ok();
        }
    }

    /// Stops every running subagent; their runs end as cancelled.
    pub fn stop_all(&mut self) {
        for canceller in self.running.drain(..) {
            tokio::spawn(async move { canceller.cancel().await.ok() });
        }
    }

    /// On exit: stops what still runs, waiting briefly for the chat.
    pub async fn stop_on_exit(&mut self) {
        let cancels: Vec<_> = self
            .running
            .drain(..)
            .map(|c| tokio::spawn(async move { c.cancel().await.ok() }))
            .collect();
        let all = async {
            for cancel in cancels {
                cancel.await.ok();
            }
        };
        tokio::time::timeout(STOP_ON_EXIT, all).await.ok();
    }
}
