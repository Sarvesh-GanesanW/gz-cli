//! Permission prompts for the interactive UI: the session's [`Approver`]
//! hands each request to the UI over a channel and waits for the answer,
//! after the current mode has had its say.

use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot};

use crate::assistant::tools::Access;
use crate::assistant::{Approver, Decision, DecisionFuture, PermissionRequest};

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
}

/// What the model is told when plan mode refuses an action.
pub const PLAN_REFUSAL: &str =
    "plan mode is on (read-only); describe the change or command instead of running it";

impl Mode {
    pub fn next(self) -> Self {
        match self {
            Mode::Ask => Mode::AcceptEdits,
            Mode::AcceptEdits => Mode::Plan,
            Mode::Plan => Mode::Ask,
        }
    }

    /// The mode line under the input box; `None` for the default mode.
    pub fn banner(self) -> Option<&'static str> {
        match self {
            Mode::Ask => None,
            Mode::AcceptEdits => Some("⏵⏵ accept edits on"),
            Mode::Plan => Some("⏸ plan mode on"),
        }
    }

    /// The answer this mode gives without asking, if any.
    pub fn auto_decision(self, access: Access) -> Option<(Decision, Option<String>)> {
        match (self, access) {
            (Mode::AcceptEdits, Access::Write) => Some((Decision::AllowOnce, None)),
            (
                Mode::Plan,
                Access::Write | Access::Command | Access::Script | Access::ServerAction,
            ) => Some((Decision::Deny, Some(PLAN_REFUSAL.to_string()))),
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
}

pub struct ChannelApprover {
    asks: mpsc::UnboundedSender<Ask>,
    mode: Arc<Mutex<Mode>>,
    note: Mutex<Option<String>>,
}

impl ChannelApprover {
    pub fn new(asks: mpsc::UnboundedSender<Ask>, mode: Arc<Mutex<Mode>>) -> Self {
        Self {
            asks,
            mode,
            note: Mutex::new(None),
        }
    }

    fn remember_note(&self, note: Option<String>) {
        if let Ok(mut slot) = self.note.lock() {
            *slot = note;
        }
    }
}

impl Approver for ChannelApprover {
    fn decide<'a>(&'a self, request: &'a PermissionRequest) -> DecisionFuture<'a> {
        Box::pin(async move {
            let mode = self.mode.lock().map(|m| *m).unwrap_or_default();
            if let Some((decision, note)) = mode.auto_decision(request.access) {
                self.remember_note(note);
                return decision;
            }
            let (reply, answer) = oneshot::channel();
            let ask = Ask {
                request: request.clone(),
                reply,
            };
            if self.asks.send(ask).is_err() {
                return Decision::Deny;
            }
            // A dropped prompt (UI closed, run interrupted) is a refusal.
            let (decision, note) = answer.await.unwrap_or((Decision::Deny, None));
            self.remember_note(note);
            decision
        })
    }

    fn take_denial_note(&self) -> Option<String> {
        self.note.lock().ok().and_then(|mut n| n.take())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(access: Access) -> PermissionRequest {
        PermissionRequest {
            access,
            summary: "edit_file a".into(),
            preview: None,
            session_scope: "all edits this session".into(),
        }
    }

    #[test]
    fn modes_cycle_and_decide() {
        assert_eq!(Mode::Ask.next(), Mode::AcceptEdits);
        assert_eq!(Mode::Plan.next(), Mode::Ask);
        assert_eq!(Mode::Ask.auto_decision(Access::Write), None);
        assert_eq!(
            Mode::AcceptEdits.auto_decision(Access::Write).map(|a| a.0),
            Some(Decision::AllowOnce)
        );
        assert_eq!(Mode::AcceptEdits.auto_decision(Access::Command), None);
        assert_eq!(
            Mode::Plan.auto_decision(Access::Script).map(|a| a.0),
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
        assert_eq!(approver.take_denial_note().as_deref(), Some(PLAN_REFUSAL));
        assert_eq!(approver.take_denial_note(), None, "taking clears it");
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
        assert_eq!(approver.take_denial_note().as_deref(), Some("use a loop"));
    }
}
