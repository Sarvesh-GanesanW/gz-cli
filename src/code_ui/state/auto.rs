//! `/auto`: auto mode's status, the user's own rules, and recent blocks.
//!
//! `/auto on|off`, `/auto allow|deny|env <rule>`, `/auto forget
//! allow|deny|env <n>`, and `/auto allow-blocked <n>` (save a blocked
//! action as an allow rule, so the review lets it run next time).

use crate::assistant::auto_rules::{AutoRules, RuleKind};
use crate::code_ui::approver::Mode;
use crate::code_ui::auto_mode::BlockedAction;
use crate::code_ui::items::Item;
use crate::code_ui::panel::{Panel, Row};

use super::{Effect, UiState};

const USAGE: &str = "Use /auto, /auto on|off, /auto allow|deny|env <rule>, \
                     /auto forget allow|deny|env <n>, or /auto allow-blocked <n>";

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
            (_, Some(kind)) => self.add_auto_rule(kind, argument),
            _ => self.record(Item::Error(USAGE.into())),
        }
        Vec::new()
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
        let panel = auto_panel(self.auto_status(), &rules, &blocks);
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
            "/auto allow|deny|env <rule> · /auto forget allow 2 · /auto allow-blocked 1 · /auto on|off",
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
        context
            .lock()
            .unwrap()
            .blocked("run_command git push", "Not asked for.");
        let push = crate::assistant::PermissionRequest {
            access: crate::assistant::tools::Access::Command,
            summary: "run_command git push".into(),
            preview: None,
            chat_scope: String::new(),
            always_scope: None,
            action: None,
            asked_by: None,
            target: None,
            note: None,
        };
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
    fn a_blocked_action_can_be_saved_as_an_allow_rule() {
        let (mut s, store) = auto_state("blocked");
        s.with_auto(|auto| {
            auto.blocked(
                "run_command make deploy-docs",
                "Deploys were not asked for.",
            );
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
