//! The session-level events: session start and end, a sent message,
//! `pre_tool_use` before the permission check, the agent stopping, a
//! subagent stopping, compaction and notifications.

use std::path::Path;

use serde_json::json;

use super::reply::Decision;
use super::{
    cap, label, reason_of, report, shown, Batch, Events, Fired, HookEvent, Hooks, Log, ToolCall,
    FEEDBACK_CHARS,
};
use crate::assistant::session::AssistantEvent;
use crate::assistant::tools::Access;
use crate::cmd::stream::ContextUsage;

/// How a session started, for `session_start` hooks (and their matcher).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StartSource {
    #[default]
    Startup,
    /// `--continue`, `/resume`.
    Resume,
    /// `/clear`.
    Clear,
}

impl StartSource {
    pub fn as_str(self) -> &'static str {
        match self {
            StartSource::Startup => "startup",
            StartSource::Resume => "resume",
            StartSource::Clear => "clear",
        }
    }
}

/// Where the main session is in its life, for `session_start` and
/// `session_end`: start hooks run before a conversation's first message,
/// end hooks when it is left or gz exits.
#[derive(Debug, Clone, Default)]
pub(super) struct Lifecycle {
    /// The conversation whose start hooks ran.
    started: Option<String>,
    source: StartSource,
    /// A conversation left (by `/clear` or `/resume`) whose end hooks have
    /// not run yet, with why it ended.
    left: Option<(String, &'static str)>,
}

/// What `user_prompt_submit` hooks made of a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptCheck {
    /// Send it, with this context for the model.
    Send { context: Vec<String> },
    /// Don't send it, for this reason (shown to the user).
    Blocked(String),
}

/// What `pre_tool_use` hooks decided about a call, before the permission
/// check: a deny from any hook wins, then ask, then allow.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PreTool {
    pub decision: Option<Decision>,
    /// The deciding hook's reason.
    pub reason: Option<String>,
    /// `additionalContext` for the model, added to the call's result.
    pub context: Vec<String>,
}

impl PreTool {
    /// The result the model gets when a hook refused the call.
    pub fn refusal(&self) -> Option<String> {
        let refused = self.decision.is_some_and(Decision::is_refusal);
        refused.then(|| {
            format!(
                "A pre_tool_use hook denied this call: {}",
                self.reason.as_deref().unwrap_or("no reason given")
            )
        })
    }

    pub fn allows(&self) -> bool {
        self.decision == Some(Decision::Allow)
    }

    pub fn asks(&self) -> bool {
        self.decision == Some(Decision::Ask)
    }

    /// Whether the hook's `allow` settles the call without the approver.
    /// Opening a folder, installs and trust stay the user's call in every
    /// mode, and plan mode keeps refusing what it refuses (the same set as
    /// `Mode::auto_decision`), so those still go to the approver.
    pub fn allows_alone(&self, access: Access, in_plan_mode: bool) -> bool {
        let approver_decides = match access {
            Access::Folder | Access::Install | Access::Trust => true,
            Access::Write
            | Access::Command
            | Access::Script
            | Access::ServerAction
            | Access::LocalTool => in_plan_mode,
            Access::Read | Access::Fetch => false,
        };
        self.allows() && !approver_decides
    }

    /// The line the permission prompt shows when a hook asked for it.
    pub fn ask_note(&self) -> String {
        match &self.reason {
            Some(reason) => format!("A pre_tool_use hook asks you to confirm: {reason}"),
            None => "A pre_tool_use hook asks you to confirm this call".to_string(),
        }
    }
}

/// How strongly a decision binds when hooks disagree.
fn rank(decision: Decision) -> u8 {
    match decision {
        Decision::Deny | Decision::Block => 3,
        Decision::Ask => 2,
        Decision::Allow => 1,
    }
}

impl Hooks {
    /// The main session moved from `old_conversation` to a new one
    /// (`next`: clear) or an earlier one (resume). Its end hooks run before
    /// the next conversation's start hooks.
    pub fn left_conversation(&self, old_conversation: &str, next: StartSource) {
        let mut state = self.session_state();
        let life = &mut state.life;
        if life.started.as_deref() == Some(old_conversation) {
            life.left = Some((old_conversation.to_string(), next.as_str()));
        }
        life.started = None;
        life.source = next;
        state.is_compact_due = false;
    }

    /// Notes how full the context was when a run ended. The chat compacts
    /// inside its runs (gz-chat agent/generate.py:802-869) and sends no
    /// event before it does, so past the auto-compact threshold is the
    /// best sign gz has that a compaction is due or just happened.
    pub fn note_context(&self, usage: &ContextUsage) {
        let is_due = usage
            .tokens
            .is_some_and(|tokens| tokens >= usage.auto_compact_at);
        self.session_state().is_compact_due = is_due;
    }

