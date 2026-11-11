//! Permission prompts for the interactive UI: the session's [`Approver`]
//! hands each request to the UI over a channel and waits for the answer,
//! after the current mode has had its say. In auto mode a review decides
//! instead (see [`crate::code_ui::auto_mode`]), and every answer the user
//! gives is remembered as precedent for it.

use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot};

use crate::assistant::action_review::{ActionReviewer, ReviewWorkspace, Verdict};
use crate::assistant::auto_rules::{Answer as PastAnswer, AutoStore};
use crate::assistant::permissions::{DecidedBy, Refusal, ReplyFuture};
use crate::assistant::questions::{Query, Reply};
use crate::assistant::sandbox::Sandbox;
use crate::assistant::tools::Access;
use crate::assistant::{Approver, Decision, DecisionFuture, PermissionRequest};
use crate::code_ui::auto_mode::{needs_review, AutoContext, PAUSED_NOTE};
use crate::code_ui::scan_cap::{with_review_note, ScanCheck};

/// How much gz may do without asking. Shift+Tab cycles through them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Ask before anything that isn't already allowed.
    #[default]
    Ask,
    /// Edits and writes go through without a prompt.
    AcceptEdits,
    /// Read-only: anything that changes files or runs code is refused.
    Plan,
    /// A review decides; see [`crate::code_ui::auto_mode`].
    Auto,
}

/// What the model is told when plan mode refuses an action.
/// The command that lets a blocked action run (`UiState::allow_command`).
pub const ALLOW_COMMAND: &str = "/allow";

pub const PLAN_REFUSAL: &str =
    "plan mode is on (read-only); describe the change or command instead of running it.";

impl Mode {
    /// The next mode for Shift+Tab: auto mode first, when the server
    /// reviews actions, then accept edits and plan.
    pub fn next(self, auto_available: bool) -> Self {
        match self {
            Mode::Ask if auto_available => Mode::Auto,
            Mode::Ask | Mode::Auto => Mode::AcceptEdits,
            Mode::AcceptEdits => Mode::Plan,
            Mode::Plan => Mode::Ask,
        }
    }

    /// Whether this mode asks the user, so a prompt can offer to switch
    /// to auto mode instead.
    pub fn is_manual(self) -> bool {
        matches!(self, Mode::Ask | Mode::AcceptEdits)
    }

    /// The mode line under the input box; `None` for the default mode.
    pub fn banner(self) -> Option<&'static str> {
        match self {
            Mode::Ask => None,
            Mode::AcceptEdits => Some("⏵⏵ accept edits on"),
            Mode::Plan => Some("⏸ plan mode on"),
            Mode::Auto => Some("⏵⏵ auto mode on"),
        }
    }

    /// The answer this mode gives without asking or reviewing, if any.
    pub fn auto_decision(self, access: Access) -> Option<Decision> {
        match (self, access) {
            (Mode::AcceptEdits, Access::Write) => Some(Decision::AllowOnce),
            (
                Mode::Plan,
                Access::Write
                | Access::Command
                | Access::Script
                | Access::ServerAction
                | Access::LocalTool,
            ) => Some(Decision::Deny),
            _ => None,
        }
    }
}

/// The user's answer: a decision plus, for "No, and tell gz…", their note.
pub type Answer = (Decision, Option<String>);

/// One prompt waiting for the user.
pub struct Ask {
    pub request: PermissionRequest,
    pub reply: oneshot::Sender<Answer>,
    /// The background subagent asking, whose prompts outlive the main turn.
    pub background: Option<String>,
}

/// What auto mode needs: the review, what the session remembers, and the
/// user's rules and past answers for this folder.
#[derive(Clone)]
pub struct AutoReview {
    pub reviewer: Arc<dyn ActionReviewer>,
    pub context: Arc<Mutex<AutoContext>>,
    pub store: AutoStore,
    /// The session's folders: answers are remembered for, and the review
    /// told about, the current one.
    pub sandbox: Sandbox,
}

