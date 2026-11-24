//! `/terminal-setup`: shows what Shift+Enter needs in this terminal and,
//! for a settings file gz can change, the exact change, asking first.

use tokio::sync::oneshot;

use crate::assistant::questions::{Choice, Query, Question, Reply};
use crate::code_ui::approver::UiQuery;
use crate::code_ui::items::Item;
use crate::code_ui::panel::{Panel, Row};
use crate::code_ui::terminal_setup::{self, FileEdit, Places, Plan, Terminal};

use super::{Effect, UiState};

const YES: &str = "Yes, back it up and change it";
const NO: &str = "No, leave it";

/// A change waiting for the user's answer.
pub struct PendingSetup {
    edit: FileEdit,
    answer: oneshot::Receiver<Reply>,
}

impl UiState {
    pub(super) fn terminal_setup_command(&mut self) -> Vec<Effect> {
        let Some(places) = Places::from_env() else {
            self.record(Item::Error(
                "cannot find your home folder, so gz can't find the terminal's settings; set HOME and try again".into(),
            ));
            return Vec::new();
        };
        let terminal = Terminal::detect(&|name| std::env::var(name).ok());
        self.run_terminal_setup(terminal, &places);
        Vec::new()
    }

    /// Shows the plan for `terminal`; an edit waits for the user's yes.
    pub(super) fn run_terminal_setup(&mut self, terminal: Terminal, places: &Places) {
        let plan = match terminal_setup::plan(terminal, places) {
            Ok(plan) => plan,
            Err(err) => return self.record(Item::Error(format!("{err:#}; nothing was changed"))),
        };
        match plan {
            Plan::Works { terminal, why } => self.record(Item::Notice(format!(
                "Shift+Enter already adds a newline in {}: {why}. {}",
                terminal.name(),
                terminal_setup::ALWAYS_WORKS
            ))),
            Plan::Configured { terminal, path } => self.record(Item::Notice(format!(
                "{} is already set up: {} sends Shift+Enter as a newline. {}",
                terminal.name(),
                path.display(),
                terminal_setup::ALWAYS_WORKS
            ))),
            Plan::Manual { terminal, steps } => {
                let rows = steps
                    .iter()
                    .flat_map(|step| step.lines().map(Row::note).collect::<Vec<_>>())
                    .collect();
                self.record(Item::Panel(
                    Panel::new("Terminal setup")
                        .aside(terminal.name())
                        .section(Some("Shift+Enter needs a change only you can make"), rows)
                        .footer(terminal_setup::ALWAYS_WORKS),
                ));
            }
            Plan::Edit(edit) => self.ask_to_apply(edit),
        }
    }

    fn ask_to_apply(&mut self, edit: FileEdit) {
        let shown = edit.path.display().to_string();
        let backup = match edit.before {
            Some(_) => format!(
                "gz first copies it to {}",
                edit.backup_path("<date-time>").display()
            ),
            None => "the file is new".to_string(),
        };
        let rows = edit.diff().into_iter().map(Row::note).collect();
        self.record(Item::Panel(
            Panel::new("Terminal setup")
                .aside(edit.terminal.name())
                .section(Some(&format!("Change to {shown}")), rows)
                .footer(format!(
                    "Shift+Enter will send Esc+Return, which gz reads as a newline; {backup}"
                )),
        ));
        let (reply, answer) = oneshot::channel();
        let question = Question {
            question: format!("Make this change to {shown}?"),
            header: "Terminal setup".into(),
            options: [YES, NO]
                .iter()
                .map(|label| Choice {
                    label: label.to_string(),
                    description: None,
                })
                .collect(),
            multi_select: false,
        };
        self.open_query(UiQuery {
            query: Query::Questions(vec![question]),
            reply,
        });
        self.terminal_setup = Some(PendingSetup { edit, answer });
    }

