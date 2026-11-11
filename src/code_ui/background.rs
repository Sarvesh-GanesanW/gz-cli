//! Starts and stops the background subagents the chat asks for (see
//! [`crate::assistant::background`]). Each runs as its own session on its
//! own task; its events join the main event stream and its permission
//! prompts the main prompt queue, so the UI shows them like any other.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;

use crate::assistant::background::{self, BackgroundRun};
use crate::assistant::checkpoints::EditHistory;
use crate::assistant::local_context::LocalContext;
use crate::assistant::permissions::Policy;
use crate::assistant::sandbox::Sandbox;
use crate::assistant::{AssistantEvent, Canceller, Session};
use crate::code_ui::approver::{Ask, AutoReview, ChannelApprover, Mode};
use crate::http::ApiClient;

/// How long gz waits on exit for the chat to stop running subagents.
const STOP_ON_EXIT: Duration = Duration::from_secs(3);

pub struct Launcher {
    http: ApiClient,
    /// The main session's folders; each subagent starts from a copy, in
    /// the main session's current folder.
    sandbox: Sandbox,
    /// Instructions, memory and allowed folders, shared with the main session.
    context: LocalContext,
    events: mpsc::UnboundedSender<AssistantEvent>,
    asks: mpsc::UnboundedSender<Ask>,
    mode: Arc<Mutex<Mode>>,
    auto: Option<AutoReview>,
    /// The main session's grants, which every subagent shares.
    policy: Policy,
    /// The chat server's client-tool catalog, for each subagent's session.
    server_tools: Option<Vec<String>>,
    /// Every subagent started, to stop them; finished ones stop at once.
    running: Vec<Canceller>,
    /// The main conversation's edit history, which subagents add to.
    edits: EditHistory,
}

impl Launcher {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        http: ApiClient,
        sandbox: Sandbox,
        context: LocalContext,
        events: mpsc::UnboundedSender<AssistantEvent>,
        asks: mpsc::UnboundedSender<Ask>,
        mode: Arc<Mutex<Mode>>,
        auto: Option<AutoReview>,
        policy: Policy,
    ) -> Self {
        Self {
            http,
            sandbox,
            context,
            events,
            asks,
            mode,
            auto,
            policy,
            running: Vec::new(),
            server_tools: None,
            edits: EditHistory::disabled(),
        }
    }

    /// Subagents record their edits in `edits`, so `/rewind` covers them.
    pub fn share_edit_history(&mut self, edits: EditHistory) {
        self.edits = edits;
    }

    /// Runs `plan` on the model, effort and context size it resolved.
    /// The server's client tools, once its capabilities arrive.
    pub fn set_server_tools(&mut self, names: Option<Vec<String>>) {
        self.server_tools = names;
    }

    pub fn start(&mut self, plan: BackgroundRun) {
        let request = &plan.request;
        let sandbox = self.sandbox.fork();
        let mut session =
            Session::with_sandbox(self.http.clone(), sandbox.clone(), self.context.clone())
                .into_subagent(&request.profile, &request.name, self.policy.shared())
                .with_edit_history(self.edits.clone());
        session.set_model(plan.model.clone());
        session.set_server_tools(self.server_tools.clone());
        session.set_auto_compact(plan.auto_compact);
        self.running.push(session.canceller());
        let mut approver =
            ChannelApprover::new(self.asks.clone(), self.mode.clone()).for_background(&request.id);
        if let Some(mut auto) = self.auto.clone() {
            // Its precedent and review follow its own folder.
            auto.sandbox = sandbox;
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
