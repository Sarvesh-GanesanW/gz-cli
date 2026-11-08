//! Folders, memory and instructions: `/cd`, `/add-dir`, `/dirs`,
//! `/memory` and `/instructions`.
//!
//! Folder changes go to the session (it owns the folders, the audit log
//! and `--yes-reads`); the panels read the shared sandbox and the user's
//! files directly, like `/auto` reads its rules.

use std::path::Path;

use crate::assistant::audit::Entry;
use crate::assistant::instructions::{self, Source};
use crate::assistant::local_context::Limits;
use crate::code_ui::items::Item;
use crate::code_ui::panel::{Panel, Row};

use super::{Effect, UiState};

const DIRS_USAGE: &str = "Use /dirs, or /dirs remove <number or path>";
const MEMORY_USAGE: &str = "Use /memory, /memory forget <id>, /memory clear, or /memory on|off";

/// What a folder command asks the session to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FolderCommand {
    Cd(String),
    Add(String),
    Remove(String),
}

impl UiState {
    pub(super) fn folder_command(&mut self, command: &str) -> Vec<Effect> {
        let (name, argument) = command.split_once(' ').unwrap_or((command, ""));
        let argument = argument.trim().to_string();
        match (name, argument.as_str()) {
            ("/cd", "") => self.show_cwd(),
            ("/cd", _) => return vec![Effect::Folder(FolderCommand::Cd(argument))],
            ("/add-dir", "") => self.record(Item::Error("Use /add-dir <path>".into())),
            ("/add-dir", _) => return vec![Effect::Folder(FolderCommand::Add(argument))],
            (_, "") => self.show_dirs(),
            (_, rest) => match rest.split_once(' ') {
                Some(("remove", which)) => return self.remove_dir(which.trim()),
                _ => self.record(Item::Error(DIRS_USAGE.into())),
            },
        }
        Vec::new()
    }

    fn show_cwd(&mut self) {
        let Some(sandbox) = self.sandbox.clone() else {
            return;
        };
        let shown = sandbox.display_folder(&sandbox.cwd());
        self.record(Item::Notice(format!("In {shown} · /cd <path> to move")));
    }

    fn show_dirs(&mut self) {
        let Some(sandbox) = self.sandbox.clone() else {
            return;
        };
        let cwd = sandbox.cwd();
        let rows = sandbox
            .roots()
            .iter()
            .enumerate()
            .map(|(i, root)| {
                let mut notes = Vec::new();
                if i == 0 {
                    notes.push("started here");
                }
                if cwd.starts_with(root) {
                    notes.push("current folder is inside");
                }
                Row::entry(
                    &format!("{}", i + 1),
                    format!("{}  {}", sandbox.display_folder(root), notes.join(" · ")),
                )
            })
            .collect();
        let panel = Panel::new("Folders")
            .aside(format!("now in {}", sandbox.display_folder(&cwd)))
            .section(None, rows)
            .footer("/cd <path> moves · /add-dir <path> opens another · /dirs remove <n>");
        self.record(Item::Panel(panel));
    }

    fn remove_dir(&mut self, which: &str) -> Vec<Effect> {
        let Some(sandbox) = self.sandbox.clone() else {
            return Vec::new();
        };
        let path = match which.parse::<usize>() {
            Ok(n) => match sandbox.roots().get(n.wrapping_sub(1)) {
                Some(root) => root.display().to_string(),
                None => {
                    self.record(Item::Error(format!(
                        "There is no folder {n}; /dirs lists them"
                    )));
                    return Vec::new();
                }
            },
            Err(_) if which.is_empty() => {
                self.record(Item::Error(DIRS_USAGE.into()));
                return Vec::new();
            }
            Err(_) => which.to_string(),
        };
        vec![Effect::Folder(FolderCommand::Remove(path))]
    }

    pub(super) fn memory_command(&mut self, command: &str) -> Vec<Effect> {
        let mut words = command.split_whitespace().skip(1);
        match (words.next(), words.next()) {
            (None, _) => self.show_memory(),
            (Some("on"), None) => return self.set_memory(true),
            (Some("off"), None) => return self.set_memory(false),
            (Some("forget"), Some(id)) => self.forget_memory(id),
            (Some("clear"), None) => self.confirm_clear_memory(),
            (Some("clear"), Some("yes")) => self.clear_memory(),
            _ => self.record(Item::Error(MEMORY_USAGE.into())),
        }
        Vec::new()
    }

    fn local_context_limits(&self) -> Option<Limits> {
        self.catalog.as_ref()?.local_context
    }

