//! Where MCP servers are configured, in three scopes:
//!
//! - **user**: `<gz config folder>/mcp.toml`, every project;
//! - **project**: `.mcp.json` or `.gz/mcp.json` in the project, checked in
//!   and shared, so each server runs only once the user approved that exact
//!   entry (by content hash, in `mcp-trust.json`, like project hooks);
//! - **local**: `<gz config folder>/mcp-local.json`, by project folder,
//!   private to this user.
//!
//! A name defined in several scopes means the local one, then the
//! project's, then the user's. One broken file or entry is reported and
//! skipped; the rest still load.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use super::config::{check_name, ServerSpec};
use super::oauth::{AuthStore, AUTH_FILE};
use crate::assistant::hooks::trust::{TrustStore, Verdict};
use crate::assistant::private_file::write_private;

pub const USER_FILE: &str = "mcp.toml";
pub const LOCAL_FILE: &str = "mcp-local.json";
const TRUST_FILE: &str = "mcp-trust.json";
const LOG_FOLDER: &str = "mcp-logs";
/// Project files, in reading order: a later one overrides an earlier one.
pub const PROJECT_FILES: [&str; 2] = [".mcp.json", ".gz/mcp.json"];
const JSON_KEY: &str = "mcpServers";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scope {
    User,
    Project,
    Local,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::User => "user",
            Scope::Project => "project",
            Scope::Local => "local",
        }
    }
}

/// Every file gz reads or writes for MCP servers.
#[derive(Debug, Clone)]
pub struct Places {
    pub config_dir: PathBuf,
    /// The user-scope file, normally `mcp.toml` in `config_dir`.
    pub user_file: PathBuf,
    /// The project folder; none outside a project.
    pub project: Option<PathBuf>,
}

impl Places {
    /// The gz config folder (honours `XDG_CONFIG_HOME`) and `project`.
    pub fn for_project(project: Option<&Path>) -> Result<Self> {
        let config = crate::config::config_path()?;
        let dir = config
            .parent()
            .context("the gz config path has no folder")?
            .to_path_buf();
        Ok(Self::at(dir, project))
    }

    pub fn at(config_dir: PathBuf, project: Option<&Path>) -> Self {
        let project = project.map(|p| p.canonicalize().unwrap_or_else(|_| p.to_path_buf()));
        Self {
            user_file: config_dir.join(USER_FILE),
            config_dir,
            project,
        }
    }

    pub fn user_file(&self) -> PathBuf {
        self.user_file.clone()
    }

    pub fn local_file(&self) -> PathBuf {
        self.config_dir.join(LOCAL_FILE)
    }

    pub fn logs(&self) -> PathBuf {
        self.config_dir.join(LOG_FOLDER)
    }

    pub fn auth(&self) -> AuthStore {
        AuthStore::at(self.config_dir.join(AUTH_FILE))
    }

    fn trust(&self) -> TrustStore {
        TrustStore::at(self.config_dir.join(TRUST_FILE))
    }

    fn project_files(&self) -> Vec<PathBuf> {
        self.project
            .iter()
            .flat_map(|root| PROJECT_FILES.map(|f| root.join(f)))
            .collect()
    }

    /// The project file `gz mcp add --scope project` writes: the one that
    /// exists (`.gz/mcp.json` first), else `.mcp.json`.
    fn project_file_for_writing(&self) -> Result<PathBuf> {
        let root = self
            .project
            .as_ref()
            .context("no project folder; run this inside the project, or use --scope user")?;
        let gz = root.join(PROJECT_FILES[1]);
        Ok(if gz.exists() {
            gz
        } else {
            root.join(PROJECT_FILES[0])
        })
    }

    fn local_key(&self) -> Result<String> {
        let root = self
            .project
            .as_ref()
            .context("no project folder; run this inside the project, or use --scope user")?;
        Ok(root.to_string_lossy().into_owned())
    }
}

