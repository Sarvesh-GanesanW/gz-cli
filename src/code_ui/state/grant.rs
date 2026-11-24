//! `/permissions`: what gz may do without asking. `/permissions` lists it,
//! `/permissions allow|ask|deny [user|project|local] <rule>` adds a rule,
//! `/permissions allow <what>` (the older forms) saves an answer for the
//! folder before gz ever asks, `/permissions revoke <id>` takes one back
//! and `/permissions trust` trusts the project's rules files.

use anyhow::{bail, Result};

use crate::assistant::permissions::Policy;
use crate::assistant::rules::{
    add_rule, trust_project, Kind, Places, RuleEdit, RuleSet, TOOL_NAMES,
};
use crate::assistant::sandbox::{FolderCheck, Sandbox};
use crate::assistant::saved_rules::{Rule, SavedRule, RULE_FORMS};
use crate::code_ui::items::Item;

use super::{Effect, UiState};

const USAGE: &str = "Use /permissions, /permissions allow|ask|deny [user|project|local] <rule>, \
                     /permissions revoke <id> (e.g. R1, L1) or /permissions trust";
/// The first words of the older `/permissions allow` forms.
const ANSWER_FORMS: [&str; 5] = ["edits", "scripts", "reads", "pages", "command"];

/// A change to the rules files, made by the UI loop (it owns the policy).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RulesAction {
    Add(RuleEdit),
    /// Trust the project's rules files as they are now.
    Trust,
}

impl UiState {
    pub(super) fn permissions_command(&mut self, command: &str) -> Vec<Effect> {
        let rest = command.trim_start_matches("/permissions").trim();
        let (word, argument) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        let argument = argument.trim();
        match (word, argument) {
            ("", _) => vec![Effect::Permissions(None)],
            ("revoke" | "remove", id) if !id.is_empty() && !id.contains(char::is_whitespace) => {
                vec![Effect::Permissions(Some(id.to_string()))]
            }
            ("trust", "") => vec![Effect::Rules(RulesAction::Trust)],
            ("allow", "") => {
                self.record(Item::Error(format!(
                    "Use /permissions allow <rule> (e.g. run_command(cargo test:*), edit(src/**); \
                     tools: {TOOL_NAMES}) or one of: {RULE_FORMS}"
                )));
                Vec::new()
            }
            ("allow", what) if ANSWER_FORMS.contains(&first_word(what)) => {
                match self.rule_to_grant(what) {
                    Ok(saved) => vec![Effect::Grant(saved)],
                    Err(err) => {
                        self.record(Item::Error(format!("{err:#}")));
                        Vec::new()
                    }
                }
            }
            (kind, what) if Kind::parse(kind).is_some() => {
                self.rule_edit(Kind::parse(kind).unwrap_or(Kind::Ask), what)
            }
            _ => {
                self.record(Item::Error(USAGE.into()));
                Vec::new()
            }
        }
    }

    /// `/permissions allow|ask|deny …` and `/allow <rule>`: the rule to add.
    pub(super) fn rule_edit(&mut self, kind: Kind, what: &str) -> Vec<Effect> {
        match RuleEdit::parse(kind, what) {
            Ok(edit) => vec![Effect::Rules(RulesAction::Add(edit))],
            Err(err) if kind == Kind::Allow => {
                self.record(Item::Error(format!(
                    "{err:#}; or save an answer with one of: {RULE_FORMS}"
                )));
                Vec::new()
            }
            Err(err) => {
                self.record(Item::Error(format!("{err:#}")));
                Vec::new()
            }
        }
    }

    /// What a rules change did.
    pub fn show_rules(&mut self, done: Result<String, String>) {
        self.record(match done {
            Ok(notice) => Item::Notice(notice),
            Err(why) => Item::Error(why),
        });
    }

