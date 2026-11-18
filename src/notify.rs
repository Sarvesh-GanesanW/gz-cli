//! Desktop notifications when something gz waited on ends: a followed run,
//! a `gz jobs try`, a `gz quality check`.
//!
//! Each platform's own notifier runs with the text as data, never as code:
//! - macOS: `osascript` with a fixed `on run argv` script and the title and
//!   message as its arguments;
//! - Linux: `notify-send` (when it is on PATH and a display is set);
//! - Windows: PowerShell's built-in WinRT toast API, with the text read from
//!   environment variables and set as XML text nodes.
//!
//! When none of them works gz says so once and rings the terminal bell,
//! adding an OSC 9 or OSC 777 notification for terminals known to show one.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use clap::Args;

use crate::cmd::Runtime;
use crate::output;

/// How long a notifier may take before gz gives up on it.
const NOTIFIER_TIMEOUT: Duration = Duration::from_secs(10);
/// Notification text past this is cut; desktops truncate long text anyway.
const MAX_TEXT_CHARS: usize = 240;
const TITLE_ENV: &str = "GZ_NOTIFY_TITLE";
const MESSAGE_ENV: &str = "GZ_NOTIFY_MESSAGE";
/// The AppleScript run with the title and message as `argv`.
const APPLESCRIPT_LINES: [&str; 3] = [
    "on run argv",
    "display notification (item 2 of argv) with title (item 1 of argv)",
    "end run",
];
/// PowerShell's own app id, which Windows lets show toasts without an
/// installed shortcut.
const POWERSHELL_APP_ID: &str =
    r"{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\WindowsPowerShell\v1.0\powershell.exe";

/// Said once per process, so a long session isn't told on every ending.
static FALLBACK_EXPLAINED: AtomicBool = AtomicBool::new(false);

/// `--notify` / `--no-notify`; without either the profile's `notify`
/// setting decides (`gz configure --notify on`).
#[derive(Args, Debug, Clone, Copy, Default)]
pub struct NotifyArgs {
    #[arg(
        long,
        overrides_with = "no_notify",
        help = "Show a desktop notification when it ends (default: the profile's `gz configure --notify`)"
    )]
    pub notify: bool,
    #[arg(long, overrides_with = "notify", help = "No desktop notification")]
    pub no_notify: bool,
}

impl NotifyArgs {
    /// `None` when neither flag was given.
    pub fn choice(&self) -> Option<bool> {
        match (self.notify, self.no_notify) {
            (true, _) => Some(true),
            (_, true) => Some(false),
            _ => None,
        }
    }
}

/// `gz configure --notify on|off`.
pub fn parse_on_off(text: &str) -> Result<bool, String> {
    match text.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" => Ok(true),
        "off" | "false" | "no" => Ok(false),
        other => Err(format!("{other:?} is neither on nor off")),
    }
}

/// Whether to notify: the flag when given, else the profile's setting.
pub fn wanted(rt: &Runtime, choice: Option<bool>) -> bool {
    choice
        .or(rt.http.resolved().profile.notify)
        .unwrap_or(false)
}

/// What ended and how, in a few words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    /// A fixed label of the command, e.g. "gz jobs try"; never user text,
    /// so no notifier can read it as an option.
    pub title: &'static str,
    pub message: String,
}

/// Shows `notice` when `enabled`. Never fails the command: a notifier that
/// doesn't work falls back to the terminal.
pub async fn send_if(enabled: bool, notice: Notice) {
    if !enabled {
        return;
    }
    let notice = Notice {
        title: notice.title,
        message: clean_text(&notice.message),
    };
    let host = Host::current();
    let failure = match desktop_command(&host, &notice) {
        Ok(command) => run_notifier(&command).await.err(),
        Err(why) => Some(why),
    };
    if let Some(why) = failure {
        terminal_fallback(&host, &notice, &why);
    }
}

// ---------- the platform notifier ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Platform {
    MacOs,
    Linux,
    Windows,
    Other,
}

/// What gz knows about the machine and terminal, read once from the
/// environment so the commands can be built (and tested) without it.
#[derive(Debug, Clone)]
struct Host {
    platform: Platform,
    /// `notify-send` on PATH, if any.
    notify_send: Option<PathBuf>,
    has_display: bool,
    term_program: String,
    term: String,
    in_tmux: bool,
}