/// A project server's approval, by content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    /// User and local servers are the user's own.
    NotNeeded,
    Approved,
    Declined,
    Pending,
}

/// One server as configured in one scope.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub spec: ServerSpec,
    pub scope: Scope,
    /// The file it came from.
    pub source: PathBuf,
    pub approval: Approval,
    /// A higher scope defines the same name, so this one is not used.
    pub shadowed: bool,
}

/// Every configured server, and what could not be read.
#[derive(Debug, Default)]
pub struct Loaded {
    pub entries: Vec<Entry>,
    pub problems: Vec<String>,
}

impl Loaded {
    /// The servers in use: one per name, from the highest scope.
    pub fn effective(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(|e| !e.shadowed)
    }

    pub fn find(&self, name: &str) -> Option<&Entry> {
        self.effective().find(|e| e.name == name)
    }
}

/// Reads every scope.
pub fn load(places: &Places) -> Loaded {
    let mut loaded = Loaded::default();
    let user = places.user_file();
    match read_user(&user) {
        Ok(servers) => add_entries(&mut loaded, servers, Scope::User, &user),
        Err(err) => loaded.problems.push(format!("{err:#}")),
    }
    for file in places.project_files() {
        match read_json_servers(&file, None) {
            Ok(servers) => add_entries(&mut loaded, servers, Scope::Project, &file),
            Err(err) => loaded.problems.push(format!("{err:#}")),
        }
    }
    if let Ok(key) = places.local_key() {
        let local = places.local_file();
        match read_json_servers(&local, Some(&key)) {
            Ok(servers) => add_entries(&mut loaded, servers, Scope::Local, &local),
            Err(err) => loaded.problems.push(format!("{err:#}")),
        }
    }
    shadow_duplicates(&mut loaded.entries);
    let trust = places.trust();
    for entry in &mut loaded.entries {
        if entry.scope == Scope::Project {
            entry.approval = match trust.verdict(&trust_key(entry), &spec_hash(&entry.spec)) {
                Ok(Verdict::Trusted) => Approval::Approved,
                Ok(Verdict::Declined) => Approval::Declined,
                Ok(Verdict::Unknown) => Approval::Pending,
                Err(err) => {
                    loaded.problems.push(format!("{err:#}"));
                    Approval::Pending
                }
            };
        }
    }
    loaded
}

/// Raw entries of one file, each checked on its own.
type RawServers = Vec<(String, Result<ServerSpec>)>;

fn add_entries(loaded: &mut Loaded, servers: RawServers, scope: Scope, source: &Path) {
    for (name, spec) in servers {
        let spec = match spec {
            Ok(spec) => spec,
            Err(err) => {
                loaded.problems.push(format!(
                    "{}: server '{name}' is skipped: {err:#}",
                    source.display()
                ));
                continue;
            }
        };
        if let Err(err) = spec.validate(&name) {
            loaded
                .problems
                .push(format!("{}: {err:#}; it is skipped", source.display()));
            continue;
        }
        loaded.entries.push(Entry {
            name,
            spec,
            scope,
            source: source.to_path_buf(),
            approval: Approval::NotNeeded,
            shadowed: false,
        });
    }
}

/// Marks every entry a later (higher) one redefines.
fn shadow_duplicates(entries: &mut [Entry]) {
    let mut winner: BTreeMap<String, usize> = BTreeMap::new();
    for (at, entry) in entries.iter().enumerate() {
        winner.insert(entry.name.clone(), at);
    }
    for (at, entry) in entries.iter_mut().enumerate() {
        entry.shadowed = winner.get(&entry.name) != Some(&at);
    }
}

fn read_text(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("cannot read {}", path.display())),
    }
}

fn read_user(path: &Path) -> Result<RawServers> {
    match read_text(path)? {
        Some(text) => parse_user(&text, path),
        None => Ok(Vec::new()),
    }
}