impl AutoReview {
    fn workspace(&self) -> ReviewWorkspace {
        ReviewWorkspace {
            name: self.sandbox.name(),
            platform: crate::assistant::session::platform(),
        }
    }
}

/// A question or plan the model puts to the user, waiting for the reply.
pub struct UiQuery {
    pub query: Query,
    pub reply: oneshot::Sender<Reply>,
}

pub struct ChannelApprover {
    asks: mpsc::UnboundedSender<Ask>,
    /// Where questions and plans go; `None` answers them with no reply.
    queries: Option<mpsc::UnboundedSender<UiQuery>>,
    mode: Arc<Mutex<Mode>>,
    refusal: Mutex<Option<Refusal>>,
    /// Who allowed the last call, for the audit log.
    decided_by: Mutex<Option<DecidedBy>>,
    auto: Option<AutoReview>,
    background: Option<String>,
}

impl ChannelApprover {
    pub fn new(asks: mpsc::UnboundedSender<Ask>, mode: Arc<Mutex<Mode>>) -> Self {
        Self {
            asks,
            mode,
            refusal: Mutex::new(None),
            decided_by: Mutex::new(None),
            auto: None,
            background: None,
            queries: None,
        }
    }

    /// Shows the model's questions and plans in the UI.
    pub fn with_queries(mut self, queries: mpsc::UnboundedSender<UiQuery>) -> Self {
        self.queries = Some(queries);
        self
    }

    fn mode(&self) -> Mode {
        self.mode.lock().map(|m| *m).unwrap_or_default()
    }

    /// Asks for the background subagent `id`, in its own approver so its
    /// refusals and audit notes never mix with the main agent's.
    pub fn for_background(mut self, id: &str) -> Self {
        self.background = Some(id.to_string());
        self
    }

    /// Enables auto mode.
    pub fn with_auto(mut self, auto: AutoReview) -> Self {
        self.auto = Some(auto);
        self
    }

    /// Allows a call without asking, remembering which mode did.
    fn allow_by(&self, by: DecidedBy) -> Decision {
        if let Ok(mut slot) = self.decided_by.lock() {
            *slot = Some(by);
        }
        Decision::AllowOnce
    }

    fn refuse(&self, refusal: Refusal) -> Decision {
        if let Ok(mut slot) = self.refusal.lock() {
            *slot = Some(refusal);
        }
        Decision::Deny
    }

    /// Asks the user and remembers the answer as auto mode's precedent.
    async fn ask_user(&self, request: &PermissionRequest) -> Decision {
        let (reply, answer) = oneshot::channel();
        let ask = Ask {
            request: request.clone(),
            reply,
            background: self.background.clone(),
        };
        if self.asks.send(ask).is_err() {
            return self.refuse(Refusal::User { note: None });
        }
        // A dropped prompt (UI closed, run interrupted) is a refusal.
        let (decision, note) = answer.await.unwrap_or((Decision::Deny, None));
        // Only actions auto mode reviews are worth remembering as precedent.
        if let Some(auto) = self.auto.as_ref().filter(|_| needs_review(request)) {
            let past = if decision == Decision::Deny {
                PastAnswer::Denied
            } else {
                PastAnswer::Allowed
            };
            if let Err(err) =
                auto.store
                    .record_decision(&auto.sandbox.cwd(), &request.summary, past)
            {
                crate::output::warn(&format!("cannot remember the answer: {err:#}"));
            }
        }
        if decision == Decision::Deny {
            return self.refuse(Refusal::User { note });
        }
        decision
    }

