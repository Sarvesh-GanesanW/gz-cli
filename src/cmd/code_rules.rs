//! `gz code`'s permission flags and mode: `--permission-mode`,
//! `--allowedTools`, `--disallowedTools` and the `default_mode` setting,
//! and the mode for front ends without the interactive UI.

use std::sync::{Arc, Mutex};

use anyhow::Result;

use crate::assistant::permissions::{
    Approver, DecidedBy, Decision, DecisionFuture, PermissionRequest, Refusal, ReplyFuture,
};
use crate::assistant::questions::{Query, Reply, WorkMode};
use crate::assistant::rules::{FlagRules, RuleSet};
use crate::assistant::tools::Access;
use crate::assistant::Session;
use crate::cli::CodeArgs;
use crate::code_ui::approver::{Mode, PLAN_REFUSAL};

/// Adds the flags' rules to `session` and works out the starting mode;
/// without the interactive UI to show them, rules gz couldn't read are
/// printed as warnings.
pub fn setup(session: &mut Session, args: &CodeArgs, interactive: bool) -> Result<Mode> {
    let flags = FlagRules::new(&args.allowed_tools, &args.disallowed_tools)?;
    session.set_flag_rules(flags);
    let rules = session.rules();
    if !interactive {
        for problem in &rules.problems {
            crate::output::warn(problem);
        }
    }
    Ok(start_mode(args, &rules))
}

/// `--auto`, then `--permission-mode`, then `default_mode` from the rules
/// files, then ask.
pub fn start_mode(args: &CodeArgs, rules: &RuleSet) -> Mode {
    if args.auto {
        return Mode::Auto;
    }
    args.permission_mode
        .as_deref()
        .or(rules.settings.default_mode.as_deref())
        .and_then(Mode::parse)
        .unwrap_or_default()
}

/// Whether Shift+Tab offers don't-ask mode: when the session starts in it
/// or `dont_ask_in_cycle` is set.
pub fn dont_ask_in_cycle(start: Mode, rules: &RuleSet) -> bool {
    start == Mode::DontAsk || rules.settings.dont_ask_in_cycle.unwrap_or(false)
}

/// `inner` with `mode` applied first, for the plain terminal and one-shot
/// runs (the interactive UI applies modes itself). Auto mode has its own
/// approver, so it and ask mode pass everything through.
pub fn apply_mode(mode: Mode, inner: Box<dyn Approver>) -> Box<dyn Approver> {
    match mode {
        Mode::Ask | Mode::Auto => inner,
        Mode::AcceptEdits | Mode::Plan | Mode::DontAsk => switchable(mode, inner).0,
    }
}

/// [`apply_mode`] with a handle that changes the mode later (the plain
/// terminal's `/mode`).
pub fn switchable(mode: Mode, inner: Box<dyn Approver>) -> (Box<dyn Approver>, ModeSwitch) {
    let switch = ModeSwitch(Arc::new(Mutex::new(mode)));
    let approver = ModeApprover {
        mode: switch.clone(),
        inner,
        refusal: Mutex::new(None),
        decided_by: Mutex::new(None),
    };
    (Box::new(approver), switch)
}

/// The mode a [`switchable`] approver applies.
#[derive(Clone)]
pub struct ModeSwitch(Arc<Mutex<Mode>>);

impl ModeSwitch {
    pub fn get(&self) -> Mode {
        self.0.lock().map(|mode| *mode).unwrap_or_default()
    }

    pub fn set(&self, mode: Mode) {
        if let Ok(mut slot) = self.0.lock() {
            *slot = mode;
        }
    }
}

/// What the plain terminal's `/mode [name]` does: shows the mode, or
/// switches to the one named; auto mode needs `--auto` or the full UI.
pub fn mode_command(switch: &ModeSwitch, arg: &str) -> Result<String> {
    let arg = arg.trim();
    if arg.is_empty() {
        return Ok(format!(
            "mode: {} · /mode ask|accept-edits|plan|dont-ask switches",
            mode_label(switch.get())
        ));
    }
    let Some(mode) = Mode::parse(arg) else {
        anyhow::bail!("'{arg}' isn't a mode; use ask, accept-edits, plan or dont-ask");
    };
    if mode == Mode::Auto {
        anyhow::bail!(
            "auto mode isn't available in the plain terminal; start with `gz code --auto` or \
             use the full-screen UI"
        );
    }
    switch.set(mode);
    Ok(format!("mode: {}", mode_label(mode)))
}

