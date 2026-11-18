//! OS limits for the commands and scripts the model runs.
//!
//! gz's file tools stay inside the open folders ([`Sandbox`]), but an
//! approved command runs as the user and could otherwise write anywhere.
//! Under a [`Confinement`] it may write only in the open folders, the
//! system temp folders, a few tool caches and folders the user added; may
//! not read the credential stores under home; and may connect only to the
//! unix sockets [`sockets`] allows. On macOS this is `sandbox-exec` with a
//! generated profile, on Linux `bwrap` (plus Landlock for abstract
//! sockets); elsewhere commands run as before and the user is told once.
//!
//! The user's own `!command`s are never confined: they typed them.

#[cfg(target_os = "linux")]
mod landlock_scope;
pub mod sockets;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::assistant::git_guard::{self, GitDir, GitWatch, Reverts};
use crate::assistant::sandbox::{home_relative, Sandbox};

/// The ssh folder: keys, `known_hosts` and config. Hidden like the other
/// credential stores; `/sandbox allow ssh` opens it with the agent.
pub const SSH_DIR: &str = ".ssh";

/// The macOS keychain files. Commands reach keychain items through the
/// keychain service, so hiding the files alone is not enough: the profile
/// also closes the service unless `/sandbox allow keychain`.
const KEYCHAIN_DIR: &str = "Library/Keychains";

/// Credential stores under home, folders and files: neither read nor
/// written by commands.
pub const CREDENTIAL_PATHS: &[&str] = &[
    SSH_DIR,
    ".aws",
    ".azure",
    ".config/gcloud",
    ".oci",
    ".kube",
    ".docker",
    ".gnupg",
    ".password-store",
    ".config/gh",
    ".config/hub",
    ".config/op",
    ".netrc",
    ".git-credentials",
    ".config/git/credentials",
    ".npmrc",
    ".pypirc",
    ".gem/credentials",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".terraform.d/credentials.tfrc.json",
    ".vault-token",
    ".local/share/keyrings",
    KEYCHAIN_DIR,
    // Browser profiles hold signed-in sessions' cookies.
    "Library/Application Support/Google/Chrome",
    "Library/Application Support/BraveSoftware",
    "Library/Application Support/Microsoft Edge",
    "Library/Application Support/Firefox",
    "Library/Cookies",
    "Library/Safari",
    ".config/google-chrome",
    ".config/chromium",
    ".config/BraveSoftware",
    ".mozilla",
];

/// The macOS keychain service: file-based keychains (login) and the
/// data-protection keychain. HTTPS trust checks use `trustd`, which stays.
const KEYCHAIN_SERVICES: &[&str] = &["com.apple.SecurityServer", "com.apple.securityd.xpc"];

/// Caches under home that package managers and compilers write to on
/// every build; without them nearly every build or install would fail.
/// Only download and build caches: folders on `PATH`, toolchains and files
/// a shell sources (`~/.cargo/bin`, `~/.cargo/env`, `~/.rustup`,
/// `~/go/bin`, `~/.yarn/bin`) stay closed, so a command can't plant a
/// program that later runs outside the sandbox.
pub const TOOL_CACHES: &[&str] = &[
    ".cargo/registry",
    ".cargo/git",
    // Cargo's lock and download-tracking files, next to the caches.
    ".cargo/.package-cache",
    ".cargo/.package-cache-mutate",
    ".cargo/.global-cache",
    ".npm",
    ".cache",
    "Library/Caches",
    ".gradle",
    ".m2",
    "go/pkg",
    ".pnpm-store",
    ".yarn/berry",
];

/// System temp folders, by their real paths (macOS's `/tmp` and
/// `/var/folders` are symlinks into `/private`, and the sandbox checks the
/// real path).
#[cfg(target_os = "macos")]
const SYSTEM_TEMP: &[&str] = &["/private/tmp", "/private/var/folders"];
#[cfg(not(target_os = "macos"))]
const SYSTEM_TEMP: &[&str] = &["/tmp", "/var/tmp"];

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// Output that shows the OS refused an operation (lowercase).
const DENIAL_SIGNS: &[&str] = &[
    "operation not permitted",
    "read-only file system",
    "sandbox-exec:",
    "bwrap:",
    "deny(1)",
];

/// Output that shows a connection could not be made (lowercase).
const OFFLINE_SIGNS: &[&str] = &[
    "could not resolve host",
    "temporary failure in name resolution",
    "nodename nor servname",
    "network is unreachable",
    "couldn't connect to server",
    "failed to connect",
];

/// Output of ssh failing without `~/.ssh` or the agent (lowercase).
const SSH_SIGNS: &[&str] = &[
    "host key verification failed",
    "error connecting to agent",
    "could not open a connection to your authentication agent",
    "permission denied (publickey",
];

/// The variable naming the ssh-agent's socket.
pub const AGENT_VARIABLE: &str = "SSH_AUTH_SOCK";

/// Output of a keychain lookup failing (lowercase): git's https
/// credentials come from the keychain, then git asks for a username.
const KEYCHAIN_SIGNS: &[&str] = &[
    "keychain",
    "could not read username",
    "could not read password",
];

const SSH_CAUSE: &str = "~/.ssh and the ssh-agent are closed to commands, so ssh (and git over \
SSH) can't check host keys or use keys; /sandbox allow ssh opens both for this session";

const KEYCHAIN_CAUSE: &str = "the macOS keychain is closed to commands, so stored passwords and \
tokens (git's https credentials included) can't be read; /sandbox allow keychain opens it for \
this session";

const GENERIC_NOTE: &str = "[gz's sandbox likely blocked this: commands gz runs for you may write \
only in the open folders, the temp folders and tool caches, can't read credential stores (~/.ssh, \
~/.aws, ~/.config/gh, ~/.netrc and the others /sandbox lists) and may connect only to unix sockets \
in the open folders. Do not try another way around it; tell the user what was blocked. They can \
allow a path with /sandbox allow-write <path> or /sandbox allow-read <path>, ssh with /sandbox \
allow ssh, a unix socket with /sandbox allow socket <path>, or turn the sandbox off with \
/sandbox off.]";

/// The user's `/sandbox` settings. Paths and switches are saved per
/// profile (`code_sandbox`); grants of ssh, the keychain and sockets last
/// only for the session, since each hands commands a power over the
/// user's accounts or machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SandboxSettings {
    pub enabled: bool,
    pub network: bool,
    /// Extra folders commands may write to (`/sandbox allow-write`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub writable: Vec<PathBuf>,
    /// Paths inside the credential stores commands may read
    /// (`/sandbox allow-read`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub readable: Vec<PathBuf>,
    /// `/sandbox allow ssh`, this session only.
    #[serde(skip)]
    pub ssh: Option<SshGrant>,
    /// `/sandbox allow keychain`, this session only.
    #[serde(skip)]
    pub keychain: bool,
    /// Unix sockets (or folders of them) commands may connect to
    /// (`/sandbox allow socket`), this session only.
    #[serde(skip)]
    pub sockets: Vec<PathBuf>,
}

/// What `/sandbox allow ssh` opened: `~/.ssh`, and the agent sockets found
/// when it was given.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SshGrant {
    pub agents: Vec<PathBuf>,
}

impl Default for SandboxSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            network: true,
            writable: Vec::new(),
            readable: Vec::new(),
            ssh: None,
            keychain: false,
            sockets: Vec::new(),
        }
    }
}

/// Something the user added, as `/sandbox` numbers it for `/sandbox forget`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Added {
    Write(PathBuf),
    Read(PathBuf),
    Ssh(SshGrant),
    Keychain,
    Socket(PathBuf),
}

impl SandboxSettings {
    /// Whether there is nothing to save: session grants are never saved.
    pub fn is_default(&self) -> bool {
        Self {
            ssh: None,
            keychain: false,
            sockets: Vec::new(),
            ..self.clone()
        } == Self::default()
    }

    /// Added writable folders, added readable paths, then this session's
    /// grants; `/sandbox` numbers them from 1 in this order.
    pub fn added(&self) -> Vec<Added> {
        let writes = self.writable.iter().cloned().map(Added::Write);
        let reads = self.readable.iter().cloned().map(Added::Read);
        let ssh = self.ssh.clone().map(Added::Ssh);
        let keychain = self.keychain.then_some(Added::Keychain);
        let sockets = self.sockets.iter().cloned().map(Added::Socket);
        writes
            .chain(reads)
            .chain(ssh)
            .chain(keychain)
            .chain(sockets)
            .collect()
    }

    /// Removes the `number`th entry of [`Self::added`].
    pub fn forget(&mut self, number: usize) -> Result<Added> {
        let added = self.added();
        let Some(which) = number.checked_sub(1).and_then(|i| added.get(i)) else {
            bail!(
                "there is nothing numbered {number}; /sandbox lists what was added ({} added)",
                added.len()
            );
        };
        match which {
            Added::Write(path) => self.writable.retain(|p| p != path),
            Added::Read(path) => self.readable.retain(|p| p != path),
            Added::Ssh(_) => self.ssh = None,
            Added::Keychain => self.keychain = false,
            Added::Socket(path) => self.sockets.retain(|p| p != path),
        }
        Ok(which.clone())
    }
}

type Save = Arc<dyn Fn(&SandboxSettings) -> Result<()> + Send + Sync>;

/// The session's sandbox settings; clones share them, so a change applies
/// to the main agent and every subagent at their next command.
#[derive(Clone)]
pub struct OsSandbox {
    settings: Arc<RwLock<SandboxSettings>>,
    save: Save,
}

impl OsSandbox {
    /// `saved` settings of `profile`, saved back there when changed.
    pub fn for_profile(profile: &str, saved: Option<SandboxSettings>) -> Self {
        let profile = profile.to_string();
        let save: Save = Arc::new(move |settings: &SandboxSettings| {
            let mut config = crate::config::load_config()?;
            config.profile_mut(&profile).code_sandbox =
                (!settings.is_default()).then(|| settings.clone());
            crate::config::save_config(&config).map(|_| ())
        });
        Self::with_save(saved.unwrap_or_default(), save)
    }

    /// Settings that live only as long as the session.
    pub fn unsaved(settings: SandboxSettings) -> Self {
        Self::with_save(settings, Arc::new(|_: &SandboxSettings| Ok(())))
    }

    fn with_save(settings: SandboxSettings, save: Save) -> Self {
        Self {
            settings: Arc::new(RwLock::new(settings)),
            save,
        }
    }

