//! `/sandbox`: the OS limits on the commands and scripts the model runs
//! (see `assistant::os_sandbox`): their status, and the settings that widen
//! or narrow them. Changes apply from the next command, for the main agent
//! and every subagent, and are saved on the profile.

use anyhow::{anyhow, bail, Context, Result};
use std::path::Path;

use crate::assistant::os_sandbox::{
    self, mechanism, Added, Mechanism, OsSandbox, SandboxSettings, CREDENTIAL_DIRS, TOOL_CACHES,
};
use crate::code_ui::items::Item;
use crate::code_ui::panel::{Panel, Row};

use super::{Effect, UiState};

const USAGE: &str =
    "/sandbox on|off · network on|off · allow-write <path> · allow-read <path> · forget <n>";

impl UiState {
    pub(super) fn sandbox_command(&mut self, command: &str) -> Vec<Effect> {
        let Some(os) = self.os_sandbox.clone() else {
            self.record(Item::Error(
                "The sandbox settings aren't available in this session".into(),
            ));
            return Vec::new();
        };
        let rest = command.trim().strip_prefix("/sandbox").unwrap_or_default();
        let rest = rest.trim();
        let (word, argument) = rest
            .split_once(char::is_whitespace)
            .map(|(word, argument)| (word, argument.trim()))
            .unwrap_or((rest, ""));
        let done = match (word, argument) {
            ("", _) => {
                self.show_sandbox(&os.settings());
                return Vec::new();
            }
            ("on" | "off", "") => switch(&os, word == "on"),
            ("network", "on" | "off") => network(&os, argument == "on"),
            ("allow-write", path) if !path.is_empty() => self.allow(&os, path, true),
            ("allow-read", path) if !path.is_empty() => self.allow(&os, path, false),
            ("forget", number) => forget(&os, number),
            _ => Err(anyhow!("Usage: {USAGE}")),
        };
        match done {
            Ok(note) => self.record(Item::Notice(note)),
            Err(err) => self.record(Item::Error(format!("{err:#}"))),
        }
        Vec::new()
    }

    fn allow(&self, os: &OsSandbox, typed: &str, is_write: bool) -> Result<String> {
        let folders = self
            .sandbox
            .as_ref()
            .context("no folder is open in this session")?;
        let home = folders.home();
        let path = os_sandbox::existing_path(typed, &folders.cwd(), home)?;
        let shown = folders.display_folder(&path);
        if is_write {
            os_sandbox::check_writable(&path, home)?;
        } else {
            os_sandbox::check_readable(&path, home)?;
        }
        os.change(|settings| {
            let list = if is_write {
                &mut settings.writable
            } else {
                &mut settings.readable
            };
            if list.contains(&path) {
                bail!("{shown} is added already");
            }
            list.push(path);
            Ok(())
        })?;
        let verb = if is_write { "write to" } else { "read" };
        Ok(if os.settings().enabled {
            format!("Commands may now {verb} {shown}")
        } else {
            format!("Added {shown}; it applies once the sandbox is on (/sandbox on)")
        })
    }

    fn show_sandbox(&mut self, settings: &SandboxSettings) {
        let found = mechanism();
        let aside = match (settings.enabled, found) {
            (false, _) => "off · /sandbox on".to_string(),
            (true, Mechanism::Unavailable(_)) => "on, but this machine can't apply it".to_string(),
            (true, _) => "on".to_string(),
        };
        let shown = |path: &Path| match &self.sandbox {
            Some(folders) => folders.display_folder(path),
            None => path.display().to_string(),
        };
        let open = self
            .sandbox
            .as_ref()
            .map(|folders| folders.roots().iter().map(|r| shown(r)).collect::<Vec<_>>())
            .unwrap_or_default();
        let network = if settings.network {
            "on"
        } else {
            "off · only localhost · /sandbox network on"
        };
        let mut writable = vec![
            Row::field("open folders", open.join(" · ")),
            Row::field("temp", temp_label()),
            Row::field("tool caches", format!("~/{}", TOOL_CACHES.join(" · ~/"))),
        ];
        let added = settings.added();
        let numbered = |want_write: bool| {
            added
                .iter()
                .enumerate()
                .filter_map(|(i, added)| match added {
                    Added::Write(path) if want_write => Some((i, path)),
                    Added::Read(path) if !want_write => Some((i, path)),
                    _ => None,
                })
                .map(|(i, path)| Row::entry(&(i + 1).to_string(), shown(path)))
                .collect::<Vec<_>>()
        };
        writable.extend(numbered(true));
        let mut hidden = vec![Row::note(format!(
            "~/{} (~/.ssh stays readable for git over SSH)",
            CREDENTIAL_DIRS.join(" · ~/")
        ))];
        hidden.extend(numbered(false));
        let panel = Panel::new("Sandbox")
            .aside(aside)
            .section(
                None,
                vec![
                    Row::field(
                        "applies to",
                        "commands and scripts the model runs, not your !commands",
                    ),
                    Row::field("mechanism", found.label()),
                    Row::field("network", network),
                ],
            )
            .section(Some("Commands may write to"), writable)
            .section(
                Some("Hidden from commands · numbered ones readable again"),
                hidden,
            )
            .footer(USAGE);
        self.record(Item::Panel(panel));
    }
}

