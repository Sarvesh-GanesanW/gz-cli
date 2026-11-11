//! `/auto`: auto mode's status, the user's own rules, and recent blocks.
//!
//! `/auto on|off`, `/auto allow|deny|env <rule>`, `/auto forget
//! allow|deny|env <n>`, and `/auto allow-blocked <n>` (save a blocked
//! action as an allow rule, so the review lets it run next time), and
//! `/auto scan cap <size>|off` (see `scan_cap_cmd`).
//!
//! `/allow [n]` lets one blocked action run now: that exact action, once.

use crate::assistant::auto_rules::{AutoRules, RuleKind};
use crate::code_ui::approver::{Mode, ALLOW_COMMAND};
use crate::code_ui::auto_mode::BlockedAction;
use crate::code_ui::items::Item;
use crate::code_ui::panel::{Panel, Row};

use super::{Effect, UiState};

const USAGE: &str = "Use /auto, /auto on|off, /auto allow|deny|env <rule>, \
                     /auto forget allow|deny|env <n>, /auto allow-blocked <n>, \
                     or /auto scan cap <size>|off";
const ALLOW_USAGE: &str = "Use /allow for the action auto mode blocked last, or \
                           /allow <n> for the one /auto numbers n";

impl UiState {
    pub(super) fn auto_command(&mut self, command: &str) -> Vec<Effect> {
        let rest = command.trim_start_matches("/auto").trim();
        let (word, argument) = rest.split_once(' ').unwrap_or((rest, ""));
        let argument = argument.trim();
        match (word, RuleKind::parse(word)) {
            ("", _) => self.show_auto(),
            ("on", _) => self.set_auto(true),
            ("off", _) => self.set_auto(false),
            ("forget", _) => self.forget_rule(argument),
            ("allow-blocked", _) => self.allow_blocked(argument),
            ("scan", _) => self.scan_cap_command(argument),
            (_, Some(kind)) => self.add_auto_rule(kind, argument),
            _ => self.record(Item::Error(USAGE.into())),
        }
        Vec::new()
    }

    /// `/allow [n]`: the user lets an action auto mode blocked run, and gz
    /// is asked to run it again. The message goes out like one the user
    /// typed, so it reaches a running turn as a note.
    pub(super) fn allow_command(&mut self, command: &str) -> Vec<Effect> {
        let number = match command.trim_start_matches(ALLOW_COMMAND).trim() {
            "" => None,
            text => match text.parse::<usize>() {
                Ok(number) => Some(number),
                Err(_) => {
                    self.record(Item::Error(ALLOW_USAGE.into()));
                    return Vec::new();
                }
            },
        };
        let mut allowed = None;
        self.with_auto(|auto| allowed = auto.allow_blocked(number));
        let Some(blocked) = allowed else {
            self.record(Item::Error(match number {
                Some(number) => format!("There is no blocked action {number}; /auto lists them"),
                None => "Auto mode hasn't blocked anything in this session.".into(),
            }));
            return Vec::new();
        };
        self.record(Item::Notice(format!("✓ Allowed once: {}", blocked.summary)));
        let message = format!(
            "I allow the action auto mode blocked: {}\nRun it again exactly as it was.",
            blocked.summary
        );
        if self.is_busy() || self.compacting {
            return self.submit_while_busy(message);
        }
        vec![Effect::Send(message)]
    }

    fn show_auto(&mut self) {
        let Some(store) = self.auto_store.clone() else {
            self.record(Item::Error("Auto mode is off in this session.".into()));
            return;
        };
        let rules = match store.rules() {
            Ok(rules) => rules,
            Err(err) => {
                self.record(Item::Error(format!(
                    "cannot read your auto mode rules: {err:#}"
                )));
                return;
            }
        };
        let mut blocks = Vec::new();
        self.with_auto(|auto| blocks = auto.recent_blocks().cloned().collect());
        let mut panel = auto_panel(self.auto_status(), &rules, &blocks);
        if let Some(rows) = self.scan_cap_rows() {
            panel = panel.section(Some("Query scan · /auto scan cap <size>|off"), rows);
        }
        self.record(Item::Panel(panel));
    }