    pub fn settings(&self) -> SandboxSettings {
        self.settings
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Applies `edit` at once and saves the result for later sessions.
    pub fn change<T>(&self, edit: impl FnOnce(&mut SandboxSettings) -> Result<T>) -> Result<T> {
        let (value, settings) = {
            let mut settings = self.settings.write().unwrap_or_else(|p| p.into_inner());
            let value = edit(&mut settings)?;
            (value, settings.clone())
        };
        (self.save)(&settings)
            .context("the change applies to this session but could not be saved for the next")?;
        Ok(value)
    }
}

impl Default for OsSandbox {
    fn default() -> Self {
        Self::unsaved(SandboxSettings::default())
    }
}

/// How this machine confines commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mechanism {
    /// macOS `sandbox-exec`.
    Seatbelt,
    /// Linux bubblewrap at `program`. `abstract_sockets` is `Ok` when
    /// Landlock closes abstract unix sockets too, else why it can't.
    Bubblewrap {
        program: PathBuf,
        abstract_sockets: std::result::Result<(), String>,
    },
    /// None: why, and what the user can do about it.
    Unavailable(String),
}

impl Mechanism {
    pub fn label(&self) -> String {
        match self {
            Mechanism::Seatbelt => "macOS sandbox-exec".into(),
            Mechanism::Bubblewrap {
                program,
                abstract_sockets: Ok(()),
            } => format!("bubblewrap ({}) and Landlock", program.display()),
            Mechanism::Bubblewrap { program, .. } => format!("bubblewrap ({})", program.display()),
            Mechanism::Unavailable(why) => format!("none: {why}"),
        }
    }
}

/// This machine's mechanism, found once: it is tried with a no-op command,
/// because a present but unusable one (inside another sandbox, or without
/// user namespaces) would otherwise make every command fail.
pub fn mechanism() -> &'static Mechanism {
    static FOUND: OnceLock<Mechanism> = OnceLock::new();
    FOUND.get_or_init(detect)
}

fn detect() -> Mechanism {
    if cfg!(target_os = "macos") {
        let trial = ["-p", "(version 1)(allow default)", "/usr/bin/true"];
        return match try_run(Path::new(SANDBOX_EXEC), &trial, false) {
            Ok(()) => Mechanism::Seatbelt,
            Err(why) => Mechanism::Unavailable(format!("sandbox-exec does not work here ({why})")),
        };
    }
    if cfg!(target_os = "linux") {
        let Some(bwrap) = find_program("bwrap") else {
            return Mechanism::Unavailable(
                "bubblewrap is not installed; install it (the `bubblewrap` package) and restart gz"
                    .into(),
            );
        };
        let trial = [
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--unshare-pid",
            "--",
            "true",
        ];
        let scoped = landlock_available().and_then(|()| {
            try_run(&bwrap, &trial, true)
                .map_err(|why| format!("bubblewrap does not start under Landlock ({why})"))
        });
        if scoped.is_ok() {
            return Mechanism::Bubblewrap {
                program: bwrap,
                abstract_sockets: Ok(()),
            };
        }
        return match try_run(&bwrap, &trial, false) {
            Ok(()) => Mechanism::Bubblewrap {
                program: bwrap,
                abstract_sockets: scoped,
            },
            Err(why) => Mechanism::Unavailable(format!(
                "bubblewrap does not work here ({why}); it needs unprivileged user namespaces"
            )),
        };
    }
    Mechanism::Unavailable("gz has no OS sandbox for this platform".into())
}

#[cfg(target_os = "linux")]
fn landlock_available() -> std::result::Result<(), String> {
    landlock_scope::scoping_available()
}

#[cfg(not(target_os = "linux"))]
fn landlock_available() -> std::result::Result<(), String> {
    Err("Landlock is Linux only".into())
}

/// Runs `program` with `args`, inside a Landlock scope when `scoped`.
fn try_run(program: &Path, args: &[&str], scoped: bool) -> std::result::Result<(), String> {
    let mut command = std::process::Command::new(program);
    command.args(args).stdin(std::process::Stdio::null());
    if scoped {
        scope_before_exec(&mut command);
    }
    let out = command.output().map_err(|err| err.to_string())?;
    if out.status.success() {
        return Ok(());
    }
    let said = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Err(if said.is_empty() {
        format!("exit {}", out.status)
    } else {
        said
    })
}

/// Makes `command` enter the Landlock scope for abstract sockets just
/// before it execs.
#[cfg(target_os = "linux")]
fn scope_before_exec(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: the hook makes raw syscalls only, which is allowed between
    // fork and exec.
    unsafe {
        command.pre_exec(landlock_scope::scope_abstract_sockets);
    }
}

#[cfg(not(target_os = "linux"))]
fn scope_before_exec(_: &mut std::process::Command) {}

/// Where `program` is on `PATH` (or `program` itself when it is a path).
fn find_program(program: &str) -> Option<PathBuf> {
    if program.contains(std::path::MAIN_SEPARATOR) {
        return Some(PathBuf::from(program));
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

/// The note shown to the user once per session when commands can't be
/// confined, or not fully; `None` when they are, or the user turned the
/// sandbox off.
pub fn unconfined_notice(settings: &SandboxSettings) -> Option<String> {
    notice_for(settings, mechanism())
}

fn notice_for(settings: &SandboxSettings, mechanism: &Mechanism) -> Option<String> {
    if !settings.enabled {
        return None;
    }
    match mechanism {
        Mechanism::Unavailable(why) => Some(format!(
            "Commands gz runs for the model have no OS limits on this machine: {why}. \
             They can change any file you can."
        )),
        Mechanism::Bubblewrap {
            abstract_sockets: Err(why),
            ..
        } => Some(format!(
            "Commands gz runs for the model can still connect to abstract unix sockets \
             (ones with no file, such as some X11 and D-Bus sockets): {why}."
        )),
        Mechanism::Seatbelt | Mechanism::Bubblewrap { .. } => None,
    }
}

/// What a confined command may touch, by real path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rules {
    pub writable: Vec<PathBuf>,
    /// Neither read nor written.
    pub hidden: Vec<PathBuf>,
    /// Readable again, inside `hidden`.
    pub readable: Vec<PathBuf>,
    /// Not writable, though inside `writable`: the hooks of each repo in
    /// the open folders, which git runs at the user's next commit, outside
    /// the sandbox, and which show in neither `git status` nor diffs.
    pub protected: Vec<PathBuf>,
    pub network: bool,
    /// Unix sockets, or folders of them, commands may connect to; all
    /// others are closed.
    pub sockets: Vec<PathBuf>,
    /// Whether commands may reach the macOS keychain service.
    pub keychain: bool,
}

/// What [`Rules::new`] needs to know about where a command starts.
pub struct Place<'a> {
    pub roots: &'a [PathBuf],
    pub home: Option<&'a Path>,
    /// The repos in `roots`.
    pub git_dirs: &'a [GitDir],
    /// gz's socket folder, the command's `TMPDIR`.
    pub socket_folder: &'a Path,
}

impl Rules {
    /// The rules for a command started now at `place`.
    pub fn new(place: &Place, settings: &SandboxSettings) -> Self {
        let home = place.home;
        let mut writable: Vec<PathBuf> = place.roots.to_vec();
        writable.extend(temp_folders());
        if let Some(home) = home {
            writable.extend(TOOL_CACHES.iter().map(|cache| real(&home.join(cache))));
        }
        writable.extend(settings.writable.iter().cloned());
        dedup(&mut writable);
        let under_home = |paths: &[&str]| -> Vec<PathBuf> {
            home.map(|home| paths.iter().map(|path| real(&home.join(path))).collect())
                .unwrap_or_default()
        };
        let hidden = under_home(CREDENTIAL_PATHS);

        let mut sockets: Vec<PathBuf> = place.roots.to_vec();
        sockets.push(place.socket_folder.to_path_buf());
        sockets.extend(sockets::SYSTEM_SOCKETS.iter().map(PathBuf::from));
        sockets.extend(settings.sockets.iter().cloned());
        let mut readable = settings.readable.clone();
        if let Some(ssh) = &settings.ssh {
            readable.extend(under_home(&[SSH_DIR]));
            sockets.extend(ssh.agents.iter().cloned());
        }
        if settings.keychain {
            readable.extend(under_home(&[KEYCHAIN_DIR]));
        }
        dedup(&mut sockets);
        // A socket the user allowed inside a credential store (Docker
        // Desktop's is in ~/.docker) must be reachable as a file too.
        let opened_inside_hidden: Vec<PathBuf> = settings
            .sockets
            .iter()
            .chain(settings.ssh.iter().flat_map(|ssh| &ssh.agents))
            .filter(|socket| hidden.iter().any(|dir| socket.starts_with(dir)))
            .cloned()
            .collect();
        readable.extend(opened_inside_hidden);
        dedup(&mut readable);
        // Missing hooks folders too: the profile denies creating them.
        let protected = place.git_dirs.iter().map(|dir| dir.hooks.clone()).collect();
        Self {
            writable,
            hidden,
            readable,
            protected,
            network: settings.network,
            sockets,
            keychain: settings.keychain,
        }
    }

    /// The credential store `path` is in, when commands can't read it.
    pub fn hidden_store_of(&self, path: &Path) -> Option<&PathBuf> {
        if self.readable.iter().any(|open| path.starts_with(open)) {
            return None;
        }
        self.hidden.iter().find(|store| path.starts_with(store))
    }

    /// Whether commands may connect to the unix socket at `path`.
    pub fn may_connect(&self, path: &Path) -> bool {
        self.sockets.iter().any(|open| path.starts_with(open))
    }

    /// The sockets of `bound` that bubblewrap must cover, by real path:
    /// not allowed, not already under a hidden folder (which is empty in
    /// the sandbox, so there is nothing to cover), and not in the fresh
    /// `/dev` and `/proc` bubblewrap makes.
    pub fn sockets_to_cover(&self, bound: &[PathBuf]) -> Vec<PathBuf> {
        let mut covered: Vec<PathBuf> = bound
            .iter()
            .filter(|socket| sockets::is_socket(socket))
            .map(|socket| real(socket))
            .filter(|socket| !socket.starts_with("/dev") && !socket.starts_with("/proc"))
            .filter(|socket| !self.may_connect(socket))
            .filter(|socket| self.hidden_store_of(socket).is_none())
            .collect();
        dedup(&mut covered);
        covered
    }
}

/// The system temp folders and `$TMPDIR`, by real path.
pub fn temp_folders() -> Vec<PathBuf> {
    let mut folders: Vec<PathBuf> = SYSTEM_TEMP.iter().map(PathBuf::from).collect();
    let own = real(&std::env::temp_dir());
    // On macOS `$TMPDIR` is inside `/private/var/folders` already.
    if !folders.iter().any(|folder| own.starts_with(folder)) {
        folders.push(own);
    }
    folders
}