fn temp_label() -> String {
    os_sandbox::temp_folders()
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(" · ")
}

fn switch(os: &OsSandbox, on: bool) -> Result<String> {
    os.change(|settings| {
        settings.enabled = on;
        Ok(())
    })?;
    if !on {
        return Ok("Sandbox off: commands the model runs can change any file you can".into());
    }
    Ok(match mechanism() {
        Mechanism::Unavailable(why) => {
            format!("Sandbox on, but this machine can't apply it: {why}")
        }
        found => format!("Sandbox on ({}) from the next command", found.label()),
    })
}

fn network(os: &OsSandbox, on: bool) -> Result<String> {
    os.change(|settings| {
        settings.network = on;
        Ok(())
    })?;
    Ok(if on {
        "Network on for commands".into()
    } else {
        "Network off for commands: only localhost answers".into()
    })
}

fn forget(os: &OsSandbox, number: &str) -> Result<String> {
    let number: usize = number
        .parse()
        .map_err(|_| anyhow!("/sandbox forget takes the number /sandbox shows"))?;
    let forgotten = os.change(|settings| settings.forget(number))?;
    Ok(match forgotten {
        Added::Write(path) => format!("Commands may no longer write to {}", path.display()),
        Added::Read(path) => format!("Commands may no longer read {}", path.display()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant::sandbox::{tests::temp_dir, Sandbox};
    use crate::code_ui::state::test_support::*;
    use crossterm::event::KeyCode;

    fn sandboxed() -> (UiState, OsSandbox, std::path::PathBuf) {
        let home = temp_dir("sandbox-cmd");
        for dir in ["work", "extra", ".aws"] {
            std::fs::create_dir_all(home.join(dir)).unwrap();
        }
        let mut s = state();
        s.sandbox = Some(Sandbox::with_home(&home.join("work"), Some(&home)).unwrap());
        let os = OsSandbox::unsaved(SandboxSettings::default());
        s.os_sandbox = Some(os.clone());
        (s, os, home)
    }

    fn run(s: &mut UiState, command: &str) -> Item {
        type_text(s, command);
        assert!(s.on_key(key(KeyCode::Enter)).is_empty());
        s.flush.last().cloned().unwrap()
    }

    #[test]
    fn settings_change_the_shared_handle_and_forget_by_number() {
        let (mut s, os, home) = sandboxed();
        run(&mut s, "/sandbox network off");
        assert!(!os.settings().network);
        let added = run(&mut s, "/sandbox allow-write ~/extra");
        assert!(
            matches!(&added, Item::Notice(n) if n.contains("~/extra")),
            "{added:?}"
        );
        run(&mut s, "/sandbox allow-read ~/.aws");
        let settings = os.settings();
        assert_eq!(settings.writable, vec![home.join("extra")]);
        assert_eq!(settings.readable, vec![home.join(".aws")]);
        let again = run(&mut s, "/sandbox allow-write ../extra");
        assert!(
            matches!(&again, Item::Error(e) if e.contains("added already")),
            "{again:?}"
        );
        run(&mut s, "/sandbox forget 1");
        assert!(os.settings().writable.is_empty());
        assert_eq!(os.settings().readable, vec![home.join(".aws")]);
        run(&mut s, "/sandbox off");
        assert!(!os.settings().enabled);
    }

    #[test]
    fn bad_paths_and_words_are_refused_with_the_reason() {
        let (mut s, os, _) = sandboxed();
        let missing = run(&mut s, "/sandbox allow-write nope");
        assert!(
            matches!(&missing, Item::Error(e) if e.contains("does not exist")),
            "{missing:?}"
        );
        let open = run(&mut s, "/sandbox allow-read ~/extra");
        assert!(
            matches!(&open, Item::Error(e) if e.contains("readable already")),
            "{open:?}"
        );
        let usage = run(&mut s, "/sandbox maybe");
        assert!(
            matches!(&usage, Item::Error(e) if e.contains("allow-write <path>")),
            "{usage:?}"
        );
        assert_eq!(os.settings(), SandboxSettings::default());
    }

    #[test]
    fn the_status_lists_open_folders_added_paths_and_network() {
        let (mut s, _, _) = sandboxed();
        run(&mut s, "/sandbox allow-write ~/extra");
        run(&mut s, "/sandbox network off");
        let Item::Panel(panel) = run(&mut s, "/sandbox") else {
            panic!("expected the status panel");
        };
        let text = panel.plain_lines().join("\n");
        for expected in [
            "~/work",
            "1  ~/extra",
            "only localhost",
            "~/.cargo",
            "~/.aws",
        ] {
            assert!(text.contains(expected), "{expected} missing from:\n{text}");
        }
    }
}
