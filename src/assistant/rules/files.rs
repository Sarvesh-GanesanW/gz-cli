//! The rules files: where they are, reading them, and adding or removing
//! one rule. Every file has the same TOML keys:
//!
//! ```toml
//! allow = ["run_command(cargo test:*)", "edit(src/**)"]
//! ask = ["run_command(git push:*)"]
//! deny = ["read(.env)", "run_command(rm:*)", "web_fetch(domain:*.internal)"]
//! default_mode = "accept-edits"   # ask, accept-edits, plan, auto, dont-ask
//! dont_ask_in_cycle = true        # shift+tab also offers dont-ask mode
//! ```
//!
//! The user file is the existing `permissions.toml` in the gz config
//! folder, whose `[workspaces]` tables keep the "always" answers; editing
//! a rule leaves them as they are.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::{Kind, Source};
use crate::assistant::hooks::trust::{TrustStore, Verdict};
use crate::assistant::private_file::write_private;

const PROJECT_DIR: &str = ".gz";
const PROJECT_FILE: &str = "permissions.toml";
const LOCAL_FILE: &str = "permissions.local.toml";
const USER_FILE: &str = "permissions.toml";
const GITIGNORE: &str = ".gitignore";

/// Where one project folder's rules files are.
#[derive(Debug, Clone)]
pub struct Places {
    /// The project folder: relative rule paths start here.
    pub folder: PathBuf,
    pub home: Option<PathBuf>,
    /// The user's `permissions.toml`; `None` without a config folder.
    pub user_file: Option<PathBuf>,
    /// Which versions of project files the user trusted.
    pub trust: Option<TrustStore>,
}

impl Places {
    /// The files for `folder`, with the user's file and trust store in the
    /// gz config folder (honours `XDG_CONFIG_HOME`).
    pub fn for_folder(folder: &Path, home: Option<&Path>) -> Self {
        let config_dir = crate::config::config_path()
            .ok()
            .and_then(|path| path.parent().map(Path::to_path_buf));
        Self {
            folder: folder.to_path_buf(),
            home: home.map(Path::to_path_buf),
            user_file: config_dir.as_ref().map(|dir| dir.join(USER_FILE)),
            trust: TrustStore::open().ok(),
        }
    }

    /// The files for `folder`, with the user's home folder for `~/…`.
    pub fn for_folder_at_home(folder: &Path) -> Self {
        let home = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf());
        Self::for_folder(folder, home.as_deref())
    }

    /// The file a scope's rules live in.
    pub fn file(&self, scope: Scope) -> Result<PathBuf> {
        match scope {
            Scope::User => self
                .user_file
                .clone()
                .context("gz has no config folder for user rules; set XDG_CONFIG_HOME or HOME"),
            Scope::Project => Ok(self.folder.join(PROJECT_DIR).join(PROJECT_FILE)),
            Scope::Local => Ok(self.folder.join(PROJECT_DIR).join(LOCAL_FILE)),
        }
    }
}

/// Which file a rule is written to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    User,
    Project,
    Local,
}

impl Scope {
    pub fn parse(word: &str) -> Option<Self> {
        match word {
            "user" => Some(Scope::User),
            "project" => Some(Scope::Project),
            "local" => Some(Scope::Local),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Scope::User => "user",
            Scope::Project => "project",
            Scope::Local => "local",
        }
    }

    pub fn source(self) -> Source {
        match self {
            Scope::User => Source::User,
            Scope::Project => Source::Project,
            Scope::Local => Source::Local,
        }
    }

    fn of(source: Source) -> Option<Self> {
        match source {
            Source::User => Some(Scope::User),
            Source::Project => Some(Scope::Project),
            Source::Local => Some(Scope::Local),
            Source::CommandLine => None,
        }
    }
}

/// The mode settings a file may hold.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Settings {
    /// The mode gz code starts in, by name.
    pub default_mode: Option<String>,
    /// Whether shift+tab also cycles to dont-ask mode.
    pub dont_ask_in_cycle: Option<bool>,
}