    /// The rule the user typed, for the folder gz is working in.
    fn rule_to_grant(&self, what: &str) -> Result<SavedRule> {
        let Some(sandbox) = &self.sandbox else {
            bail!("gz has no folder open to save the rule for");
        };
        let rule = match Rule::parse(what)? {
            Rule::Reads(folder) => Rule::Reads(open_folder(sandbox, &folder.to_string_lossy())?),
            rule => rule,
        };
        Ok(SavedRule {
            workspace: sandbox.cwd(),
            rule,
        })
    }

    /// What `/permissions allow` did: `added` is false when the rule was
    /// there already.
    pub fn show_grant(&mut self, saved: &SavedRule, added: Result<bool, String>) {
        let folder = match &self.sandbox {
            Some(sandbox) => sandbox.display_folder(&saved.workspace),
            None => saved.workspace.display().to_string(),
        };
        self.record(match added {
            Ok(true) => Item::Notice(format!(
                "✓ gz won't ask about {} in {folder} · /permissions lists and revokes",
                saved.rule
            )),
            Ok(false) => Item::Notice(format!("Already allowed in {folder}: {}", saved.rule)),
            Err(why) => Item::Error(format!("couldn't save the rule: {why}")),
        });
    }
}

fn first_word(text: &str) -> &str {
    text.split_whitespace().next().unwrap_or_default()
}

/// Makes `action`'s change to the rules files and has the session (and
/// its subagents) follow the files at once; the notice says what changed.
pub fn apply_rules(policy: &mut Policy, action: &RulesAction) -> Result<String, String> {
    let rules = policy.rules();
    let places = rules
        .places()
        .ok_or("gz has no folder open to keep rules for")?
        .clone();
    let notice = match action {
        RulesAction::Add(edit) => add(&places, edit).map_err(|err| format!("{err:#}"))?,
        RulesAction::Trust => trust(&places).map_err(|err| format!("{err:#}"))?,
    };
    policy.reload_rules();
    if let RulesAction::Add(edit) = action {
        let waits = policy.rules().rules().iter().any(|rule| {
            !rule.active
                && rule.source == edit.scope.source()
                && rule.pattern.to_string() == edit.pattern
        });
        if waits {
            return Ok(format!(
                "{notice} · the file isn't trusted, so this allow rule counts once you run \
                 /permissions trust"
            ));
        }
    }
    Ok(notice)
}

fn add(places: &Places, edit: &RuleEdit) -> Result<String> {
    let file = places.file(edit.scope)?;
    let rule = format!("{} {}", edit.kind.as_str(), edit.pattern);
    Ok(if add_rule(places, edit.scope, edit.kind, &edit.pattern)? {
        format!(
            "✓ {rule} saved in {} ({}) · /permissions lists and revokes",
            file.display(),
            edit.scope.as_str()
        )
    } else {
        format!("Already there: {rule} in {}", file.display())
    })
}

fn trust(places: &Places) -> Result<String> {
    let trusted = trust_project(places)?;
    if trusted.is_empty() {
        return Ok(format!(
            "{} has no .gz/permissions.toml or .gz/permissions.local.toml to trust",
            places.folder.display()
        ));
    }
    let names: Vec<String> = trusted.iter().map(|p| p.display().to_string()).collect();
    Ok(format!(
        "✓ Trusted {}: their allow rules and mode count now; a changed file waits for trust again",
        names.join(", ")
    ))
}

impl UiState {
    /// Says which rules or rules files gz couldn't read, so a deny rule
    /// that doesn't count never goes unnoticed.
    pub fn report_rule_problems(&mut self, rules: &RuleSet) {
        for problem in &rules.problems {
            self.record(Item::Error(problem.clone()));
        }
    }
}