    /// Runs `pre_compact` hooks (trigger auto) at the next message once the
    /// conversation passed the auto-compact size; report only, and the
    /// timing is approximate, since the chat compacts by itself.
    pub async fn auto_compact_due(&self, cwd: &Path, log: &Log<'_>, on_event: Events<'_>) {
        if !std::mem::take(&mut self.session_state().is_compact_due) {
            return;
        }
        if let Err(why) = self.pre_compact("auto", None, cwd, log, on_event).await {
            on_event(AssistantEvent::Notice(format!(
                "{why}; the chat compacts by itself, so this one goes ahead"
            )));
        }
    }

    /// Whether `session_start` hooks ran for the current conversation.
    pub fn is_started(&self) -> bool {
        self.session_state().life.started.is_some()
    }

    /// Runs `session_end` hooks when gz code exits: for the started
    /// conversation (reason exit), or for one `/clear` or `/resume` left
    /// with no message sent since (reason clear or resume).
    pub async fn exit(&self, cwd: &Path, on_event: Events<'_>) {
        let ending = {
            let mut state = self.session_state();
            let started = state.life.started.take().map(|id| (id, "exit"));
            started.or_else(|| state.life.left.take())
        };
        let Some((conversation, reason)) = ending else {
            return;
        };
        let log = Log {
            audit: None,
            conversation_id: &conversation,
            run_id: "",
        };
        self.session_end(reason, cwd, &log, on_event).await;
    }

    /// Runs `session_start` hooks (after the end hooks of a conversation
    /// just left); returns what they added for the model.
    pub async fn session_start(
        &self,
        cwd: &Path,
        log: &Log<'_>,
        on_event: Events<'_>,
    ) -> Vec<String> {
        let (left, source) = {
            let mut state = self.session_state();
            state.life.started = Some(log.conversation_id.to_string());
            (state.life.left.take(), state.life.source)
        };
        if let Some((old, reason)) = left {
            let old_log = Log {
                audit: log.audit,
                conversation_id: &old,
                run_id: log.run_id,
            };
            self.session_end(reason, cwd, &old_log, on_event).await;
        }
        let source = source.as_str();
        let fields = json!({"source": source});
        let batch = Batch::plain(
            &[HookEvent::SessionStart],
            Some(source),
            cwd,
            fields,
            source,
        );
        let fired = self.fire(&batch, log).await;
        contexts(&fired, on_event)
    }

    /// Runs `session_end` hooks (`reason`: exit, clear or resume).
    pub async fn session_end(&self, reason: &str, cwd: &Path, log: &Log<'_>, on_event: Events<'_>) {
        let fields = json!({"reason": reason});
        let batch = Batch::plain(&[HookEvent::SessionEnd], Some(reason), cwd, fields, reason);
        for one in self.fire(&batch, log).await {
            report(&one, on_event);
        }
    }

    /// Runs `user_prompt_submit` hooks on a message about to be sent.
    pub async fn user_prompt_submit(
        &self,
        prompt: &str,
        cwd: &Path,
        log: &Log<'_>,
        on_event: Events<'_>,
    ) -> PromptCheck {
        let fields = json!({"prompt": prompt});
        let batch = Batch::plain(&[HookEvent::UserPromptSubmit], None, cwd, fields, "");
        let fired = self.fire(&batch, log).await;
        let refused = fired
            .iter()
            .find(|one| one.reply.decision.is_some_and(Decision::is_refusal));
        if let Some(one) = refused {
            return PromptCheck::Blocked(format!(
                "{} blocked this message: {}",
                label(&one.hook),
                reason_of(one)
            ));
        }
        if let Some(one) = fired.iter().find(|one| one.reply.halt.is_some()) {
            let why = one.reply.halt.clone().unwrap_or_default();
            return PromptCheck::Blocked(format!("{} stopped: {why}", label(&one.hook)));
        }
        PromptCheck::Send {
            context: contexts(&fired, on_event),
        }
    }

    /// Runs `pre_tool_use` hooks on a call, before its permission check.
    pub async fn pre_tool_use(
        &self,
        call: &ToolCall<'_>,
        log: &Log<'_>,
        on_event: Events<'_>,
    ) -> PreTool {
        let events = [HookEvent::PreToolUse];
        let fired = self.fire(&Batch::tool(&events, call, None), log).await;
        let mut verdict = PreTool::default();
        for one in &fired {
            // A refusal is shown once, below, with its reason.
            if !one.reply.decision.is_some_and(Decision::is_refusal) {
                report(one, on_event);
            }
            if let Some(context) = &one.reply.context {
                if super::is_json(&one.ran) {
                    verdict.context.push(cap(context, FEEDBACK_CHARS));
                }
            }
            let Some(decision) = one.reply.decision else {
                continue;
            };
            if verdict.decision.is_none_or(|d| rank(decision) > rank(d)) {
                verdict.decision = Some(decision);
                verdict.reason = Some(cap(&reason_of(one), FEEDBACK_CHARS));
            }
        }
        if verdict.refusal().is_some() {
            on_event(AssistantEvent::Notice(format!(
                "✋ a pre_tool_use hook denied {}: {}",
                call.summary,
                shown(verdict.reason.as_deref().unwrap_or("no reason given"))
            )));
        }
        self.halt_from(&fired, on_event);
        verdict
    }

