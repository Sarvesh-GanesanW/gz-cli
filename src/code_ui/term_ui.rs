//! The interactive loop's terminal extras, kept out of the loop itself:
//! the saved terminal settings, focus reports and notifications, and the
//! `/theme`, `/notifications` and `/statusline` commands.

use std::io::stdout;
use std::time::Instant;

use crossterm::event::Event;

use crate::code_ui::approver::Mode;
use crate::code_ui::attention::{self, Attention, Moment, NotifyChoice};
use crate::code_ui::items::{self, Item};
use crate::code_ui::panel::{Panel, Row};
use crate::code_ui::state::UiState;
use crate::code_ui::status_command::{self, Payload, StatusCommand};
use crate::code_ui::term_settings::{self, TerminalSettings};
use crate::tui::theme::{self, ThemeName};

/// Longest action text a notification repeats.
const ACTION_CHARS: usize = 80;

/// A `/theme`, `/notifications` or `/statusline` command, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalCommand {
    ShowThemes,
    SetTheme(ThemeName),
    ShowNotifications,
    SetNotifications(NotifyChoice),
    ShowStatusLine,
    /// `None` turns the custom status line off.
    SetStatusLine(Option<String>),
}

impl TerminalCommand {
    /// Parses one of the commands, or says how to use it.
    pub fn parse(command: &str) -> Result<Self, String> {
        if let Some(rest) = command.trim().strip_prefix("/statusline") {
            if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
                return Err(format!(
                    "Unknown command {} · /help lists them",
                    command.trim()
                ));
            }
            return Ok(match rest.trim() {
                "" => Self::ShowStatusLine,
                "off" | "clear" => Self::SetStatusLine(None),
                shell => Self::SetStatusLine(Some(shell.to_string())),
            });
        }
        let mut words = command.split_whitespace();
        let name = words.next().unwrap_or_default();
        let argument = words.collect::<Vec<_>>().join(" ");
        match (name, argument.as_str()) {
            ("/theme", "") => Ok(Self::ShowThemes),
            ("/theme", wanted) => ThemeName::parse(wanted).map(Self::SetTheme).ok_or_else(|| {
                format!(
                    "There is no theme \"{wanted}\" · /theme {}",
                    names(ThemeName::ALL.map(ThemeName::name))
                )
            }),
            ("/notifications", "") => Ok(Self::ShowNotifications),
            ("/notifications", wanted) => NotifyChoice::parse(wanted)
                .map(Self::SetNotifications)
                .ok_or_else(|| {
                    format!(
                        "\"{wanted}\" is not a notification setting · /notifications {}",
                        names(NotifyChoice::ALL.map(NotifyChoice::name))
                    )
                }),
            (other, _) => Err(format!("Unknown command {other} · /help lists them")),
        }
    }
}

fn names<const N: usize>(names: [&str; N]) -> String {
    names.join("|")
}

/// Focus reports and notifications for one session.
pub struct TermUi {
    settings: TerminalSettings,
    attention: Attention,
    status: StatusCommand,
    branch: status_command::BranchCache,
    /// Desktop notifiers report back here: why one failed, if it did.
    desktop_tx: tokio::sync::mpsc::UnboundedSender<Result<(), String>>,
    desktop_rx: tokio::sync::mpsc::UnboundedReceiver<Result<(), String>>,
    /// A desktop notification failure is reported once per session.
    desktop_failure_shown: bool,
}

impl TermUi {
    /// Loads the saved settings and turns their theme on; a broken settings
    /// file is reported and the defaults used.
    pub fn start(state: &mut UiState) -> Self {
        let settings = TerminalSettings::load().unwrap_or_else(|err| {
            state.record(Item::Error(format!(
                "cannot read the terminal settings: {err:#}"
            )));
            TerminalSettings::default()
        });
        theme::set_active(settings.theme);
        let attention = Attention::new(settings.notifications);
        let (desktop_tx, desktop_rx) = tokio::sync::mpsc::unbounded_channel();
        // A bare run runs none of the user's shell commands.
        let status_line = settings.status_line.clone().filter(|_| !state.is_bare);
        let status = StatusCommand::new(status_line);
        Self {
            status,
            branch: status_command::BranchCache::default(),
            settings,
            attention,
            desktop_tx,
            desktop_rx,
            desktop_failure_shown: false,
        }
    }

    /// A terminal event the loop doesn't handle itself (focus).
    pub fn on_event(&mut self, event: &Event) {
        match event {
            Event::FocusGained => self.attention.focus(true),
            Event::FocusLost => self.attention.focus(false),
            Event::Key(_) | Event::Paste(_) => self.attention.key_pressed(Instant::now()),
            _ => {}
        }
    }

