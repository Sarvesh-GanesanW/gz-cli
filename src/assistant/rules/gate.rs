//! Applies a rule's verdict to one tool call's permission decision, by
//! standing in front of the session's approver: a deny rule refuses
//! without asking, an ask rule asks even when a grant or mode would
//! allow, an allow rule lets the call through without asking.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use super::{Kind, RuleVerdict};
use crate::assistant::hooks::PreTool;
use crate::assistant::permissions::{
    Approver, DecidedBy, Decision, DecisionFuture, PermissionRequest, Refusal, ReplyFuture,
};
use crate::assistant::questions::Query;
use crate::assistant::tools::Access;

/// The approver for one call, with the rule verdict for it.
pub struct Gated<'a> {
    inner: &'a dyn Approver,
    verdict: Option<RuleVerdict>,
    access: Access,
    /// The verdict decides the call's own prompt only; later prompts (a
    /// script's edits) go to the inner approver as usual.
    pending: AtomicBool,
    refusal: Mutex<Option<Refusal>>,
}

impl<'a> Gated<'a> {
    pub fn new(inner: &'a dyn Approver, verdict: Option<RuleVerdict>, access: Access) -> Self {
        Self {
            inner,
            verdict,
            access,
            pending: AtomicBool::new(true),
            refusal: Mutex::new(None),
        }
    }

    /// Whether the call goes ahead without a decision, given whether the
    /// grants (`granted`) would allow it. Plan mode stays read-only: what
    /// it refuses goes to the approver even when a rule or grant allows it.
    pub fn preapproved(&self, granted: bool) -> bool {
        if self.inner.in_plan_mode() && refused_in_plan_mode(self.access) {
            return false;
        }
        match self.kind() {
            Some(Kind::Deny | Kind::Ask) => false,
            Some(Kind::Allow) => true,
            None => granted,
        }
    }

    /// Whether a deny or ask rule covers the call, so nothing may run it
    /// without that rule's say (gz's automatic read-only approvals included).
    pub fn rule_decides(&self) -> bool {
        matches!(self.kind(), Some(Kind::Deny | Kind::Ask))
    }

    /// Whether a `pre_tool_use` hook's `allow` settles the call alone. A
    /// deny or ask rule still has its say, and so do plan mode and the
    /// calls that stay the user's own (see [`PreTool::allows_alone`]).
    pub fn hook_allows(&self, pre: &PreTool) -> bool {
        pre.allows_alone(self.access, self.inner.in_plan_mode()) && !self.rule_decides()
    }

    /// Decides the call, asking the user even where a mode would decide
    /// alone when a `pre_tool_use` hook answered `ask`. A deny or ask rule
    /// keeps its own verdict.
    pub fn decide_hooked<'b>(
        &'b self,
        request: &'b PermissionRequest,
        hook_asks: bool,
    ) -> DecisionFuture<'b> {
        if hook_asks && !self.rule_decides() {
            self.pending.store(false, Ordering::SeqCst);
            return self.inner.ask_anyway(request);
        }
        self.decide(request)
    }

    fn kind(&self) -> Option<Kind> {
        self.verdict.as_ref().map(|v| v.kind)
    }
}

/// What plan mode refuses: everything that changes something.
/// `Mode::auto_decision` refuses the same; a test keeps them in step.
pub fn refused_in_plan_mode(access: Access) -> bool {
    matches!(
        access,
        Access::Write | Access::Command | Access::Script | Access::ServerAction | Access::LocalTool
    )
}

impl Approver for Gated<'_> {
    fn decide<'b>(&'b self, request: &'b PermissionRequest) -> DecisionFuture<'b> {
        let first = self.pending.swap(false, Ordering::SeqCst);
        let verdict = self.verdict.as_ref().filter(|_| first);
        match verdict {
            Some(verdict) if verdict.kind == Kind::Deny => {
                let refusal = verdict.refusal();
                self.inner.note_refusal(request, &refusal);
                if let Ok(mut slot) = self.refusal.lock() {
                    *slot = Some(refusal);
                }
                Box::pin(async { Decision::Deny })
            }
            Some(verdict) if verdict.kind == Kind::Ask => {
                let note = verdict.ask_note();
                Box::pin(async move {
                    // The rule asks every time, so answers that would
                    // remember the call can't be offered.
                    let noted = PermissionRequest {
                        note: Some(match &request.note {
                            Some(existing) => format!("{existing}\n{note}"),
                            None => note,
                        }),
                        chat_scope: String::new(),
                        always_scope: None,
                        ..request.clone()
                    };
                    self.inner.ask_anyway(&noted).await
                })
            }
            _ => self.inner.decide(request),
        }
    }

    fn take_refusal(&self) -> Refusal {
        let own = self.refusal.lock().ok().and_then(|mut slot| slot.take());
        own.unwrap_or_else(|| self.inner.take_refusal())
    }

    fn take_decided_by(&self) -> DecidedBy {
        self.inner.take_decided_by()
    }

    fn ask_anyway<'b>(&'b self, request: &'b PermissionRequest) -> DecisionFuture<'b> {
        self.inner.ask_anyway(request)
    }

    fn note_refusal(&self, request: &PermissionRequest, refusal: &Refusal) {
        self.inner.note_refusal(request, refusal)
    }

    fn ask<'b>(&'b self, query: &'b Query) -> ReplyFuture<'b> {
        self.inner.ask(query)
    }

    fn in_plan_mode(&self) -> bool {
        self.inner.in_plan_mode()
    }

    fn enter_plan_mode(&self) -> bool {
        self.inner.enter_plan_mode()
    }

    fn accepts_edits(&self) -> bool {
        self.inner.accepts_edits()
    }

    fn refuses_unasked(&self) -> bool {
        self.inner.refuses_unasked()
    }
}