fn mode_label(mode: Mode) -> &'static str {
    match mode {
        Mode::Ask => "ask before edits and commands",
        Mode::AcceptEdits => "accept edits",
        Mode::Plan => "plan (read-only)",
        Mode::Auto => "auto",
        Mode::DontAsk => "don't ask (refuse what rules don't allow)",
    }
}

struct ModeApprover {
    /// Plan mode ends when the user approves the plan or picks another.
    mode: ModeSwitch,
    inner: Box<dyn Approver>,
    refusal: Mutex<Option<Refusal>>,
    decided_by: Mutex<Option<DecidedBy>>,
}

impl ModeApprover {
    /// What the mode decides without asking, if anything.
    fn mode(&self) -> Mode {
        self.mode.get()
    }

    fn settle(&self, access: Access) -> Option<Decision> {
        let mode = self.mode();
        let refusal = match (mode, access) {
            (Mode::DontAsk, _) => Mode::dont_ask_refusal(),
            (Mode::AcceptEdits, Access::Write) => {
                put(&self.decided_by, DecidedBy::AcceptEdits);
                return Some(Decision::AllowOnce);
            }
            (Mode::Plan, _) if mode.auto_decision(access) == Some(Decision::Deny) => {
                Refusal::Blocked {
                    by: "plan mode",
                    reason: PLAN_REFUSAL.to_string(),
                    allow_with: None,
                }
            }
            _ => return None,
        };
        put(&self.refusal, refusal);
        Some(Decision::Deny)
    }
}

fn put<T>(slot: &Mutex<Option<T>>, value: T) {
    if let Ok(mut slot) = slot.lock() {
        *slot = Some(value);
    }
}

fn take<T>(slot: &Mutex<Option<T>>) -> Option<T> {
    slot.lock().ok().and_then(|mut slot| slot.take())
}

impl Approver for ModeApprover {
    fn decide<'a>(&'a self, request: &'a PermissionRequest) -> DecisionFuture<'a> {
        match self.settle(request.access) {
            Some(decision) => Box::pin(async move { decision }),
            None => self.inner.decide(request),
        }
    }

    /// An ask rule matched: accept-edits asks after all; plan and
    /// don't-ask mode still refuse.
    fn ask_anyway<'a>(&'a self, request: &'a PermissionRequest) -> DecisionFuture<'a> {
        match self.settle(request.access) {
            Some(Decision::Deny) => Box::pin(async { Decision::Deny }),
            _ => {
                take(&self.decided_by);
                self.inner.ask_anyway(request)
            }
        }
    }

    fn take_refusal(&self) -> Refusal {
        take(&self.refusal).unwrap_or_else(|| self.inner.take_refusal())
    }

    fn take_decided_by(&self) -> DecidedBy {
        take(&self.decided_by).unwrap_or_else(|| self.inner.take_decided_by())
    }

    /// An approved plan ends plan mode, in the mode the user picked
    /// (auto mode isn't offered without the interactive UI, so it asks).
    fn ask<'a>(&'a self, query: &'a Query) -> ReplyFuture<'a> {
        Box::pin(async move {
            let reply = self.inner.ask(query).await;
            if let (Query::Plan(_), Reply::PlanApproved(work)) = (query, &reply) {
                self.mode.set(match work {
                    WorkMode::AcceptEdits => Mode::AcceptEdits,
                    WorkMode::Ask | WorkMode::Auto => Mode::Ask,
                });
            }
            reply
        })
    }

    fn note_refusal(&self, request: &PermissionRequest, refusal: &Refusal) {
        self.inner.note_refusal(request, refusal)
    }

    fn in_plan_mode(&self) -> bool {
        self.mode() == Mode::Plan
    }

    fn accepts_edits(&self) -> bool {
        self.mode() == Mode::AcceptEdits || self.inner.accepts_edits()
    }