impl Settings {
    /// Fills what is unset from a lower-priority file.
    pub(super) fn merge(&mut self, lower: &Settings) {
        if self.default_mode.is_none() {
            self.default_mode.clone_from(&lower.default_mode);
        }
        if self.dont_ask_in_cycle.is_none() {
            self.dont_ask_in_cycle = lower.dont_ask_in_cycle;
        }
    }
}

/// The three lists in a file.
#[derive(Debug, Clone, Default, Deserialize)]
pub(super) struct Lists {
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    ask: Vec<String>,
    #[serde(default)]
    deny: Vec<String>,
}

impl Lists {
    pub(super) fn of(&self, kind: Kind) -> &[String] {
        match kind {
            Kind::Allow => &self.allow,
            Kind::Ask => &self.ask,
            Kind::Deny => &self.deny,
        }
    }
}

#[derive(Deserialize)]
struct RawFile {
    #[serde(flatten)]
    lists: Lists,
    #[serde(flatten)]
    settings: Settings,
}

/// One file as read.
pub(super) struct Loaded {
    pub lists: Lists,
    pub settings: Settings,
    /// For a project file: whether the user trusted this version.
    pub trusted: bool,
}

/// Every file that exists, highest source first; a file that can't be
/// read is a problem to report, never a silent gap.
pub(super) fn read_all(places: &Places) -> Vec<(Source, Result<Option<Loaded>, String>)> {
    [Scope::Local, Scope::Project, Scope::User]
        .into_iter()
        .filter_map(|scope| {
            let path = places.file(scope).ok()?;
            Some((scope.source(), read(places, scope, &path)))
        })
        .collect()
}

fn read(places: &Places, scope: Scope, path: &Path) -> Result<Option<Loaded>, String> {
    let Some(bytes) = read_bytes(path).map_err(|err| format!("{err:#}"))? else {
        return Ok(None);
    };
    let text = std::str::from_utf8(&bytes).map_err(|_| {
        format!(
            "{} is not UTF-8 text; its rules are ignored",
            path.display()
        )
    })?;
    let raw: RawFile = toml::from_str(text).map_err(|err| {
        format!(
            "{} has a problem ({err}); its rules are ignored until it is fixed",
            path.display()
        )
    })?;
    let trusted = scope == Scope::User || is_trusted(places, path, &bytes);
    Ok(Some(Loaded {
        lists: raw.lists,
        settings: raw.settings,
        trusted,
    }))
}

fn read_bytes(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("cannot read {}", path.display())),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn is_trusted(places: &Places, path: &Path, bytes: &[u8]) -> bool {
    places.trust.as_ref().is_some_and(|store| {
        matches!(
            store.verdict(path, &sha256_hex(bytes)),
            Ok(Verdict::Trusted)
        )
    })
}

/// `/permissions trust`: trusts the project files as they are now, so
/// their allow rules and mode count. Returns the files trusted.
pub fn trust_project(places: &Places) -> Result<Vec<PathBuf>> {
    let store = places
        .trust
        .as_ref()
        .context("gz has no config folder to remember trust in")?;
    let mut trusted = Vec::new();
    for scope in [Scope::Project, Scope::Local] {
        let path = places.file(scope)?;
        if let Some(bytes) = read_bytes(&path)? {
            store.record(&path, &sha256_hex(&bytes), true)?;
            trusted.push(path);
        }
    }
    Ok(trusted)
}

/// Adds `pattern` to the `kind` list of `scope`'s file; false when it was
/// there already. Writing a project file the user already trusted (or a
/// new one) keeps it trusted, since the user made the change.
pub fn add_rule(places: &Places, scope: Scope, kind: Kind, pattern: &str) -> Result<bool> {
    edit_list(places, scope, kind, |list| {
        if list.iter().any(|v| v.as_str() == Some(pattern)) {
            return false;
        }
        list.push(pattern);
        true
    })
}