/// The folder `requested` names, when gz may already work in it. Reads
/// are confined to the open folders, so a rule for one outside them would
/// never apply.
fn open_folder(sandbox: &Sandbox, requested: &str) -> Result<std::path::PathBuf> {
    match sandbox.check_folder(requested)? {
        FolderCheck::Inside(folder) => Ok(folder),
        FolderCheck::Outside(folder) => bail!(
            "{} isn't open in gz; open it first with /add-dir {requested}",
            sandbox.display_folder(&folder)
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crossterm::event::KeyCode;

    use super::*;
    use crate::assistant::rules::Scope;
    use crate::assistant::sandbox::tests::temp_workspace;
    use crate::code_ui::state::test_support::*;

    fn run(s: &mut UiState, command: &str) -> Vec<Effect> {
        type_text(s, command);
        s.on_key(key(KeyCode::Enter))
    }

    fn granted(effects: &[Effect]) -> &Rule {
        match effects {
            [Effect::Grant(saved)] => &saved.rule,
            other => panic!("no grant: {other:?}"),
        }
    }

    #[test]
    fn allow_grants_a_rule_for_the_folder_gz_works_in() {
        let (dir, sandbox) = temp_workspace("grant-rules");
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        let mut s = state();
        s.sandbox = Some(sandbox.clone());

        let effects = run(&mut s, "/permissions allow command cargo test *");
        let [Effect::Grant(saved)] = &effects[..] else {
            panic!("{effects:?}");
        };
        assert_eq!(saved.workspace, sandbox.cwd());
        assert_eq!(saved.rule, Rule::CommandStart("cargo test".into()));

        for (typed, rule) in [
            ("command make lint", Rule::Command("make lint".into())),
            ("edits", Rule::Edits),
            ("scripts", Rule::Scripts),
            (
                "pages https://Docs.Example.com/guide",
                Rule::Host("docs.example.com".into()),
            ),
            ("reads docs", Rule::Reads(sandbox.cwd().join("docs"))),
        ] {
            let effects = run(&mut s, &format!("/permissions allow {typed}"));
            assert_eq!(granted(&effects), &rule, "{typed}");
        }
    }

    #[test]
    fn allow_refuses_what_could_never_apply_or_would_cover_too_much() {
        let (dir, sandbox) = temp_workspace("grant-refused");
        let outside = dir.parent().unwrap().join("grant-refused-outside");
        std::fs::create_dir_all(&outside).unwrap();
        let mut s = state();
        s.sandbox = Some(sandbox);
        for (typed, why) in [
            ("command git status; rm -rf ~ *", "shell syntax"),
            ("command *", "name the command"),
            ("reads ../grant-refused-outside", "/add-dir"),
            ("everything", "isn't a tool a rule can name"),
            ("", "allow <rule>"),
        ] {
            assert!(
                run(&mut s, &format!("/permissions allow {typed}")).is_empty(),
                "{typed}"
            );
            assert!(
                matches!(s.flush.last(), Some(Item::Error(e)) if e.contains(why)),
                "{typed}: {:?}",
                s.flush.last()
            );
        }
    }

    fn rules_effect(effects: &[Effect]) -> &RulesAction {
        match effects {
            [Effect::Rules(action)] => action,
            other => panic!("no rules change: {other:?}"),
        }
    }

    #[test]
    fn allow_ask_and_deny_add_rules_in_a_scope_and_old_forms_still_work() {
        let (_dir, sandbox) = temp_workspace("grant-rule-kinds");
        let mut s = state();
        s.sandbox = Some(sandbox);
        for (typed, kind, scope, pattern) in [
            (
                "/permissions deny project run_command(rm -rf:*)",
                Kind::Deny,
                Scope::Project,
                "run_command(rm -rf:*)",
            ),
            ("/permissions ask edit", Kind::Ask, Scope::User, "edit"),
            (
                "/permissions allow local edit(src/**)",
                Kind::Allow,
                Scope::Local,
                "edit(src/**)",
            ),
            (
                "/allow web_fetch(domain:docs.rs)",
                Kind::Allow,
                Scope::User,
                "web_fetch(domain:docs.rs)",
            ),
        ] {
            let effects = run(&mut s, typed);
            assert_eq!(
                rules_effect(&effects),
                &RulesAction::Add(RuleEdit {
                    kind,
                    scope,
                    pattern: pattern.into()
                }),
                "{typed}"
            );
        }
        assert_eq!(
            rules_effect(&run(&mut s, "/permissions trust")),
            &RulesAction::Trust
        );
        assert!(matches!(
            &run(&mut s, "/permissions remove R2")[..],
            [Effect::Permissions(Some(id))] if id == "R2"
        ));
        assert_eq!(
            granted(&run(&mut s, "/permissions allow edits")),
            &Rule::Edits,
            "the older form saves an answer"
        );
        assert!(run(&mut s, "/permissions deny bash(rm)").is_empty());
        assert!(
            matches!(s.flush.last(), Some(Item::Error(e)) if e.contains("isn't a tool")),
            "{:?}",
            s.flush.last()
        );
    }

    #[test]
    fn adding_and_trusting_rules_changes_what_the_session_follows() {
        let (dir, sandbox) = temp_workspace("grant-apply-rules");
        let config = crate::assistant::sandbox::tests::temp_dir("grant-apply-config");
        let places = Places {
            folder: dir.clone(),
            home: None,
            user_file: Some(config.join("permissions.toml")),
            trust: Some(crate::assistant::hooks::trust::TrustStore::at(
                config.join("trust.json"),
            )),
        };
        let mut policy = Policy::default();
        policy.set_rules(RuleSet::load(
            places.clone(),
            crate::assistant::rules::FlagRules::default(),
        ));
        let ls = crate::assistant::tools::plan(
            &sandbox,
            crate::assistant::tools::ToolName::RunCommand,
            &serde_json::json!({"command": "ls"}),
        )
        .unwrap();
        let add = |kind, scope, pattern: &str| {
            RulesAction::Add(RuleEdit {
                kind,
                scope,
                pattern: pattern.into(),
            })
        };
        let notice = apply_rules(
            &mut policy,
            &add(Kind::Deny, Scope::User, "run_command(ls)"),
        )
        .unwrap();
        assert!(
            notice.starts_with("✓ deny run_command(ls) saved in"),
            "{notice}"
        );
        assert_eq!(policy.rule_verdict(&ls).unwrap().kind, Kind::Deny);
        let again = apply_rules(
            &mut policy,
            &add(Kind::Deny, Scope::User, "run_command(ls)"),
        )
        .unwrap();
        assert!(again.starts_with("Already there"), "{again}");

        // Someone else's project file: allow rules wait for trust.
        std::fs::create_dir_all(dir.join(".gz")).unwrap();
        std::fs::write(dir.join(".gz/permissions.toml"), "allow = [\"run_script\"]").unwrap();
        let notice = apply_rules(
            &mut policy,
            &add(Kind::Allow, Scope::Project, "run_command(cargo test:*)"),
        )
        .unwrap();
        assert!(notice.contains("/permissions trust"), "{notice}");
        let notice = apply_rules(&mut policy, &RulesAction::Trust).unwrap();
        assert!(notice.starts_with("✓ Trusted"), "{notice}");
        assert!(policy.rules().rules().iter().all(|rule| rule.active));
    }

    #[test]
    fn the_user_is_told_what_was_saved_and_where() {
        let mut s = state();
        let saved = SavedRule {
            workspace: PathBuf::from("/work/app"),
            rule: Rule::CommandStart("git status".into()),
        };
        s.show_grant(&saved, Ok(true));
        assert!(matches!(
            s.flush.last(),
            Some(Item::Notice(n)) if n.starts_with("✓ gz won't ask about commands starting with `git status` in /work/app")
        ));
        s.show_grant(&saved, Ok(false));
        assert!(
            matches!(s.flush.last(), Some(Item::Notice(n)) if n.starts_with("Already allowed"))
        );
        s.show_grant(&saved, Err("disk full".into()));
        assert!(matches!(s.flush.last(), Some(Item::Error(e)) if e.contains("disk full")));
    }
}
