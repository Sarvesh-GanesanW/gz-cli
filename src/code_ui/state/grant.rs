//! `/permissions`: what gz may do without asking. `/permissions` lists it,
//! `/permissions allow <what>` grants something before gz ever asks, and
//! `/permissions revoke <id>` takes a grant back.

use anyhow::{bail, Result};

use crate::assistant::sandbox::{FolderCheck, Sandbox};
use crate::assistant::saved_rules::{Rule, SavedRule, RULE_FORMS};
use crate::code_ui::items::Item;

use super::{Effect, UiState};

const USAGE: &str =
    "Use /permissions, /permissions allow <what>, or /permissions revoke <id> (e.g. L1)";

impl UiState {
    pub(super) fn permissions_command(&mut self, command: &str) -> Vec<Effect> {
        let rest = command.trim_start_matches("/permissions").trim();
        let (word, argument) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        let argument = argument.trim();
        match (word, argument) {
            ("", _) => vec![Effect::Permissions(None)],
            ("revoke", id) if !id.is_empty() && !id.contains(char::is_whitespace) => {
                vec![Effect::Permissions(Some(id.to_string()))]
            }
            ("allow", "") => {
                self.record(Item::Error(format!(
                    "Use /permissions allow <what>: {RULE_FORMS}"
                )));
                Vec::new()
            }
            ("allow", what) => match self.rule_to_grant(what) {
                Ok(saved) => vec![Effect::Grant(saved)],
                Err(err) => {
                    self.record(Item::Error(format!("{err:#}")));
                    Vec::new()
                }
            },
            _ => {
                self.record(Item::Error(USAGE.into()));
                Vec::new()
            }
        }
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
            ("everything", "allow what?"),
            ("", "allow <what>"),
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
