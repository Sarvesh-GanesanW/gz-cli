//! OS limits for the commands and scripts the model runs.
//!
//! gz's file tools stay inside the open folders ([`Sandbox`]), but an
//! approved command runs as the user and could otherwise write anywhere.
//! Under a [`Confinement`] it may write only in the open folders, the
//! system temp folders, a few tool caches and folders the user added, and
//! may not read the credential folders under home. On macOS this is
//! `sandbox-exec` with a generated profile, on Linux `bwrap`; elsewhere
//! commands run as before and the user is told once.
//!
//! The user's own `!command`s are never confined: they typed them.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::assistant::git_guard::{self, GitDir, GitWatch, Reverts};
use crate::assistant::sandbox::{home_relative, Sandbox};

/// Folders under home that hold credentials: hidden from commands. `~/.ssh`
/// is left readable on purpose, so git over SSH keeps working.
pub const CREDENTIAL_DIRS: &[&str] = &[".aws", ".gnupg", ".kube", ".docker"];

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

const BLOCKED_NOTE: &str = "[gz's sandbox likely blocked this: commands gz runs for you may write \
only in the open folders, the temp folders and tool caches, and cannot read ~/.aws, ~/.gnupg, \
~/.kube or ~/.docker. Do not try another way around it; tell the user what was blocked. They can \
allow a path with /sandbox allow-write <path> or /sandbox allow-read <path>, or turn the sandbox \
off with /sandbox off.]";

const OFFLINE_NOTE: &str = "[gz's sandbox has the network off (/sandbox network off), so this \
command likely could not connect. Do not try another way around it; tell the user, who can turn \
it on with /sandbox network on.]";

/// The user's `/sandbox` settings, saved per profile (`code_sandbox`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SandboxSettings {
    pub enabled: bool,
    pub network: bool,
    /// Extra folders commands may write to (`/sandbox allow-write`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub writable: Vec<PathBuf>,
    /// Paths inside the credential folders commands may read
    /// (`/sandbox allow-read`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub readable: Vec<PathBuf>,
}

impl Default for SandboxSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            network: true,
            writable: Vec::new(),
            readable: Vec::new(),
        }
    }
}

/// A path the user added, as `/sandbox` numbers it for `/sandbox forget`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Added {
    Write(PathBuf),
    Read(PathBuf),
}

impl SandboxSettings {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Added writable folders, then added readable paths; `/sandbox`
    /// numbers them from 1 in this order.
    pub fn added(&self) -> Vec<Added> {
        let writes = self.writable.iter().cloned().map(Added::Write);
        let reads = self.readable.iter().cloned().map(Added::Read);
        writes.chain(reads).collect()
    }

