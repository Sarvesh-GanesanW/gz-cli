//! Running your own slash commands (`.gz/commands/*.md` and the user's
//! `commands/` folder) and listing them in `/help`.

use crate::assistant::attachments::PreparedTurn;
use crate::assistant::models::Selection;
use crate::assistant::turn_marks::CommandMark;
use crate::code_ui::custom_commands::{self, CustomCommand};
use crate::code_ui::items::Item;
use crate::code_ui::panel::Row;

use super::commands::COMMANDS;
use super::{Effect, UiState};

/// Built-in names that are not in the command table.
const BUILT_IN_ALIASES: &[&str] = &["/model", "/continue", "/quit"];

/// Whether `name` (with its `/`) is a built-in command, which wins over a
/// command file of the same name.
pub(crate) fn is_built_in(name: &str) -> bool {
    COMMANDS.iter().any(|(built_in, _)| *built_in == name) || BUILT_IN_ALIASES.contains(&name)
}

impl UiState {
    /// Starts a command file's turn on its own `model:` (`selection`): the
    /// turn's usage and cost are labelled with that model; returns the
    /// auto-compact size to send for it. The session's model stays as it
    /// was for the turns after.
    pub fn start_turn_on_model(
        &mut self,
        turn: &PreparedTurn,
        selection: &Selection,
    ) -> Option<u64> {
        let current = self.model.replace(selection.clone());
        self.start_prepared_turn(turn);
        let auto_compact = self.run_auto_compact();
        self.model = current;
        auto_compact
    }

    /// Runs the command file `command` names; `None` when there is none.
    pub(super) fn custom_slash(&mut self, command: &str) -> Option<Vec<Effect>> {
        self.custom_commands.reload();
        let (name, arguments) = command
            .split_once(char::is_whitespace)
            .unwrap_or((command, ""));
        if let Some(found) = self.custom_commands.find(name).cloned() {
            return Some(self.run_custom(&found, arguments));
        }
        let skill = self.custom_commands.skill(name)?.to_string();
        let prompt = CommandMark::wrap(command, &custom_commands::skill_prompt(&skill, arguments));
        if self.is_busy() || self.compacting {
            return Some(self.submit_while_busy(prompt));
        }
        Some(vec![Effect::Send(prompt)])
    }

    fn run_custom(&mut self, found: &CustomCommand, arguments: &str) -> Vec<Effect> {
        let typed = format!("{} {arguments}", found.name);
        let prompt = CommandMark::wrap(&typed, &custom_commands::expand(found, arguments));
        let model = match found.model.as_deref() {
            None => None,
            Some(wanted) => match self.catalog.as_ref().and_then(|c| c.resolve(wanted, None)) {
                Some(selection) => Some(selection),
                None => {
                    self.record(Item::Error(format!(
                        "{} asks for the model '{wanted}', which is not available here; fix `model:` in {} (/models lists the models)",
                        found.name,
                        found.path.display()
                    )));
                    return Vec::new();
                }
            },
        };
        if self.is_busy() || self.compacting {
            if model.is_none() {
                return self.submit_while_busy(prompt);
            }
            // A note to the running turn would go on that turn's model, so
            // a command with its own model waits and runs as its own turn.
            let line = format!("{} {}", found.name, arguments.trim());
            self.queue.push_back(line.trim_end().to_string());
            self.record(Item::Notice(format!(
                "{} starts when the current answer ends, on its own model",
                found.name
            )));
            return Vec::new();
        }
        vec![match model {
            Some(selection) => Effect::SendOnModel(prompt, selection),
            None => Effect::Send(prompt),
        }]
    }

