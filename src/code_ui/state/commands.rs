//! Slash commands: `/help`, `/usage`, `/permissions`, `/subagents` and the
//! rest, and the menu that opens when the input starts with `/`.

use crossterm::event::{KeyCode, KeyEvent};

use crate::code_ui::items::Item;
use crate::code_ui::panel::{Panel, Row};
use crate::code_ui::usage;
use crate::code_ui::PermissionsReply;

use super::{new_input, Effect, UiState};

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
        "/usage",
        "context fill, this session's tokens, cost, and tools, plus your account's usage",
    ),
    ("/last", "full analytics of the previous turn"),
    (
        "/permissions",
        "what gz may do without asking; /permissions revoke <id>",
    ),
    (
        "/subagents",
        "helper agents the chat can start; /subagents on|off",
    ),
    ("/exit", "quit (also ctrl+d, or ctrl+c twice)"),
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
    "enter send · shift/alt+enter newline · ↑↓ history",
    "esc interrupt · ctrl+u clear input · ctrl+c ctrl+c exit",
    "shift+tab cycle mode: ask → accept edits → plan (read-only)",
    "ctrl+o transcript: every step, fully expanded (esc to return)",
    "shift+↓ / shift+↑ switch between main and each subagent's page",
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

    /// `/subagents` lists the profiles; `/subagents on|off` toggles them.
    fn subagents_command(&mut self, command: &str) -> Vec<Effect> {
        match command.split_whitespace().nth(1) {
            Some("on") | Some("off") => {
                let enabled = command.split_whitespace().nth(1) == Some("on");
                self.subagents_enabled = enabled;
                let note = if enabled {
                    "Subagents on: the chat may start helper agents in parallel"
                } else {
                    "Subagents off: the chat works alone"
                };
                self.record(Item::Notice(note.into()));
                vec![Effect::SetSubagents(enabled)]
            }
            Some(other) => {
                self.record(Item::Error(format!(
                    "Unknown option {other} · use /subagents on or /subagents off"
                )));
                Vec::new()
            }
            None => {
                self.record(Item::Panel(self.subagent_overview()));
                Vec::new()
            }
        }
    }

    fn subagent_overview(&self) -> Panel {
        let (state, switch) = if self.subagents_enabled {
            ("on", "off")
        } else {
            ("off", "on")
        };
        let profiles = self
            .catalog
            .as_ref()
            .map(|c| c.subagent_profiles.as_slice())
            .unwrap_or_default();
        let mut rows: Vec<Row> = profiles
            .iter()
            .map(|profile| Row::entry(&profile.id, profile.description.clone()))
            .collect();
        if profiles.is_empty() {
            rows.push(Row::note(if self.models_loading {
                "The profile list is still loading; try again in a moment"
            } else {
                "This chat server doesn't list subagent profiles"
            }));
        }
        Panel::new("Subagents")
            .aside(state)
            .section(Some("Profiles"), rows)
            .footer(format!("/subagents {switch} to turn them {switch}"))
    }

    pub(super) fn slash(&mut self, command: &str) -> Vec<Effect> {
        match command.split_whitespace().next().unwrap_or_default() {
            "/help" => {
                self.record(Item::Panel(help_panel()));
                Vec::new()
            }
            "/subagents" => self.subagents_command(command),
            "/new" => {
                self.leave_conversation();
                self.record(Item::Notice("New conversation".into()));
                vec![Effect::NewConversation]
            }
            "/usage" => {
                self.record(Item::Panel(usage::report(&self.usage)));
                self.account_loading = true;
                vec![Effect::FetchAccountUsage]
            }
            "/permissions" => {
                let mut words = command.split_whitespace().skip(1);
                match (words.next(), words.next()) {
                    (None, _) => vec![Effect::Permissions(None)],
                    (Some("revoke"), Some(id)) => vec![Effect::Permissions(Some(id.to_string()))],
                    _ => {
                        self.record(Item::Error(
                            "Use /permissions, or /permissions revoke <id> (e.g. L1)".into(),
                        ));
                        Vec::new()
                    }
                }
            }
            "/last" => {
                match &self.usage.last {
                    Some(stats) => self.record(Item::Panel(usage::turn_details(stats))),
                    None => self.record(Item::Notice("No finished turn yet".into())),
                }
                Vec::new()
            }
            "/clear" => {
                self.leave_conversation();
                vec![Effect::ClearScreen, Effect::NewConversation]
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
    fn typing_a_slash_lists_every_command() {
        let mut s = state();
        type_text(&mut s, "/");
        assert_eq!(s.command_menu().len(), COMMANDS.len());
        type_text(&mut s, "mo");
        let names: Vec<&str> = s.command_menu().iter().map(|(n, _)| *n).collect();
        assert_eq!(names, vec!["/models", "/mode"]);
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
    fn subagents_command_lists_profiles_and_toggles() {
        let mut s = with_catalog();
        type_text(&mut s, "/subagents");
        s.on_key(key(KeyCode::Enter));
        let Some(Item::Panel(panel)) = s.flush.last() else {
            panic!("no overview");
        };
        let lines = panel.plain_lines();
        assert_eq!(lines[0], "Subagents · on");
        assert!(lines
            .iter()
            .any(|l| l.contains("analyst") && l.contains("lakehouse and SQL reads")));
        type_text(&mut s, "/subagents off");
        assert_eq!(
            s.on_key(key(KeyCode::Enter)),
            vec![Effect::SetSubagents(false)]
        );
        assert!(!s.subagents_enabled);
        type_text(&mut s, "/subagents maybe");
        assert!(s.on_key(key(KeyCode::Enter)).is_empty());
        assert!(matches!(s.flush.last(), Some(Item::Error(_))));
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