    /// Auto mode: run, review, or (paused, or the review failed) ask.
    async fn decide_auto(&self, auto: &AutoReview, request: &PermissionRequest) -> Decision {
        let with_auto = |change: &dyn Fn(&mut AutoContext)| {
            if let Ok(mut context) = auto.context.lock() {
                change(&mut context);
            }
        };
        if !needs_review(request) {
            with_auto(&|c| c.took(&request.summary));
            return self.allow_by(DecidedBy::AutoMode);
        }
        // The user's own word on this exact action outranks the review
        // that blocked it, and a pause.
        let user_allowed = auto
            .context
            .lock()
            .map(|mut c| c.take_allowance(request))
            .unwrap_or(false);
        if user_allowed {
            with_auto(&|c| {
                c.resume();
                c.took(&request.summary);
            });
            return self.allow_by(DecidedBy::UserAfterBlock);
        }
        let paused = auto.context.lock().map(|c| c.is_paused()).unwrap_or(true);
        if paused {
            let decision = self.ask_user(&noted(request, PAUSED_NOTE)).await;
            if decision != Decision::Deny {
                with_auto(&|c| {
                    c.resume();
                    c.took(&request.summary);
                });
            }
            return decision;
        }
        let reviewed = match self.check_scan(auto, request).await {
            ScanCheck::Over(reason) => return self.block(auto, request, reason),
            ScanCheck::Within(note) | ScanCheck::Unknown(note) => with_review_note(request, &note),
            ScanCheck::NotAQuery => request.clone(),
        };
        let rules = auto.store.rules().unwrap_or_default();
        let decisions = auto
            .store
            .decisions_for(&auto.sandbox.cwd())
            .unwrap_or_default();
        let review = auto
            .context
            .lock()
            .ok()
            .and_then(|c| c.review_request(&reviewed, auto.workspace(), rules, decisions));
        let Some(review) = review else {
            return self
                .ask_user(&noted(
                    request,
                    "Auto mode has no message from you to judge this by.",
                ))
                .await;
        };
        match auto.reviewer.review(&review).await {
            Ok(Verdict::Allow) => {
                with_auto(&|c| {
                    c.allowed();
                    c.took(&request.summary);
                });
                self.allow_by(DecidedBy::AutoMode)
            }
            Ok(Verdict::Block(reason)) => self.block(auto, request, reason),
            Err(err) => {
                let note = format!("Auto mode couldn't check this ({err:#}), so it asks.");
                self.ask_user(&noted(request, &note)).await
            }
        }
    }

    /// Auto mode blocks `request`; `/allow` lets that same action run once.
    fn block(&self, auto: &AutoReview, request: &PermissionRequest, reason: String) -> Decision {
        let paused = auto
            .context
            .lock()
            .map(|mut context| context.blocked(request, &reason))
            .unwrap_or(false);
        let reason = if paused {
            format!("{reason} Auto mode is now paused: gz asks the user next time.")
        } else {
            reason
        };
        self.refuse(Refusal::Blocked {
            by: "auto mode",
            reason,
            allow_with: Some(ALLOW_COMMAND),
        })
    }

    /// The scan cap's word on the queries `request` runs.
    async fn check_scan(&self, auto: &AutoReview, request: &PermissionRequest) -> ScanCheck {
        let gate = auto.context.lock().ok().and_then(|c| c.scan_gate());
        match gate {
            Some(gate) => gate.check(request, &auto.sandbox.cwd()).await,
            None => ScanCheck::NotAQuery,
        }
    }
}

fn noted(request: &PermissionRequest, note: &str) -> PermissionRequest {
    PermissionRequest {
        note: Some(note.to_string()),
        ..request.clone()
    }
}

impl Approver for ChannelApprover {
    fn decide<'a>(&'a self, request: &'a PermissionRequest) -> DecisionFuture<'a> {
        Box::pin(async move {
            // Opening another folder widens what gz can reach, so the user
            // decides it in every mode.
            if request.access == Access::Folder {
                return self.ask_user(request).await;
            }
            let mode = self.mode.lock().map(|m| *m).unwrap_or_default();
            if let (Mode::Auto, Some(auto)) = (mode, &self.auto) {
                return self.decide_auto(auto, request).await;
            }
            match mode.auto_decision(request.access) {
                Some(Decision::Deny) => self.refuse(Refusal::Blocked {
                    by: "plan mode",
                    reason: PLAN_REFUSAL.to_string(),
                    allow_with: None,
                }),
                Some(_) => self.allow_by(DecidedBy::AcceptEdits),
                None => self.ask_user(request).await,
            }
        })
    }

