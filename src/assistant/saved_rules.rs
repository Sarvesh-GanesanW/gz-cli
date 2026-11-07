//! "Allow always" answers for local tools, saved in `permissions.toml` next
//! to the gz config (owner-only, written atomically). Rules are keyed by
//! the absolute workspace folder; a folder's rules also apply to folders
//! inside it.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::assistant::permissions::Rules;
use crate::assistant::tools::Planned;

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    /// Workspace folder → its saved rules.
    #[serde(default)]
    workspaces: BTreeMap<PathBuf, Rules>,
}

/// One saved rule, for listing and revoking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rule {
    Reads(PathBuf),
    Edits,
    Command(String),
    Scripts,
}

impl fmt::Display for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Rule::Reads(folder) => write!(f, "reads in {}", folder.display()),
            Rule::Edits => write!(f, "all edits"),
            Rule::Command(command) => write!(f, "command `{command}`"),
            Rule::Scripts => write!(f, "scripts"),
        }
    }
}

/// A saved rule and the workspace folder it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedRule {
    pub workspace: PathBuf,
    pub rule: Rule,
}

/// The rules file, at an explicit path (tests) or the default location.
#[derive(Debug, Clone)]
pub struct Store {
    path: PathBuf,
}

impl Store {
    /// `<gz config folder>/permissions.toml` (honors `XDG_CONFIG_HOME`).
    pub fn open() -> Result<Self> {
        let config = crate::config::config_path()?;
        let dir = config.parent().context("config path has no folder")?;
        Ok(Self {
            path: dir.join("permissions.toml"),
        })
    }

    #[cfg(test)]
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    fn read(&self) -> Result<File> {
        if !self.path.exists() {
            return Ok(File::default());
        }
        let text = std::fs::read_to_string(&self.path)
            .with_context(|| format!("cannot read {}", self.path.display()))?;
        toml::from_str(&text).with_context(|| format!("cannot parse {}", self.path.display()))
    }

    /// Writes to a temp file and renames it, so a crash never leaves a
    /// half-written rules file.
    fn write(&self, file: &File) -> Result<()> {
        let dir = self.path.parent().context("rules path has no folder")?;
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        let text = toml::to_string_pretty(file).context("cannot encode saved permissions")?;
        let tmp = self.path.with_extension("toml.tmp");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        {
            use std::io::Write;
            let mut handle = options
                .open(&tmp)
                .with_context(|| format!("cannot write {}", tmp.display()))?;
            handle
                .write_all(text.as_bytes())
                .with_context(|| format!("cannot write {}", tmp.display()))?;
        }
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("cannot save {}", self.path.display()))
    }

    /// The rules that apply in `root`: its own plus those of any folder
    /// containing it.
    pub fn rules_for(&self, root: &Path) -> Result<Rules> {
        let file = self.read()?;
        let mut rules = Rules::default();
        for (workspace, saved) in &file.workspaces {
            if root.starts_with(workspace) {
                rules.merge(saved);
            }
        }
        Ok(rules)
    }

    /// Saves "always" for what `planned` needs, under `root`.
    pub fn grant(&self, root: &Path, planned: &Planned) -> Result<()> {
        let mut file = self.read()?;
        file.workspaces
            .entry(root.to_path_buf())
            .or_default()
            .grant(planned);
        self.write(&file)
    }

    /// Every saved rule that applies in `root` (or all of them).
    pub fn list(&self, root: Option<&Path>) -> Result<Vec<SavedRule>> {
        let file = self.read()?;
        let mut out = Vec::new();
        for (workspace, rules) in &file.workspaces {
            if root.is_some_and(|r| !r.starts_with(workspace)) {
                continue;
            }
            let entry = |rule| SavedRule {
                workspace: workspace.clone(),
                rule,
            };
            out.extend(
                rules
                    .read_folders
                    .iter()
                    .cloned()
                    .map(Rule::Reads)
                    .map(entry),
            );
            if rules.edits {
                out.push(entry(Rule::Edits));
            }
            out.extend(rules.commands.iter().cloned().map(Rule::Command).map(entry));
            if rules.scripts {
                out.push(entry(Rule::Scripts));
            }
        }
        Ok(out)
    }

    /// Removes one saved rule; returns whether it existed.
    pub fn revoke(&self, saved: &SavedRule) -> Result<bool> {
        let mut file = self.read()?;
        let Some(rules) = file.workspaces.get_mut(&saved.workspace) else {
            return Ok(false);
        };
        let removed = match &saved.rule {
            Rule::Reads(folder) => {
                let before = rules.read_folders.len();
                rules.read_folders.retain(|f| f != folder);
                before != rules.read_folders.len()
            }
            Rule::Edits => std::mem::take(&mut rules.edits),
            Rule::Command(command) => rules.commands.remove(command),
            Rule::Scripts => std::mem::take(&mut rules.scripts),
        };
        if rules.is_empty() {
            file.workspaces.remove(&saved.workspace);
        }
        if removed {
            self.write(&file)?;
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant::sandbox::tests::temp_workspace;
    use crate::assistant::tools::{plan, ToolName};
    use serde_json::json;

    fn store(name: &str) -> Store {
        let dir = std::env::temp_dir().join(format!("gz-rules-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Store::at(dir.join("permissions.toml"))
    }

    #[test]
    fn saved_rules_round_trip_and_stay_private() {
        let (dir, sandbox) = temp_workspace("rules-roundtrip");
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        let store = store("roundtrip");
        let cmd = plan(
            &sandbox,
            ToolName::RunCommand,
            &json!({"command": "cargo test"}),
        )
        .unwrap();
        let write = plan(
            &sandbox,
            ToolName::WriteFile,
            &json!({"path": "a.txt", "content": "y"}),
        )
        .unwrap();
        store.grant(sandbox.root(), &cmd).unwrap();
        store.grant(sandbox.root(), &write).unwrap();
        let rules = store.rules_for(sandbox.root()).unwrap();
        assert!(rules.allows(&cmd) && rules.allows(&write));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&store.path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let listed = store.list(Some(sandbox.root())).unwrap();
        assert_eq!(listed.len(), 2);
        assert!(store.revoke(&listed[0]).unwrap());
        assert_eq!(store.list(None).unwrap().len(), 1);
    }

    #[test]
    fn a_folders_rules_apply_inside_it_only() {
        let (_dir, sandbox) = temp_workspace("rules-ancestor");
        let store = store("ancestor");
        let cmd = plan(&sandbox, ToolName::RunCommand, &json!({"command": "ls"})).unwrap();
        store.grant(sandbox.root(), &cmd).unwrap();
        let inner = sandbox.root().join("sub");
        assert!(
            store.rules_for(&inner).unwrap().allows(&cmd),
            "applies to subfolders"
        );
        let outside = sandbox.root().parent().unwrap().join("elsewhere");
        assert!(!store.rules_for(&outside).unwrap().allows(&cmd));
        assert!(store.list(Some(&outside)).unwrap().is_empty());
    }
}