    fn show_memory(&mut self) {
        let Some(store) = self.memory.clone() else {
            self.record(Item::Error(
                "Memory needs the gz config folder, which can't be found.".into(),
            ));
            return;
        };
        let memories = match store.list() {
            Ok(memories) => memories,
            Err(err) => {
                return self.record(Item::Error(format!("cannot read your memories: {err:#}")))
            }
        };
        let status = match (self.memory_on, self.local_context_limits().is_some()) {
            (false, _) => "off · /memory on",
            (true, true) => "on",
            (true, false) if self.models_loading => "on",
            (true, false) => "on, but this chat server doesn't take memories yet",
        };
        let rows = if memories.is_empty() {
            vec![Row::note(
                "Nothing yet — gz saves what it learns about how you work",
            )]
        } else {
            memories
                .iter()
                .map(|m| Row::entry(&m.id, m.text.clone()))
                .collect()
        };
        let panel = Panel::new("Memory")
            .aside(status)
            .section(None, rows)
            .footer("/memory forget <id> · /memory clear · /memory on|off");
        self.record(Item::Panel(panel));
    }

    fn set_memory(&mut self, on: bool) -> Vec<Effect> {
        self.memory_on = on;
        self.record(Item::Notice(
            if on {
                "Memory on: gz remembers what it learns about how you work"
            } else {
                "Memory off: gz neither saves nor sends memories"
            }
            .into(),
        ));
        vec![Effect::SetMemory(on)]
    }

    fn forget_memory(&mut self, id: &str) {
        let Some(store) = self.memory.clone() else {
            return;
        };
        match store.delete(id) {
            Ok(memory) => {
                self.audit_memory("memory/delete", &memory.id);
                self.record(Item::Notice(format!(
                    "Forgot memory {}: {}",
                    memory.id, memory.text
                )));
            }
            Err(err) => self.record(Item::Error(format!("{err:#}"))),
        }
    }

    fn confirm_clear_memory(&mut self) {
        let count = self
            .memory
            .as_ref()
            .and_then(|store| store.list().ok())
            .map_or(0, |all| all.len());
        self.record(Item::Notice(format!(
            "This deletes all {count} memories. Type /memory clear yes to confirm."
        )));
    }

    fn clear_memory(&mut self) {
        let Some(store) = self.memory.clone() else {
            return;
        };
        match store.clear() {
            Ok(count) => {
                self.audit_memory("memory/clear", &format!("{count} memories"));
                self.record(Item::Notice(format!("Forgot all {count} memories")));
            }
            Err(err) => self.record(Item::Error(format!("{err:#}"))),
        }
    }

    fn audit_memory(&self, tool: &str, target: &str) {
        let Some(log) = &self.audit else {
            return;
        };
        let entry = Entry {
            conversation_id: "",
            run_id: "",
            tool,
            target,
            decision: "user",
            reason: None,
            is_error: false,
        };
        if let Err(err) = log.record(&entry) {
            crate::output::warn(&format!("cannot write the audit log: {err:#}"));
        }
    }

    pub(super) fn instructions_command(&mut self, command: &str) -> Vec<Effect> {
        match command.split_whitespace().nth(1) {
            None => self.show_instructions(),
            Some("edit") => match self.user_instructions.clone() {
                Some(path) => return vec![Effect::EditInstructions(path)],
                None => self.record(Item::Error(
                    "The gz config folder can't be found, so there is no instructions file.".into(),
                )),
            },
            Some(_) => self.record(Item::Error(
                "Use /instructions or /instructions edit".into(),
            )),
        }
        Vec::new()
    }

    fn show_instructions(&mut self) {
        let Some(sandbox) = self.sandbox.clone() else {
            return;
        };
        let limits = self.local_context_limits().unwrap_or_default();
        let found = instructions::discover(&sandbox, self.user_instructions.as_deref(), &limits);
        let mut rows: Vec<Row> = found
            .iter()
            .map(|file| {
                let kind = match file.source {
                    Source::User => "yours",
                    Source::Project => "project",
                };
                let cut = if file.truncated { " · truncated" } else { "" };
                Row::entry(kind, format!("{}  {} bytes{cut}", file.shown, file.bytes))
            })
            .collect();
        if rows.is_empty() {
            rows.push(Row::note("None in use"));
        }
        let mut panel = Panel::new("Instructions")
            .aside(format!("for {}", sandbox.display_folder(&sandbox.cwd())))
            .section(None, rows);
        if self.local_context_limits().is_none() && !self.models_loading {
            panel = panel.section(
                None,
                vec![Row::note(
                    "This chat server doesn't take instruction files yet",
                )],
            );
        }
        let footer = match self.user_instructions.as_deref() {
            Some(path) => format!(
                "/instructions edit opens {} · GZ.md, AGENTS.md, CLAUDE.md apply from here up to ~",
                sandbox.display_folder(path)
            ),
            None => "GZ.md, AGENTS.md, CLAUDE.md apply from here up to ~".to_string(),
        };
        self.record(Item::Panel(panel.footer(footer)));
    }