/// The real path when it exists; the path as given otherwise (it may be
/// created later, e.g. a tool cache on first use).
fn real(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn dedup(paths: &mut Vec<PathBuf>) {
    let mut seen = std::collections::HashSet::new();
    paths.retain(|path| seen.insert(path.clone()));
}

/// A `sandbox-exec` profile for `rules`. Later rules win, so each allow
/// follows the deny it carves out of.
pub fn seatbelt_profile(rules: &Rules) -> String {
    let mut profile = String::from("(version 1)\n(allow default)\n(deny file-write*)\n");
    profile.push_str("(allow file-write*\n");
    for path in &rules.writable {
        profile.push_str(&format!("  (subpath {})\n", seatbelt_string(path)));
    }
    // Where shells and tools send output they don't keep.
    profile.push_str(concat!(
        "  (literal \"/dev/null\") (literal \"/dev/zero\") (literal \"/dev/dtracehelper\")\n",
        "  (literal \"/dev/stdout\") (literal \"/dev/stderr\")\n",
        "  (regex #\"^/dev/tty\") (regex #\"^/dev/fd/\"))\n",
    ));
    if !rules.protected.is_empty() {
        let protected = subpaths(&rules.protected);
        profile.push_str(&format!("(deny file-write*{protected})\n"));
    }
    // Apple events would let a command ask another app (Terminal, Finder,
    // System Events) to act for it, and LaunchServices (`open`) would start
    // an app, one the command just wrote included: both run outside the
    // sandbox.
    profile.push_str("(deny appleevent-send)\n(deny lsopen)\n");
    if !rules.hidden.is_empty() {
        let hidden = subpaths(&rules.hidden);
        profile.push_str(&format!("(deny file-read* file-write*{hidden})\n"));
    }
    if !rules.readable.is_empty() {
        let readable = subpaths(&rules.readable);
        profile.push_str(&format!("(allow file-read*{readable})\n"));
    }
    if !rules.network {
        // Loopback stays open: local servers, and run_script's bridge to gz.
        profile.push_str(concat!(
            "(deny network-outbound)\n",
            "(allow network-outbound (remote ip \"localhost:*\"))\n",
        ));
    }
    // Seatbelt matches a socket by its real path, so a symlink or hard link
    // to a closed socket from an open folder stays closed.
    profile.push_str("(deny network-outbound (remote unix-socket))\n");
    if !rules.sockets.is_empty() {
        let open: String = rules
            .sockets
            .iter()
            .map(|path| format!(" (remote unix-socket (subpath {}))", seatbelt_string(path)))
            .collect();
        profile.push_str(&format!("(allow network-outbound{open})\n"));
    }
    if !rules.keychain {
        let services: String = KEYCHAIN_SERVICES
            .iter()
            .map(|name| format!(" (global-name \"{name}\")"))
            .collect();
        profile.push_str(&format!("(deny mach-lookup{services})\n"));
    }
    profile
}

fn subpaths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| format!(" (subpath {})", seatbelt_string(path)))
        .collect()
}

/// `path` as a profile string literal: backslashes and quotes escaped, so a
/// folder name can't end the string and add rules of its own.
fn seatbelt_string(path: &Path) -> String {
    let text = path.to_string_lossy();
    let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// `bwrap` arguments for `rules`, up to the command. `covered` are the
/// sockets to put `/dev/null` over (from [`Rules::sockets_to_cover`]).
/// Only existing paths are bound, since bwrap can't mount over a missing
/// one. A separate PID namespace keeps the user's other processes out of
/// reach: a command can't signal them, or look through their `/proc`.
pub fn bwrap_args(rules: &Rules, covered: &[PathBuf]) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc"]
        .map(OsString::from)
        .to_vec();
    fn bind(args: &mut Vec<OsString>, flag: &str, from: &Path, to: &Path) {
        args.extend([flag.into(), from.into(), to.into()]);
    }
    let null = Path::new("/dev/null");
    for path in rules.writable.iter().filter(|p| p.exists()) {
        bind(&mut args, "--bind", path, path);
    }
    for path in rules.protected.iter().filter(|p| p.exists()) {
        bind(&mut args, "--ro-bind", path, path);
    }
    for hidden in rules.hidden.iter().filter(|p| p.exists()) {
        let reopened_whole = rules.readable.iter().any(|r| hidden.starts_with(r));
        if reopened_whole {
            continue;
        }
        if hidden.is_dir() {
            args.extend(["--tmpfs".into(), hidden.into()]);
        } else {
            bind(&mut args, "--ro-bind", null, hidden);
        }
    }
    for path in rules.readable.iter().filter(|p| p.exists()) {
        bind(&mut args, "--ro-bind", path, path);
    }
    for socket in covered {
        bind(&mut args, "--ro-bind", null, socket);
    }
    args.push("--unshare-pid".into());
    if !rules.network {
        args.push("--unshare-net".into());
    }
    args.extend(["--die-with-parent".into(), "--".into()]);
    args
}

/// How a confined command ran, to explain its failures to the model and
/// undo risky git config changes after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confined {
    pub rules: Rules,
    pub home: Option<PathBuf>,
    /// The protected repos' configs as they were when it started.
    pub git: GitWatch,
}

impl Confined {
    /// What to tell the model when a failed `command` (or script) and its
    /// `output` show the sandbox likely stopped it: each rule that did, and
    /// the `/sandbox` command that lifts it. Paths come from both, since
    /// some tools fail on a closed socket without naming it.
    pub fn blocked_note(&self, command: &str, output: &str) -> Option<String> {
        let lower = output.to_lowercase();
        let shows = |signs: &[&str]| signs.iter().any(|sign| lower.contains(sign));
        let mut causes: Vec<String> = Vec::new();
        if !self.rules.network && shows(OFFLINE_SIGNS) {
            causes.push(
                "the network is off for commands; /sandbox network on turns it on".to_string(),
            );
        }
        let mut paths = sockets::paths_in(command);
        paths.extend(sockets::paths_in(output));
        for path in paths {
            causes.extend(self.cause_for_path(Path::new(&path)));
        }
        // A command can reach the agent by `$SSH_AUTH_SOCK` without its
        // path showing anywhere.
        let uses_agent = command.contains(AGENT_VARIABLE);
        if self.ssh_closed() && (shows(SSH_SIGNS) || uses_agent) {
            causes.push(SSH_CAUSE.to_string());
        }
        if cfg!(target_os = "macos") && !self.rules.keychain && shows(KEYCHAIN_SIGNS) {
            causes.push(KEYCHAIN_CAUSE.to_string());
        }
        let mut seen = std::collections::HashSet::new();
        causes.retain(|cause| seen.insert(cause.clone()));
        if causes.is_empty() {
            return shows(DENIAL_SIGNS).then(|| GENERIC_NOTE.to_string());
        }
        Some(format!(
            "[gz's sandbox blocked this: {}. Do not try another way around it; tell the user \
             what was blocked and the command that allows it.]",
            causes.join("; ")
        ))
    }

    fn ssh_closed(&self) -> bool {
        self.home.as_ref().is_some_and(|home| {
            self.rules
                .hidden_store_of(&real(&home.join(SSH_DIR)))
                .is_some()
        })
    }

    /// Why the sandbox kept a command from `path`, when it did.
    fn cause_for_path(&self, path: &Path) -> Option<String> {
        let real_path = real(path);
        if sockets::is_socket(&real_path) && !self.rules.may_connect(&real_path) {
            let shown = self.shown(path);
            return Some(format!(
                "{shown} is a unix socket, closed to commands because a connection acts with \
                 the powers of the program behind it; /sandbox allow socket {shown} opens it \
                 for this session"
            ));
        }
        let store = self.rules.hidden_store_of(&real_path)?;
        let ssh_dir = self.home.as_ref().map(|home| real(&home.join(SSH_DIR)));
        if Some(store) == ssh_dir.as_ref() {
            return Some(SSH_CAUSE.to_string());
        }
        let shown = self.shown(store);
        Some(format!(
            "{shown} is a credential store hidden from commands; /sandbox allow-read {shown} \
             opens it"
        ))
    }

    /// `path` with home as `~`.
    fn shown(&self, path: &Path) -> String {
        match self
            .home
            .as_ref()
            .and_then(|home| path.strip_prefix(home).ok())
        {
            Some(rest) => format!("~/{}", rest.display()),
            None => path.display().to_string(),
        }
    }
}

/// Starts the model's commands under the sandbox: the open folders are
/// read at each start, so `/add-dir` and `/sandbox` changes apply to the
/// next command.
#[derive(Clone)]
pub struct Confinement {
    folders: Sandbox,
    settings: OsSandbox,
    /// Notices for the user about git configs put back after a foreground
    /// command or a script, until the session shows them.
    user_notices: Arc<Mutex<Vec<String>>>,
}

impl Confinement {
    pub fn new(folders: Sandbox, settings: OsSandbox) -> Self {
        Self {
            folders,
            settings,
            user_notices: Arc::default(),
        }
    }

    /// Queues the user's notice of `reverts` for [`Self::take_user_notices`].
    pub fn tell_user(&self, reverts: &Reverts) {
        if reverts.is_empty() {
            return;
        }
        self.user_notices
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(reverts.user_notice());
    }

    /// The notices queued since the last call.
    pub fn take_user_notices(&self) -> Vec<String> {
        std::mem::take(&mut *self.user_notices.lock().unwrap_or_else(|p| p.into_inner()))
    }

    /// A command running `program` (arguments follow), inside the sandbox
    /// when it is on and this machine has one. `Confined` is `None` when it
    /// runs without limits. A missing `program` is `NotFound`, as from a
    /// plain spawn.
    pub fn command(&self, program: &str) -> std::io::Result<(Command, Option<Confined>)> {
        let settings = self.settings.settings();
        let mechanism = mechanism();
        if !settings.enabled || matches!(mechanism, Mechanism::Unavailable(_)) {
            return Ok((Command::new(program), None));
        }
        let Some(program) = find_program(program) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("{program} is not on PATH"),
            ));
        };
        let socket_folder = sockets::socket_folder().map_err(std::io::Error::other)?;
        // Found once per command and shared, so the walk runs once.
        let roots = self.folders.roots();
        let git_dirs = git_guard::find_git_dirs(&roots);
        let place = Place {
            roots: &roots,
            home: self.folders.home(),
            git_dirs: &git_dirs,
            socket_folder: &socket_folder,
        };
        let rules = Rules::new(&place, &settings);
        let mut command = match mechanism {
            Mechanism::Seatbelt => {
                let mut command = Command::new(SANDBOX_EXEC);
                command.arg("-p").arg(seatbelt_profile(&rules));
                command
            }
            Mechanism::Bubblewrap {
                program: bwrap,
                abstract_sockets,
            } => {
                let mut command = Command::new(bwrap);
                command.args(bwrap_args(
                    &rules,
                    &rules.sockets_to_cover(&sockets::bound_sockets()),
                ));
                if abstract_sockets.is_ok() {
                    scope_tokio_before_exec(&mut command);
                }
                command
            }
            Mechanism::Unavailable(_) => return Ok((Command::new(program), None)),
        };
        // Sockets a command makes in its temp folder are ones it may reach.
        command.arg(program).env("TMPDIR", &socket_folder);
        let confined = Confined {
            rules,
            home: self.folders.home().map(Path::to_path_buf),
            git: GitWatch::save(&git_dirs, &roots),
        };
        Ok((command, Some(confined)))
    }
}

