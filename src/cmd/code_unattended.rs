//! Permissions for one-shot `gz code "<prompt>"` runs that must not wait for
//! a person: script mode (`-o json`/`-o yaml`), and `--auto`, whose review
//! decides without a terminal. What was already allowed (saved rules,
//! `--yes-reads`, this chat's grants) never reaches these approvers; they
//! only see actions that would otherwise prompt.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use tokio::sync::mpsc;

use crate::assistant::action_review::HttpReviewer;
use crate::assistant::auto_rules::AutoStore;
use crate::assistant::models::Catalog;
use crate::assistant::permissions::{DecidedBy, Refusal, ReplyFuture};
use crate::assistant::questions::Query;
use crate::assistant::sandbox::Sandbox;
use crate::assistant::tools::Access;
use crate::assistant::{Approver, Decision, DecisionFuture, PermissionRequest};
use crate::code_ui::approver::{AutoReview, ChannelApprover, Mode};
use crate::code_ui::auto_mode::AutoContext;
use crate::http::ApiClient;

/// Who refuses an action nobody could approve, as the model and the audit
/// log name it.
const UNATTENDED: &str = "unattended run";

/// Why: no one is at a terminal. The model reads this as the tool result,
/// so it says what to do instead of retrying.
const NEEDS_APPROVAL: &str = "this action needs the user's approval and no one is at a terminal \
     to give it. Finish what you can without it and say what was left undone.";

/// The refusal for an action that needed a person's approval.
fn needs_approval() -> Refusal {
    Refusal::Blocked {
        by: UNATTENDED,
        reason: NEEDS_APPROVAL.to_string(),
        allow_with: None,
    }
}

/// What one approver said about an action, kept for the run's report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decided {
    /// The tool the request is for (`run_command`, or a server action's own
    /// name).
    pub tool: String,
    pub summary: String,
    /// A Groundzero action the server runs, rather than a local tool.
    pub server_action: bool,
    /// The audit log's word: `once`, `auto`, `blocked`, …
    pub decision: &'static str,
    /// Why it was refused, when a mode refused it.
    pub reason: Option<String>,
    pub refused: bool,
}

/// Decisions in the order they were made; the report drains it as the run's
/// events arrive.
#[derive(Clone, Default)]
pub struct DecisionLog(Arc<Mutex<Vec<Decided>>>);

impl DecisionLog {
    fn push(&self, decided: Decided) {
        if let Ok(mut log) = self.0.lock() {
            log.push(decided);
        }
    }

    pub fn drain(&self) -> Vec<Decided> {
        self.0
            .lock()
            .map(|mut log| std::mem::take(&mut *log))
            .unwrap_or_default()
    }
}

/// The session asks for "who allowed it" or "why refused" right after a
/// decision; wrappers settle both at decision time and hand them back here.
#[derive(Default)]
struct Settled {
    refusal: Mutex<Option<Refusal>>,
    decided_by: Mutex<Option<DecidedBy>>,
}

impl Settled {
    /// Takes `inner`'s account of `decision` now, so this wrapper can answer
    /// the session's follow-up questions itself.
    fn settle(&self, inner: &dyn Approver, decision: Decision) -> Settlement {
        if decision == Decision::Deny {
            let refusal = inner.take_refusal();
            let settlement = Settlement::Refused(refusal.clone());
            store(&self.refusal, refusal);
            return settlement;
        }
        let by = inner.take_decided_by();
        store(&self.decided_by, by);
        Settlement::Allowed(by.audit_label(decision))
    }

    fn take_refusal(&self) -> Refusal {
        take(&self.refusal).unwrap_or(Refusal::User { note: None })
    }

    fn take_decided_by(&self) -> DecidedBy {
        take(&self.decided_by).unwrap_or(DecidedBy::User)
    }
}

enum Settlement {
    Allowed(&'static str),
    Refused(Refusal),
}

fn store<T>(slot: &Mutex<Option<T>>, value: T) {
    if let Ok(mut slot) = slot.lock() {
        *slot = Some(value);
    }
}

fn take<T>(slot: &Mutex<Option<T>>) -> Option<T> {
    slot.lock().ok().and_then(|mut slot| slot.take())
}

/// Refuses every action it is asked about: it is only asked when nothing
/// already allows the action, and there is no one to ask.
pub struct Nobody;

impl Approver for Nobody {
    fn decide<'a>(&'a self, _request: &'a PermissionRequest) -> DecisionFuture<'a> {
        Box::pin(async { Decision::Deny })
    }