impl Host {
    fn current() -> Self {
        let platform = if cfg!(target_os = "macos") {
            Platform::MacOs
        } else if cfg!(target_os = "windows") {
            Platform::Windows
        } else if cfg!(target_os = "linux") {
            Platform::Linux
        } else {
            Platform::Other
        };
        let env = |name: &str| std::env::var(name).unwrap_or_default();
        Self {
            platform,
            notify_send: find_on_path("notify-send"),
            has_display: !env("DISPLAY").is_empty() || !env("WAYLAND_DISPLAY").is_empty(),
            term_program: env("TERM_PROGRAM"),
            term: env("TERM"),
            in_tmux: !env("TMUX").is_empty(),
        }
    }
}

fn find_on_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// A notifier to run: program, arguments and extra environment.
#[derive(Debug, Clone, PartialEq, Eq)]
struct NotifierCommand {
    program: PathBuf,
    args: Vec<String>,
    env: Vec<(&'static str, String)>,
}

/// The notifier for this host, or why there is none.
fn desktop_command(host: &Host, notice: &Notice) -> Result<NotifierCommand, String> {
    match host.platform {
        Platform::MacOs => Ok(osascript_command(notice)),
        Platform::Windows => Ok(powershell_command(notice)),
        Platform::Linux => {
            let program = host
                .notify_send
                .clone()
                .ok_or("notify-send is not installed")?;
            if !host.has_display {
                return Err(
                    "there is no desktop session (DISPLAY and WAYLAND_DISPLAY are unset)".into(),
                );
            }
            Ok(notify_send_command(program, notice))
        }
        Platform::Other => Err("this system has no desktop notifier gz knows".into()),
    }
}

/// The script is fixed; the text only ever arrives as `argv`. The title is
/// first and never starts with `-`, so osascript stops reading options
/// before the message.
fn osascript_command(notice: &Notice) -> NotifierCommand {
    let mut args = Vec::new();
    for line in APPLESCRIPT_LINES {
        args.push("-e".to_string());
        args.push(line.to_string());
    }
    args.push(notice.title.to_string());
    args.push(notice.message.clone());
    NotifierCommand {
        program: PathBuf::from("osascript"),
        args,
        env: Vec::new(),
    }
}

/// `--` ends the options, so text that starts with `-` stays text.
fn notify_send_command(program: PathBuf, notice: &Notice) -> NotifierCommand {
    NotifierCommand {
        program,
        args: vec![
            "--app-name=gz".to_string(),
            "--".to_string(),
            notice.title.to_string(),
            notice.message.clone(),
        ],
        env: Vec::new(),
    }
}

/// A fixed script that reads the text from the environment and adds it as
/// XML text nodes, which escape it.
fn powershell_command(notice: &Notice) -> NotifierCommand {
    let script = format!(
        "$ErrorActionPreference = 'Stop'
[void][Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime]
$toast = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02)
$lines = $toast.GetElementsByTagName('text')
[void]$lines.Item(0).AppendChild($toast.CreateTextNode($env:{TITLE_ENV}))
[void]$lines.Item(1).AppendChild($toast.CreateTextNode($env:{MESSAGE_ENV}))
$notifier = [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('{POWERSHELL_APP_ID}')
$notifier.Show([Windows.UI.Notifications.ToastNotification]::new($toast))"
    );
    NotifierCommand {
        program: PathBuf::from("powershell.exe"),
        args: vec![
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-ExecutionPolicy".to_string(),
            "Bypass".to_string(),
            "-Command".to_string(),
            script,
        ],
        env: vec![
            (TITLE_ENV, notice.title.to_string()),
            (MESSAGE_ENV, notice.message.clone()),
        ],
    }
}

async fn run_notifier(command: &NotifierCommand) -> Result<(), String> {
    let mut process = tokio::process::Command::new(&command.program);
    process
        .args(&command.args)
        .envs(command.env.iter().map(|(k, v)| (*k, v.as_str())))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let name = command.program.display().to_string();
    let child = process
        .spawn()
        .map_err(|err| format!("{name} could not start ({err})"))?;
    match tokio::time::timeout(NOTIFIER_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(done)) if done.status.success() => Ok(()),
        Ok(Ok(done)) => Err(format!("{name} failed ({})", done.status)),
        Ok(Err(err)) => Err(format!("{name} failed ({err})")),
        Err(_) => Err(format!("{name} did not answer")),
    }
}