#[cfg(target_os = "linux")]
fn scope_tokio_before_exec(command: &mut Command) {
    // SAFETY: the hook makes raw syscalls only, which is allowed between
    // fork and exec.
    unsafe {
        command.pre_exec(landlock_scope::scope_abstract_sockets);
    }
}

#[cfg(not(target_os = "linux"))]
fn scope_tokio_before_exec(_: &mut Command) {}

/// A path the user typed in `/sandbox allow…`: `~` is home, relative
/// paths start at `cwd`; it must exist.
pub fn existing_path(typed: &str, cwd: &Path, home: Option<&Path>) -> Result<PathBuf> {
    let typed = typed.trim();
    let candidate = match home_relative(typed)? {
        Some(rest) => {
            let Some(home) = home else {
                bail!("'{typed}': the home folder is unknown; type the full path");
            };
            home.join(rest)
        }
        None => cwd.join(typed),
    };
    candidate
        .canonicalize()
        .with_context(|| format!("{typed} does not exist; create it first or check the path"))
}

/// Checks a path for `/sandbox allow-write`.
pub fn check_writable(path: &Path, home: Option<&Path>) -> Result<()> {
    if path.parent().is_none() {
        bail!(
            "{} is the whole disk; allow a folder instead",
            path.display()
        );
    }
    if let Some(store) = credential_store(path, home) {
        bail!(
            "{} is inside {}, a credential store; commands may not change it",
            path.display(),
            store.display()
        );
    }
    Ok(())
}

/// Checks a path for `/sandbox allow-read`: only credential stores are
/// hidden, so anything else is readable already.
pub fn check_readable(path: &Path, home: Option<&Path>) -> Result<()> {
    if credential_store(path, home).is_none() {
        bail!(
            "{} is readable already: the sandbox hides only the credential stores /sandbox lists",
            path.display(),
        );
    }
    Ok(())
}

/// Checks a path for `/sandbox allow socket`: a unix socket, or a folder
/// of them.
pub fn check_socket(path: &Path) -> Result<()> {
    if path.parent().is_none() {
        bail!(
            "{} is the whole disk; allow one socket instead",
            path.display()
        );
    }
    if !path.is_dir() && !sockets::is_socket(path) {
        bail!(
            "{} is not a unix socket; give the socket's path (start its service first)",
            path.display()
        );
    }
    Ok(())
}