    fn take_refusal(&self) -> Refusal {
        needs_approval()
    }
}

/// Auto mode without the interactive UI: the review runs or blocks each
/// action; whatever the review would hand to a person (a new folder, a
/// paused review, a failed review) goes to `escalate` instead.
pub struct AutoApprover {
    review: ChannelApprover,
    escalate: Box<dyn Approver>,
    settled: Settled,
}

impl AutoApprover {
    /// Fails when the chat server has no action review, since auto mode
    /// would then refuse every command.
    pub fn new(
        http: &ApiClient,
        sandbox: &Sandbox,
        catalog: Option<&Catalog>,
        prompt: &str,
        escalate: Box<dyn Approver>,
    ) -> Result<Self> {
        let catalog = catalog.context(
            "--auto needs the chat server's model list to check for its action review, and it \
             couldn't be loaded; check `gz auth status`, or allow actions ahead of time with \
             saved rules (/permissions allow) or --yes-reads",
        )?;
        if !catalog.action_review {
            anyhow::bail!(
                "--auto needs a chat server that reviews actions (features.actionReview), and \
                 this one doesn't; allow actions ahead of time with saved rules \
                 (/permissions allow) or --yes-reads"
            );
        }
        let store = AutoStore::open().context("auto mode can't find the gz config folder")?;
        let mut context = AutoContext::default();
        context.set_review_takes_blocks(catalog.action_review_authorizations);
        // The review judges each action by what the user asked for.
        context.user_said(prompt);
        // Nobody reads the prompts auto mode would raise: with the receiver
        // gone, each one comes back as a refusal for `escalate` to settle,
        // and none is remembered as the user's own answer.
        let (asks, _) = mpsc::unbounded_channel();
        let review =
            ChannelApprover::new(asks, Arc::new(Mutex::new(Mode::Auto))).with_auto(AutoReview {
                reviewer: Arc::new(HttpReviewer::new(http.clone())),
                context: Arc::new(Mutex::new(context)),
                store,
                sandbox: sandbox.clone(),
            });
        Ok(Self::with_review(review, escalate))
    }

    fn with_review(review: ChannelApprover, escalate: Box<dyn Approver>) -> Self {
        Self {
            review,
            escalate,
            settled: Settled::default(),
        }
    }
}

impl Approver for AutoApprover {
    fn decide<'a>(&'a self, request: &'a PermissionRequest) -> DecisionFuture<'a> {
        Box::pin(async move {
            let decision = self.review.decide(request).await;
            if decision != Decision::Deny {
                self.settled.settle(&self.review, decision);
                return decision;
            }
            match self.review.take_refusal() {
                // The review wanted to ask a person.
                Refusal::User { .. } => {
                    let decision = self.escalate.decide(request).await;
                    self.settled.settle(self.escalate.as_ref(), decision);
                    decision
                }
                blocked => {
                    store(&self.settled.refusal, blocked);
                    Decision::Deny
                }
            }
        })
    }

    fn take_refusal(&self) -> Refusal {
        self.settled.take_refusal()
    }

    fn take_decided_by(&self) -> DecidedBy {
        self.settled.take_decided_by()
    }
}

/// Passes every request to `inner` and notes each decision in a
/// [`DecisionLog`] for the run's report.
pub struct Recording {
    inner: Box<dyn Approver>,
    log: DecisionLog,
    settled: Settled,
}

impl Recording {
    pub fn new(inner: Box<dyn Approver>, log: DecisionLog) -> Self {
        Self {
            inner,
            log,
            settled: Settled::default(),
        }
    }
}

impl Approver for Recording {
    fn decide<'a>(&'a self, request: &'a PermissionRequest) -> DecisionFuture<'a> {
        Box::pin(async move {
            let decision = self.inner.decide(request).await;
            let settlement = self.settled.settle(self.inner.as_ref(), decision);
            self.log.push(decided(request, settlement));
            decision
        })
    }

    fn take_refusal(&self) -> Refusal {
        self.settled.take_refusal()
    }

    fn take_decided_by(&self) -> DecidedBy {
        self.settled.take_decided_by()
    }