fn parse_user(text: &str, path: &Path) -> Result<RawServers> {
    let table: toml::Table =
        toml::from_str(text).with_context(|| format!("{} is not valid TOML", path.display()))?;
    if let Some(other) = table.keys().find(|key| *key != "servers") {
        bail!(
            "{}: unknown section `{other}`; servers go under [servers.<name>]",
            path.display()
        );
    }
    let Some(servers) = table.get("servers") else {
        return Ok(Vec::new());
    };
    let servers = servers
        .as_table()
        .with_context(|| format!("{}: `servers` must be a table", path.display()))?;
    Ok(servers
        .iter()
        .map(|(name, value)| {
            let spec = value
                .clone()
                .try_into::<ServerSpec>()
                .map_err(anyhow::Error::from);
            (name.clone(), spec)
        })
        .collect())
}

/// `mcpServers` of a project file, or of one project in the local file.
fn read_json_servers(path: &Path, project: Option<&str>) -> Result<RawServers> {
    let Some(text) = read_text(path)? else {
        return Ok(Vec::new());
    };
    let root: Value = serde_json::from_str(&text)
        .with_context(|| format!("{} is not valid JSON", path.display()))?;
    let holder = match project {
        Some(key) => &root["projects"][key],
        None => &root,
    };
    let servers = match &holder[JSON_KEY] {
        Value::Null => return Ok(Vec::new()),
        Value::Object(map) => map,
        _ => bail!("{}: `{JSON_KEY}` must be an object", path.display()),
    };
    Ok(servers
        .iter()
        .map(|(name, value)| {
            let spec =
                serde_json::from_value::<ServerSpec>(value.clone()).map_err(anyhow::Error::from);
            (name.clone(), spec)
        })
        .collect())
}

fn trust_key(entry: &Entry) -> PathBuf {
    PathBuf::from(format!("{}#{}", entry.source.display(), entry.name))
}

