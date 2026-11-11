//! Slash commands: `/help`, `/usage`, `/permissions` and the rest (each
//! bigger one in its own module), and the menu that opens when the input
//! starts with `/`.

use crossterm::event::{KeyCode, KeyEvent};

use crate::code_ui::items::Item;
use crate::code_ui::panel::{Panel, Row};
use crate::code_ui::skills::{self, SkillsView};
use crate::code_ui::usage;
use crate::code_ui::PermissionsReply;

use super::{new_input, Effect, Picker, UiState};

/// Every slash command with its one-line description: the source for both
/// `/help` and the menu that opens when the input starts with `/`.
pub const COMMANDS: &[(&str, &str)] = &[
    ("/help", "show this help"),
    ("/new", "start a new conversation"),
    ("/clear", "new conversation and a clean screen"),
    ("/resume", "continue an earlier conversation"),
    (
        "/models",
        "choose the model (Groundzero modes or your LLM connections)",
    ),
    (
        "/effort",
        "choose the reasoning effort for the current model",
    ),
    ("/mode", "cycle permission mode (also shift+tab)"),
    (
        "/auto",
        "auto mode: your rules and recent blocks; /auto allow|deny|env <rule>, /auto on|off",
    ),
    (
        "/allow",
        "run an action auto mode blocked, once; /allow <n> picks one from /auto",
    ),
    (
        "/usage",
        "context fill, this session's tokens, cost, and tools, plus your account's usage",
    ),
    ("/last", "full analytics of the previous turn"),
    (
        "/budget",
        "spend, your caps and the Groundzero budget; /budget session|day <usd>|off, /budget off",
    ),
    (
        "/compact",
        "summarize the conversation so far to free context; /compact <what to keep>",
    ),
    (
        "/autocompact",
        "when the chat compacts by itself; /autocompact 200k|300k|…|default",
    ),
    (
        "/permissions",
        "what gz may do without asking; /permissions allow <what>, /permissions revoke <id>",
    ),
    (
        "/subagents",
        "helper agents: /subagents model|effort|context|max|parallel <value>, reset, on|off|stop",
    ),
    (
        "/cd",
        "switch gz to another folder; /cd <path> (~ and relative paths work)",
    ),
    ("/add-dir", "let gz work in another folder too; /add-dir <path>"),
    ("/dirs", "the open folders and the current one; /dirs remove <n|path>"),
    (
        "/memory",
        "what gz remembers about you; /memory forget <id>, clear, on|off",
    ),
    (
        "/instructions",
        "instruction files in use (instructions.md, GZ.md, AGENTS.md, CLAUDE.md); /instructions edit",
    ),
    (
        "/skills",
        "skills: on/off list; /skills install|update|remove|enable|disable|folders",
    ),
    (
        "/shells",
        "background commands: list; /shells <id> shows output, /shells stop <id|all>",
    ),
    (
        "/rewind",
        "put files gz changed back as they were before one of your messages",
    ),
    (
        "/diff",
        "every file gz changed in this conversation, as a diff; /diff <path> for one",
    ),
    ("/exit", "quit (also ctrl+d, or ctrl+c twice)"),
    (
        "/export",
        "save the conversation as Markdown; /export <path>, /export clipboard",
    ),
    (
        "/hooks",
        "your commands around tool calls: list; /hooks off|on, /hooks trust",
    ),
];

fn help_panel() -> Panel {
    let rows = COMMANDS
        .iter()
        .map(|(name, what)| Row::entry(name, *what))
        .collect();
    Panel::new("Commands")
        .aside("gz code")
        .section(None, rows)
        .footer("? for keyboard shortcuts · @ to attach files · esc to interrupt")
}