    fn auto_status(&self) -> String {
        let mut paused = false;
        self.with_auto(|auto| paused = auto.is_paused());
        match (self.mode(), self.auto_available(), paused) {
            (Mode::Auto, _, true) => "on, paused after repeated blocks".into(),
            (Mode::Auto, _, false) => "on".into(),
            (_, true, _) => "off · shift+tab or /auto on".into(),
            (_, false, _) => "unavailable: this chat server doesn't review actions".into(),
        }
    }

    pub(super) fn set_auto(&mut self, on: bool) {
        if on && !self.auto_available() {
            self.record(Item::Error(
                "Auto mode is unavailable: this chat server doesn't review actions.".into(),
            ));
            return;
        }
        if let Ok(mut mode) = self.mode.lock() {
            *mode = if on { Mode::Auto } else { Mode::Ask };
        }
        let note = if on {
            "Auto mode on: gz runs routine actions and blocks risky ones"
        } else {
            "Auto mode off: gz asks before acting"
        };
        self.record(Item::Notice(note.into()));
    }

    fn add_auto_rule(&mut self, kind: RuleKind, rule: &str) {
        let Some(store) = self.auto_store.clone() else {
            self.record(Item::Error("Auto mode is off in this session.".into()));
            return;
        };
        match store.add_rule(kind, rule) {
            Ok(()) => self.record(Item::Notice(format!("Saved: {} {rule}", kind_word(kind)))),
            Err(err) => self.record(Item::Error(format!("{err:#}"))),
        }
    }

    fn forget_rule(&mut self, argument: &str) {
        let mut words = argument.split_whitespace();
        let (Some(kind), Some(number)) = (
            words.next().and_then(RuleKind::parse),
            words.next().and_then(|n| n.parse::<usize>().ok()),
        ) else {
            self.record(Item::Error(USAGE.into()));
            return;
        };
        let Some(store) = self.auto_store.clone() else {
            self.record(Item::Error("Auto mode is off in this session.".into()));
            return;
        };
        match store.remove_rule(kind, number) {
            Ok(rule) => self.record(Item::Notice(format!("Forgot: {} {rule}", kind_word(kind)))),
            Err(err) => self.record(Item::Error(format!("{err:#}"))),
        }
    }

    fn allow_blocked(&mut self, argument: &str) {
        let Ok(number) = argument.parse::<usize>() else {
            self.record(Item::Error(USAGE.into()));
            return;
        };
        let mut blocked = None;
        self.with_auto(|auto| blocked = auto.recent_blocks().nth(number.wrapping_sub(1)).cloned());
        let Some(blocked) = blocked else {
            self.record(Item::Error(format!(
                "There is no blocked action {number}; /auto lists them"
            )));
            return;
        };
        self.add_auto_rule(RuleKind::Allow, &blocked.summary);
    }
}

fn kind_word(kind: RuleKind) -> &'static str {
    match kind {
        RuleKind::Allow => "allow",
        RuleKind::Deny => "deny",
        RuleKind::Environment => "env",
    }
}

fn numbered(rules: &[String]) -> Vec<Row> {
    rules
        .iter()
        .enumerate()
        .map(|(i, rule)| Row::entry(&format!("{}", i + 1), rule.clone()))
        .collect()
}