    fn ask<'a>(&'a self, query: &'a Query) -> ReplyFuture<'a> {
        self.inner.ask(query)
    }
}

fn decided(request: &PermissionRequest, settlement: Settlement) -> Decided {
    let server_action = request.access == Access::ServerAction;
    // Local requests' summaries start with the tool (`run_command ls`).
    let tool = match &request.action {
        Some(action) if server_action => action.clone(),
        _ => request
            .summary
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string(),
    };
    let (decision, reason, refused) = match settlement {
        Settlement::Allowed(label) => (label, None, false),
        Settlement::Refused(refusal) => (refusal.audit_label(), refusal.audit_reason(), true),
    };
    Decided {
        tool,
        summary: request.summary.clone(),
        server_action,
        decision,
        reason,
        refused,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant::action_review::{ActionReviewer, ReviewFuture, ReviewRequest, Verdict};

    fn command(line: &str) -> PermissionRequest {
        PermissionRequest {
            access: Access::Command,
            summary: format!("run_command {line}"),
            preview: Some(line.into()),
            chat_scope: "this command".into(),
            always_scope: Some("always".into()),
            action: None,
            asked_by: None,
            target: None,
            note: None,
        }
    }

    struct Says(Verdict);

    impl ActionReviewer for Says {
        fn review<'a>(&'a self, _: &'a ReviewRequest) -> ReviewFuture<'a> {
            let verdict = self.0.clone();
            Box::pin(async move { Ok(verdict) })
        }
    }

    fn auto(verdict: Verdict) -> AutoApprover {
        let dir = std::env::temp_dir().join(format!(
            "gz-unattended-{}-{}",
            std::process::id(),
            matches!(verdict, Verdict::Allow)
        ));
        let mut context = AutoContext::default();
        context.user_said("run the tests");
        let (asks, _) = mpsc::unbounded_channel();
        let review =
            ChannelApprover::new(asks, Arc::new(Mutex::new(Mode::Auto))).with_auto(AutoReview {
                reviewer: Arc::new(Says(verdict)),
                context: Arc::new(Mutex::new(context)),
                store: AutoStore::in_folder(&dir),
                sandbox: Sandbox::new(&std::env::temp_dir()).unwrap(),
            });
        AutoApprover::with_review(review, Box::new(Nobody))
    }

    #[tokio::test]
    async fn nobody_refuses_saying_approval_was_needed() {
        let log = DecisionLog::default();
        let approver = Recording::new(Box::new(Nobody), log.clone());
        assert_eq!(
            approver.decide(&command("cargo test")).await,
            Decision::Deny
        );
        let refusal = approver.take_refusal();
        assert!(refusal.message().contains("needs the user's approval"));
        assert_eq!(
            log.drain(),
            vec![Decided {
                tool: "run_command".into(),
                summary: "run_command cargo test".into(),
                server_action: false,
                decision: "blocked",
                reason: Some(format!("{UNATTENDED}: {NEEDS_APPROVAL}")),
                refused: true,
            }]
        );
    }

    #[tokio::test]
    async fn auto_mode_runs_what_the_review_allows_without_a_terminal() {
        let log = DecisionLog::default();
        let approver = Recording::new(Box::new(auto(Verdict::Allow)), log.clone());
        assert_eq!(
            approver.decide(&command("cargo test")).await,
            Decision::AllowOnce
        );
        assert_eq!(approver.take_decided_by(), DecidedBy::AutoMode);
        assert_eq!(log.drain()[0].decision, "auto");
    }

    #[tokio::test]
    async fn auto_mode_blocks_keep_the_reviews_reason() {
        let approver = auto(Verdict::Block("Not asked for.".into()));
        assert_eq!(approver.decide(&command("rm -rf x")).await, Decision::Deny);
        assert_eq!(approver.take_refusal().audit_label(), "blocked");
    }

    #[tokio::test]
    async fn what_auto_mode_would_ask_a_person_is_refused_as_needing_approval() {
        let approver = auto(Verdict::Allow);
        let folder = PermissionRequest {
            access: Access::Folder,
            summary: "change_directory ~/other".into(),
            ..command("x")
        };
        assert_eq!(approver.decide(&folder).await, Decision::Deny);
        assert_eq!(approver.take_refusal(), needs_approval());
    }
}