    fn refuses_unasked(&self) -> bool {
        self.mode() == Mode::DontAsk || self.inner.refuses_unasked()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use clap::Parser;

    use super::*;

    /// Allows what it is asked, counting the prompts.
    #[derive(Default)]
    struct Person {
        asked: AtomicUsize,
    }

    impl Approver for Person {
        fn decide<'a>(&'a self, _: &'a PermissionRequest) -> DecisionFuture<'a> {
            self.asked.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Decision::AllowOnce })
        }
    }

    struct Shared(std::sync::Arc<Person>);

    impl Approver for Shared {
        fn decide<'a>(&'a self, request: &'a PermissionRequest) -> DecisionFuture<'a> {
            self.0.decide(request)
        }
    }

    fn request(access: Access) -> PermissionRequest {
        PermissionRequest {
            access,
            summary: "x".into(),
            preview: None,
            chat_scope: String::new(),
            always_scope: None,
            action: None,
            asked_by: None,
            target: None,
            note: None,
        }
    }

    fn code_args(argv: &[&str]) -> CodeArgs {
        let mut full = vec!["gz", "code"];
        full.extend_from_slice(argv);
        match crate::cli::Cli::try_parse_from(full).unwrap().command {
            crate::cli::Command::Code(args) => args,
            other => panic!("not gz code: {other:?}"),
        }
    }

    #[tokio::test]
    async fn each_mode_settles_what_it_can_and_passes_the_rest_on() {
        let person = std::sync::Arc::new(Person::default());
        let with = |mode| apply_mode(mode, Box::new(Shared(person.clone())));

        let dont_ask = with(Mode::DontAsk);
        for access in [Access::Read, Access::Write, Access::Command, Access::Folder] {
            assert_eq!(dont_ask.decide(&request(access)).await, Decision::Deny);
            let message = dont_ask.take_refusal().message();
            assert!(message.contains("don't-ask mode is on"), "{message}");
        }
        assert_eq!(
            dont_ask.ask_anyway(&request(Access::Command)).await,
            Decision::Deny
        );
        assert_eq!(person.asked.load(Ordering::SeqCst), 0, "never asks");

        let plan = with(Mode::Plan);
        assert_eq!(plan.decide(&request(Access::Write)).await, Decision::Deny);
        assert!(plan.take_refusal().message().contains("plan mode"));
        assert_eq!(
            plan.decide(&request(Access::Read)).await,
            Decision::AllowOnce
        );
        assert!(plan.in_plan_mode());

        let accept = with(Mode::AcceptEdits);
        assert_eq!(
            accept.decide(&request(Access::Write)).await,
            Decision::AllowOnce
        );
        assert_eq!(accept.take_decided_by(), DecidedBy::AcceptEdits);
        assert_eq!(
            accept.ask_anyway(&request(Access::Write)).await,
            Decision::AllowOnce
        );
        assert_eq!(
            person.asked.load(Ordering::SeqCst),
            2,
            "read in plan, ask rule"
        );
    }

    /// Approves every plan with `work`.
    struct Approves(WorkMode);

    impl Approver for Approves {
        fn decide<'a>(&'a self, _: &'a PermissionRequest) -> DecisionFuture<'a> {
            Box::pin(async { Decision::AllowOnce })
        }

        fn ask<'a>(&'a self, _: &'a Query) -> ReplyFuture<'a> {
            Box::pin(async { Reply::PlanApproved(self.0) })
        }
    }

    #[tokio::test]
    async fn an_approved_plan_ends_plan_mode_in_the_mode_picked() {
        let plan = apply_mode(Mode::Plan, Box::new(Approves(WorkMode::AcceptEdits)));
        assert!(plan.in_plan_mode());
        let reply = plan.ask(&Query::Plan("1. edit".into())).await;
        assert_eq!(reply, Reply::PlanApproved(WorkMode::AcceptEdits));
        assert!(!plan.in_plan_mode());
        assert_eq!(
            plan.decide(&request(Access::Write)).await,
            Decision::AllowOnce
        );
        assert_eq!(plan.take_decided_by(), DecidedBy::AcceptEdits);

        let plan = apply_mode(Mode::Plan, Box::new(Approves(WorkMode::Ask)));
        plan.ask(&Query::Plan("1. edit".into())).await;
        assert!(!plan.in_plan_mode());
        assert_eq!(
            plan.decide(&request(Access::Write)).await,
            Decision::AllowOnce
        );
        assert_eq!(
            plan.take_decided_by(),
            DecidedBy::User,
            "asked, not accepted"
        );
    }

    #[tokio::test]
    async fn script_runs_report_plan_mode_through_the_recording() {
        let log = crate::cmd::code_unattended::DecisionLog::default();
        let plan = apply_mode(Mode::Plan, Box::new(crate::cmd::code_unattended::Nobody));
        let recording = crate::cmd::code_unattended::Recording::new(plan, log);
        assert!(recording.in_plan_mode());
        assert_eq!(
            recording.ask(&Query::Plan("1. edit".into())).await,
            Reply::NoAnswer,
            "nobody approves a plan in a script run"
        );
        assert!(recording.in_plan_mode());
    }

    #[tokio::test]
    async fn the_plain_terminals_mode_command_switches_modes() {
        let (approver, switch) = switchable(Mode::Plan, Box::new(Approves(WorkMode::Ask)));
        assert!(approver.in_plan_mode());
        assert!(mode_command(&switch, "").unwrap().contains("plan"));
        assert_eq!(
            mode_command(&switch, "accept-edits").unwrap(),
            "mode: accept edits"
        );
        assert!(!approver.in_plan_mode());
        assert_eq!(
            approver.decide(&request(Access::Write)).await,
            Decision::AllowOnce
        );
        assert_eq!(approver.take_decided_by(), DecidedBy::AcceptEdits);
        let err = mode_command(&switch, "auto").unwrap_err();
        assert!(format!("{err:#}").contains("--auto"));
        assert!(mode_command(&switch, "yolo").is_err());
        assert_eq!(
            switch.get(),
            Mode::AcceptEdits,
            "a refused switch changes nothing"
        );
        mode_command(&switch, "dontAsk").unwrap();
        assert_eq!(
            approver.decide(&request(Access::Read)).await,
            Decision::Deny
        );
    }

    #[test]
    fn the_mode_comes_from_the_flag_then_the_files_then_ask() {
        let rules = |mode: Option<&str>| {
            let mut set = RuleSet::default();
            set.settings.default_mode = mode.map(str::to_string);
            set
        };
        let mode = |argv: &[&str], file: Option<&str>| start_mode(&code_args(argv), &rules(file));
        assert_eq!(mode(&[], None), Mode::Ask);
        assert_eq!(mode(&[], Some("plan")), Mode::Plan);
        assert_eq!(
            mode(&["--permission-mode", "dontAsk"], Some("plan")),
            Mode::DontAsk
        );
        assert_eq!(
            mode(&["--permission-mode", "accept-edits"], None),
            Mode::AcceptEdits
        );
        assert_eq!(
            mode(&["--permission-mode", "default"], Some("auto")),
            Mode::Ask
        );
        assert_eq!(mode(&["--auto"], Some("plan")), Mode::Auto);
        assert!(dont_ask_in_cycle(Mode::DontAsk, &rules(None)));
        assert!(!dont_ask_in_cycle(Mode::Plan, &rules(None)));
        let mut cycle = rules(None);
        cycle.settings.dont_ask_in_cycle = Some(true);
        assert!(dont_ask_in_cycle(Mode::Ask, &cycle));
    }

    #[test]
    fn the_flags_parse_repeat_and_refuse_what_isnt_a_mode_or_rule() {
        let args = code_args(&[
            "--allowedTools",
            "read, run_command(git status:*)",
            "--allowed-tools",
            "edit(src/**)",
            "--disallowedTools",
            "web_fetch",
        ]);
        let flags = FlagRules::new(&args.allowed_tools, &args.disallowed_tools).unwrap();
        assert_eq!(
            flags.allow,
            ["read", "run_command(git status:*)", "edit(src/**)"]
        );
        assert_eq!(flags.deny, ["web_fetch"]);
        let parse = |argv: &[&str]| {
            let mut full = vec!["gz", "code"];
            full.extend_from_slice(argv);
            crate::cli::Cli::try_parse_from(full).map(|_| ())
        };
        assert!(parse(&["--permission-mode", "yolo"]).is_err());
        assert!(parse(&["--permission-mode", "plan", "--auto"]).is_err());
        let bad = code_args(&["--allowedTools", "bash(ls)"]);
        let err = FlagRules::new(&bad.allowed_tools, &[]).unwrap_err();
        assert!(format!("{err:#}").contains("isn't a tool"));
    }
}