    /// Removes the `number`th path of [`Self::added`].
    pub fn forget(&mut self, number: usize) -> Result<Added> {
        let added = self.added();
        let Some(which) = number.checked_sub(1).and_then(|i| added.get(i)) else {
            bail!(
                "there is no added path {number}; /sandbox lists them ({} added)",
                added.len()
            );
        };
        match which {
            Added::Write(path) => self.writable.retain(|p| p != path),
            Added::Read(path) => self.readable.retain(|p| p != path),
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
    /// Linux bubblewrap, at this path.
    Bubblewrap(PathBuf),
    /// None: why, and what the user can do about it.
    Unavailable(String),
}

impl Mechanism {
    pub fn label(&self) -> String {
        match self {
            Mechanism::Seatbelt => "macOS sandbox-exec".into(),
            Mechanism::Bubblewrap(path) => format!("bubblewrap ({})", path.display()),
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
        return match try_run(Path::new(SANDBOX_EXEC), &trial) {
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
        let trial = ["--ro-bind", "/", "/", "--dev", "/dev", "--", "true"];
        return match try_run(&bwrap, &trial) {
            Ok(()) => Mechanism::Bubblewrap(bwrap),
            Err(why) => Mechanism::Unavailable(format!(
                "bubblewrap does not work here ({why}); it needs unprivileged user namespaces"
            )),
        };
    }
    Mechanism::Unavailable("gz has no OS sandbox for this platform".into())
}

fn try_run(program: &Path, args: &[&str]) -> std::result::Result<(), String> {
    let out = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|err| err.to_string())?;
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
/// confined; `None` when they are, or the user turned the sandbox off.
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
        Mechanism::Seatbelt | Mechanism::Bubblewrap(_) => None,
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
}

impl Rules {
    /// The rules for a command started now in `roots`, which hold the
    /// repos `git_dirs`.
    pub fn new(
        roots: &[PathBuf],
        home: Option<&Path>,
        settings: &SandboxSettings,
        git_dirs: &[GitDir],
    ) -> Self {
        let mut writable: Vec<PathBuf> = roots.to_vec();
        writable.extend(temp_folders());
        if let Some(home) = home {
            writable.extend(TOOL_CACHES.iter().map(|cache| real(&home.join(cache))));
        }
        writable.extend(settings.writable.iter().cloned());
        dedup(&mut writable);
        let hidden = home
            .map(|home| {
                CREDENTIAL_DIRS
                    .iter()
                    .map(|dir| real(&home.join(dir)))
                    .collect()
            })
            .unwrap_or_default();
        // Missing hooks folders too: the profile denies creating them.
        let protected = git_dirs.iter().map(|dir| dir.hooks.clone()).collect();
        Self {
            writable,
            hidden,
            readable: settings.readable.clone(),
            protected,
            network: settings.network,
        }
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
    // System Events) to act for it, outside the sandbox.
    profile.push_str("(deny appleevent-send)\n");
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
            "(allow network-outbound (remote unix-socket))\n",
        ));
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

/// `bwrap` arguments for `rules`, up to the command. Only existing paths
/// are bound, since bwrap can't mount over a missing one.
pub fn bwrap_args(rules: &Rules) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc"]
        .map(OsString::from)
        .to_vec();
    fn bind(args: &mut Vec<OsString>, flag: &str, path: &Path) {
        args.extend([flag.into(), path.into(), path.into()]);
    }
    for path in rules.writable.iter().filter(|p| p.exists()) {
        bind(&mut args, "--bind", path);
    }
    for path in rules.protected.iter().filter(|p| p.exists()) {
        bind(&mut args, "--ro-bind", path);
    }
    for hidden in rules.hidden.iter().filter(|p| p.is_dir()) {
        let reopened_whole = rules.readable.iter().any(|r| hidden.starts_with(r));
        if !reopened_whole {
            args.extend(["--tmpfs".into(), hidden.into()]);
        }
    }
    for path in rules.readable.iter().filter(|p| p.exists()) {
        bind(&mut args, "--ro-bind", path);
    }
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
    pub network: bool,
    /// The protected repos' configs as they were when it started.
    pub git: GitWatch,
}

impl Confined {
    /// What to tell the model when a failed command's `output` shows the
    /// sandbox likely stopped it.
    pub fn blocked_note(&self, output: &str) -> Option<&'static str> {
        let lower = output.to_lowercase();
        let shows = |signs: &[&str]| signs.iter().any(|sign| lower.contains(sign));
        if !self.network && shows(OFFLINE_SIGNS) {
            return Some(OFFLINE_NOTE);
        }
        shows(DENIAL_SIGNS).then_some(BLOCKED_NOTE)
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
        // Found once per command and shared, so the walk runs once.
        let roots = self.folders.roots();
        let git_dirs = git_guard::find_git_dirs(&roots);
        let rules = Rules::new(&roots, self.folders.home(), &settings, &git_dirs);
        let mut command = match mechanism {
            Mechanism::Seatbelt => {
                let mut command = Command::new(SANDBOX_EXEC);
                command.arg("-p").arg(seatbelt_profile(&rules));
                command
            }
            Mechanism::Bubblewrap(bwrap) => {
                let mut command = Command::new(bwrap);
                command.args(bwrap_args(&rules));
                command
            }
            Mechanism::Unavailable(_) => return Ok((Command::new(program), None)),
        };
        command.arg(program);
        let confined = Confined {
            network: settings.network,
            git: GitWatch::save(&git_dirs, &roots),
        };
        Ok((command, Some(confined)))
    }
}

/// A path the user typed in `/sandbox allow-…`: `~` is home, relative
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
    if let Some(dir) = credential_dir(path, home) {
        bail!(
            "{} is inside {}, a credential folder; commands may not change it",
            path.display(),
            dir.display()
        );
    }
    Ok(())
}

/// Checks a path for `/sandbox allow-read`: only credential folders are
/// hidden, so anything else is readable already.
pub fn check_readable(path: &Path, home: Option<&Path>) -> Result<()> {
    if credential_dir(path, home).is_none() {
        bail!(
            "{} is readable already: the sandbox hides only ~/{}",
            path.display(),
            CREDENTIAL_DIRS.join(", ~/")
        );
    }
    Ok(())
}

