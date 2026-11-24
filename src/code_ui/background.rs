//! Starts and stops the background subagents the chat asks for (see
//! [`crate::assistant::background`]). Each runs as its own session on its
//! own task; its events join the main event stream and its permission
//! prompts the main prompt queue, so the UI shows them like any other.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;

use crate::assistant::background::{self, BackgroundRun};
use crate::assistant::checkpoints::EditHistory;
use crate::assistant::hooks::Hooks;
use crate::assistant::local_context::LocalContext;
use crate::assistant::permissions::Policy;
use crate::assistant::run_controls::RunControls;
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
    /// The main session's hooks, which run around subagents' tool calls too.
    hooks: Hooks,
    /// The user's local MCP servers, shared with the main session.
    local_mcp: Option<crate::assistant::mcp_local::LocalMcp>,
    /// `--tools`, `--bare` and appended instructions, as the main session
    /// has them.
    controls: RunControls,
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
            hooks: Hooks::default(),
            local_mcp: None,
            controls: RunControls::default(),
        }
    }

    /// Subagents record their edits in `edits`, so `/rewind` covers them.
    pub fn share_edit_history(&mut self, edits: EditHistory) {
        self.edits = edits;
    }

    /// Runs the main session's hooks around every subagent's tool calls.
    pub fn set_hooks(&mut self, hooks: Hooks) {
        self.hooks = hooks;
    }

    /// The local MCP servers subagents share with the main session.
    pub fn set_local_mcp(&mut self, servers: Option<crate::assistant::mcp_local::LocalMcp>) {
        self.local_mcp = servers;
    }

    /// Gives every subagent the main session's tool choice, `--bare` and
    /// appended instructions. A scripted run's turn and budget guard stays
    /// with the main session.
    pub fn set_run_controls(&mut self, controls: RunControls) {
        self.controls = RunControls {
            guard: None,
            ..controls
        };
    }

    /// The server's client tools, once its capabilities arrive.
    pub fn set_server_tools(&mut self, names: Option<Vec<String>>) {
        self.server_tools = names;
    }

    /// Runs `plan` on the model, effort and context size it resolved.
    pub fn start(&mut self, plan: BackgroundRun) {
        let request = &plan.request;
        let sandbox = self.sandbox.fork();
        let session = self.subagent_session(&plan, sandbox.clone());
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

    /// The session `plan` runs in: the main session's settings, in
    /// `sandbox`.
    fn subagent_session(&self, plan: &BackgroundRun, sandbox: Sandbox) -> Session {
        let request = &plan.request;
        let mut session = Session::with_sandbox(self.http.clone(), sandbox, self.context.clone())
            .into_subagent(&request.profile, &request.name, self.policy.shared())
            .with_edit_history(self.edits.clone());
        session.set_model(plan.model.clone());
        session.set_server_tools(self.server_tools.clone());
        session.set_local_mcp(self.local_mcp.clone());
        session.set_auto_compact(plan.auto_compact);
        session.set_hooks(self.hooks.for_subagent());
        session.set_run_controls(self.controls.clone());
        if !self.controls.bare {
            session.set_diagnostics(crate::assistant::diagnostics::Diagnostics::load());
        }
        session
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant::run_controls::{ExtraInstruction, StepGuard};
    use crate::subagents::SubagentRequest;

    struct StopsEverything;

    impl StepGuard for StopsEverything {
        fn before_leg(&self) -> Option<String> {
            Some("stop".into())
        }
    }

    fn launcher(name: &str) -> Launcher {
        let dir = crate::assistant::sandbox::tests::temp_dir(name);
        let resolved = crate::config::resolve(
            &crate::config::Config::default(),
            Some("launcher-test"),
            Some("http://127.0.0.1:9".into()),
            Some("stub-token".to_string()),
        );
        let http = ApiClient::new(resolved, crate::output::OutputFormat::Json, true, 0, 5).unwrap();
        let (allowed, _) = crate::assistant::local_context::tests::recording_folders(Vec::new());
        let (events, _) = mpsc::unbounded_channel();
        let (asks, _) = mpsc::unbounded_channel();
        Launcher::new(
            http,
            Sandbox::new(&dir).unwrap(),
            LocalContext::new(None, None, allowed),
            events,
            asks,
            Arc::new(Mutex::new(Mode::default())),
            None,
            Policy::default(),
        )
    }

    fn plan() -> BackgroundRun {
        BackgroundRun {
            request: SubagentRequest {
                id: "sa_1".into(),
                name: "Look around".into(),
                profile: "general".into(),
                prompt: "list the files".into(),
                model: None,
                reasoning_effort: None,
                context_tokens: None,
            },
            model: None,
            auto_compact: None,
        }
    }

    #[test]
    fn subagents_follow_the_main_sessions_tools_bare_and_instructions() {
        let mut launcher = launcher("launcher-controls");
        launcher.set_run_controls(RunControls {
            tools: Some(vec!["read_file".into()]),
            extra_instructions: vec![
                ExtraInstruction::new("--append-system-prompt", "Be brief").unwrap()
            ],
            bare: true,
            guard: Some(Arc::new(StopsEverything)),
        });
        let session = launcher.subagent_session(&plan(), launcher.sandbox.fork());
        assert!(session.offers_tool("read_file"));
        assert!(!session.offers_tool("run_command"));
        assert!(!session.offers_tool("edit_file"));
        let controls = session.run_controls();
        assert!(controls.bare);
        assert!(!session.checks_edits(), "--bare runs no diagnostics");
        assert_eq!(controls.extra_instructions.len(), 1);
        assert!(
            controls.before_leg().is_none(),
            "the main run's turn and budget guard is not the subagent's"
        );
    }
}