/// Removes `pattern` from the `kind` list of the file `source` names;
/// false when it wasn't there.
pub fn remove_rule(places: &Places, source: Source, kind: Kind, pattern: &str) -> Result<bool> {
    let Some(scope) = Scope::of(source) else {
        bail!("command-line rules last for this session only; start gz code without the flag");
    };
    edit_list(places, scope, kind, |list| {
        let before = list.len();
        list.retain(|v| v.as_str().map(normalized).as_deref() != Some(pattern));
        before != list.len()
    })
}

/// A stored rule as `/permissions` shows it, so a rule written by hand
/// with other spacing or case is found too.
fn normalized(text: &str) -> String {
    super::Pattern::parse(text)
        .map(|p| p.to_string())
        .unwrap_or_else(|_| text.to_string())
}

fn edit_list(
    places: &Places,
    scope: Scope,
    kind: Kind,
    change: impl FnOnce(&mut toml_edit::Array) -> bool,
) -> Result<bool> {
    let path = places.file(scope)?;
    if scope != Scope::User {
        refuse_linked_project_files(&path, scope)?;
    }
    let old = read_bytes(&path)?;
    let was_trusted = match &old {
        None => true,
        Some(bytes) => scope == Scope::User || is_trusted(places, &path, bytes),
    };
    // Edited in place, so the file's comments and layout stay.
    let mut doc: toml_edit::DocumentMut = match &old {
        None => toml_edit::DocumentMut::new(),
        Some(bytes) => std::str::from_utf8(bytes)
            .ok()
            .and_then(|text| text.parse().ok())
            .with_context(|| {
                format!(
                    "{} isn't valid TOML; fix it by hand before adding rules",
                    path.display()
                )
            })?,
    };
    let key = kind.as_str();
    if !doc.contains_key(key) {
        doc.insert(key, toml_edit::value(toml_edit::Array::new()));
    }
    let Some(list) = doc.get_mut(key).and_then(|item| item.as_array_mut()) else {
        bail!(
            "`{}` in {} isn't a list; fix it by hand",
            kind.as_str(),
            path.display()
        );
    };
    if !change(list) {
        return Ok(false);
    }
    // An emptied list stays (`deny = []`): removing the key would drop
    // the comments written above it.
    let text = doc.to_string();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    write_private(&path, &text)?;
    if scope == Scope::Local {
        ignore_local_file(&path)?;
    }
    if scope != Scope::User && was_trusted {
        if let Some(store) = &places.trust {
            store.record(&path, &sha256_hex(text.as_bytes()), true)?;
        }
    }
    Ok(true)
}

/// A cloned repository could ship `.gz`, a rules file, the temp file gz
/// writes through or `.gz/.gitignore` as a symlink, so editing rules would
/// write wherever it points, outside the project.
fn refuse_linked_project_files(path: &Path, scope: Scope) -> Result<()> {
    let Some(dir) = path.parent() else {
        return Ok(());
    };
    let mut written = vec![
        dir.to_path_buf(),
        path.to_path_buf(),
        path.with_extension("tmp"),
    ];
    if scope == Scope::Local {
        written.push(dir.join(GITIGNORE));
    }
    for target in written {
        let is_link = std::fs::symlink_metadata(&target).is_ok_and(|m| m.file_type().is_symlink());
        if is_link {
            bail!(
                "{} is a symlink, so gz won't write through it; replace it with a plain {} \
                 or edit the rules by hand",
                target.display(),
                if target == dir { "folder" } else { "file" }
            );
        }
    }
    Ok(())
}

/// Keeps the private project file out of git with a `.gz/.gitignore`.
fn ignore_local_file(path: &Path) -> Result<()> {
    let Some(dir) = path.parent() else {
        return Ok(());
    };
    let ignore = dir.join(GITIGNORE);
    let existing = std::fs::read_to_string(&ignore).unwrap_or_default();
    if existing.lines().any(|line| line.trim() == LOCAL_FILE) {
        return Ok(());
    }
    let mut text = existing;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(LOCAL_FILE);
    text.push('\n');
    std::fs::write(&ignore, text).with_context(|| format!("cannot write {}", ignore.display()))
}