    /// Once per frame: notifies when a turn ended or gz needs you while you
    /// look away, and reports a desktop notifier that failed.
    pub fn frame(&mut self, state: &mut UiState, conversation_id: &str) {
        self.refresh_status_line(state, conversation_id);
        if let Some(alert) = self.attention.observe(moment(state), Instant::now()) {
            if let Some(notice) = attention::send(self.attention.choice(), alert, &mut stdout()) {
                let tx = self.desktop_tx.clone();
                tokio::spawn(async move {
                    tx.send(crate::notify::send_desktop(notice).await).ok();
                });
            }
        }
        self.collect_desktop_results(state);
    }

    fn refresh_status_line(&mut self, state: &mut UiState, conversation_id: &str) {
        if self.status.command().is_none() {
            state.custom_status = None;
            return;
        }
        let cwd = state
            .sandbox
            .as_ref()
            .map(|sandbox| sandbox.cwd())
            .unwrap_or_default();
        let now = Instant::now();
        let branch = self.branch.get(&cwd, now);
        let payload = status_payload(state, &cwd, conversation_id, branch);
        if self.status.tick(payload, &cwd, now) {
            state.custom_status = self.status.shown().cloned();
        }
    }

    fn collect_desktop_results(&mut self, state: &mut UiState) {
        while let Ok(result) = self.desktop_rx.try_recv() {
            let Err(why) = result else {
                continue;
            };
            if !std::mem::replace(&mut self.desktop_failure_shown, true) {
                state.record(Item::Error(format!(
                    "no desktop notification: {why} · /notifications bell or osc uses the terminal instead"
                )));
            }
        }
    }

    /// Runs a parsed terminal command; what it changed is saved for the
    /// next session.
    pub fn apply(&mut self, command: TerminalCommand, state: &mut UiState) {
        match command {
            TerminalCommand::ShowThemes => {
                state.record(Item::Panel(themes_panel(self.settings.theme)))
            }
            TerminalCommand::SetTheme(name) => {
                theme::set_active(name);
                self.settings.theme = name;
                self.save(state, &format!("Theme: {}", name.name()));
            }
            TerminalCommand::ShowNotifications => state.record(Item::Panel(notifications_panel(
                self.settings.notifications,
            ))),
            TerminalCommand::ShowStatusLine => state.record(Item::Notice(
                match self.status.command() {
                    Some(command) => format!(
                        "Status line: {command} · /statusline <command> changes it, /statusline off removes it"
                    ),
                    None => "No custom status line · /statusline <command> shows the first line the command prints (it gets the session as JSON on stdin)".into(),
                },
            )),
            TerminalCommand::SetStatusLine(command) => {
                let done = match &command {
                    Some(command) => format!("Status line: {command}"),
                    None => "Status line: off".into(),
                };
                self.status.set(command.clone());
                state.custom_status = None;
                self.settings.status_line = command;
                self.save(state, &done);
            }
            TerminalCommand::SetNotifications(choice) => {
                self.attention.set_choice(choice);
                self.settings.notifications = choice;
                self.save(state, &format!("Notifications: {}", choice.describe()));
            }
        }
    }

    fn save(&self, state: &mut UiState, done: &str) {
        match self.settings.save() {
            Ok(()) => state.record(Item::Notice(done.to_string())),
            Err(err) => state.record(Item::Error(format!(
                "{done}, for this session only: cannot save {}: {err:#}",
                term_settings::path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "the terminal settings".into())
            ))),
        }
    }
}

/// Asks the terminal to report focus changes while gz code runs; most
/// terminals do, and the rest ignore the request.
pub struct FocusReports;

impl FocusReports {
    pub fn enable() -> Self {
        crate::inline_term::set_focus_reports(true);
        Self
    }
}

impl Drop for FocusReports {
    fn drop(&mut self) {
        crate::inline_term::set_focus_reports(false);
    }
}

/// What the custom status line command is told.
fn status_payload(
    state: &UiState,
    cwd: &std::path::Path,
    conversation_id: &str,
    git_branch: Option<String>,
) -> Payload {
    let context_percent = state.usage.context.as_ref().and_then(|context| {
        let tokens = context.tokens?;
        let window = context.window.max(1);
        Some((tokens.saturating_mul(100) / window).min(100) as u8)
    });
    Payload {
        model: state.model_label(),
        cwd: cwd.display().to_string(),
        conversation_id: conversation_id.to_string(),
        cost_usd: state.spend.meter.guard.session_usd(),
        context_percent,
        git_branch,
        mode: mode_key(state.mode()).into(),
        version: env!("CARGO_PKG_VERSION"),
    }
}