pub const SHORTCUTS: &[&str] = &[
    "enter send (while gz works: it reads it at its next step) · shift/alt+enter newline · ↑↓ history",
    "↑ takes back messages not yet sent · ctrl+enter (or ctrl+x ctrl+s) stops and sends now",
    "esc interrupt · ctrl+u clear input · ctrl+c ctrl+c exit",
    "shift+tab cycle mode: ask → auto → accept edits → plan (read-only)",
    "ctrl+o transcript: every step, fully expanded (esc to return)",
    "shift+↓ / shift+↑ switch between main and each subagent's page",
    "! runs a shell command yourself (esc leaves it running in the background) · /shells lists them",
    "/help commands",
];

impl UiState {
    /// The `/permissions` panel (after what was revoked), or why it
    /// couldn't load.
    pub fn show_permissions(&mut self, reply: PermissionsReply) {
        if let Some(revoked) = reply.revoked {
            self.record(Item::Notice(format!("✓ {revoked}")));
        }
        match reply.panel {
            Ok(panel) => self.record(Item::Panel(panel)),
            Err(why) => self.record(Item::Error(why)),
        }
    }

    /// What a `/skills` action did: a notice, the panel, and the on/off
    /// list to open.
    pub fn show_skills(&mut self, view: Result<SkillsView, String>) {
        let view = match view {
            Ok(view) => view,
            Err(why) => return self.record(Item::Error(why)),
        };
        if let Some(notice) = view.notice {
            self.record(Item::Notice(notice));
        }
        if let Some(panel) = view.panel {
            self.record(Item::Panel(panel));
        }
        if let Some(rows) = view.toggles {
            self.picker = Some(Picker {
                title: "Skills · enter switches on/off".into(),
                rows,
                selected: 0,
                model: None,
            });
        }
    }

    /// Sets the skills `@name` may ask for, rescanning the `@` index.
    pub fn set_skill_names(&mut self, names: Vec<String>) {
        if names != self.skill_names {
            self.skill_names = names;
            self.files = None;
        }
    }

    /// The account panel for `/usage`, or why it couldn't load.
    pub fn show_account_usage(&mut self, panel: Result<Panel, String>) {
        self.account_loading = false;
        match panel {
            Ok(panel) => self.record(Item::Panel(panel)),
            Err(why) => self.record(Item::Error(format!(
                "couldn't load your account's usage: {why}"
            ))),
        }
    }

    pub(super) fn slash(&mut self, command: &str) -> Vec<Effect> {
        match command.split_whitespace().next().unwrap_or_default() {
            "/help" => {
                self.record(Item::Panel(help_panel()));
                Vec::new()
            }
            "/subagents" => self.subagents_command(command),
            "/auto" => self.auto_command(command),
            "/allow" => self.allow_command(command),
            "/skills" => match skills::parse(command) {
                Ok(action) => vec![Effect::Skills(action)],
                Err(usage) => {
                    self.record(Item::Error(usage));
                    Vec::new()
                }
            },
            "/compact" => self.compact_command(command),
            "/budget" => self.budget_command(command),
            "/cd" | "/add-dir" | "/dirs" => self.folder_command(command),
            "/memory" => self.memory_command(command),
            "/instructions" => self.instructions_command(command),
            "/autocompact" => self.auto_compact_command(command),
            "/shells" => self.shells_command(command),
            "/export" => self.export_command(command),
            "/rewind" => self.rewind_command(),
            "/diff" => self.diff_command(command),
            "/hooks" => self.hooks_command(command),
            "/new" => {
                let mut effects = self.leave_conversation();
                self.record(Item::Notice("New conversation".into()));
                effects.push(Effect::NewConversation);
                effects
            }
            "/usage" => {
                self.record(Item::Panel(usage::report(&self.usage)));
                self.account_loading = true;
                vec![Effect::FetchAccountUsage]
            }
            "/permissions" => self.permissions_command(command),
            "/last" => {
                match &self.usage.last {
                    Some(stats) => self.record(Item::Panel(usage::turn_details(stats))),
                    None => self.record(Item::Notice("No finished turn yet".into())),
                }
                Vec::new()
            }
            "/clear" => {
                let mut effects = self.leave_conversation();
                effects.extend([Effect::ClearScreen, Effect::NewConversation]);
                effects
            }
            "/resume" | "/continue" => {
                if self.is_busy() {
                    self.record(Item::Error(
                        "Wait for the answer to finish (or esc) before resuming another conversation"
                            .into(),
                    ));
                    return Vec::new();
                }
                vec![Effect::ListConversations]
            }
            "/exit" | "/quit" => vec![Effect::Exit],
            "/models" | "/model" => {
                self.open_model_picker();
                Vec::new()
            }
            "/effort" => {
                let current = self
                    .model
                    .clone()
                    .or_else(|| self.catalog.as_ref()?.default_selection());
                match current {
                    Some(selection) if !selection.model.efforts().is_empty() => {
                        self.open_effort_picker(selection.model)
                    }
                    _ => self.record(Item::Error(
                        "This model has no reasoning-effort levels".into(),
                    )),
                }
                Vec::new()
            }
            "/mode" => {
                self.cycle_mode();
                let name = self.mode().banner().unwrap_or("ask before changes");
                self.record(Item::Notice(format!("Mode: {name}")));
                Vec::new()
            }
            other => {
                self.record(Item::Error(format!(
                    "Unknown command {other} · /help lists them"
                )));
                Vec::new()
            }
        }
    }