// ---------- the terminal fallback ----------

/// Which notification escape a terminal shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalNotice {
    /// `OSC 9 ; message BEL` (iTerm2).
    Osc9,
    /// `OSC 777 ; notify ; title ; message BEL` (rxvt-unicode, foot, kitty,
    /// WezTerm, Ghostty).
    Osc777,
}

/// tmux swallows escapes it doesn't know, so none is sent inside it.
fn terminal_notice(host: &Host) -> Option<TerminalNotice> {
    if host.in_tmux {
        return None;
    }
    match host.term_program.as_str() {
        "iTerm.app" => return Some(TerminalNotice::Osc9),
        "WezTerm" | "ghostty" => return Some(TerminalNotice::Osc777),
        _ => {}
    }
    let osc777_terms = ["xterm-kitty", "foot", "rxvt-unicode"];
    osc777_terms
        .iter()
        .any(|prefix| host.term.starts_with(prefix))
        .then_some(TerminalNotice::Osc777)
}

/// The escape sequence for `notice`, text stripped of anything that could
/// end it early.
fn terminal_escape(kind: TerminalNotice, notice: &Notice) -> String {
    let plain = |text: &str| -> String { text.chars().filter(|c| !c.is_control()).collect() };
    match kind {
        TerminalNotice::Osc9 => format!(
            "\x1b]9;{}: {}\x07",
            plain(notice.title),
            plain(&notice.message)
        ),
        TerminalNotice::Osc777 => format!(
            "\x1b]777;notify;{};{}\x07",
            plain(notice.title).replace(';', ","),
            plain(&notice.message)
        ),
    }
}

fn terminal_fallback(host: &Host, notice: &Notice, why: &str) {
    if !FALLBACK_EXPLAINED.swap(true, Ordering::Relaxed) {
        output::warn(&format!(
            "no desktop notification: {why}; gz rings the terminal bell instead"
        ));
    }
    let stderr = std::io::stderr();
    if !stderr.is_terminal() {
        return;
    }
    let mut signal = terminal_notice(host)
        .map(|kind| terminal_escape(kind, notice))
        .unwrap_or_default();
    signal.push('\x07');
    let mut handle = stderr.lock();
    // A bell that can't be written has nowhere else to go.
    let _ = handle.write_all(signal.as_bytes());
    let _ = handle.flush();
}

