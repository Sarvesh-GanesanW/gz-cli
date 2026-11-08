//! Permission prompts for the interactive UI: the session's [`Approver`]
//! hands each request to the UI over a channel and waits for the answer,
//! after the current mode has had its say. In auto mode a review decides
//! instead (see [`crate::code_ui::auto_mode`]), and every answer the user
//! gives is remembered as precedent for it.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot};

use crate::assistant::action_review::{ActionReviewer, ReviewWorkspace, Verdict};
use crate::assistant::auto_rules::{Answer as PastAnswer, AutoStore};
use crate::assistant::permissions::{DecidedBy, Refusal};
use crate::assistant::tools::Access;
use crate::assistant::{Approver, Decision, DecisionFuture, PermissionRequest};
use crate::code_ui::auto_mode::{needs_review, AutoContext, PAUSED_NOTE};

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
                Access::Write | Access::Command | Access::Script | Access::ServerAction,
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
    pub workspace: ReviewWorkspace,
    /// The folder answers are remembered for.
    pub root: PathBuf,
}

pub struct ChannelApprover {
    asks: mpsc::UnboundedSender<Ask>,
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
        }
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
            if let Err(err) = auto
                .store
                .record_decision(&auto.root, &request.summary, past)
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
        let rules = auto.store.rules().unwrap_or_default();
        let decisions = auto.store.decisions_for(&auto.root).unwrap_or_default();
        let review = auto
            .context
            .lock()
            .ok()
            .and_then(|c| c.review_request(request, auto.workspace.clone(), rules, decisions));
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
            Ok(Verdict::Block(reason)) => {
                let mut paused = false;
                if let Ok(mut context) = auto.context.lock() {
                    paused = context.blocked(&request.summary, &reason);
                }
                let reason = if paused {
                    format!("{reason} Auto mode is now paused: gz asks the user next time.")
                } else {
                    reason
                };
                self.refuse(Refusal::Blocked {
                    by: "auto mode",
                    reason,
                })
            }
            Err(err) => {
                let note = format!("Auto mode couldn't check this ({err:#}), so it asks.");
                self.ask_user(&noted(request, &note)).await
            }
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
            let mode = self.mode.lock().map(|m| *m).unwrap_or_default();
            if let (Mode::Auto, Some(auto)) = (mode, &self.auto) {
                return self.decide_auto(auto, request).await;
            }
            match mode.auto_decision(request.access) {
                Some(Decision::Deny) => self.refuse(Refusal::Blocked {
                    by: "plan mode",
                    reason: PLAN_REFUSAL.to_string(),
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
        let approver =
            ChannelApprover::new(tx, Arc::new(Mutex::new(Mode::Auto))).with_auto(AutoReview {
                reviewer: reviewer.clone(),
                context: context.clone(),
                store: store.clone(),
                workspace: ReviewWorkspace {
                    name: "w".into(),
                    platform: "macos",
                },
                root: PathBuf::from("/work"),
            });
        AutoSetup {
            approver,
            asks,
            reviewer,
            context,
            store,
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
        assert!(refusal
            .notice("run_command git push -f")
            .starts_with("auto mode blocked:"));
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
        let past = setup
            .store
            .decisions_for(std::path::Path::new("/work"))
            .unwrap();
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
            .record_decision(
                std::path::Path::new("/work"),
                "run_command git push",
                PastAnswer::Denied,
            )
            .unwrap();
        setup.approver.decide(&command("make deploy-docs")).await;
        let seen = setup.reviewer.seen.lock().unwrap();
        assert_eq!(seen[0].user_rules.allow, vec!["make deploy-docs"]);
        assert_eq!(seen[0].past_decisions[0].action, "run_command git push");
    }
}
