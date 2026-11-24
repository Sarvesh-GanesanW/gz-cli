//! The session's allow/ask/deny rules ([`crate::assistant::rules`]): the
//! command-line ones, and what the files said, for the front ends.

use super::Session;
use crate::assistant::rules::{FlagRules, RuleSet};

impl Session {
    /// Adds the `--allowedTools` / `--disallowedTools` rules for this
    /// session (shared with its subagents, kept when files are re-read).
    pub fn set_flag_rules(&mut self, flags: FlagRules) {
        let rules = self.policy.rules().with_flags(flags);
        self.policy.set_rules(rules);
    }

    /// The rules as they are now: for listing, problems and mode settings.
    pub fn rules(&self) -> RuleSet {
        self.policy.rules()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;

    use super::*;
    use crate::assistant::hooks::trust::TrustStore;
    use crate::assistant::permissions::{Approver, Decision, DecisionFuture, PermissionRequest};
    use crate::assistant::rules::Places;
    use crate::assistant::sandbox::tests::temp_dir;
    use crate::http::ApiClient;

    /// Allows everything it is asked, counting the prompts.
    #[derive(Default)]
    struct Counting {
        asked: AtomicUsize,
    }

    impl Approver for Counting {
        fn decide<'a>(&'a self, _: &'a PermissionRequest) -> DecisionFuture<'a> {
            self.asked.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Decision::AllowOnce })
        }
    }

    /// A session in `dir` whose user rules are `user_toml`.
    fn session_with_rules(dir: &Path, name: &str, user_toml: &str) -> Session {
        let resolved = crate::config::resolve(
            &crate::config::Config::default(),
            Some("rules-test"),
            Some("http://127.0.0.1:9".into()),
            Some("stub-token".to_string()),
        );
        let http = ApiClient::new(resolved, crate::output::OutputFormat::Json, true, 0, 5).unwrap();
        let mut session = Session::new(http, dir).unwrap();
        session.audit = None;
        session.history = None;
        let config = temp_dir(&format!("session-rules-{name}"));
        std::fs::write(config.join("permissions.toml"), user_toml).unwrap();
        let places = Places {
            folder: dir.to_path_buf(),
            home: None,
            user_file: Some(config.join("permissions.toml")),
            trust: Some(TrustStore::at(config.join("trust.json"))),
        };
        session
            .policy
            .set_rules(RuleSet::load(places, FlagRules::default()));
        // These tests are not about the read-before-edit rule.
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            if entry.path().is_file() {
                crate::assistant::sandbox::tests::seen_by_model(&session.sandbox, &entry.path());
            }
        }
        session
    }

    #[tokio::test]
    async fn a_deny_rule_refuses_without_asking_and_tells_the_model_why() {
        let dir = temp_dir("session-rules-deny");
        std::fs::write(dir.join("a.txt"), "old").unwrap();
        let mut session = session_with_rules(
            &dir,
            "deny",
            "deny = [\"edit(*.txt)\", \"run_command(rm:*)\"]\nallow = [\"run_command\"]",
        );
        let approver = Counting::default();
        let mut notices = Vec::new();
        let input = json!({"path": "a.txt", "content": "new"});
        let out = session
            .answer_one("run_1", "write_file", &input, None, &approver, &mut |e| {
                notices.push(format!("{e:?}"))
            })
            .await;
        assert!(out.is_error);
        assert!(
            out.output.contains(
                "the deny rule `edit(*.txt)` from the user's permissions.toml forbids it"
            ),
            "{}",
            out.output
        );
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "old");
        let out = session
            .answer_one(
                "run_1",
                "run_command",
                &json!({"command": "ls && rm -rf a.txt"}),
                None,
                &approver,
                &mut |_| {},
            )
            .await;
        assert!(out.is_error && out.output.contains("run_command(rm:*)"));
        assert_eq!(approver.asked.load(Ordering::SeqCst), 0, "never asked");
        assert!(dir.join("a.txt").exists());
    }

    #[tokio::test]
    async fn an_ask_rule_prompts_even_with_a_chat_grant_and_allow_rules_skip_the_prompt() {
        let dir = temp_dir("session-rules-ask");
        let mut session = session_with_rules(
            &dir,
            "ask",
            "ask = [\"run_command(echo loud:*)\"]\nallow = [\"run_command(echo:*)\"]",
        );
        let approver = Counting::default();
        let run = |line: &str| json!({"command": line});
        let out = session
            .answer_one(
                "r",
                "run_command",
                &run("echo quiet"),
                None,
                &approver,
                &mut |_| {},
            )
            .await;
        assert!(!out.is_error, "{}", out.output);
        assert_eq!(
            approver.asked.load(Ordering::SeqCst),
            0,
            "allowed by a rule"
        );
        for _ in 0..2 {
            session
                .answer_one(
                    "r",
                    "run_command",
                    &run("echo loud"),
                    None,
                    &approver,
                    &mut |_| {},
                )
                .await;
        }
        assert_eq!(approver.asked.load(Ordering::SeqCst), 2, "asked every time");
    }

    #[tokio::test]
    async fn command_line_rules_join_the_files_and_a_file_deny_still_wins() {
        let dir = temp_dir("session-rules-flags");
        let mut session = session_with_rules(&dir, "flags", "deny = [\"run_command(curl:*)\"]");
        session.set_flag_rules(
            FlagRules::new(&["run_command".into()], &["run_command(wget:*)".into()]).unwrap(),
        );
        let approver = Counting::default();
        for (line, refused) in [("echo hi", false), ("curl x", true), ("wget x", true)] {
            let input = json!({"command": line});
            let out = session
                .answer_one("r", "run_command", &input, None, &approver, &mut |_| {})
                .await;
            assert_eq!(out.is_error, refused, "{line}: {}", out.output);
        }
        assert_eq!(approver.asked.load(Ordering::SeqCst), 0);
    }
}