    /// After `/instructions edit`: what happened, the terminal back.
    pub fn edited_instructions(&mut self, path: &Path, result: Result<(), String>) {
        let shown = self
            .sandbox
            .as_ref()
            .map_or_else(|| path.display().to_string(), |s| s.display_folder(path));
        match result {
            Ok(()) => self.record(Item::Notice(format!(
                "Saved {shown}; it applies from your next message"
            ))),
            Err(why) => self.record(Item::Error(why)),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::code_ui::state::test_support::*;

    use crossterm::event::KeyCode;

    use super::FolderCommand;
    use crate::assistant::local_context::Limits;
    use crate::assistant::memory::MemoryStore;
    use crate::assistant::sandbox::{tests::temp_dir, Sandbox};
    use crate::code_ui::items::Item;
    use crate::code_ui::state::Effect;

    fn run(s: &mut crate::code_ui::state::UiState, command: &str) -> Vec<Effect> {
        type_text(s, command);
        s.on_key(key(KeyCode::Enter))
    }

    #[test]
    fn folder_commands_go_to_the_session() {
        let dir = temp_dir("ui-dirs");
        let other = temp_dir("ui-dirs-other");
        let mut s = state();
        let sandbox = Sandbox::new(&dir).unwrap();
        assert!(sandbox.add_root(&other).unwrap());
        s.sandbox = Some(sandbox);
        assert_eq!(
            run(&mut s, "/cd ~/code"),
            vec![Effect::Folder(FolderCommand::Cd("~/code".into()))]
        );
        assert_eq!(
            run(&mut s, "/add-dir ../x"),
            vec![Effect::Folder(FolderCommand::Add("../x".into()))]
        );
        assert_eq!(
            run(&mut s, "/dirs remove 2"),
            vec![Effect::Folder(FolderCommand::Remove(
                other.display().to_string()
            ))],
            "a number names an open folder"
        );
        assert!(run(&mut s, "/dirs remove 9").is_empty());
        assert!(run(&mut s, "/dirs").is_empty());
        assert!(matches!(s.flush.last(), Some(Item::Panel(p)) if p.title == "Folders"));
    }

    #[test]
    fn memory_commands_list_forget_and_clear_only_when_confirmed() {
        let dir = temp_dir("ui-memory");
        let store = MemoryStore::at(dir.join("memory.json"), "default");
        let kept = store.save("Uses tabs", &Limits::default()).unwrap();
        let gone = store.save("Likes tea", &Limits::default()).unwrap();
        let mut s = with_catalog();
        s.memory = Some(store.clone());
        run(&mut s, "/memory");
        assert!(
            matches!(s.flush.last(), Some(Item::Panel(p)) if p.plain_lines().iter().any(|l| l.contains("Uses tabs")))
        );
        run(&mut s, &format!("/memory forget {}", gone.id));
        assert_eq!(store.list().unwrap(), vec![kept]);
        run(&mut s, "/memory clear");
        assert_eq!(store.list().unwrap().len(), 1, "asks before clearing");
        run(&mut s, "/memory clear yes");
        assert!(store.list().unwrap().is_empty());
        assert_eq!(run(&mut s, "/memory off"), vec![Effect::SetMemory(false)]);
        assert!(!s.memory_on);
    }

    #[test]
    fn instructions_lists_the_files_in_use_and_edit_opens_the_personal_one() {
        let dir = temp_dir("ui-instructions");
        std::fs::write(dir.join("AGENTS.md"), "be careful").unwrap();
        let mut s = with_catalog();
        s.sandbox = Some(Sandbox::new(&dir).unwrap());
        s.user_instructions = Some(dir.join("config/instructions.md"));
        run(&mut s, "/instructions");
        let Some(Item::Panel(panel)) = s.flush.last() else {
            panic!("a panel");
        };
        assert!(panel.plain_lines().iter().any(|l| l.contains("AGENTS.md")));
        assert_eq!(
            run(&mut s, "/instructions edit"),
            vec![Effect::EditInstructions(dir.join("config/instructions.md"))]
        );
    }
}
