//! `/sandbox`: the OS limits on the commands and scripts the model runs
//! (see `assistant::os_sandbox`): their status, and the settings that widen
//! or narrow them. Changes apply from the next command, for the main agent
//! and every subagent. Paths and switches are saved on the profile; ssh,
//! keychain and socket grants last for the session. Every change goes to
//! the audit log.

use anyhow::{anyhow, bail, Context, Result};
use std::path::Path;

use crate::assistant::audit::Entry;
use crate::assistant::os_sandbox::{
    self, mechanism, sockets, Added, Mechanism, OsSandbox, SandboxSettings, SshGrant,
    CREDENTIAL_PATHS, TOOL_CACHES,
};
use crate::code_ui::items::Item;
use crate::code_ui::panel::{Panel, Row};

use super::{Effect, UiState};

const USAGE: &str = "/sandbox on|off · network on|off · allow-write <path> · allow-read <path> · \
allow ssh · allow keychain · allow socket <path> · forget <n>";

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
        let (word, argument) = split_word(rest);
        let done = match (word, argument) {
            ("", _) => {
                self.show_sandbox(&os.settings());
                return Vec::new();
            }
            ("on" | "off", "") => switch(&os, word == "on"),
            ("network", "on" | "off") => network(&os, argument == "on"),
            ("allow-write", path) if !path.is_empty() => self.allow_path(&os, path, true),
            ("allow-read", path) if !path.is_empty() => self.allow_path(&os, path, false),
            ("allow", grant) => self.allow(&os, grant),
            ("forget", number) => forget(&os, number),
            _ => Err(anyhow!("Usage: {USAGE}")),
        };
        match done {
            Ok(note) => {
                self.audit_sandbox_change(rest);
                self.record(Item::Notice(note));
            }
            Err(err) => self.record(Item::Error(format!("{err:#}"))),
        }
        Vec::new()
    }

    /// `/sandbox allow ssh|keychain|socket <path>`: grants for this session.
    fn allow(&self, os: &OsSandbox, grant: &str) -> Result<String> {
        match split_word(grant) {
            ("ssh", "") => self.allow_ssh(os),
            ("keychain", "") => allow_keychain(os),
            ("socket", path) if !path.is_empty() => self.allow_socket(os, path),
            _ => Err(anyhow!(
                "Usage: /sandbox allow ssh · allow keychain · allow socket <path>"
            )),
        }
    }

    fn allow_ssh(&self, os: &OsSandbox) -> Result<String> {
        let home = self.sandbox.as_ref().and_then(|folders| folders.home());
        let agents = sockets::ssh_agent_sockets(
            home,
            std::env::var_os(os_sandbox::AGENT_VARIABLE).as_deref(),
        );
        let shown: Vec<String> = agents.iter().map(|a| self.shown_folder(a)).collect();
        os.change(|settings| {
            if settings.ssh.is_some() {
                bail!("ssh is allowed already");
            }
            settings.ssh = Some(SshGrant { agents });
            Ok(())
        })?;
        let agent = if shown.is_empty() {
            "no ssh-agent socket was found ($SSH_AUTH_SOCK is unset or gone, and ~/.ssh/config \
             names no IdentityAgent), so only key files work"
                .to_string()
        } else {
            format!("and use the ssh-agent at {}", shown.join(" · "))
        };
        Ok(with_sandbox_state(
            os,
            format!("For this session commands may read ~/.ssh {agent}"),
        ))
    }

    fn allow_socket(&self, os: &OsSandbox, typed: &str) -> Result<String> {
        let folders = self
            .sandbox
            .as_ref()
            .context("no folder is open in this session")?;
        let path = os_sandbox::existing_path(typed, &folders.cwd(), folders.home())?;
        os_sandbox::check_socket(&path)?;
        let shown = self.shown_folder(&path);
        os.change(|settings| {
            if settings.sockets.contains(&path) {
                bail!("{shown} is allowed already");
            }
            settings.sockets.push(path);
            Ok(())
        })?;
        Ok(with_sandbox_state(
            os,
            format!("For this session commands may connect to {shown}"),
        ))
    }

    fn allow_path(&self, os: &OsSandbox, typed: &str, is_write: bool) -> Result<String> {
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
        Ok(with_sandbox_state(
            os,
            format!("Commands may now {verb} {shown}"),
        ))
    }

    /// A `/sandbox` change in the audit log, next to the tool calls it
    /// lets through.
    fn audit_sandbox_change(&self, change: &str) {
        let Some(log) = &self.audit else {
            return;
        };
        let entry = Entry {
            conversation_id: "",
            run_id: "",
            tool: "sandbox",
            target: change,
            decision: "user",
            reason: None,
            is_error: false,
        };
        if let Err(err) = log.record(&entry) {
            crate::output::warn(&format!("cannot write the audit log: {err:#}"));
        }
    }

    fn shown_folder(&self, path: &Path) -> String {
        match &self.sandbox {
            Some(folders) => folders.display_folder(path),
            None => path.display().to_string(),
        }
    }

    fn show_sandbox(&mut self, settings: &SandboxSettings) {
        let found = mechanism();
        let aside = match (settings.enabled, found) {
            (false, _) => "off · /sandbox on".to_string(),
            (true, Mechanism::Unavailable(_)) => "on, but this machine can't apply it".to_string(),
            (true, _) => "on".to_string(),
        };
        let open = self
            .sandbox
            .as_ref()
            .map(|folders| {
                folders
                    .roots()
                    .iter()
                    .map(|r| self.shown_folder(r))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
            .join(" · ");
        let network = if settings.network {
            "on"
        } else {
            "off · only localhost · /sandbox network on"
        };
        let added = settings.added();
        let numbered = |pick: fn(&Added) -> bool| {
            added
                .iter()
                .enumerate()
                .filter(|(_, added)| pick(added))
                .map(|(i, added)| Row::entry(&(i + 1).to_string(), self.added_label(added)))
                .collect::<Vec<_>>()
        };

        let mut summary = vec![
            Row::field(
                "applies to",
                "commands and scripts the model runs, not your !commands",
            ),
            Row::field("mechanism", found.label()),
            Row::field("network", network),
        ];
        if let Mechanism::Bubblewrap {
            abstract_sockets, ..
        } = found
        {
            let state = match abstract_sockets {
                Ok(()) => "closed (Landlock)".to_string(),
                Err(why) => format!("reachable: {why}"),
            };
            summary.push(Row::field("abstract sockets", state));
        }

        let mut writable = vec![
            Row::field("open folders", open.clone()),
            Row::field("temp", temp_label()),
            Row::field("tool caches", format!("~/{}", TOOL_CACHES.join(" · ~/"))),
        ];
        writable.extend(numbered(|a| matches!(a, Added::Write(_))));

        let mut hidden = vec![Row::note(format!("~/{}", CREDENTIAL_PATHS.join(" · ~/")))];
        if cfg!(target_os = "macos") {
            hidden.push(Row::note("the keychain service"));
        }
        hidden.extend(numbered(|a| matches!(a, Added::Read(_))));

        let socket_folder = sockets::socket_folder()
            .map(|folder| folder.display().to_string())
            .unwrap_or_else(|err| format!("unavailable: {err:#}"));
        let reachable = vec![
            Row::field("open folders", open),
            Row::field("commands' TMPDIR", socket_folder),
            Row::field("system", sockets::SYSTEM_SOCKETS.join(" · ")),
            Row::note("all other unix sockets are closed: ssh-agent, Docker, tmux, databases…"),
        ];

        let mut session =
            numbered(|a| matches!(a, Added::Ssh(_) | Added::Keychain | Added::Socket(_)));
        if settings.ssh.is_none() {
            session.push(Row::field("ssh", "closed · /sandbox allow ssh"));
        }
        if cfg!(target_os = "macos") && !settings.keychain {
            session.push(Row::field("keychain", "closed · /sandbox allow keychain"));
        }

        let panel = Panel::new("Sandbox")
            .aside(aside)
            .section(None, summary)
            .section(Some("Commands may write to"), writable)
            .section(
                Some("Hidden from commands · numbered ones readable again"),
                hidden,
            )
            .section(Some("Unix sockets commands may connect to"), reachable)
            .section(
                Some("Allowed this session only · /sandbox forget <n> closes"),
                session,
            )
            .footer(USAGE);
        self.record(Item::Panel(panel));
    }

    fn added_label(&self, added: &Added) -> String {
        match added {
            Added::Write(path) | Added::Read(path) => self.shown_folder(path),
            Added::Ssh(grant) if grant.agents.is_empty() => "ssh: ~/.ssh, no agent found".into(),
            Added::Ssh(grant) => {
                let agents: Vec<String> =
                    grant.agents.iter().map(|a| self.shown_folder(a)).collect();
                format!("ssh: ~/.ssh and the agent at {}", agents.join(" · "))
            }
            Added::Keychain => "keychain".into(),
            Added::Socket(path) => format!("socket {}", self.shown_folder(path)),
        }
    }
}

/// The first word of `text`, and the rest trimmed.
fn split_word(text: &str) -> (&str, &str) {
    text.split_once(char::is_whitespace)
        .map(|(word, rest)| (word, rest.trim()))
        .unwrap_or((text, ""))
}

/// `note`, or a reminder that it waits for `/sandbox on`.
fn with_sandbox_state(os: &OsSandbox, note: String) -> String {
    if os.settings().enabled {
        note
    } else {
        format!("{note}, once the sandbox is on (/sandbox on)")
    }
}

#[cfg(target_os = "macos")]
fn allow_keychain(os: &OsSandbox) -> Result<String> {
    os.change(|settings| {
        if settings.keychain {
            bail!("the keychain is allowed already");
        }
        settings.keychain = true;
        Ok(())
    })?;
    Ok(with_sandbox_state(
        os,
        "For this session commands may read keychain items your login can reach, \
         git's https credentials included"
            .into(),
    ))
}

/// Elsewhere keyrings answer over the session D-Bus, which also starts
/// services outside the sandbox, so it is opened as a socket, knowingly.
#[cfg(not(target_os = "macos"))]
fn allow_keychain(_: &OsSandbox) -> Result<String> {
    bail!(
        "on this system the keyring answers over the session D-Bus socket, which also lets a \
         program start services outside the sandbox; to allow it anyway, \
         /sandbox allow socket $XDG_RUNTIME_DIR/bus"
    )
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
        Added::Ssh(_) => "Commands may no longer read ~/.ssh or use the ssh-agent".into(),
        Added::Keychain => "Commands may no longer reach the keychain".into(),
        Added::Socket(path) => format!("Commands may no longer connect to {}", path.display()),
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

    #[cfg(unix)]
    #[test]
    fn socket_ssh_and_keychain_grants_last_the_session_and_are_audited() {
        let (mut s, os, home) = sandboxed();
        let log = home.join("audit.log");
        s.audit = Some(crate::assistant::audit::AuditLog::at(log.clone()));
        let socket = std::path::PathBuf::from(format!("/tmp/gz-cmd-sock-{}", std::process::id()));
        std::fs::remove_file(&socket).ok();
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();

        let added = run(
            &mut s,
            &format!("/sandbox allow socket {}", socket.display()),
        );
        assert!(
            matches!(&added, Item::Notice(n) if n.contains("may connect to")),
            "{added:?}"
        );
        let real = socket.canonicalize().unwrap();
        assert_eq!(os.settings().sockets, vec![real.clone()]);
        let ssh = run(&mut s, "/sandbox allow ssh");
        assert!(
            matches!(&ssh, Item::Notice(n) if n.contains("~/.ssh")),
            "{ssh:?}"
        );
        assert!(os.settings().ssh.is_some());
        let keychain = run(&mut s, "/sandbox allow keychain");
        if cfg!(target_os = "macos") {
            assert!(os.settings().keychain, "{keychain:?}");
        } else {
            assert!(
                matches!(&keychain, Item::Error(e) if e.contains("D-Bus")),
                "{keychain:?}"
            );
        }
        assert!(os.settings().is_default(), "grants are not saved");

        let Item::Panel(panel) = run(&mut s, "/sandbox") else {
            panic!("expected the status panel");
        };
        let text = panel.plain_lines().join("\n");
        for expected in [
            "Allowed this session only",
            &format!("socket {}", real.display()),
            "ssh: ~/.ssh",
            "all other unix sockets are closed",
        ] {
            assert!(text.contains(expected), "{expected} missing from:\n{text}");
        }

        run(&mut s, "/sandbox forget 2");
        assert!(os.settings().ssh.is_some(), "numbered ssh, then the socket");
        run(&mut s, "/sandbox forget 1");
        assert!(os.settings().ssh.is_none());

        let audit = std::fs::read_to_string(&log).unwrap();
        for change in ["allow socket", "allow ssh", "forget 2", "forget 1"] {
            assert!(
                audit.contains(&format!("\"target\":\"{change}")),
                "{change} missing from:\n{audit}"
            );
        }
        assert!(audit.contains("\"tool\":\"sandbox\""), "{audit}");
        std::fs::remove_file(&socket).ok();
    }

    #[test]
    fn a_file_is_not_a_socket_to_allow() {
        let (mut s, os, home) = sandboxed();
        std::fs::write(home.join("work/plain.txt"), "").unwrap();
        let refused = run(&mut s, "/sandbox allow socket plain.txt");
        assert!(
            matches!(&refused, Item::Error(e) if e.contains("not a unix socket")),
            "{refused:?}"
        );
        let usage = run(&mut s, "/sandbox allow everything");
        assert!(
            matches!(&usage, Item::Error(e) if e.contains("allow socket <path>")),
            "{usage:?}"
        );
        assert_eq!(os.settings(), SandboxSettings::default());
    }

    #[test]
    fn the_status_says_ssh_is_closed_and_how_to_open_it() {
        let (mut s, _, _) = sandboxed();
        let Item::Panel(panel) = run(&mut s, "/sandbox") else {
            panic!("expected the status panel");
        };
        let text = panel.plain_lines().join("\n");
        for expected in [
            "closed · /sandbox allow ssh",
            "~/.ssh",
            "~/.netrc",
            "Unix sockets",
        ] {
            assert!(text.contains(expected), "{expected} missing from:\n{text}");
        }
    }
}