    fn take_decided_by(&self) -> DecidedBy {
        self.decided_by
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
            .unwrap_or(DecidedBy::User)
    }

    fn take_refusal(&self) -> Refusal {
        self.refusal
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
            .unwrap_or(Refusal::User { note: None })
    }

    fn ask<'a>(&'a self, query: &'a Query) -> ReplyFuture<'a> {
        Box::pin(async move {
            let Some(queries) = &self.queries else {
                return Reply::NoAnswer;
            };
            let (reply, answer) = oneshot::channel();
            let sent = queries.send(UiQuery {
                query: query.clone(),
                reply,
            });
            if sent.is_err() {
                return Reply::NoAnswer;
            }
            // A dropped query (UI closed, run ended) is no answer.
            answer.await.unwrap_or(Reply::NoAnswer)
        })
    }

    fn in_plan_mode(&self) -> bool {
        self.mode() == Mode::Plan
    }

    fn enter_plan_mode(&self) -> bool {
        if let Ok(mut mode) = self.mode.lock() {
            *mode = Mode::Plan;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant::action_review::{ReviewFuture, ReviewRequest};

    fn request(access: Access) -> PermissionRequest {
        PermissionRequest {
            access,
            summary: "edit_file a".into(),
            preview: None,
            chat_scope: "all edits this session".into(),
            always_scope: Some("always scope".into()),
            action: None,
            asked_by: None,
            target: Some("a".into()),
            note: None,
        }
    }

    fn command(line: &str) -> PermissionRequest {
        PermissionRequest {
            access: Access::Command,
            summary: format!("run_command {line}"),
            preview: Some(line.into()),
            target: None,
            ..request(Access::Command)
        }
    }

    /// Answers each review from a script and records what it was sent.
    struct Scripted {
        verdicts: Mutex<Vec<anyhow::Result<Verdict>>>,
        seen: Mutex<Vec<ReviewRequest>>,
    }

    impl ActionReviewer for Scripted {
        fn review<'a>(&'a self, request: &'a ReviewRequest) -> ReviewFuture<'a> {
            self.seen.lock().unwrap().push(request.clone());
            let verdict = self.verdicts.lock().unwrap().remove(0);
            Box::pin(async move { verdict })
        }
    }

    struct AutoSetup {
        approver: ChannelApprover,
        asks: mpsc::UnboundedReceiver<Ask>,
        reviewer: Arc<Scripted>,
        context: Arc<Mutex<AutoContext>>,
        store: AutoStore,
        /// The folder answers are remembered for.
        folder: std::path::PathBuf,
    }

    fn auto(name: &str, verdicts: Vec<anyhow::Result<Verdict>>) -> AutoSetup {
        let dir = std::env::temp_dir().join(format!("gz-approver-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = AutoStore::in_folder(&dir);
        let reviewer = Arc::new(Scripted {
            verdicts: Mutex::new(verdicts),
            seen: Mutex::new(Vec::new()),
        });
        let context = Arc::new(Mutex::new(AutoContext::default()));
        context.lock().unwrap().user_said("run the tests");
        let (tx, asks) = mpsc::unbounded_channel();
        let (folder, sandbox) = crate::assistant::sandbox::tests::temp_workspace(name);
        let approver =
            ChannelApprover::new(tx, Arc::new(Mutex::new(Mode::Auto))).with_auto(AutoReview {
                reviewer: reviewer.clone(),
                context: context.clone(),
                store: store.clone(),
                sandbox,
            });
        AutoSetup {
            approver,
            asks,
            reviewer,
            context,
            store,
            folder,
        }
    }

    #[test]
    fn modes_cycle_and_decide() {
        assert_eq!(Mode::Ask.next(false), Mode::AcceptEdits);
        assert_eq!(Mode::Ask.next(true), Mode::Auto);
        assert_eq!(Mode::Auto.next(true), Mode::AcceptEdits);
        assert_eq!(Mode::Plan.next(true), Mode::Ask);
        assert_eq!(Mode::Ask.auto_decision(Access::Write), None);
        assert_eq!(
            Mode::AcceptEdits.auto_decision(Access::Write),
            Some(Decision::AllowOnce)
        );
        assert_eq!(Mode::AcceptEdits.auto_decision(Access::Command), None);
        assert_eq!(
            Mode::Plan.auto_decision(Access::Script),
            Some(Decision::Deny)
        );
        assert_eq!(Mode::Plan.auto_decision(Access::Read), None);
    }

    #[tokio::test]
    async fn plan_mode_refuses_without_asking_and_explains() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let approver = ChannelApprover::new(tx, Arc::new(Mutex::new(Mode::Plan)));
        assert_eq!(
            approver.decide(&request(Access::Write)).await,
            Decision::Deny
        );
        assert!(rx.try_recv().is_err(), "nothing was asked");
        let refusal = approver.take_refusal();
        assert!(refusal.message().contains("plan mode is on"), "{refusal:?}");
        assert_eq!(
            approver.take_refusal(),
            Refusal::User { note: None },
            "taking clears it"
        );
    }

    #[tokio::test]
    async fn opening_a_folder_is_always_the_users_call() {
        let mut setup = auto("folder", vec![Ok(Verdict::Allow)]);
        let answering = tokio::spawn(async move {
            let ask = setup.asks.recv().await.unwrap();
            ask.reply.send((Decision::AllowChat, None)).ok();
            setup.asks
        });
        assert_eq!(
            setup.approver.decide(&request(Access::Folder)).await,
            Decision::AllowChat,
            "auto mode asks instead of reviewing"
        );
        answering.await.unwrap();
        assert!(setup.reviewer.seen.lock().unwrap().is_empty());

        let (tx, mut rx) = mpsc::unbounded_channel::<Ask>();
        let plan = ChannelApprover::new(tx, Arc::new(Mutex::new(Mode::Plan)));
        let answering = tokio::spawn(async move {
            let ask = rx.recv().await.unwrap();
            ask.reply.send((Decision::Deny, None)).ok();
        });
        assert_eq!(plan.decide(&request(Access::Folder)).await, Decision::Deny);
        answering.await.unwrap();
    }

    #[tokio::test]
    async fn asks_the_ui_and_carries_the_note() {
        let (tx, mut rx) = mpsc::unbounded_channel::<Ask>();
        let approver = ChannelApprover::new(tx, Arc::new(Mutex::new(Mode::Ask)));
        let answering = tokio::spawn(async move {
            let ask = rx.recv().await.unwrap();
            assert_eq!(ask.request.summary, "edit_file a");
            ask.reply
                .send((Decision::Deny, Some("use a loop".into())))
                .ok();
        });
        assert_eq!(
            approver.decide(&request(Access::Write)).await,
            Decision::Deny
        );
        answering.await.unwrap();
        assert_eq!(
            approver.take_refusal().message(),
            "The user denied this action: use a loop"
        );
    }

    #[tokio::test]
    async fn auto_mode_runs_edits_at_once_and_reviews_commands() {
        let mut setup = auto("allow", vec![Ok(Verdict::Allow)]);
        assert_eq!(
            setup.approver.decide(&request(Access::Write)).await,
            Decision::AllowOnce
        );
        assert_eq!(
            setup.approver.decide(&command("cargo test")).await,
            Decision::AllowOnce
        );
        assert!(setup.asks.try_recv().is_err(), "nothing was asked");
        let seen = setup.reviewer.seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "only the command was reviewed");
        assert_eq!(seen[0].user_messages, vec!["run the tests"]);
        assert_eq!(seen[0].recent_actions, vec!["edit_file a"]);
    }

    #[tokio::test]
    async fn the_audit_log_learns_who_decided() {
        let mut setup = auto(
            "decided",
            vec![Ok(Verdict::Allow), Ok(Verdict::Block("No.".into()))],
        );
        setup.approver.decide(&request(Access::Write)).await;
        assert_eq!(
            setup.approver.take_decided_by(),
            DecidedBy::AutoMode,
            "routine edit"
        );
        setup.approver.decide(&command("cargo test")).await;
        assert_eq!(
            setup.approver.take_decided_by(),
            DecidedBy::AutoMode,
            "reviewed"
        );
        setup.approver.decide(&command("rm -rf build")).await;
        let refusal = setup.approver.take_refusal();
        assert_eq!(refusal.audit_label(), "blocked");
        assert_eq!(refusal.audit_reason().as_deref(), Some("auto mode: No."));
        assert!(setup.asks.try_recv().is_err());

        let (tx, _rx) = mpsc::unbounded_channel();
        let accept = ChannelApprover::new(tx, Arc::new(Mutex::new(Mode::AcceptEdits)));
        accept.decide(&request(Access::Write)).await;
        assert_eq!(accept.take_decided_by(), DecidedBy::AcceptEdits);
        assert_eq!(
            accept.take_decided_by(),
            DecidedBy::User,
            "taking clears it"
        );
        assert_eq!(DecidedBy::User.audit_label(Decision::AllowChat), "chat");
    }

    #[tokio::test]
    async fn a_block_tells_the_model_why_and_three_in_a_row_pause() {
        let blocks = (0..3)
            .map(|_| Ok(Verdict::Block("Not asked for.".into())))
            .collect();
        let mut setup = auto("block", blocks);
        for _ in 0..3 {
            assert_eq!(
                setup.approver.decide(&command("git push -f")).await,
                Decision::Deny
            );
        }
        let refusal = setup.approver.take_refusal();
        assert!(refusal
            .message()
            .starts_with("Blocked by auto mode: Not asked for."));
        assert!(refusal.message().contains("now paused"));
        assert!(
            refusal.message().contains("/allow lets it run"),
            "the model can tell the user how to allow it: {}",
            refusal.message()
        );
        let notice = refusal.notice("run_command git push -f");
        assert!(notice.starts_with("auto mode blocked:"));
        assert!(notice.ends_with("/allow lets it run"), "{notice}");
        assert!(setup.context.lock().unwrap().is_paused());

        // Paused: the next action is asked, with a note, and approving resumes.
        let answering = tokio::spawn(async move {
            let ask = setup.asks.recv().await.unwrap();
            assert_eq!(ask.request.note.as_deref(), Some(PAUSED_NOTE));
            ask.reply.send((Decision::AllowOnce, None)).ok();
        });
        assert_eq!(
            setup.approver.decide(&command("cargo test")).await,
            Decision::AllowOnce
        );
        answering.await.unwrap();
        assert!(!setup.context.lock().unwrap().is_paused());
    }

    #[tokio::test]
    async fn an_action_the_user_allowed_runs_once_without_another_review() {
        let blocks = (0..4)
            .map(|_| Ok(Verdict::Block("Not asked for.".into())))
            .collect();
        let mut setup = auto("allow-blocked", blocks);
        let push = command("git push -f");
        for _ in 0..3 {
            assert_eq!(setup.approver.decide(&push).await, Decision::Deny);
        }
        setup.approver.take_refusal();
        assert!(setup.context.lock().unwrap().is_paused());

        setup.context.lock().unwrap().allow_blocked(None).unwrap();
        assert_eq!(setup.approver.decide(&push).await, Decision::AllowOnce);
        assert_eq!(
            setup
                .approver
                .take_decided_by()
                .audit_label(Decision::AllowOnce),
            "allowed-after-block"
        );
        assert!(
            setup.asks.try_recv().is_err(),
            "the user already answered: no prompt, even while paused"
        );
        assert!(
            !setup.context.lock().unwrap().is_paused(),
            "allowing it is the approval that resumes auto mode"
        );

        assert_eq!(
            setup.approver.decide(&push).await,
            Decision::Deny,
            "running it again is reviewed again"
        );
    }

    #[tokio::test]
    async fn authorizing_a_blocked_action_lets_the_retry_run() {
        use crate::assistant::action_review::HttpReviewer;
        use crate::tui::test_stub::{client, serve, Reply};

        let (base, server) = serve(vec![
            Reply::json(r#"{"decision": "block", "reason": "Force push was not asked for."}"#),
            Reply::json(r#"{"decision": "allow", "reason": "The user authorized it."}"#),
        ])
        .await;
        let context = Arc::new(Mutex::new(AutoContext::default()));
        context.lock().unwrap().user_said("rebase onto main");
        context.lock().unwrap().set_review_takes_blocks(true);
        let (tx, mut asks) = mpsc::unbounded_channel();
        let (_folder, sandbox) = crate::assistant::sandbox::tests::temp_workspace("authorize");
        let store_dir =
            std::env::temp_dir().join(format!("gz-approver-authorize-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&store_dir);
        let approver =
            ChannelApprover::new(tx, Arc::new(Mutex::new(Mode::Auto))).with_auto(AutoReview {
                reviewer: Arc::new(HttpReviewer::new(client(&base))),
                context: context.clone(),
                store: AutoStore::in_folder(&store_dir),
                sandbox,
            });

        let push = command("git push --force");
        assert_eq!(approver.decide(&push).await, Decision::Deny);
        let told = approver.take_refusal().message();
        assert!(
            told.contains(
                "once the user allows or authorizes this action, call it again exactly as it was"
            ),
            "the model must know a retry is right once the user authorizes: {told}"
        );
        context.lock().unwrap().user_said("I authorize it");
        assert_eq!(approver.decide(&push).await, Decision::AllowOnce);
        assert!(asks.try_recv().is_err(), "the review decided both times");

        let seen = server.await.unwrap();
        let body = |raw: &str| -> serde_json::Value {
            assert!(raw.starts_with("POST /chat/action-review "), "{raw}");
            serde_json::from_str(raw.split_once("\r\n\r\n").unwrap().1).unwrap()
        };
        let first = body(&seen[0]);
        assert!(first.get("recentBlocks").is_none(), "nothing blocked yet");
        let retry = body(&seen[1]);
        assert_eq!(
            retry["userMessages"],
            serde_json::json!(["rebase onto main", "I authorize it"])
        );
        assert_eq!(
            retry["recentBlocks"],
            serde_json::json!([{
                "summary": "run_command git push --force",
                "reason": "Force push was not asked for.",
            }])
        );
        assert_eq!(retry["action"]["summary"], "run_command git push --force");
    }

    #[tokio::test]
    async fn when_the_review_fails_the_user_is_asked_and_the_answer_is_learned() {
        let mut setup = auto("fail", vec![Err(anyhow::anyhow!("offline"))]);
        let answering = tokio::spawn(async move {
            let ask = setup.asks.recv().await.unwrap();
            assert!(ask.request.note.as_deref().unwrap().contains("offline"));
            ask.reply.send((Decision::Deny, None)).ok();
        });
        assert_eq!(
            setup.approver.decide(&command("make deploy")).await,
            Decision::Deny
        );
        answering.await.unwrap();
        let past = setup.store.decisions_for(&setup.folder).unwrap();
        assert_eq!(past.len(), 1);
        assert_eq!(past[0].action, "run_command make deploy");
        assert_eq!(past[0].decision, PastAnswer::Denied);
    }

    #[tokio::test]
    async fn the_review_gets_the_users_rules_and_past_answers() {
        let setup = auto("rules", vec![Ok(Verdict::Allow)]);
        setup
            .store
            .add_rule(
                crate::assistant::auto_rules::RuleKind::Allow,
                "make deploy-docs",
            )
            .unwrap();
        setup
            .store
            .record_decision(&setup.folder, "run_command git push", PastAnswer::Denied)
            .unwrap();
        setup.approver.decide(&command("make deploy-docs")).await;
        let seen = setup.reviewer.seen.lock().unwrap();
        assert_eq!(seen[0].user_rules.allow, vec!["make deploy-docs"]);
        assert_eq!(seen[0].past_decisions[0].action, "run_command git push");
    }
}