fn credential_store(path: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let home = home?;
    CREDENTIAL_PATHS
        .iter()
        .map(|store| real(&home.join(store)))
        .find(|store| path.starts_with(store))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(network: bool) -> Rules {
        Rules {
            writable: vec![PathBuf::from("/work/app")],
            hidden: vec![PathBuf::from("/home/me/.aws")],
            readable: vec![PathBuf::from("/home/me/.aws/config")],
            protected: vec![PathBuf::from("/work/app/.git/hooks")],
            network,
            sockets: vec![
                PathBuf::from("/work/app"),
                PathBuf::from("/tmp/gz-sandbox-1"),
            ],
            keychain: false,
        }
    }

    fn rules_at(home: &Path, roots: &[PathBuf], settings: &SandboxSettings) -> Rules {
        let place = Place {
            roots,
            home: Some(home),
            git_dirs: &git_guard::find_git_dirs(roots),
            socket_folder: Path::new("/tmp/gz-sandbox-1"),
        };
        Rules::new(&place, settings)
    }

    #[test]
    fn the_profile_allows_writes_only_where_listed_and_hides_credentials() {
        let profile = seatbelt_profile(&rules(true));
        let deny = profile.find("(deny file-write*)").unwrap();
        let allow = profile.find("(subpath \"/work/app\")").unwrap();
        assert!(deny < allow, "the allow must follow the deny:\n{profile}");
        let hide = profile
            .find("(deny file-read* file-write* (subpath \"/home/me/.aws\"))")
            .unwrap();
        let reopen = profile
            .find("(allow file-read* (subpath \"/home/me/.aws/config\"))")
            .unwrap();
        assert!(hide < reopen, "{profile}");
        let hooks = profile
            .find("(deny file-write* (subpath \"/work/app/.git/hooks\"))")
            .unwrap();
        assert!(
            allow < hooks,
            "the hooks deny must follow the allow:\n{profile}"
        );
        assert!(profile.contains("(deny appleevent-send)"), "{profile}");
        assert!(profile.contains("(deny lsopen)"), "{profile}");
        assert!(
            !profile.contains("(deny network-outbound)\n"),
            "network stays on: {profile}"
        );
    }

    #[test]
    fn the_profile_closes_unix_sockets_except_the_allowed_ones() {
        let profile = seatbelt_profile(&rules(true));
        let deny = profile
            .find("(deny network-outbound (remote unix-socket))")
            .unwrap();
        let allow = profile
            .find(
                "(allow network-outbound (remote unix-socket (subpath \"/work/app\")) \
                 (remote unix-socket (subpath \"/tmp/gz-sandbox-1\")))",
            )
            .unwrap();
        assert!(deny < allow, "{profile}");
    }

    #[test]
    fn the_profile_closes_the_keychain_service_unless_allowed() {
        let closed = seatbelt_profile(&rules(true));
        assert!(
            closed.contains(
                "(deny mach-lookup (global-name \"com.apple.SecurityServer\") \
                 (global-name \"com.apple.securityd.xpc\"))"
            ),
            "{closed}"
        );
        let open = seatbelt_profile(&Rules {
            keychain: true,
            ..rules(true)
        });
        assert!(!open.contains("mach-lookup"), "{open}");
    }

    #[test]
    fn network_off_blocks_outbound_except_loopback_and_allowed_sockets() {
        let profile = seatbelt_profile(&rules(false));
        let deny = profile.find("(deny network-outbound)").unwrap();
        let loopback = profile
            .find("(allow network-outbound (remote ip \"localhost:*\"))")
            .unwrap();
        assert!(deny < loopback, "{profile}");
        assert!(
            !profile.contains("(allow network-outbound (remote unix-socket))"),
            "every socket would be open: {profile}"
        );
    }

    #[test]
    fn quotes_and_backslashes_in_paths_cannot_end_the_profile_string() {
        let hostile = Path::new("/w/a\") (allow file-write* (subpath \"/\") \\");
        assert_eq!(
            seatbelt_string(hostile),
            "\"/w/a\\\") (allow file-write* (subpath \\\"/\\\") \\\\\""
        );
    }

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn bwrap_binds_writable_folders_and_cuts_the_network_when_off() {
        let dir = crate::assistant::sandbox::tests::temp_dir("bwrap-args");
        let missing = dir.join("missing");
        let rules = Rules {
            writable: vec![dir.clone(), missing.clone()],
            hidden: vec![],
            readable: vec![],
            protected: vec![dir.clone()],
            network: false,
            sockets: vec![],
            keychain: false,
        };
        let args = strings(&bwrap_args(&rules, &[]));
        let d = dir.to_string_lossy().into_owned();
        let bound = args.windows(3).position(|w| w == ["--bind", &d, &d]);
        let sealed = args.windows(3).position(|w| w == ["--ro-bind", &d, &d]);
        assert!(
            bound < sealed && bound.is_some(),
            "read-only after: {args:?}"
        );
        assert!(
            !args.contains(&missing.to_string_lossy().into_owned()),
            "{args:?}"
        );
        assert!(args.contains(&"--unshare-net".to_string()));
        assert!(
            args.contains(&"--unshare-pid".to_string()),
            "/proc/<pid>/root of other processes leads around the mounts: {args:?}"
        );
        assert_eq!(args.last().map(String::as_str), Some("--"));
    }

    #[test]
    fn bwrap_covers_hidden_files_and_closed_sockets_with_dev_null() {
        let dir = crate::assistant::sandbox::tests::temp_dir("bwrap-cover");
        let netrc = dir.join(".netrc");
        std::fs::write(&netrc, "machine x").unwrap();
        let aws = dir.join(".aws");
        std::fs::create_dir_all(&aws).unwrap();
        let rules = Rules {
            writable: vec![dir.clone()],
            hidden: vec![netrc.clone(), aws.clone()],
            readable: vec![],
            protected: vec![],
            network: true,
            sockets: vec![],
            keychain: false,
        };
        let socket = PathBuf::from("/run/docker.sock");
        let args = strings(&bwrap_args(&rules, std::slice::from_ref(&socket)));
        let (n, a) = (
            netrc.to_string_lossy().into_owned(),
            aws.to_string_lossy().into_owned(),
        );
        let write = args
            .windows(3)
            .position(|w| w == ["--bind", &d(&dir), &d(&dir)]);
        let file = args
            .windows(3)
            .position(|w| w == ["--ro-bind", "/dev/null", &n]);
        let folder = args.windows(2).position(|w| w == ["--tmpfs", &a]);
        let covered = args
            .windows(3)
            .position(|w| w == ["--ro-bind", "/dev/null", "/run/docker.sock"]);
        assert!(
            file.is_some() && folder.is_some() && covered.is_some(),
            "{args:?}"
        );
        assert!(
            write < file && write < covered,
            "covers go on top: {args:?}"
        );
    }

    fn d(path: &Path) -> String {
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn rules_cover_open_folders_temp_caches_and_added_folders() {
        // Not a temp folder, which would be writable as a whole.
        let home = PathBuf::from("/gz-test-home");
        let app = crate::assistant::sandbox::tests::temp_dir("rules-app");
        std::fs::create_dir_all(app.join(".git/hooks")).unwrap();
        let settings = SandboxSettings {
            writable: vec![PathBuf::from("/opt/extra")],
            ..SandboxSettings::default()
        };
        let rules = rules_at(&home, std::slice::from_ref(&app), &settings);
        for expected in [
            app.clone(),
            PathBuf::from("/opt/extra"),
            home.join(".cargo/registry"),
            home.join("go/pkg"),
            home.join("Library/Caches"),
        ] {
            assert!(rules.writable.contains(&expected), "{expected:?}");
        }
        for closed in [".cargo", ".cargo/bin", ".rustup", ".yarn", "go"] {
            let closed = home.join(closed);
            assert!(
                !rules.writable.iter().any(|w| closed.starts_with(w)),
                "{closed:?}"
            );
        }
        assert_eq!(rules.protected, vec![app.join(".git/hooks")]);
        let temp = real(&std::env::temp_dir());
        assert!(
            rules.writable.iter().any(|w| temp.starts_with(w)),
            "$TMPDIR"
        );
        assert!(!rules.writable.contains(&home), "home itself stays closed");
    }

    #[test]
    fn credential_stores_are_hidden_ssh_included() {
        let home = PathBuf::from("/gz-test-home");
        let rules = rules_at(&home, &[], &SandboxSettings::default());
        for store in [
            ".ssh",
            ".aws",
            ".config/gcloud",
            ".azure",
            ".config/gh",
            ".netrc",
            ".git-credentials",
            ".kube",
            ".docker",
            "Library/Keychains",
        ] {
            let path = home.join(store);
            assert!(rules.hidden.contains(&path), "{store}");
            assert_eq!(rules.hidden_store_of(&path.join("x")), Some(&path));
        }
        assert!(rules.readable.is_empty(), "{:?}", rules.readable);
        assert!(!rules.keychain);
    }

    #[test]
    fn sockets_open_only_in_open_folders_the_socket_folder_and_the_system() {
        let home = PathBuf::from("/gz-test-home");
        let app = PathBuf::from("/work/app");
        let rules = rules_at(
            &home,
            std::slice::from_ref(&app),
            &SandboxSettings::default(),
        );
        assert!(rules.may_connect(&app.join("dev.sock")));
        assert!(rules.may_connect(Path::new("/tmp/gz-sandbox-1/pymp-x/listener")));
        for system in sockets::SYSTEM_SOCKETS {
            assert!(rules.may_connect(Path::new(system)), "{system}");
        }
        for closed in [
            "/private/var/run/com.apple.launchd.x/Listeners",
            "/var/run/docker.sock",
            "/private/tmp/tmux-501/default",
        ] {
            assert!(!rules.may_connect(Path::new(closed)), "{closed}");
        }
    }

    #[test]
    fn ssh_and_keychain_grants_open_their_stores_and_agents() {
        let home = PathBuf::from("/gz-test-home");
        let agent = PathBuf::from("/private/tmp/launchd-1/Listeners");
        let inside = home.join(".docker/run/docker.sock");
        let settings = SandboxSettings {
            ssh: Some(SshGrant {
                agents: vec![agent.clone()],
            }),
            keychain: true,
            sockets: vec![inside.clone()],
            ..SandboxSettings::default()
        };
        let rules = rules_at(&home, &[], &settings);
        assert!(rules.may_connect(&agent) && rules.may_connect(&inside));
        assert_eq!(rules.hidden_store_of(&home.join(".ssh/known_hosts")), None);
        assert_eq!(rules.hidden_store_of(&inside), None, "reachable as a file");
        assert!(
            rules
                .hidden_store_of(&home.join(".docker/config.json"))
                .is_some(),
            "the rest of ~/.docker stays hidden"
        );
        assert!(rules.keychain);
    }

    #[cfg(unix)]
    #[test]
    fn only_closed_existing_sockets_outside_hidden_stores_get_covered() {
        let dir = crate::assistant::sandbox::tests::temp_dir("cover");
        let short = PathBuf::from(format!("/tmp/gz-cover-{}", std::process::id()));
        std::fs::create_dir_all(short.join("open")).unwrap();
        std::fs::create_dir_all(short.join("store")).unwrap();
        let bind = |name: &str| {
            let path = short.join(name);
            std::fs::remove_file(&path).ok();
            (
                std::os::unix::net::UnixListener::bind(&path).unwrap(),
                real(&path),
            )
        };
        let (_a, closed) = bind("closed.sock");
        let (_b, open) = bind("open/ok.sock");
        let (_c, hidden) = bind("store/agent.sock");
        let file = dir.join("plain");
        std::fs::write(&file, "").unwrap();
        let rules = Rules {
            writable: vec![],
            hidden: vec![real(&short.join("store"))],
            readable: vec![],
            protected: vec![],
            network: true,
            sockets: vec![real(&short.join("open"))],
            keychain: false,
        };
        let found = [
            closed.clone(),
            open,
            hidden,
            file,
            PathBuf::from("/gone.sock"),
            closed.clone(),
        ];
        assert_eq!(rules.sockets_to_cover(&found), vec![closed]);
        std::fs::remove_dir_all(&short).ok();
    }

    fn confined(home: &Path, rules: Rules) -> Confined {
        Confined {
            rules,
            home: Some(home.to_path_buf()),
            git: GitWatch::default(),
        }
    }

    #[test]
    fn denials_get_a_note_naming_the_sandbox_command() {
        let home = PathBuf::from("/gz-test-home");
        let on = confined(&home, rules_at(&home, &[], &SandboxSettings::default()));
        let note = on
            .blocked_note("", "sh: /x/f: Operation not permitted")
            .unwrap();
        assert!(note.contains("/sandbox allow-write"), "{note}");
        assert_eq!(on.blocked_note("", "error: expected `;`"), None);
        let offline_settings = SandboxSettings {
            network: false,
            ..SandboxSettings::default()
        };
        let offline = confined(&home, rules_at(&home, &[], &offline_settings));
        let note = offline
            .blocked_note("", "curl: (6) Could not resolve host: example.com")
            .unwrap();
        assert!(note.contains("/sandbox network on"), "{note}");
        assert_eq!(on.blocked_note("", "Could not resolve host: x"), None);
    }

    #[test]
    fn a_hidden_store_named_in_the_output_gets_its_own_allow_command() {
        let home = PathBuf::from("/gz-test-home");
        let jail = confined(&home, rules_at(&home, &[], &SandboxSettings::default()));
        let note = jail
            .blocked_note(
                "",
                "cat: /gz-test-home/.config/gcloud/credentials.db: Operation not permitted",
            )
            .unwrap();
        assert!(
            note.contains("~/.config/gcloud is a credential store")
                && note.contains("/sandbox allow-read ~/.config/gcloud"),
            "{note}"
        );
        let note = jail
            .blocked_note(
                "",
                "cat: /gz-test-home/.ssh/known_hosts: Operation not permitted",
            )
            .unwrap();
        assert!(note.contains("/sandbox allow ssh"), "{note}");
    }

    #[test]
    fn ssh_failures_point_at_allow_ssh_until_it_is_given() {
        let home = PathBuf::from("/gz-test-home");
        let output = "Host key verification failed.\nfatal: Could not read from remote repository.";
        let closed = confined(&home, rules_at(&home, &[], &SandboxSettings::default()));
        let note = closed.blocked_note("", output).unwrap();
        assert!(note.contains("/sandbox allow ssh"), "{note}");
        let granted = SandboxSettings {
            ssh: Some(SshGrant::default()),
            ..SandboxSettings::default()
        };
        let open = confined(&home, rules_at(&home, &[], &granted));
        assert_eq!(
            open.blocked_note("", output),
            None,
            "a real host key problem"
        );
        let by_variable = closed
            .blocked_note(
                "python3 agent.py \"$SSH_AUTH_SOCK\"",
                "PermissionError: [Errno 1] Operation not permitted",
            )
            .unwrap();
        assert!(by_variable.contains("/sandbox allow ssh"), "{by_variable}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn keychain_failures_point_at_allow_keychain() {
        let home = PathBuf::from("/gz-test-home");
        let jail = confined(&home, rules_at(&home, &[], &SandboxSettings::default()));
        let output = "fatal: could not read Username for 'https://github.com': \
                      terminal prompts disabled";
        let note = jail.blocked_note("", output).unwrap();
        assert!(note.contains("/sandbox allow keychain"), "{note}");
    }

    #[cfg(unix)]
    #[test]
    fn a_closed_socket_named_in_the_output_gets_allow_socket() {
        let short = PathBuf::from(format!("/tmp/gz-note-{}", std::process::id()));
        std::fs::create_dir_all(&short).unwrap();
        let socket = short.join("docker.sock");
        std::fs::remove_file(&socket).ok();
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let home = PathBuf::from("/gz-test-home");
        let jail = confined(&home, rules_at(&home, &[], &SandboxSettings::default()));
        let output = format!(
            "permission denied while trying to connect to the docker API at unix://{}",
            socket.display()
        );
        let note = jail.blocked_note("", &output).unwrap();
        assert!(
            note.contains(&format!("/sandbox allow socket {}", socket.display())),
            "{note}"
        );
        let silent = jail
            .blocked_note(&format!("nc -U {}", socket.display()), "")
            .unwrap();
        assert!(
            silent.contains("/sandbox allow socket"),
            "from the command: {silent}"
        );
        let allowed = SandboxSettings {
            sockets: vec![real(&socket)],
            ..SandboxSettings::default()
        };
        let open = confined(&home, rules_at(&home, &[], &allowed));
        assert_eq!(open.blocked_note("", &output), None);
        std::fs::remove_dir_all(&short).ok();
    }

    #[test]
    fn only_an_enabled_sandbox_without_a_mechanism_gets_the_notice() {
        let missing = Mechanism::Unavailable("bubblewrap is not installed".into());
        let note = notice_for(&SandboxSettings::default(), &missing).unwrap();
        assert!(
            note.contains("no OS limits") && note.contains("bubblewrap"),
            "{note}"
        );
        let off = SandboxSettings {
            enabled: false,
            ..SandboxSettings::default()
        };
        assert_eq!(notice_for(&off, &missing), None);
        assert_eq!(
            notice_for(&SandboxSettings::default(), &Mechanism::Seatbelt),
            None
        );
    }

    #[test]
    fn bubblewrap_without_landlock_scoping_tells_the_user_why() {
        let open = Mechanism::Bubblewrap {
            program: PathBuf::from("/usr/bin/bwrap"),
            abstract_sockets: Err("this kernel's Landlock (ABI 4) can't scope them".into()),
        };
        let note = notice_for(&SandboxSettings::default(), &open).unwrap();
        assert!(
            note.contains("abstract unix sockets") && note.contains("ABI 4"),
            "{note}"
        );
        let scoped = Mechanism::Bubblewrap {
            program: PathBuf::from("/usr/bin/bwrap"),
            abstract_sockets: Ok(()),
        };
        assert_eq!(notice_for(&SandboxSettings::default(), &scoped), None);
    }

    #[test]
    fn added_paths_are_numbered_writes_first_and_forgotten_by_number() {
        let mut settings = SandboxSettings {
            writable: vec![PathBuf::from("/a")],
            readable: vec![PathBuf::from("/h/.aws/config")],
            ..SandboxSettings::default()
        };
        assert_eq!(
            settings.forget(2).unwrap(),
            Added::Read(PathBuf::from("/h/.aws/config"))
        );
        assert!(settings.forget(2).is_err());
        assert_eq!(
            settings.forget(1).unwrap(),
            Added::Write(PathBuf::from("/a"))
        );
        assert!(settings.is_default());
    }

    #[test]
    fn session_grants_are_numbered_after_paths_and_never_saved() {
        let mut settings = SandboxSettings {
            writable: vec![PathBuf::from("/a")],
            ssh: Some(SshGrant::default()),
            keychain: true,
            sockets: vec![PathBuf::from("/run/docker.sock")],
            ..SandboxSettings::default()
        };
        assert_eq!(
            settings.added()[1..],
            [
                Added::Ssh(SshGrant::default()),
                Added::Keychain,
                Added::Socket(PathBuf::from("/run/docker.sock")),
            ]
        );
        let text = toml::to_string(&settings).unwrap();
        assert!(
            !text.contains("ssh") && !text.contains("docker") && !text.contains("keychain"),
            "{text}"
        );
        settings.forget(1).unwrap();
        assert!(settings.is_default(), "grants alone are nothing to save");
        assert_eq!(settings.forget(1).unwrap(), Added::Ssh(SshGrant::default()));
        assert_eq!(settings.forget(1).unwrap(), Added::Keychain);
        assert_eq!(
            settings.forget(1).unwrap(),
            Added::Socket(PathBuf::from("/run/docker.sock"))
        );
        assert_eq!(settings, SandboxSettings::default());
    }

    #[test]
    fn settings_save_only_what_differs_from_the_defaults() {
        let text = toml::to_string(&SandboxSettings {
            network: false,
            ..SandboxSettings::default()
        })
        .unwrap();
        assert_eq!(text.trim(), "enabled = true\nnetwork = false");
        let read: SandboxSettings = toml::from_str("network = false").unwrap();
        assert!(read.enabled, "missing fields take the defaults");
        assert!(!read.network);
    }

    #[test]
    fn the_profile_keeps_sandbox_settings_next_to_the_others() {
        let mut config: crate::config::Config =
            toml::from_str("[profiles.p]\nsite = \"acme\"\n").unwrap();
        let settings = SandboxSettings {
            network: false,
            writable: vec![PathBuf::from("/opt/extra")],
            ..SandboxSettings::default()
        };
        config.profile_mut("p").code_sandbox = Some(settings.clone());
        let saved = toml::to_string_pretty(&config).unwrap();
        let read: crate::config::Config = toml::from_str(&saved).unwrap();
        assert_eq!(read.profile("p").code_sandbox, Some(settings), "{saved}");
        assert_eq!(read.profile("p").site.as_deref(), Some("acme"));
    }

    #[test]
    fn allow_read_takes_only_paths_in_credential_stores() {
        let home = crate::assistant::sandbox::tests::temp_dir("allow-read");
        std::fs::create_dir_all(home.join(".aws")).unwrap();
        std::fs::create_dir_all(home.join("code")).unwrap();
        std::fs::write(home.join(".netrc"), "").unwrap();
        let aws = existing_path("~/.aws", &home, Some(&home)).unwrap();
        assert!(check_readable(&aws, Some(&home)).is_ok());
        assert!(check_writable(&aws, Some(&home)).is_err());
        let netrc = existing_path("~/.netrc", &home, Some(&home)).unwrap();
        assert!(check_readable(&netrc, Some(&home)).is_ok());
        let code = existing_path("code", &home, Some(&home)).unwrap();
        let why = check_readable(&code, Some(&home)).unwrap_err().to_string();
        assert!(why.contains("readable already"), "{why}");
        assert!(check_writable(&code, Some(&home)).is_ok());
        assert!(existing_path("nope", &home, Some(&home)).is_err());
        assert!(check_writable(Path::new("/"), Some(&home)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn allow_socket_takes_sockets_and_folders_only() {
        let short = PathBuf::from(format!("/tmp/gz-allow-sock-{}", std::process::id()));
        std::fs::create_dir_all(&short).unwrap();
        let socket = short.join("s.sock");
        std::fs::remove_file(&socket).ok();
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let file = short.join("plain");
        std::fs::write(&file, "").unwrap();
        assert!(check_socket(&socket).is_ok());
        assert!(check_socket(&short).is_ok());
        let why = check_socket(&file).unwrap_err().to_string();
        assert!(why.contains("not a unix socket"), "{why}");
        assert!(check_socket(Path::new("/")).is_err());
        std::fs::remove_dir_all(&short).ok();
    }
}

/// Folders and helpers for tests that run real commands through the real
/// sandbox. The folders live under the build's `target/`, which is neither
/// a temp folder nor a cache, so a write there is allowed only because of
/// the rule under test.
#[cfg(all(test, unix))]
mod test_support {
    use super::*;
    use crate::assistant::jobs::Jobs;
    use std::time::Duration;

    pub struct Place {
        pub base: PathBuf,
        pub work: PathBuf,
        pub outside: PathBuf,
        pub home: PathBuf,
    }

    impl Drop for Place {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.base).ok();
        }
    }

    pub fn place(name: &str) -> Place {
        let base = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/os-sandbox-tests")
            .join(format!("{}-{name}", std::process::id()));
        let (work, outside, home) = (base.join("work"), base.join("outside"), base.join("home"));
        for dir in [&work, &outside, &home.join(".aws")] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(home.join(".aws/credentials"), "secret").unwrap();
        let base = base.canonicalize().unwrap();
        let temp = real(&std::env::temp_dir());
        assert!(
            !base.starts_with(&temp) && !base.starts_with("/private/tmp"),
            "the test folders must not be temp folders: {base:?}"
        );
        Place {
            work: base.join("work"),
            outside: base.join("outside"),
            home: base.join("home"),
            base,
        }
    }

    pub fn confined_jobs(place: &Place, settings: SandboxSettings) -> Jobs {
        let folders = Sandbox::with_home(&place.work, Some(&place.home)).unwrap();
        let confinement = Confinement::new(folders, OsSandbox::unsaved(settings));
        Jobs::new().0.confined(confinement)
    }

    pub async fn run(
        jobs: &Jobs,
        place: &Place,
        command: &str,
    ) -> crate::assistant::tools::ToolOutput {
        jobs.run(command, &place.work, Duration::from_secs(20), false, false)
            .await
    }

    /// A command that connects to the unix socket at `path` and prints
    /// what it answered.
    pub fn connect_command(path: &Path) -> String {
        format!(
            "python3 -c 'import socket,sys; s=socket.socket(socket.AF_UNIX); \
             s.connect(sys.argv[1]); print(s.recv(5).decode())' '{}'",
            path.display()
        )
    }

    /// A unix socket at `path` that says "hello" to each connection.
    pub fn greeting_socket(path: &Path) -> std::thread::JoinHandle<()> {
        std::fs::remove_file(path).ok();
        let listener = std::os::unix::net::UnixListener::bind(path).unwrap();
        std::thread::spawn(move || {
            use std::io::Write;
            for mut stream in listener.incoming().flatten() {
                stream.write_all(b"hello").ok();
            }
        })
    }
}

/// Real commands through the real macOS sandbox.
#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    use super::test_support::*;
    use super::*;
    use crate::assistant::jobs::{Jobs, State};
    use std::time::Duration;

    #[tokio::test]
    async fn a_command_writes_inside_the_open_folder_but_not_outside_it() {
        let place = place("writes");
        let jobs = confined_jobs(&place, SandboxSettings::default());
        let inside = run(&jobs, &place, "echo hi > made.txt && cat made.txt").await;
        assert!(!inside.is_error, "{}", inside.output);
        assert_eq!(
            std::fs::read_to_string(place.work.join("made.txt")).unwrap(),
            "hi\n"
        );

        let target = place.outside.join("escaped.txt");
        let outside = run(&jobs, &place, &format!("echo hi > '{}'", target.display())).await;
        assert!(outside.is_error, "{}", outside.output);
        assert!(!target.exists());
        assert!(
            outside.output.contains("/sandbox allow-write"),
            "{}",
            outside.output
        );
    }

    #[tokio::test]
    async fn an_allowed_folder_and_sandbox_off_let_the_write_through() {
        let place = place("allowed");
        let target = place.outside.join("ok.txt");
        let command = format!("echo hi > '{}'", target.display());
        let added = SandboxSettings {
            writable: vec![place.outside.clone()],
            ..SandboxSettings::default()
        };
        let out = run(&confined_jobs(&place, added), &place, &command).await;
        assert!(!out.is_error && target.exists(), "{}", out.output);
        std::fs::remove_file(&target).unwrap();
        let off = SandboxSettings {
            enabled: false,
            ..SandboxSettings::default()
        };
        let out = run(&confined_jobs(&place, off), &place, &command).await;
        assert!(!out.is_error && target.exists(), "{}", out.output);
    }

    #[tokio::test]
    async fn credential_folders_under_home_cannot_be_read_unless_allowed() {
        let place = place("creds");
        let command = format!("cat '{}'", place.home.join(".aws/credentials").display());
        let jobs = confined_jobs(&place, SandboxSettings::default());
        let out = run(&jobs, &place, &command).await;
        assert!(
            out.is_error && !out.output.contains("secret"),
            "{}",
            out.output
        );
        assert!(out.output.contains("/sandbox"), "{}", out.output);
        let reopened = SandboxSettings {
            readable: vec![place.home.join(".aws")],
            ..SandboxSettings::default()
        };
        let out = run(&confined_jobs(&place, reopened), &place, &command).await;
        assert!(out.output.contains("secret"), "{}", out.output);
    }

    #[tokio::test]
    async fn a_background_command_is_confined_too() {
        let place = place("background");
        let jobs = confined_jobs(&place, SandboxSettings::default());
        let target = place.outside.join("bg.txt");
        let command = format!("sleep 0.2; echo hi > '{}'", target.display());
        jobs.run(&command, &place.work, Duration::from_secs(5), true, true)
            .await;
        let end = jobs.output("bg_1", Duration::from_secs(10)).await;
        assert!(end.is_error && !target.exists(), "{}", end.output);
        assert!(
            end.output.contains("/sandbox allow-write"),
            "{}",
            end.output
        );
    }

    #[tokio::test]
    async fn the_users_own_command_is_not_confined() {
        let place = place("user");
        let jobs = confined_jobs(&place, SandboxSettings::default());
        let target = place.outside.join("mine.txt");
        let id = jobs
            .start_for_user(&format!("echo hi > '{}'", target.display()), &place.work)
            .unwrap();
        let mut state = jobs.state_of(&id);
        for _ in 0..200 {
            if state != Some(State::Running) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
            state = jobs.state_of(&id);
        }
        assert_eq!(state, Some(State::Exited(Some(0))));
        assert!(target.exists());
    }

    #[tokio::test]
    async fn tool_caches_are_writable_but_their_program_folders_are_not() {
        let place = place("caches");
        std::fs::create_dir_all(place.home.join(".cargo/bin")).unwrap();
        std::fs::create_dir_all(place.home.join(".rustup")).unwrap();
        let jobs = confined_jobs(&place, SandboxSettings::default());
        let cache = place.home.join(".cargo/registry/cache/x.crate");
        let out = run(
            &jobs,
            &place,
            &format!(
                "mkdir -p '{}' && echo c > '{}'",
                cache.parent().unwrap().display(),
                cache.display()
            ),
        )
        .await;
        assert!(!out.is_error && cache.exists(), "{}", out.output);
        for planted in [".cargo/bin/x", ".cargo/env", ".rustup/x"] {
            let planted = place.home.join(planted);
            let out = run(&jobs, &place, &format!("echo x > '{}'", planted.display())).await;
            assert!(
                out.is_error && !planted.exists(),
                "{planted:?}: {}",
                out.output
            );
        }
    }

    #[tokio::test]
    async fn git_hooks_in_the_open_folder_are_closed_but_commits_work() {
        let place = place("hooks");
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(&place.work)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        };
        git(&["init", "-q"]);
        let jobs = confined_jobs(&place, SandboxSettings::default());
        let hook = place.work.join(".git/hooks/pre-commit");
        let out = run(&jobs, &place, "echo 'touch pwned' > .git/hooks/pre-commit").await;
        assert!(out.is_error && !hook.exists(), "{}", out.output);
        let commit = "echo a > a.txt && git add a.txt && git -c user.name=t \
                      -c user.email=t@example.com commit -qm first && git log --oneline";
        let out = run(&jobs, &place, commit).await;
        assert!(
            !out.is_error && out.output.contains("first"),
            "{}",
            out.output
        );
        let out = run(
            &jobs,
            &place,
            "git config gz.check yes && git config gz.check",
        )
        .await;
        assert!(
            out.output.contains("yes"),
            ".git/config stays writable: {}",
            out.output
        );
    }

    fn git_init(folder: &Path) {
        std::fs::create_dir_all(folder).unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(folder)
            .status()
            .unwrap();
        assert!(status.success(), "git init in {folder:?}");
    }

    fn git_get(folder: &Path, key: &str) -> Option<String> {
        let out = std::process::Command::new("git")
            .args(["config", "--local", "--get", key])
            .current_dir(folder)
            .output()
            .unwrap();
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    #[tokio::test]
    async fn a_command_setting_a_program_in_git_config_is_reverted_and_reported() {
        let place = place("git-config-risky");
        git_init(&place.work);
        let folders = Sandbox::with_home(&place.work, Some(&place.home)).unwrap();
        let confinement = Confinement::new(folders, OsSandbox::default());
        let jobs = Jobs::new().0.confined(confinement.clone());
        let out = run(
            &jobs,
            &place,
            "git config core.hooksPath x && git remote add origin https://example.com/r.git",
        )
        .await;
        assert_eq!(git_get(&place.work, "core.hooksPath"), None, "reverted");
        assert!(
            out.output.contains("core.hooksPath") && out.output.contains("!command"),
            "the model is told: {}",
            out.output
        );
        assert_eq!(git_get(&place.work, "remote.origin.url"), None);
        assert!(
            out.output.contains("remote.origin.url"),
            "the model hears which safe change the restore undid: {}",
            out.output
        );
        let notices = confinement.take_user_notices();
        assert!(
            matches!(&notices[..], [n] if n.contains("core.hooksPath") && n.contains("!command")),
            "the user is told: {notices:?}"
        );
    }

    #[tokio::test]
    async fn a_command_setting_other_git_config_keeps_it() {
        let place = place("git-config-kept");
        git_init(&place.work);
        let folders = Sandbox::with_home(&place.work, Some(&place.home)).unwrap();
        let confinement = Confinement::new(folders, OsSandbox::default());
        let jobs = Jobs::new().0.confined(confinement.clone());
        let out = run(
            &jobs,
            &place,
            "git config user.name y && git remote add origin https://example.com/r.git",
        )
        .await;
        assert!(!out.is_error, "{}", out.output);
        assert_eq!(git_get(&place.work, "user.name").as_deref(), Some("y"));
        assert!(git_get(&place.work, "remote.origin.url").is_some());
        assert!(!out.output.contains("gz restored"), "{}", out.output);
        assert!(confinement.take_user_notices().is_empty());
    }

    #[tokio::test]
    async fn a_background_command_reports_its_git_config_revert_when_it_ends() {
        let place = place("git-config-bg");
        git_init(&place.work);
        let folders = Sandbox::with_home(&place.work, Some(&place.home)).unwrap();
        let (jobs, mut notices) = Jobs::new();
        let jobs = jobs.confined(Confinement::new(folders, OsSandbox::default()));
        jobs.run(
            "sleep 0.2; git config core.fsmonitor ./x",
            &place.work,
            Duration::from_secs(5),
            true,
            true,
        )
        .await;
        let finished = tokio::time::timeout(Duration::from_secs(10), notices.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(git_get(&place.work, "core.fsmonitor"), None);
        assert!(
            finished
                .git_reverts
                .user_notice()
                .contains("core.fsmonitor"),
            "{finished:?}"
        );
        assert!(finished.message().contains("core.fsmonitor"));
        let out = jobs.output("bg_1", Duration::ZERO).await;
        assert!(out.output.contains("core.fsmonitor"), "{}", out.output);
    }

    #[tokio::test]
    async fn a_repo_the_command_creates_loses_keys_that_run_programs() {
        let place = place("new-repo-config");
        let folders = Sandbox::with_home(&place.work, Some(&place.home)).unwrap();
        let confinement = Confinement::new(folders, OsSandbox::default());
        let jobs = Jobs::new().0.confined(confinement.clone());
        let out = run(
            &jobs,
            &place,
            "git init -q sub && git -C sub config core.hooksPath x && git -C sub config user.name y",
        )
        .await;
        assert!(
            !out.is_error,
            "git init works in the sandbox: {}",
            out.output
        );
        let sub = place.work.join("sub");
        assert_eq!(git_get(&sub, "core.hooksPath"), None, "{}", out.output);
        assert_eq!(
            git_get(&sub, "user.name").as_deref(),
            Some("y"),
            "the rest stays"
        );
        assert!(
            out.output.contains("core.hooksPath") && out.output.contains("!command"),
            "{}",
            out.output
        );
        let notices = confinement.take_user_notices();
        assert!(
            matches!(&notices[..], [n] if n.contains("core.hooksPath") && n.contains("!command")),
            "{notices:?}"
        );
    }

    #[tokio::test]
    async fn a_hook_written_into_a_new_repo_is_removed_but_samples_stay() {
        let place = place("new-repo-hooks");
        let jobs = confined_jobs(&place, SandboxSettings::default());
        let out = run(
            &jobs,
            &place,
            "git init -q sub && echo 'touch pwned' > sub/.git/hooks/pre-commit \
             && echo kept > sub/.git/hooks/mine.sample",
        )
        .await;
        let hooks = place.work.join("sub/.git/hooks");
        assert!(!hooks.join("pre-commit").exists(), "{}", out.output);
        assert!(hooks.join("mine.sample").exists(), "{}", out.output);
        assert!(out.output.contains("the hook pre-commit"), "{}", out.output);
        assert!(!out.output.contains("mine.sample"), "{}", out.output);
    }

    #[tokio::test]
    async fn a_nested_repos_hooks_are_closed_too() {
        let place = place("nested-hooks");
        let nested = place.work.join("services/api");
        git_init(&nested);
        let jobs = confined_jobs(&place, SandboxSettings::default());
        let hook = nested.join(".git/hooks/pre-commit");
        let out = run(
            &jobs,
            &place,
            "echo 'touch pwned' > services/api/.git/hooks/pre-commit",
        )
        .await;
        assert!(out.is_error && !hook.exists(), "{}", out.output);
        let out = run(&jobs, &place, "echo ok > services/api/file.txt").await;
        assert!(
            !out.is_error,
            "the repo itself stays writable: {}",
            out.output
        );
    }

    #[tokio::test]
    async fn a_command_cannot_ask_another_app_to_act_for_it() {
        let place = place("appleevents");
        let jobs = confined_jobs(&place, SandboxSettings::default());
        let made = place.outside.join("made");
        let script = format!(
            "tell application \"Finder\" to make new folder at (POSIX file \"{}\" as alias) \
             with properties {{name:\"made\"}}",
            place.outside.display()
        );
        let out = run(&jobs, &place, &format!("osascript -e '{script}'")).await;
        assert!(out.is_error && !made.exists(), "{}", out.output);
    }

    struct NoHelpers;

    impl crate::assistant::script::HelperHandler for NoHelpers {
        fn handle<'a>(
            &'a mut self,
            _: &'a str,
            _: &'a serde_json::Value,
        ) -> crate::assistant::script::HelperFuture<'a> {
            Box::pin(async { Err("no helpers in this test".to_string()) })
        }
    }

    #[tokio::test]
    async fn a_script_is_confined_like_a_command() {
        use crate::assistant::tools::{Language, ScriptSpec};
        let place = place("script");
        let folders = Sandbox::with_home(&place.work, Some(&place.home)).unwrap();
        let confinement = Confinement::new(folders, OsSandbox::default());
        let target = place.outside.join("script.txt");
        let code = format!(
            "open('inside.txt', 'w').write('ok')\nopen({:?}, 'w').write('no')\n",
            target.to_string_lossy()
        );
        let spec = ScriptSpec {
            language: Language::Python,
            code,
            timeout: Duration::from_secs(20),
        };
        let out = crate::assistant::script::execute(
            &spec,
            &place.work,
            Some(&confinement),
            &mut NoHelpers,
        )
        .await;
        assert!(place.work.join("inside.txt").exists(), "{}", out.output);
        assert!(out.is_error && !target.exists(), "{}", out.output);
        assert!(
            out.output.contains("/sandbox allow-write"),
            "{}",
            out.output
        );
    }

    #[tokio::test]
    async fn with_the_network_off_a_script_still_reaches_gz_on_loopback() {
        use crate::assistant::tools::{Language, ScriptSpec};
        let place = place("script-offline");
        let folders = Sandbox::with_home(&place.work, Some(&place.home)).unwrap();
        let offline = SandboxSettings {
            network: false,
            ..SandboxSettings::default()
        };
        let confinement = Confinement::new(folders, OsSandbox::unsaved(offline));
        let code =
            "import gz\ntry:\n    gz.list_directory()\nexcept gz.ToolError as e:\n    print(e)\n";
        let spec = ScriptSpec {
            language: Language::Python,
            code: code.into(),
            timeout: Duration::from_secs(20),
        };
        let out = crate::assistant::script::execute(
            &spec,
            &place.work,
            Some(&confinement),
            &mut NoHelpers,
        )
        .await;
        assert_eq!(out.output.trim(), "no helpers in this test");
    }

    #[tokio::test]
    async fn a_folder_with_quotes_in_its_name_stays_writable() {
        let place = place("quoted");
        let odd = place.base.join("we\"ird\\name (x)");
        std::fs::create_dir_all(&odd).unwrap();
        let folders = Sandbox::with_home(&odd, Some(&place.home)).unwrap();
        let jobs = Jobs::new()
            .0
            .confined(Confinement::new(folders, OsSandbox::default()));
        let out = jobs
            .run(
                "echo hi > f.txt",
                &odd,
                Duration::from_secs(20),
                false,
                false,
            )
            .await;
        assert!(
            !out.is_error && odd.join("f.txt").exists(),
            "{}",
            out.output
        );
    }

    #[tokio::test]
    async fn sockets_outside_the_open_folders_are_closed_until_allowed() {
        let place = place("sockets");
        let outside = place.outside.join("agent.sock");
        let inside = place.work.join("dev.sock");
        greeting_socket(&outside);
        greeting_socket(&inside);
        let jobs = confined_jobs(&place, SandboxSettings::default());
        let out = run(&jobs, &place, &connect_command(&outside)).await;
        assert!(
            out.is_error && !out.output.contains("hello"),
            "{}",
            out.output
        );
        let out = run(&jobs, &place, &connect_command(&inside)).await;
        assert!(out.output.contains("hello"), "open folder: {}", out.output);

        let allowed = SandboxSettings {
            sockets: vec![outside.canonicalize().unwrap()],
            ..SandboxSettings::default()
        };
        let jobs = confined_jobs(&place, allowed);
        let out = run(&jobs, &place, &connect_command(&outside)).await;
        assert!(out.output.contains("hello"), "{}", out.output);
    }

    #[tokio::test]
    async fn a_link_to_a_closed_socket_from_an_open_folder_stays_closed() {
        let place = place("socket-link");
        let outside = place.outside.join("agent.sock");
        greeting_socket(&outside);
        std::os::unix::fs::symlink(&outside, place.work.join("sym.sock")).unwrap();
        std::fs::hard_link(&outside, place.work.join("hard.sock")).unwrap();
        let jobs = confined_jobs(&place, SandboxSettings::default());
        for link in ["sym.sock", "hard.sock"] {
            let out = run(&jobs, &place, &connect_command(&place.work.join(link))).await;
            assert!(!out.output.contains("hello"), "{link}: {}", out.output);
        }
    }

    #[tokio::test]
    async fn a_socket_a_command_makes_in_its_temp_folder_works() {
        let place = place("own-socket");
        let jobs = confined_jobs(&place, SandboxSettings::default());
        let code = "import socket,tempfile,os; d=tempfile.mkdtemp(); p=os.path.join(d,'s'); \
                    l=socket.socket(socket.AF_UNIX); l.bind(p); l.listen(); \
                    c=socket.socket(socket.AF_UNIX); c.connect(p); print('own ok')";
        let out = run(&jobs, &place, &format!("python3 -c \"{code}\"")).await;
        assert!(out.output.contains("own ok"), "{}", out.output);
    }

    #[tokio::test]
    async fn the_failure_names_the_socket_and_the_command_that_opens_it() {
        let place = place("socket-note");
        let outside = place.outside.join("d.sock");
        greeting_socket(&outside);
        let jobs = confined_jobs(&place, SandboxSettings::default());
        let out = run(
            &jobs,
            &place,
            &format!("nc -U '{}' </dev/null", outside.display()),
        )
        .await;
        let expected = format!(
            "/sandbox allow socket {}",
            outside.canonicalize().unwrap().display()
        );
        assert!(out.output.contains(&expected), "{}", out.output);
    }

    #[tokio::test]
    async fn ssh_files_are_hidden_until_ssh_is_allowed() {
        let place = place("ssh");
        std::fs::create_dir_all(place.home.join(".ssh")).unwrap();
        std::fs::write(
            place.home.join(".ssh/known_hosts"),
            "github.com ssh-ed25519 AAAA",
        )
        .unwrap();
        let command = format!("cat '{}'", place.home.join(".ssh/known_hosts").display());
        let jobs = confined_jobs(&place, SandboxSettings::default());
        let out = run(&jobs, &place, &command).await;
        assert!(
            out.is_error && !out.output.contains("ssh-ed25519"),
            "{}",
            out.output
        );
        assert!(out.output.contains("/sandbox allow ssh"), "{}", out.output);
        let granted = SandboxSettings {
            ssh: Some(SshGrant::default()),
            ..SandboxSettings::default()
        };
        let out = run(&confined_jobs(&place, granted), &place, &command).await;
        assert!(out.output.contains("ssh-ed25519"), "{}", out.output);
    }

    #[tokio::test]
    async fn the_ssh_grant_opens_the_agent_socket_it_found() {
        let place = place("ssh-agent");
        let agent = place.outside.join("agent.sock");
        greeting_socket(&agent);
        let found = sockets::ssh_agent_sockets(Some(&place.home), Some(agent.as_os_str()));
        let jobs = confined_jobs(&place, SandboxSettings::default());
        let out = run(&jobs, &place, &connect_command(&agent)).await;
        assert!(!out.output.contains("hello"), "{}", out.output);
        let granted = SandboxSettings {
            ssh: Some(SshGrant { agents: found }),
            ..SandboxSettings::default()
        };
        let out = run(
            &confined_jobs(&place, granted),
            &place,
            &connect_command(&agent),
        )
        .await;
        assert!(out.output.contains("hello"), "{}", out.output);
    }
}

/// Real commands through real bubblewrap (and Landlock where the kernel has
/// it). Skipped, saying so, where bubblewrap can't run.
#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::test_support::*;
    use super::*;

    fn bubblewrap() -> Option<&'static std::result::Result<(), String>> {
        match mechanism() {
            Mechanism::Bubblewrap {
                abstract_sockets, ..
            } => Some(abstract_sockets),
            other => {
                eprintln!("skipped: {}", other.label());
                None
            }
        }
    }

    #[tokio::test]
    async fn credential_folders_and_files_are_hidden_unless_allowed() {
        if bubblewrap().is_none() {
            return;
        }
        let place = place("creds");
        std::fs::write(place.home.join(".netrc"), "machine x password secret").unwrap();
        let jobs = confined_jobs(&place, SandboxSettings::default());
        for store in [".aws/credentials", ".netrc"] {
            let command = format!("cat '{}'", place.home.join(store).display());
            let out = run(&jobs, &place, &command).await;
            assert!(!out.output.contains("secret"), "{store}: {}", out.output);
        }
        let reopened = SandboxSettings {
            readable: vec![place.home.join(".aws"), place.home.join(".netrc")],
            ..SandboxSettings::default()
        };
        let jobs = confined_jobs(&place, reopened);
        for store in [".aws/credentials", ".netrc"] {
            let command = format!("cat '{}'", place.home.join(store).display());
            let out = run(&jobs, &place, &command).await;
            assert!(out.output.contains("secret"), "{store}: {}", out.output);
        }
    }

    #[tokio::test]
    async fn the_users_other_processes_are_out_of_reach() {
        if bubblewrap().is_none() {
            return;
        }
        let place = place("other-processes");
        let mut editor = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let jobs = confined_jobs(&place, SandboxSettings::default());
        let signal = format!("kill -0 {} && echo reached", editor.id());
        let out = run(&jobs, &place, &signal).await;
        editor.kill().ok();
        editor.wait().ok();
        assert!(!out.output.contains("reached"), "{}", out.output);
    }

    #[tokio::test]
    async fn sockets_outside_the_open_folders_are_closed_until_allowed() {
        if bubblewrap().is_none() {
            return;
        }
        let place = place("sockets");
        let outside = place.outside.join("agent.sock");
        let inside = place.work.join("dev.sock");
        greeting_socket(&outside);
        greeting_socket(&inside);
        // Not in /proc/net/unix's usual places? It is: this process bound it.
        let jobs = confined_jobs(&place, SandboxSettings::default());
        let out = run(&jobs, &place, &connect_command(&outside)).await;
        assert!(!out.output.contains("hello"), "{}", out.output);
        assert!(
            out.output.contains("/sandbox allow socket"),
            "{}",
            out.output
        );
        let out = run(&jobs, &place, &connect_command(&inside)).await;
        assert!(out.output.contains("hello"), "open folder: {}", out.output);
        let allowed = SandboxSettings {
            sockets: vec![outside.clone()],
            ..SandboxSettings::default()
        };
        let out = run(
            &confined_jobs(&place, allowed),
            &place,
            &connect_command(&outside),
        )
        .await;
        assert!(out.output.contains("hello"), "{}", out.output);
    }

    #[tokio::test]
    async fn a_socket_a_command_makes_in_its_temp_folder_works() {
        if bubblewrap().is_none() {
            return;
        }
        let place = place("own-socket");
        let jobs = confined_jobs(&place, SandboxSettings::default());
        let code = "import socket,tempfile,os; d=tempfile.mkdtemp(); p=os.path.join(d,'s'); \
                    l=socket.socket(socket.AF_UNIX); l.bind(p); l.listen(); \
                    c=socket.socket(socket.AF_UNIX); c.connect(p); print('own ok')";
        let out = run(&jobs, &place, &format!("python3 -c \"{code}\"")).await;
        assert!(out.output.contains("own ok"), "{}", out.output);
    }

    #[tokio::test]
    async fn abstract_sockets_outside_the_sandbox_are_closed_with_landlock() {
        let Some(scoping) = bubblewrap() else {
            return;
        };
        if let Err(why) = scoping {
            eprintln!("skipped: no Landlock scoping: {why}");
            return;
        }
        use std::os::linux::net::SocketAddrExt;
        let name = format!("gz-test-{}", std::process::id());
        let address = std::os::unix::net::SocketAddr::from_abstract_name(&name).unwrap();
        let listener = std::os::unix::net::UnixListener::bind_addr(&address).unwrap();
        std::thread::spawn(move || {
            use std::io::Write;
            for mut stream in listener.incoming().flatten() {
                stream.write_all(b"hello").ok();
            }
        });
        let place = place("abstract");
        let jobs = confined_jobs(&place, SandboxSettings::default());
        let connect = format!(
            "python3 -c 'import socket; s=socket.socket(socket.AF_UNIX); \
             s.connect(\"\\0{name}\"); print(s.recv(5).decode())'"
        );
        let out = run(&jobs, &place, &connect).await;
        assert!(!out.output.contains("hello"), "{}", out.output);
        let own = "python3 -c 'import socket; l=socket.socket(socket.AF_UNIX); \
                   l.bind(\"\\0gz-own\"); l.listen(); c=socket.socket(socket.AF_UNIX); \
                   c.connect(\"\\0gz-own\"); print(\"own ok\")'";
        let out = run(&jobs, &place, own).await;
        assert!(out.output.contains("own ok"), "{}", out.output);
    }

    #[tokio::test]
    async fn writes_stay_in_the_open_folder() {
        if bubblewrap().is_none() {
            return;
        }
        let place = place("writes");
        let jobs = confined_jobs(&place, SandboxSettings::default());
        let out = run(&jobs, &place, "echo hi > made.txt && cat made.txt").await;
        assert!(!out.is_error, "{}", out.output);
        let target = place.outside.join("escaped.txt");
        let out = run(&jobs, &place, &format!("echo hi > '{}'", target.display())).await;
        assert!(out.is_error && !target.exists(), "{}", out.output);
    }
}
