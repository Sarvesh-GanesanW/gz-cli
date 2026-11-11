//! `/hooks`: lists the session's hooks, turns them off or on for the
//! session, and reviews the project hooks file for trust.

use std::path::Path;

use crate::assistant::hooks::{Hooks, ProjectState};
use crate::code_ui::hooks;
use crate::code_ui::items::Item;

use super::{Effect, UiState};

const USAGE: &str = "usage: /hooks, /hooks off|on, /hooks trust";

impl UiState {
    pub(super) fn hooks_command(&mut self, command: &str) -> Vec<Effect> {
        let Some(loaded) = self.hooks.clone() else {
            self.record(Item::Error("hooks are not loaded in this session".into()));
            return Vec::new();
        };
        let mut words = command.split_whitespace().skip(1);
        match (words.next(), words.next()) {
            (None, _) => {
                let sandbox = self.sandbox.clone();
                let show = |path: &Path| match &sandbox {
                    Some(sandbox) => sandbox.display_folder(path),
                    None => path.display().to_string(),
                };
                self.record(Item::Panel(hooks::panel(&loaded.status(), &show)))
            }
            (Some("off"), None) => {
                loaded.set_enabled(false);
                self.record(Item::Notice(
                    "Hooks are off for this session; /hooks on turns them back on".into(),
                ));
            }
            (Some("on"), None) => {
                loaded.set_enabled(true);
                self.record(Item::Notice("Hooks are on".into()));
            }
            (Some("trust"), None) => return vec![Effect::TrustHooks],
            _ => self.record(Item::Error(USAGE.into())),
        }
        Vec::new()
    }

    /// Takes the session's hooks at start: reports broken files, says why
    /// declined project hooks are off, and asks about an untrusted file.
    pub fn use_hooks(&mut self, loaded: Hooks) -> Vec<Effect> {
        let status = loaded.status();
        self.hooks = Some(loaded);
        for problem in status.problems {
            self.record(Item::Error(problem));
        }
        match status.project {
            Some(ProjectState::Untrusted) => vec![Effect::TrustHooks],
            Some(ProjectState::Declined) => {
                self.record(Item::Notice(
                    "Project hooks are off: you declined this version of .gz/hooks.toml (/hooks trust to review)".into(),
                ));
                Vec::new()
            }
            Some(ProjectState::Trusted) | None => Vec::new(),
        }
    }

    /// What `/hooks trust` (or the start-up question) ended with.
    pub fn hooks_trusted(&mut self, done: Result<String, String>) {
        match done {
            Ok(text) => self.record(Item::Notice(text)),
            Err(why) => self.record(Item::Error(why)),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::assistant::hooks::test_support::{places, write};
    use crate::assistant::hooks::Hooks;
    use crate::code_ui::items::Item;
    use crate::code_ui::state::test_support::*;
    use crate::code_ui::state::Effect;
    use crossterm::event::KeyCode;

    #[test]
    fn hooks_command_lists_switches_and_asks_for_trust() {
        let dir = crate::assistant::sandbox::tests::temp_dir("hooks-cmd");
        write(
            &dir.join("config/hooks.toml"),
            "[[turn_end]]\ncommand = \"true\"",
        );
        write(
            &dir.join("project/.gz/hooks.toml"),
            "[[turn_end]]\ncommand = \"make\"",
        );
        let hooks = Hooks::load(places(&dir));
        let mut s = state();
        assert_eq!(s.use_hooks(hooks.clone()), vec![Effect::TrustHooks]);

        type_text(&mut s, "/hooks");
        assert!(s.on_key(key(KeyCode::Enter)).is_empty());
        assert!(matches!(s.flush.last(), Some(Item::Panel(p)) if p.title == "Hooks"));

        type_text(&mut s, "/hooks off");
        s.on_key(key(KeyCode::Enter));
        assert!(!hooks.status().enabled, "the session's hooks are off");

        type_text(&mut s, "/hooks trust");
        assert_eq!(s.on_key(key(KeyCode::Enter)), vec![Effect::TrustHooks]);

        type_text(&mut s, "/hooks maybe");
        s.on_key(key(KeyCode::Enter));
        assert!(matches!(s.flush.last(), Some(Item::Error(e)) if e.starts_with("usage: /hooks")));
    }
}