fn auto_panel(status: String, rules: &AutoRules, blocks: &[BlockedAction]) -> Panel {
    let blocked: Vec<Row> = blocks
        .iter()
        .enumerate()
        .map(|(i, b)| {
            Row::entry(
                &format!("{}", i + 1),
                format!("{} · {}", b.summary, b.reason),
            )
        })
        .collect();
    let mut panel = Panel::new("Auto mode").aside(status);
    if rules == &AutoRules::default() {
        panel = panel.section(
            Some("Your rules"),
            vec![Row::note(
                "None yet. gz also learns from how you answer its prompts in this folder.",
            )],
        );
    }
    panel
        .section(Some("Allow · runs even if risky"), numbered(&rules.allow))
        .section(Some("Deny · always blocked"), numbered(&rules.deny))
        .section(Some("Environment · trusted"), numbered(&rules.environment))
        .section(Some("Recently blocked"), blocked)
        .footer(
            "/allow 1 runs a blocked action once · /auto allow-blocked 1 allows it from now on · \
             /auto allow|deny|env <rule> · /auto forget allow 2 · /auto on|off",
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant::auto_rules::AutoStore;
    use crate::code_ui::auto_mode::AutoContext;
    use crate::code_ui::state::test_support::*;
    use crossterm::event::KeyCode;
    use std::sync::{Arc, Mutex};

    fn auto_state(name: &str) -> (UiState, AutoStore) {
        let dir = std::env::temp_dir().join(format!("gz-auto-cmd-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = AutoStore::in_folder(&dir);
        let mut s = with_catalog();
        s.auto = Some(Arc::new(Mutex::new(AutoContext::default())));
        s.auto_store = Some(store.clone());
        (s, store)
    }

    fn run(s: &mut UiState, command: &str) {
        type_text(s, command);
        s.on_key(key(KeyCode::Enter));
    }

    /// What `run_command` asks permission for.
    fn command(line: &str) -> crate::assistant::PermissionRequest {
        crate::assistant::PermissionRequest {
            access: crate::assistant::tools::Access::Command,
            summary: format!("run_command {line}"),
            preview: Some(line.into()),
            chat_scope: String::new(),
            always_scope: None,
            action: None,
            asked_by: None,
            target: None,
            note: None,
        }
    }

    fn last_panel(s: &UiState) -> Vec<String> {
        match s.flush.last() {
            Some(Item::Panel(panel)) => panel.plain_lines(),
            other => panic!("no panel: {other:?}"),
        }
    }

    #[test]
    fn the_review_gets_recent_blocks_only_from_a_server_that_advertises_them() {
        let (mut s, _store) = auto_state("advertised");
        let context = s.auto.clone().unwrap();
        context.lock().unwrap().user_said("push it");
        let push = command("git push");
        context.lock().unwrap().blocked(&push, "Not asked for.");
        let blocks_sent = |s: &UiState| {
            let workspace = crate::assistant::action_review::ReviewWorkspace {
                name: "w".into(),
                platform: "macos",
            };
            let mut count = 0;
            s.with_auto(|auto| {
                count = auto
                    .review_request(&push, workspace, AutoRules::default(), vec![])
                    .unwrap()
                    .recent_blocks
                    .len();
            });
            count
        };
        let mut catalog = s.catalog.clone().unwrap();
        for (advertised, expected) in [(false, 0), (true, 1), (false, 0)] {
            catalog.action_review_authorizations = advertised;
            s.set_catalog(catalog.clone());
            assert_eq!(blocks_sent(&s), expected, "advertised: {advertised}");
        }
    }

    #[test]
    fn rules_are_saved_listed_and_forgotten() {
        let (mut s, store) = auto_state("rules");
        run(&mut s, "/auto allow make deploy-docs");
        run(&mut s, "/auto deny pushing to main");
        run(&mut s, "/auto env github.com/acme");
        assert_eq!(store.rules().unwrap().deny, vec!["pushing to main"]);
        run(&mut s, "/auto");
        let panel = last_panel(&s).join("\n");
        for part in ["make deploy-docs", "pushing to main", "github.com/acme"] {
            assert!(panel.contains(part), "{panel}");
        }
        run(&mut s, "/auto forget deny 1");
        assert!(store.rules().unwrap().deny.is_empty());
    }

    #[test]
    fn allow_lets_the_blocked_action_run_and_asks_gz_to_run_it_again() {
        let (mut s, _store) = auto_state("allow");
        run(&mut s, "/allow");
        assert!(
            matches!(s.flush.last(), Some(Item::Error(e)) if e.contains("hasn't blocked anything"))
        );

        let scan = command("find / -type f");
        let push = command("git push -f");
        s.with_auto(|auto| {
            auto.blocked(&scan, "Leaves the folder.");
            auto.blocked(&push, "Not asked for.");
        });
        type_text(&mut s, "/allow");
        assert_eq!(
            s.on_key(key(KeyCode::Enter)),
            vec![Effect::Send(
                "I allow the action auto mode blocked: run_command git push -f\n\
                 Run it again exactly as it was."
                    .into()
            )],
            "the newest block, sent as the user's next message"
        );
        assert!(
            matches!(s.flush.last(), Some(Item::Notice(n)) if n == "✓ Allowed once: run_command git push -f")
        );
        let mut allowed = (false, false);
        s.with_auto(|auto| allowed = (auto.take_allowance(&push), auto.take_allowance(&scan)));
        assert_eq!(allowed, (true, false), "only the action that was named");

        type_text(&mut s, "/allow 1");
        assert!(matches!(
            &s.on_key(key(KeyCode::Enter))[..],
            [Effect::Send(m)] if m.contains("run_command find / -type f")
        ));
        for wrong in ["/allow 7", "/allow everything"] {
            type_text(&mut s, wrong);
            assert!(s.on_key(key(KeyCode::Enter)).is_empty(), "{wrong}");
            assert!(matches!(s.flush.last(), Some(Item::Error(_))), "{wrong}");
        }
    }

    #[test]
    fn allow_reaches_a_running_turn_instead_of_waiting_for_it() {
        let (mut s, _store) = auto_state("allow-busy");
        s.with_auto(|auto| {
            auto.blocked(&command("git push -f"), "Not asked for.");
        });
        s.start_turn("rebase onto main");
        type_text(&mut s, "/allow");
        assert!(s.on_key(key(KeyCode::Enter)).is_empty());
        assert_eq!(
            s.queue.back().map(String::as_str),
            Some(
                "I allow the action auto mode blocked: run_command git push -f\n\
                 Run it again exactly as it was."
            ),
            "queued for the turn like a typed message, not dropped"
        );
        let mut allowed = false;
        s.with_auto(|auto| allowed = auto.take_allowance(&command("git push -f")));
        assert!(allowed, "allowed at once, while the turn still runs");
    }

    #[test]
    fn a_blocked_action_can_be_saved_as_an_allow_rule() {
        let (mut s, store) = auto_state("blocked");
        s.with_auto(|auto| {
            auto.blocked(&command("make deploy-docs"), "Deploys were not asked for.");
        });
        run(&mut s, "/auto");
        assert!(last_panel(&s)
            .join("\n")
            .contains("run_command make deploy-docs · Deploys were not asked for."));
        run(&mut s, "/auto allow-blocked 1");
        assert_eq!(
            store.rules().unwrap().allow,
            vec!["run_command make deploy-docs"]
        );
        run(&mut s, "/auto allow-blocked 9");
        assert!(matches!(s.flush.last(), Some(Item::Error(_))));
    }

    #[test]
    fn on_and_off_switch_the_mode_when_the_server_reviews() {
        let (mut s, _store) = auto_state("onoff");
        run(&mut s, "/auto on");
        assert_eq!(s.mode(), Mode::Auto);
        run(&mut s, "/auto off");
        assert_eq!(s.mode(), Mode::Ask);
        s.catalog.as_mut().unwrap().action_review = false;
        run(&mut s, "/auto on");
        assert_eq!(s.mode(), Mode::Ask);
        assert!(matches!(s.flush.last(), Some(Item::Error(_))));
    }

    #[test]
    fn shift_tab_reaches_auto_mode_only_when_available() {
        let (mut s, _store) = auto_state("cycle");
        let tab = || {
            crossterm::event::KeyEvent::new(KeyCode::BackTab, crossterm::event::KeyModifiers::SHIFT)
        };
        let modes: Vec<Mode> = (0..4)
            .map(|_| {
                s.on_key(tab());
                s.mode()
            })
            .collect();
        assert_eq!(
            modes,
            vec![Mode::Auto, Mode::AcceptEdits, Mode::Plan, Mode::Ask]
        );
        s.auto = None;
        for _ in 0..3 {
            s.on_key(tab());
        }
        assert_eq!(
            s.mode(),
            Mode::Ask,
            "plan cycles back to ask without auto mode"
        );
    }
}
