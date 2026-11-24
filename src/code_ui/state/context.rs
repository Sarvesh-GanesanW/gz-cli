//! Folders, memory and instructions: `/cd`, `/add-dir`, `/dirs`,
//! `/memory` and `/instructions`.
//!
//! Folder changes go to the session (it owns the folders, the audit log
//! and `--yes-reads`); the panels read the shared sandbox and the user's
//! files directly, like `/auto` reads its rules.

use std::path::Path;

use crate::assistant::audit::Entry;
use crate::assistant::instructions::{self, Instruction, Source, Why};
use crate::assistant::local_context::Limits;
use crate::assistant::memory::{Change, Memory, Scope};
use crate::code_ui::items::Item;
use crate::code_ui::panel::{Panel, Row};

use super::{context_window, Effect, UiState};

/// What `/init` asks the model to do.
const INIT_PROMPT: &str = "Study this folder and write GZ.md here, or improve it if it exists: \
gz sends GZ.md with every message in this folder, so it should hold what you would need to \
know to work here well.

1. Read the existing GZ.md first, and AGENTS.md, CLAUDE.md, .gz/rules and the README if \
present; keep what is still true.
2. Find how to build, run, lint and test (including a single test) from the manifests, \
Makefile, scripts and CI config; the layout of the main folders; and the conventions the \
code follows that a newcomer would get wrong.
3. Keep GZ.md short and specific to this project: exact commands, the architecture in a few \
lines, conventions and gotchas. No generic advice, nothing obvious from the file list, no \
secrets.
4. Then tell me in a few lines what you wrote or changed.";