    /// Applies the waiting change once the user said yes (checked every
    /// tick; the answer comes through the question box).
    pub(super) fn poll_terminal_setup(&mut self) {
        let Some(pending) = self.terminal_setup.as_mut() else {
            return;
        };
        let reply = match pending.answer.try_recv() {
            Ok(reply) => Some(reply),
            Err(oneshot::error::TryRecvError::Empty) => return,
            Err(oneshot::error::TryRecvError::Closed) => None,
        };
        let Some(PendingSetup { edit, .. }) = self.terminal_setup.take() else {
            return;
        };
        let agreed = matches!(&reply, Some(Reply::Answers(answers))
            if answers.first().and_then(|a| a.as_deref()) == Some(YES));
        if !agreed {
            return self.record(Item::Notice(format!(
                "Left {} unchanged. {}",
                edit.path.display(),
                terminal_setup::ALWAYS_WORKS
            )));
        }
        let stamp = backup_stamp();
        match edit.apply(&stamp) {
            Ok(backup) => {
                let kept = backup
                    .map(|b| format!(" (backup: {})", b.display()))
                    .unwrap_or_default();
                self.record(Item::Notice(format!(
                    "✓ Changed {}{kept}. Shift+Enter adds a newline in {} from its next window or reload.",
                    edit.path.display(),
                    edit.terminal.name()
                )));
            }
            Err(err) => self.record(Item::Error(format!("{err:#}"))),
        }
    }
}

/// `YYYYMMDD-HHMMSS` (UTC) for backup names.
fn backup_stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let (days, rest) = (secs / 86_400, secs % 86_400);
    let (year, month, day) = crate::cmd::civil::civil_from_days(days as i64);
    format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code_ui::state::test_support::*;
    use crossterm::event::KeyCode;

    fn places(name: &str) -> Places {
        let home = crate::assistant::sandbox::tests::temp_dir(&format!("setup-cmd-{name}"));
        Places {
            xdg_config: Some(home.join(".config")),
            app_data: None,
            home,
        }
    }

    fn alacritty_file(places: &Places) -> std::path::PathBuf {
        let path = places
            .xdg_config
            .clone()
            .unwrap()
            .join("alacritty/alacritty.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "[font]\nsize = 12\n").unwrap();
        path
    }

    #[test]
    fn the_change_is_shown_and_made_only_after_yes() {
        let places = places("yes");
        let path = alacritty_file(&places);
        let mut s = state();
        s.run_terminal_setup(Terminal::Alacritty, &places);
        let shown = format!("{:?}", s.flush);
        assert!(shown.contains("+[[keyboard.bindings]]"), "{shown}");
        assert!(s.query.is_some(), "asks first");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[font]\nsize = 12\n"
        );
        s.on_key(key(KeyCode::Enter));
        s.on_tick();
        let changed = std::fs::read_to_string(&path).unwrap();
        assert!(changed.contains("mods = \"Shift\""), "{changed}");
        assert!(matches!(s.flush.last(), Some(Item::Notice(n)) if n.contains("backup")));
        let backups = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains("gz-backup-")
            })
            .count();
        assert_eq!(backups, 1);
    }

    #[test]
    fn no_leaves_the_file_alone() {
        let places = places("no");
        let path = alacritty_file(&places);
        let mut s = state();
        s.run_terminal_setup(Terminal::Alacritty, &places);
        s.on_key(key(KeyCode::Down));
        s.on_key(key(KeyCode::Enter));
        s.on_tick();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[font]\nsize = 12\n"
        );
        assert!(matches!(s.flush.last(), Some(Item::Notice(n)) if n.contains("unchanged")));
        assert!(s.terminal_setup.is_none());
    }

    #[test]
    fn manual_terminals_print_their_steps() {
        let mut s = state();
        s.run_terminal_setup(Terminal::ITerm2, &places("manual"));
        let shown = format!("{:?}", s.flush);
        assert!(shown.contains("0x1b 0x0d"), "{shown}");
        assert!(s.query.is_none());
    }

    #[test]
    fn the_command_is_in_the_menu_and_help() {
        let mut s = state();
        type_text(&mut s, "/terminal-se");
        assert_eq!(s.command_menu()[0].0, "/terminal-setup");
    }

    #[test]
    fn backup_stamps_are_date_and_time() {
        let stamp = backup_stamp();
        assert_eq!(stamp.len(), "20261125-010203".len(), "{stamp}");
        assert!(stamp.starts_with("20"), "{stamp}");
    }
}