    /// Runs `stop` hooks when the agent finished its answer; `Some` is why
    /// it must carry on (sent to the model as the next message).
    pub async fn stop(
        &self,
        is_continuation: bool,
        cwd: &Path,
        log: &Log<'_>,
        on_event: Events<'_>,
    ) -> Option<String> {
        let fields = json!({"stopHookActive": is_continuation});
        let batch = Batch::plain(&[HookEvent::Stop], None, cwd, fields, "");
        let fired = self.fire(&batch, log).await;
        carry_on(&fired, on_event)
    }

    /// Runs `subagent_stop` hooks for a subagent that finished; `Some` is
    /// why it must carry on (only a background subagent can).
    pub async fn subagent_stop(
        &self,
        subagent: &SubagentEnd<'_>,
        cwd: &Path,
        log: &Log<'_>,
        on_event: Events<'_>,
    ) -> Option<String> {
        let fields = json!({
            "subagent": subagent.name,
            "status": subagent.status,
            "summary": cap(subagent.summary, super::RESULT_CHARS),
            "stopHookActive": subagent.is_continuation,
        });
        let name = subagent.name;
        let batch = Batch::plain(&[HookEvent::SubagentStop], Some(name), cwd, fields, name);
        let fired = self.fire(&batch, log).await;
        carry_on(&fired, on_event)
    }

    /// Runs `pre_compact` hooks (`trigger`: manual or auto); `Err` is why
    /// a hook blocked a manual compaction.
    pub async fn pre_compact(
        &self,
        trigger: &str,
        instructions: Option<&str>,
        cwd: &Path,
        log: &Log<'_>,
        on_event: Events<'_>,
    ) -> Result<(), String> {
        let fields = json!({"trigger": trigger, "instructions": instructions});
        let batch = Batch::plain(
            &[HookEvent::PreCompact],
            Some(trigger),
            cwd,
            fields,
            trigger,
        );
        let fired = self.fire(&batch, log).await;
        let mut blocked = None;
        for one in &fired {
            match one.reply.decision {
                Some(decision) if decision.is_refusal() => {
                    blocked.get_or_insert_with(|| {
                        format!(
                            "{} blocked compaction: {}",
                            label(&one.hook),
                            reason_of(one)
                        )
                    });
                }
                _ => report(one, on_event),
            }
        }
        blocked.map_or(Ok(()), Err)
    }

    /// Runs `notification` hooks: gz needs the user (`kind`: permission or
    /// question). They cannot block anything.
    pub async fn notification(
        &self,
        kind: &str,
        message: &str,
        cwd: &Path,
        log: &Log<'_>,
        on_event: Events<'_>,
    ) {
        let fields = json!({"kind": kind, "message": message});
        let batch = Batch::plain(&[HookEvent::Notification], Some(kind), cwd, fields, kind);
        for one in self.fire(&batch, log).await {
            report(&one, on_event);
        }
    }
}

/// A subagent that finished, for `subagent_stop` hooks.
pub struct SubagentEnd<'a> {
    pub name: &'a str,
    /// completed, failed, cancelled, …
    pub status: &'a str,
    pub summary: &'a str,
    /// It is already carrying on because of a `subagent_stop` hook.
    pub is_continuation: bool,
}

/// What hooks added for the model (plain stdout on exit 0, or
/// `additionalContext`), shown to the user too unless suppressed.
fn contexts(fired: &[Fired], on_event: Events<'_>) -> Vec<String> {
    let mut added = Vec::new();
    for one in fired {
        let context = one
            .reply
            .context
            .as_ref()
            .filter(|_| one.reply.problem.is_none());
        let Some(context) = context else {
            report(one, on_event);
            continue;
        };
        if !one.reply.suppress_output {
            on_event(AssistantEvent::Notice(format!(
                "{} added this for the model:\n{}",
                label(&one.hook),
                shown(context)
            )));
        }
        added.push(cap(context, FEEDBACK_CHARS));
    }
    added
}

/// Why the agent must carry on: the first hook that blocked stopping
/// (exit 2 or `"decision": "block"`), unless another said
/// `"continue": false`.
fn carry_on(fired: &[Fired], on_event: Events<'_>) -> Option<String> {
    let halted = fired.iter().any(|one| one.reply.halt.is_some());
    let mut reason = None;
    for one in fired {
        match one.reply.decision {
            Some(decision) if decision.is_refusal() && !halted => {
                let why = reason_of(one);
                on_event(AssistantEvent::Notice(format!(
                    "↻ {} asked the agent to carry on: {}",
                    label(&one.hook),
                    shown(&why)
                )));
                reason.get_or_insert(cap(&why, FEEDBACK_CHARS));
            }
            _ => report(one, on_event),
        }
    }
    reason
}