    /// `/help` rows for command files: each with its description, then
    /// the ones a built-in or a project command hides.
    pub(super) fn custom_help_rows(&mut self) -> Vec<Row> {
        self.custom_commands.reload();
        let catalog = &self.custom_commands;
        let mut rows: Vec<Row> = catalog
            .commands
            .iter()
            .filter(|c| !is_built_in(&c.name))
            .map(|c| Row::entry(&c.name, c.menu_text.clone()))
            .collect();
        let shadowed = catalog.commands.iter().filter(|c| is_built_in(&c.name));
        rows.extend(shadowed.map(|c| {
            Row::note(format!(
                "{} in {} is not used: the built-in {} wins",
                c.name,
                c.path.display(),
                c.name
            ))
        }));
        rows.extend(catalog.hidden.iter().map(|c| {
            Row::note(format!(
                "{} in {} is not used: the project's {} wins",
                c.name,
                c.path.display(),
                c.name
            ))
        }));
        rows.extend(catalog.problems.iter().map(Row::note));
        if !self.skill_names.is_empty() {
            rows.push(Row::note(format!(
                "/<skill> [text] loads one of your {} skills and sends the text (/skills lists them)",
                self.skill_names.len()
            )));
        }
        rows
    }

    /// Command files for the `/` menu (never one a built-in hides).
    pub(super) fn custom_menu(&self) -> impl Iterator<Item = (&str, &str)> {
        let files = self
            .custom_commands
            .commands
            .iter()
            .filter(|c| !is_built_in(&c.name))
            .map(|c| (c.name.as_str(), c.menu_text.as_str()));
        let skills = self
            .custom_commands
            .skill_menu()
            .filter(|(name, _)| !is_built_in(name));
        files.chain(skills)
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;

    use super::*;
    use crate::assistant::turn_marks::CommandMark;
    use crate::code_ui::custom_commands::Catalog;
    use crate::code_ui::state::test_support::*;

    fn with_commands(name: &str, files: &[(&str, &str)]) -> crate::code_ui::state::UiState {
        let dir = crate::assistant::sandbox::tests::temp_dir(name);
        for (path, text) in files {
            let path = dir.join("project/.gz/commands").join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let mut s = state();
        s.custom_commands = Catalog::load(Some(&dir.join("project")), None);
        s
    }

    #[test]
    fn a_command_file_sends_its_body_with_the_arguments() {
        let mut s = with_commands(
            "custom-send",
            &[(
                "fix/issue.md",
                "---\ndescription: Fix an issue\n---\nFix issue #$1 then @notes.md",
            )],
        );
        type_text(&mut s, "/fix:issue 42 extra");
        assert_eq!(
            s.on_key(key(KeyCode::Enter)),
            vec![Effect::Send(CommandMark::wrap(
                "/fix:issue 42 extra",
                "Fix issue #42 then @notes.md"
            ))]
        );
    }

    #[test]
    fn a_command_file_with_capitals_shows_in_the_menu() {
        let mut s = with_commands("custom-caps", &[("Deploy.md", "Deploy it")]);
        type_text(&mut s, "/dep");
        assert_eq!(s.command_menu(), vec![("/Deploy", "Deploy it (project)")]);
    }

    #[test]
    fn the_menu_and_help_list_command_files_and_built_ins_win() {
        let mut s = with_commands(
            "custom-menu",
            &[
                (
                    "deploy.md",
                    "---\ndescription: Ship it\nargument-hint: <env>\n---\nDeploy $1",
                ),
                ("help.md", "Not the real help"),
            ],
        );
        type_text(&mut s, "/dep");
        assert_eq!(
            s.command_menu(),
            vec![("/deploy", "Ship it · <env> (project)")]
        );
        s.input = crate::code_ui::state::new_input();
        type_text(&mut s, "/help");
        assert!(s.on_key(key(KeyCode::Enter)).is_empty(), "the built-in ran");
        let help = match s.flush.last() {
            Some(Item::Panel(panel)) => panel.plain_lines().join("\n"),
            other => panic!("{other:?}"),
        };
        assert!(help.contains("/deploy"), "{help}");
        assert!(help.contains("Ship it · <env> (project)"), "{help}");
        assert!(help.contains("/help in "), "{help}");
        assert!(
            help.contains("is not used: the built-in /help wins"),
            "{help}"
        );
    }

    #[test]
    fn a_command_file_model_sends_on_that_model() {
        let dir = crate::assistant::sandbox::tests::temp_dir("custom-on-model");
        let path = dir.join("project/.gz/commands/quick.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "---\nmodel: opus\n---\nSummarize $ARGUMENTS").unwrap();
        let mut s = with_catalog();
        s.custom_commands = Catalog::load(Some(&dir.join("project")), None);
        type_text(&mut s, "/quick the log");
        let effects = s.on_key(key(KeyCode::Enter));
        let [Effect::SendOnModel(prompt, selection)] = effects.as_slice() else {
            panic!("{effects:?}");
        };
        assert_eq!(
            prompt,
            &CommandMark::wrap("/quick the log", "Summarize the log")
        );
        let before = s.model.clone();
        let turn = PreparedTurn::plain(prompt);
        s.start_turn_on_model(&turn, selection);
        assert_eq!(
            s.usage.turn.model,
            Some(selection.label()),
            "billed to its model"
        );
        assert_eq!(s.model, before, "the session's model is kept");
        assert_eq!(
            selection.model.key(),
            s.catalog
                .as_ref()
                .unwrap()
                .resolve("opus", None)
                .unwrap()
                .model
                .key()
        );
    }

    #[test]
    fn a_command_with_its_own_model_typed_while_busy_waits_for_its_own_turn() {
        let dir = crate::assistant::sandbox::tests::temp_dir("custom-busy-model");
        let path = dir.join("project/.gz/commands/quick.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "---\nmodel: opus\n---\nSummarize $ARGUMENTS").unwrap();
        let mut s = with_catalog();
        s.custom_commands = Catalog::load(Some(&dir.join("project")), None);
        s.start_turn("working");
        type_text(&mut s, "/quick the log");
        assert!(s.on_key(key(KeyCode::Enter)).is_empty());
        assert_eq!(s.queue, vec!["/quick the log".to_string()]);
        let effects = s.apply(crate::assistant::AssistantEvent::Done(
            crate::assistant::TurnEnd::Completed,
        ));
        let [Effect::SendOnModel(prompt, _)] = effects.as_slice() else {
            panic!("{effects:?}");
        };
        assert_eq!(
            prompt,
            &CommandMark::wrap("/quick the log", "Summarize the log")
        );
    }

    #[test]
    fn a_bare_run_offers_no_skills_and_says_why_a_command_is_unknown() {
        let mut s = state();
        s.is_bare = true;
        s.set_skill_names(vec!["pdf".into()]);
        assert!(s.skill_names.is_empty());
        type_text(&mut s, "/pdf fill it");
        let effects = s.on_key(key(KeyCode::Enter));
        assert!(effects.is_empty(), "{effects:?}");
        assert!(
            matches!(s.flush.last(), Some(Item::Error(e)) if e.contains("--bare skips command files and skills")),
            "{:?}",
            s.flush.last()
        );
    }

    #[test]
    fn a_skill_runs_as_a_slash_command_after_command_files() {
        let mut s = with_commands("custom-skill", &[("notes.md", "my notes command")]);
        s.set_skill_names(vec!["pdf".into(), "notes".into()]);
        type_text(&mut s, "/pd");
        assert_eq!(
            s.command_menu(),
            vec![("/pdf", "skill: load it and send what follows")]
        );
        s.input = crate::code_ui::state::new_input();
        type_text(&mut s, "/pdf fill in form.pdf");
        let effects = s.on_key(key(KeyCode::Enter));
        let [Effect::Send(prompt)] = effects.as_slice() else {
            panic!("{effects:?}");
        };
        let (typed, expansion) = CommandMark::split(prompt).expect("marked");
        assert_eq!(typed, "/pdf fill in form.pdf");
        assert!(
            expansion.starts_with(
                "fill in form.pdf\n\n(The user asked you to use the local skill `pdf`"
            ),
            "{prompt}"
        );
        type_text(&mut s, "/notes");
        assert_eq!(
            s.on_key(key(KeyCode::Enter)),
            vec![Effect::Send(CommandMark::wrap(
                "/notes",
                "my notes command"
            ))],
            "the command file wins"
        );
    }

    #[test]
    fn a_model_that_is_not_available_is_named() {
        let mut s = with_commands("custom-model", &[("quick.md", "---\nmodel: nope\n---\nhi")]);
        type_text(&mut s, "/quick");
        assert!(s.on_key(key(KeyCode::Enter)).is_empty());
        assert!(
            matches!(s.flush.last(), Some(Item::Error(e)) if e.contains("asks for the model 'nope'")),
            "{:?}",
            s.flush.last()
        );
    }
}