fn mode_key(mode: Mode) -> &'static str {
    match mode {
        Mode::Ask => "ask",
        Mode::AcceptEdits => "accept-edits",
        Mode::Plan => "plan",
        Mode::Auto => "auto",
        Mode::DontAsk => "dont-ask",
    }
}

/// What gz is waiting on this frame.
fn moment(state: &UiState) -> Moment {
    let needs_you = if let Some(prompt) = &state.prompt {
        Some(format!(
            "approve {}",
            items::fit(&prompt.request.summary, ACTION_CHARS)
        ))
    } else {
        state
            .query
            .as_ref()
            .map(|_| "answer a question".to_string())
    };
    Moment {
        busy: state.is_busy(),
        needs_you,
    }
}

fn themes_panel(current: ThemeName) -> Panel {
    let rows = ThemeName::ALL
        .iter()
        .map(|theme| {
            let mark = if *theme == current { " (on)" } else { "" };
            Row::entry(theme.name(), format!("{}{mark}", theme.describe()))
        })
        .collect();
    Panel::new("Themes")
        .section(None, rows)
        .footer("/theme <name> switches and remembers it (gz tui too)")
}

fn notifications_panel(current: NotifyChoice) -> Panel {
    let rows = NotifyChoice::ALL
        .iter()
        .map(|choice| {
            let mark = if *choice == current { " (on)" } else { "" };
            Row::entry(choice.name(), format!("{}{mark}", choice.describe()))
        })
        .collect();
    Panel::new("Notifications").section(None, rows).footer(
        "When a turn ends or gz needs you while the terminal is in the background \
             (or after a few idle seconds where the terminal doesn't report focus) · \
             /notifications <setting>",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_parse_and_explain_bad_values() {
        assert_eq!(
            TerminalCommand::parse("/theme"),
            Ok(TerminalCommand::ShowThemes)
        );
        assert_eq!(
            TerminalCommand::parse("/theme light-daltonized"),
            Ok(TerminalCommand::SetTheme(ThemeName::LightDaltonized))
        );
        let err = TerminalCommand::parse("/theme neon").unwrap_err();
        assert!(
            err.contains("dark|light|dark-daltonized|light-daltonized|ansi"),
            "{err}"
        );
        assert_eq!(
            TerminalCommand::parse("/notifications off"),
            Ok(TerminalCommand::SetNotifications(NotifyChoice::Off))
        );
        assert!(TerminalCommand::parse("/notifications loud")
            .unwrap_err()
            .contains("auto|bell|osc|desktop|off"));
    }

    #[test]
    fn the_slash_commands_reach_the_driver_or_explain_themselves() {
        use crate::code_ui::state::test_support::{key, state, type_text};
        use crate::code_ui::state::Effect;
        use crossterm::event::KeyCode;
        let mut s = state();
        type_text(&mut s, "/theme ansi");
        let effects = s.on_key(key(KeyCode::Enter));
        assert!(
            effects.contains(&Effect::Terminal(TerminalCommand::SetTheme(
                ThemeName::Ansi
            ))),
            "{effects:?}"
        );
        type_text(&mut s, "/notifications sometimes");
        assert!(s.on_key(key(KeyCode::Enter)).is_empty());
        assert!(matches!(s.flush.last(), Some(Item::Error(why)) if why.contains("auto|bell")));
    }

    #[test]
    fn statusline_keeps_the_command_as_typed() {
        assert_eq!(
            TerminalCommand::parse("/statusline"),
            Ok(TerminalCommand::ShowStatusLine)
        );
        assert_eq!(
            TerminalCommand::parse("/statusline  ~/bin/line --short  'a  b'"),
            Ok(TerminalCommand::SetStatusLine(Some(
                "~/bin/line --short  'a  b'".into()
            )))
        );
        assert_eq!(
            TerminalCommand::parse("/statusline off"),
            Ok(TerminalCommand::SetStatusLine(None))
        );
        assert!(TerminalCommand::parse("/statuslinex").is_err());
    }

    #[test]
    fn a_waiting_prompt_is_named_in_the_moment() {
        let mut state = crate::code_ui::state::test_support::state();
        assert_eq!(moment(&state).needs_you, None);
        state.start_turn("fix it");
        assert!(moment(&state).busy);
    }
}