/// One line of printable text, cut to a length desktops show.
fn clean_text(text: &str) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= MAX_TEXT_CHARS {
        return flat;
    }
    let cut: String = flat.chars().take(MAX_TEXT_CHARS - 1).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notice(message: &str) -> Notice {
        Notice {
            title: "gz jobs run",
            message: message.to_string(),
        }
    }

    fn host(platform: Platform) -> Host {
        Host {
            platform,
            notify_send: Some(PathBuf::from("/usr/bin/notify-send")),
            has_display: true,
            term_program: String::new(),
            term: "xterm-256color".into(),
            in_tmux: false,
        }
    }

    const NASTY: &str = r#"Job "x" -e 'do shell script "rm -rf ~"' $(whoami) `id` & <b>; 100%"#;

    #[test]
    fn macos_passes_the_text_as_arguments_to_a_fixed_script() {
        let command = desktop_command(&host(Platform::MacOs), &notice(NASTY)).unwrap();
        assert_eq!(command.program, PathBuf::from("osascript"));
        assert_eq!(
            command.args,
            vec![
                "-e",
                "on run argv",
                "-e",
                "display notification (item 2 of argv) with title (item 1 of argv)",
                "-e",
                "end run",
                "gz jobs run",
                NASTY,
            ]
        );
        let script = command.args[..6].join("\n");
        assert!(!script.contains("Job"), "the script never holds the text");
        assert!(command.env.is_empty());
    }

    #[test]
    fn linux_ends_options_before_the_text() {
        let command = desktop_command(&host(Platform::Linux), &notice("-u critical")).unwrap();
        assert_eq!(command.program, PathBuf::from("/usr/bin/notify-send"));
        assert_eq!(
            command.args,
            vec!["--app-name=gz", "--", "gz jobs run", "-u critical"]
        );
    }

    #[test]
    fn linux_without_notify_send_or_a_display_falls_back() {
        let mut missing = host(Platform::Linux);
        missing.notify_send = None;
        assert!(desktop_command(&missing, &notice("x"))
            .unwrap_err()
            .contains("notify-send is not installed"));
        let mut headless = host(Platform::Linux);
        headless.has_display = false;
        assert!(desktop_command(&headless, &notice("x"))
            .unwrap_err()
            .contains("no desktop session"));
        assert!(desktop_command(&host(Platform::Other), &notice("x")).is_err());
    }

    #[test]
    fn windows_reads_the_text_from_the_environment_as_xml_text() {
        let command = desktop_command(&host(Platform::Windows), &notice(NASTY)).unwrap();
        assert_eq!(command.program, PathBuf::from("powershell.exe"));
        let script = command.args.last().unwrap();
        assert!(!script.contains("Job"), "the script never holds the text");
        assert!(script.contains("CreateTextNode($env:GZ_NOTIFY_TITLE)"));
        assert!(script.contains("CreateTextNode($env:GZ_NOTIFY_MESSAGE)"));
        assert!(script.contains("ToastNotificationManager]::CreateToastNotifier"));
        assert_eq!(
            command.env,
            vec![
                (TITLE_ENV, "gz jobs run".to_string()),
                (MESSAGE_ENV, NASTY.to_string()),
            ]
        );
        assert_eq!(
            command.args[..5],
            [
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command"
            ]
        );
    }

    #[test]
    fn terminals_that_show_notifications_get_their_escape() {
        let mut iterm = host(Platform::MacOs);
        iterm.term_program = "iTerm.app".into();
        assert_eq!(terminal_notice(&iterm), Some(TerminalNotice::Osc9));
        let mut kitty = host(Platform::Linux);
        kitty.term = "xterm-kitty".into();
        assert_eq!(terminal_notice(&kitty), Some(TerminalNotice::Osc777));
        let mut wezterm = host(Platform::Linux);
        wezterm.term_program = "WezTerm".into();
        assert_eq!(terminal_notice(&wezterm), Some(TerminalNotice::Osc777));
        assert_eq!(terminal_notice(&host(Platform::Linux)), None);
        let mut tmux = iterm.clone();
        tmux.in_tmux = true;
        assert_eq!(terminal_notice(&tmux), None);
    }

    #[test]
    fn escapes_drop_control_characters_that_could_end_them() {
        let sneaky = notice("done\x07\x1b]0;owned\x07; ok");
        assert_eq!(
            terminal_escape(TerminalNotice::Osc9, &sneaky),
            "\x1b]9;gz jobs run: done]0;owned; ok\x07"
        );
        let titled = Notice {
            title: "a;b",
            message: "m".into(),
        };
        assert_eq!(
            terminal_escape(TerminalNotice::Osc777, &titled),
            "\x1b]777;notify;a,b;m\x07"
        );
    }

    #[test]
    fn text_is_one_line_and_cut_to_length() {
        assert_eq!(clean_text("run\n  failed\tnow"), "run failed now");
        let long = "x".repeat(500);
        let cut = clean_text(&long);
        assert_eq!(cut.chars().count(), MAX_TEXT_CHARS);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn the_setting_reads_on_and_off() {
        assert_eq!(parse_on_off("ON"), Ok(true));
        assert_eq!(parse_on_off("off"), Ok(false));
        assert!(parse_on_off("sometimes").is_err());
    }

    #[test]
    fn the_flag_wins_over_the_profile() {
        let none = NotifyArgs::default();
        assert_eq!(none.choice(), None);
        let on = NotifyArgs {
            notify: true,
            no_notify: false,
        };
        assert_eq!(on.choice(), Some(true));
        let off = NotifyArgs {
            notify: false,
            no_notify: true,
        };
        assert_eq!(off.choice(), Some(false));
    }
}