/// The hash approval is kept by: any change to the entry asks again.
fn spec_hash(spec: &ServerSpec) -> String {
    let canonical = serde_json::to_string(spec).unwrap_or_default();
    let digest = Sha256::digest(canonical.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Records the user's answer for a project server's current content.
pub fn approve(places: &Places, entry: &Entry, approved: bool) -> Result<()> {
    if entry.scope != Scope::Project {
        bail!(
            "{} is a {} server; only project servers need approval",
            entry.name,
            entry.scope.as_str()
        );
    }
    places
        .trust()
        .record(&trust_key(entry), &spec_hash(&entry.spec), approved)
}

/// Adds `name` to `scope`; fails when that scope already has it.
pub fn add(places: &Places, scope: Scope, name: &str, spec: &ServerSpec) -> Result<PathBuf> {
    spec.validate(name)?;
    let loaded = load(places);
    if let Some(existing) = loaded
        .entries
        .iter()
        .find(|e| e.name == name && e.scope == scope)
    {
        bail!(
            "{scope} scope already has a server '{name}' (in {}); remove it first with \
             `gz mcp remove {name} --scope {scope}`",
            existing.source.display(),
            scope = scope.as_str()
        );
    }
    match scope {
        Scope::User => {
            let path = places.user_file();
            add_to_toml(&path, name, spec)?;
            Ok(path)
        }
        Scope::Project => {
            let path = places.project_file_for_writing()?;
            edit_json(&path, None, false, |servers| {
                servers.insert(name.to_string(), json!(spec));
            })?;
            Ok(path)
        }
        Scope::Local => {
            let path = places.local_file();
            let key = places.local_key()?;
            edit_json(&path, Some(&key), true, |servers| {
                servers.insert(name.to_string(), json!(spec));
            })?;
            Ok(path)
        }
    }
}

/// Removes `name` from `scope`, or from the one scope that has it.
pub fn remove(places: &Places, scope: Option<Scope>, name: &str) -> Result<(Scope, PathBuf)> {
    check_name(name)?;
    let loaded = load(places);
    let matching: Vec<&Entry> = loaded
        .entries
        .iter()
        .filter(|e| e.name == name && scope.is_none_or(|s| s == e.scope))
        .collect();
    let entry = match matching.as_slice() {
        [] => bail!(
            "there is no server '{name}'{}; `gz mcp list` shows them all",
            scope.map_or(String::new(), |s| format!(" in the {} scope", s.as_str()))
        ),
        [one] => *one,
        several => bail!(
            "'{name}' is in several scopes ({}); add --scope to say which",
            several
                .iter()
                .map(|e| e.scope.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    let path = entry.source.clone();
    match entry.scope {
        Scope::User => remove_from_toml(&path, name)?,
        Scope::Project => edit_json(&path, None, false, |servers| {
            servers.remove(name);
        })?,
        Scope::Local => edit_json(&path, Some(&places.local_key()?), true, |servers| {
            servers.remove(name);
        })?,
    }
    Ok((entry.scope, path))
}

/// Appends a `[servers.<name>]` table, leaving the rest of the file (and
/// its comments) as it was.
fn add_to_toml(path: &Path, name: &str, spec: &ServerSpec) -> Result<()> {
    let mut text = read_text(path)?.unwrap_or_default();
    let table = toml::to_string(&json_to_toml_table(name, spec)?)
        .context("cannot write the server as TOML")?;
    if !text.is_empty() && !text.ends_with("\n\n") {
        text.push_str(if text.ends_with('\n') { "\n" } else { "\n\n" });
    }
    text.push_str(&table);
    read_user_checked(&text, path)?;
    write_private(path, &text)
}

fn json_to_toml_table(name: &str, spec: &ServerSpec) -> Result<toml::Table> {
    let value = toml::Value::try_from(spec).context("cannot write the server as TOML")?;
    let mut servers = toml::Table::new();
    servers.insert(name.to_string(), value);
    let mut root = toml::Table::new();
    root.insert("servers".to_string(), toml::Value::Table(servers));
    Ok(root)
}

/// Removes the `[servers.<name>]` table and its sub-tables by line, so
/// the rest of the file keeps its comments and layout.
fn remove_from_toml(path: &Path, name: &str) -> Result<()> {
    let text = read_text(path)?.unwrap_or_default();
    let headers = [format!("servers.{name}"), format!("servers.\"{name}\"")];
    let is_ours = |header: &str| {
        headers
            .iter()
            .any(|h| header == h || header.starts_with(&format!("{h}.")))
    };
    let mut kept = Vec::new();
    let mut skipping = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(header) = trimmed
            .strip_prefix('[')
            .and_then(|rest| rest.split(']').next())
        {
            skipping = is_ours(header.trim().trim_start_matches('['));
        }
        if !skipping {
            kept.push(line);
        }
    }
    let mut updated = kept.join("\n");
    while updated.ends_with("\n\n") {
        updated.pop();
    }
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    let still = read_user_checked(&updated, path)?;
    if still.iter().any(|(n, _)| n == name) {
        bail!(
            "'{name}' is written inline in {}; remove it there by hand",
            path.display()
        );
    }
    write_private(path, &updated)
}

/// Parses would-be file content, so an edit never leaves a broken file.
fn read_user_checked(text: &str, path: &Path) -> Result<RawServers> {
    parse_user(text, path)
        .with_context(|| format!("the edit would break {}; nothing changed", path.display()))
}

/// Changes `mcpServers` of a JSON file (or of one project in it), keeping
/// every other key. A private file is written owner-only.
fn edit_json(
    path: &Path,
    project: Option<&str>,
    private: bool,
    change: impl FnOnce(&mut Map<String, Value>),
) -> Result<()> {
    let mut root: Value = match read_text(path)? {
        Some(text) => serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid JSON; fix it first", path.display()))?,
        None => json!({}),
    };
    let holder = match project {
        Some(key) => object_at(object_at(&mut root, "projects", path)?, key, path)?,
        None => as_object(&mut root, path)?,
    };
    let servers = object_at(holder, JSON_KEY, path)?;
    change(
        servers
            .as_object_mut()
            .context("mcpServers is not an object")?,
    );
    let text = serde_json::to_string_pretty(&root).context("cannot encode the file")? + "\n";
    if private {
        return write_private(path, &text);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    std::fs::write(path, text).with_context(|| format!("cannot write {}", path.display()))
}

fn as_object<'v>(value: &'v mut Value, path: &Path) -> Result<&'v mut Value> {
    if !value.is_object() {
        bail!("{} must hold a JSON object", path.display());
    }
    Ok(value)
}

fn object_at<'v>(value: &'v mut Value, key: &str, path: &Path) -> Result<&'v mut Value> {
    let map = value
        .as_object_mut()
        .with_context(|| format!("{} must hold a JSON object", path.display()))?;
    let slot = map.entry(key.to_string()).or_insert_with(|| json!({}));
    if !slot.is_object() {
        bail!("{}: `{key}` must be an object", path.display());
    }
    Ok(slot)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn places(name: &str) -> Places {
        let dir = std::env::temp_dir().join(format!("gz-mcp-scopes-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("project")).unwrap();
        Places::at(dir.join("config"), Some(&dir.join("project")))
    }

    fn stdio(command: &str) -> ServerSpec {
        ServerSpec {
            command: Some(command.into()),
            ..ServerSpec::default()
        }
    }

    #[test]
    fn a_name_in_several_scopes_means_local_then_project_then_user() {
        let places = places("precedence");
        add(&places, Scope::User, "files", &stdio("user-srv")).unwrap();
        add(&places, Scope::User, "only-user", &stdio("u")).unwrap();
        add(&places, Scope::Project, "files", &stdio("project-srv")).unwrap();
        let loaded = load(&places);
        assert_eq!(
            loaded.find("files").unwrap().spec.command.as_deref(),
            Some("project-srv")
        );
        add(&places, Scope::Local, "files", &stdio("local-srv")).unwrap();
        let loaded = load(&places);
        let files = loaded.find("files").unwrap();
        assert_eq!(files.scope, Scope::Local);
        assert_eq!(files.spec.command.as_deref(), Some("local-srv"));
        assert_eq!(loaded.entries.iter().filter(|e| e.shadowed).count(), 2);
        assert!(loaded.find("only-user").is_some());
        assert!(loaded.problems.is_empty(), "{:?}", loaded.problems);
    }

    #[test]
    fn a_project_server_needs_approval_again_when_it_changes() {
        let places = places("approval");
        add(&places, Scope::Project, "tracker", &stdio("tracker")).unwrap();
        let entry = load(&places).find("tracker").unwrap().clone();
        assert_eq!(entry.approval, Approval::Pending);
        approve(&places, &entry, true).unwrap();
        assert_eq!(
            load(&places).find("tracker").unwrap().approval,
            Approval::Approved
        );
        let file = places.project.clone().unwrap().join(".mcp.json");
        let changed = std::fs::read_to_string(&file)
            .unwrap()
            .replace("\"tracker\"\n", "\"evil\"\n");
        std::fs::write(&file, changed).unwrap();
        assert_eq!(
            load(&places).find("tracker").unwrap().approval,
            Approval::Pending,
            "a changed command is asked about again"
        );
        let entry = load(&places).find("tracker").unwrap().clone();
        approve(&places, &entry, false).unwrap();
        assert_eq!(
            load(&places).find("tracker").unwrap().approval,
            Approval::Declined
        );
        add(&places, Scope::User, "mine", &stdio("x")).unwrap();
        let mine = load(&places).find("mine").unwrap().clone();
        assert_eq!(mine.approval, Approval::NotNeeded);
        assert!(approve(&places, &mine, true).is_err());
    }

    #[test]
    fn adding_and_removing_keep_the_rest_of_each_file() {
        let places = places("edit");
        let user = places.user_file();
        std::fs::create_dir_all(user.parent().unwrap()).unwrap();
        std::fs::write(
            &user,
            "# my servers\n[servers.keep]\ncommand = \"k\"\n\n[servers.gone]\ncommand = \"g\"\n\n[servers.gone.env]\nA = \"1\"\n",
        )
        .unwrap();
        let remote = ServerSpec {
            url: Some("https://x.test/mcp".into()),
            headers: BTreeMap::from([("X-Key".into(), "${KEY}".into())]),
            ..ServerSpec::default()
        };
        add(&places, Scope::User, "remote", &remote).unwrap();
        remove(&places, Some(Scope::User), "gone").unwrap();
        let text = std::fs::read_to_string(&user).unwrap();
        assert!(text.starts_with("# my servers\n[servers.keep]"), "{text}");
        assert!(!text.contains("gone") && !text.contains("A = "), "{text}");
        let loaded = load(&places);
        assert_eq!(
            loaded.find("remote").unwrap().spec.headers["X-Key"],
            "${KEY}"
        );

        let project = places.project.clone().unwrap().join(".mcp.json");
        std::fs::write(&project, r#"{"other": 1, "mcpServers": {}}"#).unwrap();
        add(&places, Scope::Project, "p", &stdio("p")).unwrap();
        let value: Value =
            serde_json::from_str(&std::fs::read_to_string(&project).unwrap()).unwrap();
        assert_eq!(value["other"], 1);
        assert_eq!(value["mcpServers"]["p"]["command"], "p");

        add(&places, Scope::Local, "l", &stdio("l")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(places.local_file())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "the local file is private");
        }
        let err = add(&places, Scope::Local, "l", &stdio("l")).unwrap_err();
        assert!(format!("{err:#}").contains("remove it first"), "{err:#}");
        assert_eq!(remove(&places, None, "l").unwrap().0, Scope::Local);

        add(&places, Scope::User, "dup", &stdio("a")).unwrap();
        add(&places, Scope::Project, "dup", &stdio("b")).unwrap();
        let err = remove(&places, None, "dup").unwrap_err();
        assert!(format!("{err:#}").contains("several scopes"), "{err:#}");
        let err = remove(&places, None, "nothing").unwrap_err();
        assert!(format!("{err:#}").contains("gz mcp list"), "{err:#}");
    }

    #[test]
    fn a_broken_entry_or_file_is_reported_and_the_rest_load() {
        let places = places("broken");
        let user = places.user_file();
        std::fs::create_dir_all(user.parent().unwrap()).unwrap();
        std::fs::write(
            &user,
            "[servers.good]\ncommand = \"g\"\n\n[servers.bad]\nargs = []\n\n[servers.typo]\ncommand = \"x\"\nargz = []\n",
        )
        .unwrap();
        std::fs::write(places.project.clone().unwrap().join(".mcp.json"), "{oops").unwrap();
        let loaded = load(&places);
        assert!(loaded.find("good").is_some());
        assert!(loaded.find("bad").is_none() && loaded.find("typo").is_none());
        let problems = loaded.problems.join("\n");
        assert!(
            problems.contains("server 'bad' needs a command") && problems.contains("skipped"),
            "{problems}"
        );
        assert!(problems.contains("unknown field `argz`"), "{problems}");
        assert!(
            problems.contains(".mcp.json is not valid JSON"),
            "{problems}"
        );
    }

    #[test]
    fn outside_a_project_only_the_user_scope_is_read_or_written() {
        let mut places = places("no-project");
        places.project = None;
        let err = add(&places, Scope::Project, "p", &stdio("p")).unwrap_err();
        assert!(format!("{err:#}").contains("--scope user"), "{err:#}");
        add(&places, Scope::User, "u", &stdio("u")).unwrap();
        assert!(load(&places).find("u").is_some());
    }
}