    /// Commands matching what's typed while the input is a lone `/word`:
    /// prefix matches first, then ones that merely contain it.
    pub fn command_menu(&self) -> Vec<(&'static str, &'static str)> {
        let text = self.input_text();
        let typed = text.trim_end();
        if !typed.starts_with('/')
            || typed.contains(char::is_whitespace)
            || self.input.lines().len() > 1
        {
            return Vec::new();
        }
        let needle = typed.to_lowercase();
        let (mut prefix, mut contains): (Vec<_>, Vec<_>) = (Vec::new(), Vec::new());
        for &(name, what) in COMMANDS {
            if name.starts_with(&needle) {
                prefix.push((name, what));
            } else if name.contains(needle.trim_start_matches('/')) {
                contains.push((name, what));
            }
        }
        prefix.extend(contains);
        prefix
    }

    /// Keys while the slash menu is showing; `None` means "not handled here".
    pub(super) fn on_menu_key(&mut self, key: KeyEvent) -> Option<Vec<Effect>> {
        let menu = self.command_menu();
        if menu.is_empty() {
            return None;
        }
        let selected = self.menu_selected.min(menu.len() - 1);
        match key.code {
            KeyCode::Up => {
                self.menu_selected = selected.checked_sub(1).unwrap_or(menu.len() - 1);
                Some(Vec::new())
            }
            KeyCode::Down => {
                self.menu_selected = (selected + 1) % menu.len();
                Some(Vec::new())
            }
            KeyCode::Tab => {
                self.set_input(menu[selected].0);
                Some(Vec::new())
            }
            KeyCode::Enter if key.modifiers.is_empty() => {
                let typed = self.input_text().trim().to_string();
                let exact = menu.iter().any(|(name, _)| *name == typed);
                if !exact {
                    self.set_input(menu[selected].0);
                }
                Some(self.submit())
            }
            KeyCode::Esc => {
                self.input = new_input();
                Some(Vec::new())
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code_ui::state::test_support::*;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use crate::code_ui::items::Item;

    #[test]
    fn permissions_command_lists_and_revokes() {
        let mut s = state();
        type_text(&mut s, "/permissions");
        assert_eq!(
            s.on_key(key(KeyCode::Enter)),
            vec![Effect::Permissions(None)]
        );
        type_text(&mut s, "/permissions revoke l2");
        assert_eq!(
            s.on_key(key(KeyCode::Enter)),
            vec![Effect::Permissions(Some("l2".into()))]
        );
        type_text(&mut s, "/permissions nope");
        assert!(s.on_key(key(KeyCode::Enter)).is_empty());
        assert!(matches!(s.flush.last(), Some(Item::Error(e)) if e.contains("revoke <id>")));
    }

    #[test]
    fn skills_command_opens_toggles_that_switch_skills() {
        use crate::code_ui::skills::SkillsAction;
        use crate::code_ui::state::PickerRow;
        let mut s = state();
        type_text(&mut s, "/skills");
        assert_eq!(
            s.on_key(key(KeyCode::Enter)),
            vec![Effect::Skills(SkillsAction::Show)]
        );
        s.show_skills(Ok(SkillsView {
            toggles: Some(vec![PickerRow::Skill {
                name: "pdf".into(),
                on: true,
                detail: "PDF forms".into(),
                needs_trust: false,
            }]),
            ..SkillsView::default()
        }));
        let off = s.on_key(key(KeyCode::Enter));
        assert_eq!(
            off,
            vec![Effect::Skills(SkillsAction::SetEnabled {
                name: "pdf".into(),
                on: false
            })]
        );
        assert!(
            matches!(
                &s.picker.as_ref().unwrap().rows[0],
                PickerRow::Skill { on: false, .. }
            ),
            "the list stays open, showing the new state"
        );
        let on = s.on_key(key(KeyCode::Char(' ')));
        assert!(matches!(
            &on[0],
            Effect::Skills(SkillsAction::SetEnabled { on: true, .. })
        ));
        s.on_key(key(KeyCode::Esc));
        type_text(&mut s, "/skills bogus");
        assert!(s.on_key(key(KeyCode::Enter)).is_empty());
        assert!(matches!(s.flush.last(), Some(Item::Error(e)) if e.contains("/skills install")));
    }

    #[test]
    fn typing_a_slash_lists_every_command() {
        let mut s = state();
        type_text(&mut s, "/");
        assert_eq!(s.command_menu().len(), COMMANDS.len());
        type_text(&mut s, "mo");
        let names: Vec<&str> = s.command_menu().iter().map(|(n, _)| *n).collect();
        assert_eq!(names, vec!["/models", "/mode", "/memory"]);
        type_text(&mut s, " x");
        assert!(s.command_menu().is_empty(), "arguments close the menu");
    }

    #[test]
    fn menu_keys_move_complete_and_run() {
        let mut s = state();
        type_text(&mut s, "/");
        s.on_key(key(KeyCode::Down));
        assert_eq!(s.menu_selected, 1);
        s.on_key(key(KeyCode::Tab));
        assert_eq!(s.input_text(), "/new");
        let mut s = with_catalog();
        type_text(&mut s, "/mod");
        let effects = s.on_key(key(KeyCode::Enter));
        assert!(
            effects.is_empty() && s.picker.is_some(),
            "Enter runs the highlighted /models"
        );
        let mut s = state();
        type_text(&mut s, "/ex");
        s.on_key(key(KeyCode::Esc));
        assert!(s.input_text().is_empty());
    }

    #[test]
    fn slash_commands() {
        let mut s = state();
        type_text(&mut s, "/clear");
        assert_eq!(
            s.on_key(key(KeyCode::Enter)),
            vec![Effect::ClearScreen, Effect::NewConversation]
        );
        type_text(&mut s, "/nope");
        s.on_key(key(KeyCode::Enter));
        assert!(matches!(s.flush.last(), Some(Item::Error(_))));
        assert_eq!(
            s.on_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)),
            vec![Effect::Exit]
        );
    }

    #[test]
    fn usage_command_shows_the_session_and_fetches_the_account() {
        let mut s = state();
        type_text(&mut s, "/usage");
        assert_eq!(
            s.on_key(key(KeyCode::Enter)),
            vec![Effect::FetchAccountUsage]
        );
        assert!(matches!(&s.flush[0], Item::Panel(panel) if panel.title == "This session"));
        assert!(s.account_loading, "the live area shows it's loading");
        s.show_account_usage(Err("offline".into()));
        assert!(!s.account_loading);
        assert!(matches!(s.flush.last(), Some(Item::Error(e)) if e.contains("offline")));
    }
}