fn credential_dir(path: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let home = home?;
    CREDENTIAL_DIRS
        .iter()
        .map(|dir| real(&home.join(dir)))
        .find(|dir| path.starts_with(dir))
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
        }
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
        assert!(!profile.contains("network"), "network stays on: {profile}");
    }

    #[test]
    fn network_off_blocks_outbound_except_loopback() {
        let profile = seatbelt_profile(&rules(false));
        let deny = profile.find("(deny network-outbound)").unwrap();
        let loopback = profile
            .find("(allow network-outbound (remote ip \"localhost:*\"))")
            .unwrap();
        assert!(deny < loopback, "{profile}");
    }

    #[test]
    fn quotes_and_backslashes_in_paths_cannot_end_the_profile_string() {
        let hostile = Path::new("/w/a\") (allow file-write* (subpath \"/\") \\");
        assert_eq!(
            seatbelt_string(hostile),
            "\"/w/a\\\") (allow file-write* (subpath \\\"/\\\") \\\\\""
        );
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
        };
        let args: Vec<String> = bwrap_args(&rules)
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
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
        assert_eq!(args.last().map(String::as_str), Some("--"));
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
        let roots = [app.clone()];
        let rules = Rules::new(
            &roots,
            Some(&home),
            &settings,
            &git_guard::find_git_dirs(&roots),
        );
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
        assert!(rules.hidden.contains(&home.join(".aws")));
        assert!(!rules.hidden.contains(&home.join(".ssh")), "git over SSH");
    }

    #[test]
    fn denials_get_a_note_naming_the_sandbox_command() {
        let on = Confined {
            network: true,
            git: GitWatch::default(),
        };
        let note = on
            .blocked_note("sh: /x/f: Operation not permitted")
            .unwrap();
        assert!(note.contains("/sandbox allow-write"), "{note}");
        assert_eq!(on.blocked_note("error: expected `;`"), None);
        let offline = Confined {
            network: false,
            git: GitWatch::default(),
        };
        let note = offline
            .blocked_note("curl: (6) Could not resolve host: example.com")
            .unwrap();
        assert!(note.contains("/sandbox network on"), "{note}");
        assert_eq!(on.blocked_note("Could not resolve host: x"), None);
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
    fn allow_read_takes_only_paths_in_credential_folders() {
        let home = crate::assistant::sandbox::tests::temp_dir("allow-read");
        std::fs::create_dir_all(home.join(".aws")).unwrap();
        std::fs::create_dir_all(home.join("code")).unwrap();
        let aws = existing_path("~/.aws", &home, Some(&home)).unwrap();
        assert!(check_readable(&aws, Some(&home)).is_ok());
        assert!(check_writable(&aws, Some(&home)).is_err());
        let code = existing_path("code", &home, Some(&home)).unwrap();
        let why = check_readable(&code, Some(&home)).unwrap_err().to_string();
        assert!(why.contains("readable already"), "{why}");
        assert!(check_writable(&code, Some(&home)).is_ok());
        assert!(existing_path("nope", &home, Some(&home)).is_err());
        assert!(check_writable(Path::new("/"), Some(&home)).is_err());
    }
}

/// Real commands through the real macOS sandbox. The folders live under
/// the build's `target/`, which is neither a temp folder nor a cache, so a
/// write there is allowed only because of the rule under test.
#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    use super::*;
    use crate::assistant::jobs::{Jobs, State};
    use std::time::Duration;

    struct Place {
        base: PathBuf,
        work: PathBuf,
        outside: PathBuf,
        home: PathBuf,
    }

    impl Drop for Place {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.base).ok();
        }
    }

    fn place(name: &str) -> Place {
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

    fn confined_jobs(place: &Place, settings: SandboxSettings) -> Jobs {
        let folders = Sandbox::with_home(&place.work, Some(&place.home)).unwrap();
        let confinement = Confinement::new(folders, OsSandbox::unsaved(settings));
        Jobs::new().0.confined(confinement)
    }

    async fn run(jobs: &Jobs, place: &Place, command: &str) -> crate::assistant::tools::ToolOutput {
        jobs.run(command, &place.work, Duration::from_secs(20), false, false)
            .await
    }

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
}
