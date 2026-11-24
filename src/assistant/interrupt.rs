//! Stopping a turn: the user's Esc becomes one signal per turn that every
//! part of the turn watches (the stream, open prompts, running commands and
//! scripts, the rest of the batch).
//!
//! The front end holds a [`TurnSignal`] through the session's `Canceller`.
//! It may raise the signal before the turn has a run id (Esc right after
//! Enter); the turn picks the request up as soon as it starts.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::watch;

use crate::assistant::permissions::{
    Approver, DecidedBy, Decision, DecisionFuture, PermissionRequest, Refusal, ReplyFuture,
};
use crate::assistant::questions::{Query, Reply};
use crate::assistant::tools::ToolOutput;

/// The result every tool call the user interrupted gets, so the
/// conversation stays well-formed: each call the model made has an answer.
pub const INTERRUPTED: &str = "Interrupted by the user";

/// One turn's stop request; cheap to clone.
#[derive(Clone)]
pub struct Interrupt(Arc<watch::Sender<bool>>);

impl Default for Interrupt {
    fn default() -> Self {
        Self(Arc::new(watch::channel(false).0))
    }
}

impl Interrupt {
    pub fn request(&self) {
        self.0.send_replace(true);
    }

    pub fn is_requested(&self) -> bool {
        *self.0.borrow()
    }

    /// Resolves once the stop is requested (at once if it already was).
    pub async fn requested(&self) {
        let mut seen = self.0.subscribe();
        // The sender lives as long as `self`, so waiting cannot fail.
        seen.wait_for(|stop| *stop).await.ok();
    }

    /// Runs `work` unless the stop comes first; `None` means it was dropped
    /// unfinished.
    pub async fn unless_stopped<T>(&self, work: impl Future<Output = T>) -> Option<T> {
        tokio::select! {
            biased;
            _ = self.requested() => None,
            done = work => Some(done),
        }
    }

    /// The tool result for a call the stop cut short or skipped.
    pub fn tool_output() -> ToolOutput {
        ToolOutput::error(INTERRUPTED)
    }
}

#[derive(Default)]
struct Slot {
    /// Turns a front end announced that have not started yet, oldest first.
    expected: VecDeque<Interrupt>,
    /// The turn running now (or the last one).
    current: Option<Interrupt>,
}

/// Where the front end and the session meet: the signal of the turn in
/// progress, and of the ones about to start.
#[derive(Clone, Default)]
pub struct TurnSignal(Arc<Mutex<Slot>>);

impl TurnSignal {
    fn slot(&self) -> MutexGuard<'_, Slot> {
        // Plain handles, still valid if a holder panicked.
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// A front end is about to send a turn: an interrupt from now on stops
    /// that turn too, even before it starts.
    pub fn expect_turn(&self) {
        self.slot().expected.push_back(Interrupt::default());
    }

    /// The signal for the turn starting now: the oldest announced one, with
    /// any stop already requested for it, else a fresh one. A stop for an
    /// earlier turn never carries over.
    pub fn claim(&self) -> Interrupt {
        let mut slot = self.slot();
        let next = slot.expected.pop_front().unwrap_or_default();
        slot.current = Some(next.clone());
        next
    }

    /// Stops the turn in progress and every announced one.
    pub fn interrupt(&self) {
        let slot = self.slot();
        slot.current
            .iter()
            .chain(&slot.expected)
            .for_each(Interrupt::request);
    }
}

/// An approver whose questions end the moment the turn is interrupted:
/// an open permission prompt answers "deny", an open question no answer.
pub struct Interruptible<'a> {
    inner: &'a dyn Approver,
    interrupt: Interrupt,
}

impl<'a> Interruptible<'a> {
    pub fn new(inner: &'a dyn Approver, interrupt: Interrupt) -> Self {
        Self { inner, interrupt }
    }
}

impl Approver for Interruptible<'_> {
    fn decide<'a>(&'a self, request: &'a PermissionRequest) -> DecisionFuture<'a> {
        Box::pin(async move {
            self.interrupt
                .unless_stopped(self.inner.decide(request))
                .await
                .unwrap_or(Decision::Deny)
        })
    }

    fn take_refusal(&self) -> Refusal {
        if self.interrupt.is_requested() {
            // The prompt the user walked away from is not a refusal.
            self.inner.take_refusal();
            return Refusal::User {
                note: Some(INTERRUPTED.to_string()),
            };
        }
        self.inner.take_refusal()
    }

    fn take_decided_by(&self) -> DecidedBy {
        self.inner.take_decided_by()
    }

    fn ask<'a>(&'a self, query: &'a Query) -> ReplyFuture<'a> {
        Box::pin(async move {
            self.interrupt
                .unless_stopped(self.inner.ask(query))
                .await
                .unwrap_or(Reply::NoAnswer)
        })
    }

    fn in_plan_mode(&self) -> bool {
        self.inner.in_plan_mode()
    }

    fn enter_plan_mode(&self) -> bool {
        self.inner.enter_plan_mode()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn a_stop_before_the_turn_starts_reaches_that_turn() {
        let signal = TurnSignal::default();
        signal.expect_turn();
        signal.interrupt();
        assert!(signal.claim().is_requested());
    }

    #[tokio::test]
    async fn a_stop_for_an_earlier_turn_does_not_carry_over() {
        let signal = TurnSignal::default();
        let first = signal.claim();
        signal.interrupt();
        assert!(first.is_requested());
        assert!(!signal.claim().is_requested(), "the next turn starts clean");
        signal.expect_turn();
        assert!(!signal.claim().is_requested(), "an announced turn too");
    }

    #[tokio::test]
    async fn a_stop_reaches_a_turn_announced_but_not_yet_started() {
        let signal = TurnSignal::default();
        signal.expect_turn();
        signal.interrupt();
        signal.expect_turn();
        assert!(signal.claim().is_requested(), "the first was stopped");
        assert!(
            !signal.claim().is_requested(),
            "the one sent after the stop runs"
        );
    }

    #[tokio::test]
    async fn unless_stopped_drops_work_the_stop_overtakes() {
        let interrupt = Interrupt::default();
        let stopper = interrupt.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            stopper.request();
        });
        let slow = tokio::time::sleep(Duration::from_secs(30));
        let done = tokio::time::timeout(Duration::from_secs(5), interrupt.unless_stopped(slow))
            .await
            .expect("the stop ends the wait");
        assert_eq!(done, None);
    }

    struct NeverAnswers;

    impl Approver for NeverAnswers {
        fn decide<'a>(&'a self, _: &'a PermissionRequest) -> DecisionFuture<'a> {
            Box::pin(std::future::pending())
        }
        fn ask<'a>(&'a self, _: &'a Query) -> ReplyFuture<'a> {
            Box::pin(std::future::pending())
        }
    }

    #[tokio::test]
    async fn an_open_prompt_or_question_closes_on_the_stop() {
        let interrupt = Interrupt::default();
        let approver = Interruptible::new(&NeverAnswers, interrupt.clone());
        interrupt.request();
        let request = PermissionRequest {
            access: crate::assistant::tools::Access::Command,
            summary: "run_command make".into(),
            preview: None,
            chat_scope: String::new(),
            always_scope: None,
            action: None,
            asked_by: None,
            target: None,
            note: None,
        };
        assert_eq!(approver.decide(&request).await, Decision::Deny);
        assert_eq!(
            approver.take_refusal().message(),
            format!("The user denied this action: {INTERRUPTED}")
        );
        let asked = approver.ask(&Query::Questions(Vec::new())).await;
        assert_eq!(asked, Reply::NoAnswer);
    }
}