/// `/memory add --project <text>` files the memory under this project.
const PROJECT_FLAG: &str = "--project";
const DIRS_USAGE: &str = "Use /dirs, or /dirs remove <number or path>";
const MEMORY_USAGE: &str =
    "Use /memory, /memory add [--project] <text>, /memory edit <id> <text>, \
     /memory move <id>, /memory forget <id>, /memory clear [project], or /memory on|off";

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
        let rest = command
            .trim_start()
            .strip_prefix("/memory")
            .unwrap_or("")
            .trim();
        let (verb, argument) = rest.split_once(' ').unwrap_or((rest, ""));
        let argument = argument.trim();
        match (verb, argument) {
            ("", _) => self.show_memory(),
            ("on", "") => return self.set_memory(true),
            ("off", "") => return self.set_memory(false),
            ("add", text) if !text.is_empty() => self.add_memory(text),
            ("edit", rest) if rest.contains(' ') => {
                let (id, text) = rest.split_once(' ').unwrap_or_default();
                self.edit_memory(id, text.trim());
            }
            ("move", id) if !id.is_empty() && !id.contains(' ') => self.move_memory(id),
            ("forget", id) if !id.is_empty() && !id.contains(' ') => self.forget_memory(id),
            ("clear", "") => self.confirm_clear_memory(false),
            ("clear", "project") => self.confirm_clear_memory(true),
            ("clear", "yes") => self.clear_memory(false),
            ("clear", "project yes") => self.clear_memory(true),
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
        let lists = store
            .list_scope(Scope::Project)
            .and_then(|project| Ok((project, store.list_scope(Scope::Everywhere)?)));
        let (project, everywhere) = match lists {
            Ok(lists) => lists,
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
        let rows = |memories: &[Memory], empty: &str| -> Vec<Row> {
            if memories.is_empty() {
                return vec![Row::note(empty)];
            }
            memories
                .iter()
                .map(|m| Row::entry(&m.id, m.text.clone()))
                .collect()
        };
        let mut panel = Panel::new("Memory").aside(status);
        if let Some(folder) = store.project() {
            let shown = self.sandbox.as_ref().map_or_else(
                || folder.display().to_string(),
                |s| s.display_folder(folder),
            );
            let heading = format!("This project · {shown}");
            panel = panel.section(
                Some(&heading),
                rows(&project, "Nothing yet — /memory add --project <text>"),
            );
        }
        let panel = panel
            .section(
                Some("Everywhere"),
                rows(
                    &everywhere,
                    "Nothing yet — gz saves what it learns about how you work",
                ),
            )
            .footer(
                "/memory add [--project] <text> · edit <id> <text> · move <id> · forget <id> \
                 · clear [project] · on|off",
            );
        self.record(Item::Panel(panel));
    }

    fn memory_limits(&self) -> Limits {
        self.local_context_limits().unwrap_or_default()
    }

    fn add_memory(&mut self, text: &str) {
        let Some(store) = self.memory.clone() else {
            return;
        };
        let limits = self.memory_limits();
        let (flag, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
        let saved = match flag {
            PROJECT_FLAG if rest.trim().is_empty() => {
                self.record(Item::Error(
                    "Nothing to remember: use /memory add --project <text>".into(),
                ));
                return;
            }
            PROJECT_FLAG => store.save_in(Scope::Project, rest.trim(), &limits),
            _ => store.save(text, &limits),
        };
        match saved {
            Ok(memory) => {
                self.audit_memory("memory/save", &memory.id);
                self.record(Item::Notice(
                    Change::Saved(memory).notice() + " · applies from your next message",
                ));
            }
            Err(err) => self.record(Item::Error(format!("{err:#}"))),
        }
    }

    fn edit_memory(&mut self, id: &str, text: &str) {
        let Some(store) = self.memory.clone() else {
            return;
        };
        match store.update(id, text, &self.memory_limits()) {
            Ok(memory) => {
                self.audit_memory("memory/update", &memory.id);
                self.record(Item::Notice(Change::Updated(memory).notice()));
            }
            Err(err) => self.record(Item::Error(format!("{err:#}"))),
        }
    }

    fn move_memory(&mut self, id: &str) {
        let Some(store) = self.memory.clone() else {
            return;
        };
        match store.move_to_other_scope(id) {
            Ok(memory) => {
                self.audit_memory("memory/move", &memory.id);
                let place = match Scope::of_id(&memory.id) {
                    Scope::Project => "this project only",
                    Scope::Everywhere => "everywhere",
                };
                self.record(Item::Notice(format!(
                    "Memory {id} now applies to {place}, as {}: {}",
                    memory.id, memory.text
                )));
            }
            Err(err) => self.record(Item::Error(format!("{err:#}"))),
        }
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

    fn confirm_clear_memory(&mut self, project_only: bool) {
        let Some(store) = self.memory.clone() else {
            return;
        };
        let (count, what, confirm) = if project_only {
            let count = store.list_scope(Scope::Project).map_or(0, |all| all.len());
            (count, "this project's", "/memory clear project yes")
        } else {
            let count = store.list().map_or(0, |all| all.len());
            (count, "your", "/memory clear yes")
        };
        self.record(Item::Notice(format!(
            "This deletes all {count} of {what} memories. Type {confirm} to confirm."
        )));
    }

    fn clear_memory(&mut self, project_only: bool) {
        let Some(store) = self.memory.clone() else {
            return;
        };
        let cleared = if project_only {
            store.clear_project()
        } else {
            store.clear()
        };
        match cleared {
            Ok(count) => {
                self.audit_memory("memory/clear", &format!("{count} memories"));
                let scope = if project_only { " of this project" } else { "" };
                self.record(Item::Notice(format!("Forgot all {count} memories{scope}")));
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
        let found = match &self.local_context {
            Some(context) => context.instruction_report(&sandbox, &limits),
            None => instructions::find(
                &sandbox,
                self.user_instructions.as_deref(),
                &limits,
                &Default::default(),
            ),
        };
        let mut rows: Vec<Row> = found.sent.iter().flat_map(instruction_rows).collect();
        if rows.is_empty() {
            rows.push(Row::note("None in use"));
        }
        let left_out: Vec<Row> = found
            .left_out
            .iter()
            .map(|file| Row::entry("left out", format!("{} · {}", file.shown, file.why)))
            .collect();
        let notes: Vec<Row> = found
            .sent
            .iter()
            .filter_map(|file| file.note.clone())
            .map(Row::note)
            .collect();
        let mut panel = Panel::new("Instructions")
            .aside(format!(
                "for {} · {} of {} entries",
                sandbox.display_folder(&sandbox.cwd()),
                found.sent.len(),
                limits.instruction_files
            ))
            .section(None, rows)
            .section(Some("Not sent"), left_out)
            .section(None, notes);
        if self.local_context_limits().is_none() && !self.models_loading {
            panel = panel.section(
                None,
                vec![Row::note(
                    "This chat server doesn't take instruction files yet",
                )],
            );
        }
        let files =
            "GZ.md, GZ.local.md, AGENTS.md, CLAUDE.md and .gz/rules apply from here up to ~; \
                     a subfolder's once gz works there";
        let footer = match self.user_instructions.as_deref() {
            Some(path) => format!(
                "/instructions edit opens {} · {files}",
                sandbox.display_folder(path)
            ),
            None => files.to_string(),
        };
        self.record(Item::Panel(panel.footer(footer)));
    }

    /// `/context`: what fills the context window, measured and estimated.
    pub(super) fn context_command(&mut self) -> Vec<Effect> {
        let mut parts = context_window::LocalParts {
            mcp_servers: self
                .local_mcp
                .as_ref()
                .map_or(0, |mcp| mcp.statuses().len()),
            ..Default::default()
        }
        .with_transcript(&self.transcript);
        let field = match (&self.local_context, &self.sandbox) {
            (Some(context), Some(sandbox)) => context.request_field(sandbox),
            _ => None,
        };
        if let Some(field) = field {
            parts = parts.with_client_context(&field);
        }
        let panel = context_window::panel(self.usage.context.as_ref(), &parts);
        self.record(Item::Panel(panel));
        Vec::new()
    }

    /// `/init [focus]`: asks the model to study the folder and write or
    /// improve `GZ.md`, as a normal turn (its edits ask as usual).
    pub(super) fn init_command(&mut self, command: &str) -> Vec<Effect> {
        let focus = command.trim().strip_prefix("/init").unwrap_or("").trim();
        let mut prompt = INIT_PROMPT.to_string();
        if !focus.is_empty() {
            prompt.push_str(&format!("\n\nAlso pay attention to: {focus}"));
        }
        if self.is_busy() || self.compacting {
            return self.submit_while_busy(prompt);
        }
        vec![Effect::Send(prompt)]
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

/// One sent entry: what it is and why, then its imports and anything
/// left out of it.
fn instruction_rows(file: &Instruction) -> Vec<Row> {
    let kind = match (&file.why, file.source) {
        (Why::Environment, _) => "gz",
        (Why::Rule | Why::MatchedRule { .. }, _) => "rule",
        (_, Source::User) => "yours",
        (_, Source::Project) => "project",
    };
    let cut = if file.truncated { " · cut to fit" } else { "" };
    let mut rows = vec![Row::entry(
        kind,
        format!(
            "{} · {} bytes · {}{cut}",
            file.shown,
            file.bytes,
            file.why.describe()
        ),
    )];
    rows.extend(
        file.imports
            .iter()
            .map(|import| Row::field("  imports", import.clone())),
    );
    rows.extend(
        file.dropped
            .iter()
            .map(|why| Row::field("  skipped", why.clone())),
    );
    rows
}

#[cfg(test)]
mod tests {
    use crate::code_ui::state::test_support::*;

    use crossterm::event::KeyCode;

    use super::FolderCommand;
    use crate::assistant::local_context::Limits;
    use crate::assistant::memory::{MemoryStore, Scope};
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

    #[test]
    fn instructions_show_why_imports_skips_and_what_was_left_out() {
        let dir = temp_dir("ui-instructions-report");
        std::fs::create_dir_all(dir.join(".gz/rules")).unwrap();
        std::fs::write(dir.join("style.md"), "tabs").unwrap();
        std::fs::write(dir.join("GZ.md"), "@style.md @gone.md").unwrap();
        std::fs::write(dir.join(".gz/rules/bad.md"), "---\npaths: [\n---\n").unwrap();
        let mut s = with_catalog();
        s.sandbox = Some(Sandbox::new(&dir).unwrap());
        run(&mut s, "/instructions");
        let Some(Item::Panel(panel)) = s.flush.last() else {
            panic!("a panel");
        };
        let lines = panel.plain_lines().join("\n");
        for want in [
            "GZ.md",
            "in this folder or above",
            "style.md",
            "@gone.md: not found",
            "Not sent",
            "bad.md",
        ] {
            assert!(lines.contains(want), "{want:?} missing:\n{lines}");
        }
    }

    #[test]
    fn memory_shows_both_scopes_and_adds_edits_and_moves() {
        let dir = temp_dir("ui-memory-scopes");
        let store = MemoryStore::at(dir.join("memory.json"), "default").with_project(&dir);
        let mut s = with_catalog();
        s.sandbox = Some(Sandbox::new(&dir).unwrap());
        s.memory = Some(store.clone());
        run(&mut s, "/memory add --project Uses make test");
        run(&mut s, "/memory add Prefers pnpm");
        let project = store.list_scope(Scope::Project).unwrap();
        let everywhere = store.list_scope(Scope::Everywhere).unwrap();
        assert_eq!(project[0].text, "Uses make test");
        assert_eq!(everywhere[0].text, "Prefers pnpm");
        run(&mut s, "/memory");
        let Some(Item::Panel(panel)) = s.flush.last() else {
            panic!("a panel");
        };
        let lines = panel.plain_lines().join("\n");
        assert!(
            lines.contains("This project") && lines.contains("Everywhere"),
            "{lines}"
        );
        assert!(lines.find("Uses make test") < lines.find("Prefers pnpm"));
        run(
            &mut s,
            &format!("/memory edit {} Uses make check", project[0].id),
        );
        assert_eq!(
            store.list_scope(Scope::Project).unwrap()[0].text,
            "Uses make check"
        );
        run(&mut s, &format!("/memory move {}", everywhere[0].id));
        assert_eq!(store.list_scope(Scope::Project).unwrap().len(), 2);
        run(&mut s, "/memory clear project");
        assert_eq!(store.list().unwrap().len(), 2, "asks first");
        run(&mut s, "/memory clear project yes");
        assert!(store.list().unwrap().is_empty());
        run(&mut s, "/memory edit nope");
        assert!(matches!(s.flush.last(), Some(Item::Error(e)) if e.contains("/memory add")));

        run(&mut s, "/memory add --projectFoo");
        assert!(
            store
                .list_scope(Scope::Everywhere)
                .unwrap()
                .iter()
                .any(|m| m.text == "--projectFoo"),
            "the flag counts only as a whole word"
        );
        assert!(store.list_scope(Scope::Project).unwrap().is_empty());
        run(&mut s, "/memory add --project");
        assert!(matches!(s.flush.last(), Some(Item::Error(e)) if e.contains("--project <text>")));
    }

    #[test]
    fn context_shows_the_breakdown_and_init_sends_the_canned_turn() {
        let dir = temp_dir("ui-context");
        let mut s = with_catalog();
        s.sandbox = Some(Sandbox::new(&dir).unwrap());
        assert!(run(&mut s, "/context").is_empty());
        assert!(matches!(s.flush.last(), Some(Item::Panel(p)) if p.title == "Context"));
        let effects = run(&mut s, "/init the API layer");
        let [Effect::Send(prompt)] = effects.as_slice() else {
            panic!("{effects:?}");
        };
        assert!(prompt.starts_with("Study this folder and write GZ.md"));
        assert!(prompt.ends_with("Also pay attention to: the API layer"));
    }
}
