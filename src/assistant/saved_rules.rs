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
    /// The allow/ask/deny lists and mode settings
    /// ([`crate::assistant::rules`]), kept as they are when "always"
    /// answers are saved.
    #[serde(flatten)]
    rules: toml::Table,
    /// Workspace folder → its saved rules.
    #[serde(default)]
    workspaces: BTreeMap<PathBuf, Rules>,
}

/// Just the saved answers, written below the rest of the file.
#[derive(Serialize)]
struct Answers<'a> {
    workspaces: &'a BTreeMap<PathBuf, Rules>,
}

/// The keys of a folder's table that gz writes ([`Rules`]' fields); any
/// other key in that table is the user's and is left alone.
const RULE_KEYS: [&str; 7] = [
    "read_folders",
    "edits",
    "commands",
    "command_starts",
    "scripts",
    "hosts",
    "local_tools",
];

/// Makes `doc`'s `[workspaces]` say `workspaces`, changing only what
/// differs: a folder whose rules are all gone loses them (and its table,
/// unless the user put other keys in it), a new one gets a table at the
/// end, and a list gains or loses just the entries that changed.
fn merge_workspaces(
    doc: &mut toml_edit::DocumentMut,
    workspaces: &BTreeMap<PathBuf, Rules>,
) -> Result<()> {
    let fresh: toml_edit::DocumentMut = toml::to_string(&Answers { workspaces })
        .context("cannot encode saved permissions")?
        .parse()
        .context("cannot re-read the encoded permissions")?;
    let wanted = fresh
        .get("workspaces")
        .and_then(|item| item.as_table_like());
    let has_content = !doc.to_string().trim().is_empty();
    // Inline `workspaces = { ... }` takes no positions or line breaks.
    let as_own_tables = doc.get("workspaces").is_none_or(|item| item.is_table());
    let mut next_position = last_table_position(doc.as_table()).map_or(0, |last| last + 1);
    if !doc.contains_key("workspaces") {
        let mut table = toml_edit::Table::new();
        table.set_implicit(true);
        doc.insert("workspaces", toml_edit::Item::Table(table));
    }
    let tables = doc
        .get_mut("workspaces")
        .and_then(|item| item.as_table_like_mut())
        .context("`workspaces` isn't a table; fix it by hand")?;
    let gone: Vec<String> = tables
        .iter()
        .map(|(folder, _)| folder.to_string())
        .filter(|folder| wanted.is_none_or(|wanted| !wanted.contains_key(folder)))
        .collect();
    for folder in gone {
        forget_folder(tables, &folder);
    }
    for (folder, rules) in wanted.iter().flat_map(|wanted| wanted.iter()) {
        match tables.get_mut(folder) {
            None => {
                let mut rules = rules.clone();
                if let Some(table) = rules.as_table_mut().filter(|_| as_own_tables) {
                    // Below everything already in the file, set off by a
                    // blank line, so it never lands between another
                    // table and the comment written above that table.
                    table.set_position(next_position);
                    next_position += 1;
                    if has_content {
                        table.decor_mut().set_prefix("\n");
                    }
                }
                tables.insert(folder, rules);
            }
            Some(kept) => {
                let kept = kept.as_table_like_mut().with_context(|| {
                    format!("the rules for {folder} aren't a table; fix them by hand")
                })?;
                let rules = rules
                    .as_table_like()
                    .context("encoded rules aren't a table")?;
                merge_rules(kept, rules);
            }
        }
    }
    if tables.is_empty() {
        doc.remove("workspaces");
    }
    Ok(())
}

/// Drops the rules gz wrote for a folder that has none left. The table
/// itself goes only when nothing else (a key gz doesn't know) is in it.
fn forget_folder(tables: &mut dyn toml_edit::TableLike, folder: &str) {
    let Some(table) = tables
        .get_mut(folder)
        .and_then(|item| item.as_table_like_mut())
    else {
        tables.remove(folder);
        return;
    };
    for key in RULE_KEYS {
        table.remove(key);
    }
    if table.is_empty() {
        tables.remove(folder);
    }
}

/// The highest print position of any table in `table` or below it.
fn last_table_position(table: &toml_edit::Table) -> Option<usize> {
    table
        .iter()
        .filter_map(|(_, item)| item.as_table())
        .flat_map(|child| [child.position(), last_table_position(child)])
        .flatten()
        .max()
}

/// Brings one folder's table in line with `wanted`, keeping comments and
/// the user's own keys.
fn merge_rules(kept: &mut dyn toml_edit::TableLike, wanted: &dyn toml_edit::TableLike) {
    for key in RULE_KEYS {
        match (wanted.get(key), kept.get_mut(key)) {
            (None, Some(_)) => {
                kept.remove(key);
            }
            (Some(value), None) => {
                kept.insert(key, value.clone());
            }
            (Some(value), Some(old)) => merge_value(old, value),
            (None, None) => {}
        }
    }
}

/// A list keeps its entries (and their comments) that stay and gains the
/// new ones at the end; any other value is replaced, keeping its comment.
fn merge_value(old: &mut toml_edit::Item, wanted: &toml_edit::Item) {
    if let (Some(list), Some(wanted)) = (old.as_array_mut(), wanted.as_array()) {
        let wanted: Vec<&str> = wanted.iter().filter_map(|v| v.as_str()).collect();
        for index in (0..list.len()).rev() {
            let stays = list
                .get(index)
                .and_then(|v| v.as_str())
                .is_some_and(|text| wanted.contains(&text));
            if !stays {
                remove_entry(list, index);
            }
        }
        let missing: Vec<&str> = wanted
            .iter()
            .copied()
            .filter(|text| !list.iter().any(|v| v.as_str() == Some(text)))
            .collect();
        for text in missing {
            list.push(text);
        }
        return;
    }
    match (old.as_value_mut(), wanted.as_value()) {
        (Some(old), Some(wanted)) => {
            let decor = old.decor().clone();
            *old = wanted.clone();
            *old.decor_mut() = decor;
        }
        _ => *old = wanted.clone(),
    }
}

/// Removes one entry of a list, keeping the layout around it. In a
/// multi-line list the text before an entry's first line break is the end
/// of the line above (with that line's comment) and the rest is the entry's
/// own indent; so what follows keeps the line end above the removed entry
/// and drops the removed entry's own line end, comment included. In a
/// one-line list what follows takes the removed entry's spacing, so
/// `["a", "b"]` loses "a" as `["b"]`.
fn remove_entry(list: &mut toml_edit::Array, index: usize) {
    let removed = list.remove(index);
    let Some(removed_prefix) = removed.decor().prefix().and_then(|raw| raw.as_str()) else {
        return;
    };
    let joined = |after: &str| match (removed_prefix.split_once('\n'), after.split_once('\n')) {
        (Some((line_end_above, _)), Some((_, indent))) => {
            Some(format!("{line_end_above}\n{indent}"))
        }
        (None, None) => Some(removed_prefix.to_string()),
        _ => None,
    };
    match list.get_mut(index) {
        Some(next) => {
            let after = next
                .decor()
                .prefix()
                .and_then(|raw| raw.as_str())
                .unwrap_or_default()
                .to_string();
            if let Some(prefix) = joined(&after) {
                next.decor_mut().set_prefix(prefix);
            }
        }
        None => {
            let after = list.trailing().as_str().unwrap_or_default().to_string();
            // A one-line list's closing bracket keeps its own spacing.
            if after.contains('\n') {
                if let Some(trailing) = joined(&after) {
                    list.set_trailing(trailing);
                }
            }
        }
    }
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
    /// One tool of a local MCP server, as `server/tool`.
    LocalTool(String),
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
            Rule::LocalTool(tool) => write!(f, "MCP tool `{tool}`"),
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

    /// Saves the answers, edited into the file in place: its comments,
    /// layout and any keys gz doesn't know (outside or inside a folder's
    /// table) stay as written.
    fn write(&self, file: &File) -> Result<()> {
        let text = match self.read_document() {
            Some(mut doc) => {
                merge_workspaces(&mut doc, &file.workspaces)
                    .with_context(|| format!("cannot update {}", self.path.display()))?;
                doc.to_string()
            }
            None => toml::to_string_pretty(file).context("cannot encode saved permissions")?,
        };
        write_private(&self.path, &text)
    }

    /// The file as an editable document, when it exists and parses.
    fn read_document(&self) -> Option<toml_edit::DocumentMut> {
        std::fs::read_to_string(&self.path).ok()?.parse().ok()
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
            Rule::LocalTool(tool) => rules.local_tools.insert(tool.clone()),
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
            out.extend(
                rules
                    .local_tools
                    .iter()
                    .cloned()
                    .map(Rule::LocalTool)
                    .map(entry),
            );
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
            Rule::LocalTool(tool) => rules.local_tools.remove(tool),
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
        crate::assistant::sandbox::tests::seen_by_model(&sandbox, &dir.join("a.txt"));
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
    fn saving_an_answer_keeps_the_allow_ask_deny_lists() {
        let (_dir, sandbox) = temp_workspace("rules-keep-lists");
        let store = store("keep-lists");
        std::fs::create_dir_all(store.path.parent().unwrap()).unwrap();
        std::fs::write(
            &store.path,
            "# never delete\ndeny = [\"run_command(rm:*)\"]  # rm stays manual\n\
             default_mode = \"plan\"\n",
        )
        .unwrap();
        let cmd = plan(&sandbox, ToolName::RunCommand, &json!({"command": "ls"})).unwrap();
        store.grant(&sandbox.cwd(), &cmd).unwrap();
        let other = plan(&sandbox, ToolName::RunCommand, &json!({"command": "pwd"})).unwrap();
        store.grant(&sandbox.cwd(), &other).unwrap();
        let text = std::fs::read_to_string(&store.path).unwrap();
        assert!(
            text.contains("# never delete") && text.contains("# rm stays manual"),
            "comments stay: {text}"
        );
        assert!(store.all().unwrap().allows(&other));
        let table: toml::Table = toml::from_str(&text).unwrap();
        assert_eq!(
            table["deny"][0].as_str(),
            Some("run_command(rm:*)"),
            "{text}"
        );
        assert_eq!(table["default_mode"].as_str(), Some("plan"), "{text}");
        assert!(store.all().unwrap().allows(&cmd));
    }

    #[test]
    fn rule_keys_name_every_field_gz_writes() {
        let every = Rules {
            read_folders: vec!["/a".into()],
            edits: true,
            commands: ["ls".to_string()].into(),
            command_starts: ["git".to_string()].into(),
            scripts: true,
            hosts: ["example.com".to_string()].into(),
            local_tools: ["fs/read".to_string()].into(),
        };
        let table: toml::Table = toml::from_str(&toml::to_string(&every).unwrap()).unwrap();
        let mut keys: Vec<&str> = table.keys().map(String::as_str).collect();
        let mut known = RULE_KEYS.to_vec();
        keys.sort_unstable();
        known.sort_unstable();
        assert_eq!(keys, known);
    }

    #[test]
    fn adding_and_removing_rules_keeps_comments_and_other_keys_in_folder_tables() {
        let store = store("keep-folder-comments");
        std::fs::create_dir_all(store.path.parent().unwrap()).unwrap();
        std::fs::write(
            &store.path,
            "# top note\n\
             default_mode = \"plan\" # mine\n\n\
             # the repo I work in\n\
             [workspaces.\"/work\"]\n\
             note = \"left by hand\"\n\
             commands = [\n    \"cargo test\", # the usual\n    \"ls\",\n]\n\
             edits = true # trusted\n\n\
             # an old checkout\n\
             [workspaces.\"/old\"]\n\
             scripts = true\n",
        )
        .unwrap();
        let work = Path::new("/work");
        assert!(store
            .add(work, &Rule::Host("docs.example.com".into()))
            .unwrap());
        assert!(store
            .revoke(&SavedRule {
                workspace: work.into(),
                rule: Rule::Command("ls".into()),
            })
            .unwrap());
        assert!(store
            .revoke(&SavedRule {
                workspace: "/old".into(),
                rule: Rule::Scripts,
            })
            .unwrap());
        let text = std::fs::read_to_string(&store.path).unwrap();
        for kept in [
            "# top note",
            "# mine",
            "# the repo I work in",
            "note = \"left by hand\"",
            "\"cargo test\", # the usual",
            "edits = true # trusted",
        ] {
            assert!(text.contains(kept), "{kept:?} stays: {text}");
        }
        assert!(!text.contains("\"ls\""), "{text}");
        assert!(!text.contains("/old"), "an emptied folder goes: {text}");
        let rules = store.list(Some(work)).unwrap();
        assert!(rules
            .iter()
            .any(|r| r.rule == Rule::Host("docs.example.com".into())));
        assert!(rules
            .iter()
            .any(|r| r.rule == Rule::Command("cargo test".into())));
        assert!(rules.iter().any(|r| r.rule == Rule::Edits));
        // Off again: the key goes, the rest stays.
        assert!(store
            .revoke(&SavedRule {
                workspace: work.into(),
                rule: Rule::Edits,
            })
            .unwrap());
        let text = std::fs::read_to_string(&store.path).unwrap();
        assert!(!text.contains("edits"), "{text}");
        assert!(text.contains("note = \"left by hand\""), "{text}");
    }

    #[test]
    fn a_first_rule_in_a_commented_file_adds_its_table_below() {
        let store = store("first-rule");
        std::fs::create_dir_all(store.path.parent().unwrap()).unwrap();
        std::fs::write(&store.path, "# only a note\nallow = []\n").unwrap();
        store.add(Path::new("/w"), &Rule::Scripts).unwrap();
        let text = std::fs::read_to_string(&store.path).unwrap();
        assert!(text.starts_with("# only a note\nallow = []\n"), "{text}");
        assert!(
            text.contains("[workspaces.\"/w\"]\nscripts = true"),
            "{text}"
        );
        assert!(!text.contains("[workspaces]\n"), "no bare header: {text}");
    }

    #[test]
    fn revoking_a_folders_last_rule_keeps_the_users_own_keys() {
        let store = store("keep-folder-note");
        std::fs::create_dir_all(store.path.parent().unwrap()).unwrap();
        std::fs::write(
            &store.path,
            "[workspaces.\"/old\"]\n# why this folder\nnote = \"x\"\nscripts = true\n",
        )
        .unwrap();
        assert!(store
            .revoke(&SavedRule {
                workspace: "/old".into(),
                rule: Rule::Scripts,
            })
            .unwrap());
        let text = std::fs::read_to_string(&store.path).unwrap();
        assert_eq!(
            text,
            "[workspaces.\"/old\"]\n# why this folder\nnote = \"x\"\n"
        );
        assert!(store.list(None).unwrap().is_empty(), "{text}");
    }

    #[test]
    fn a_new_folder_table_goes_last_after_a_blank_line() {
        let store = store("new-folder-last");
        std::fs::create_dir_all(store.path.parent().unwrap()).unwrap();
        let before =
            "[workspaces]\n# folders\n\n[workspaces.\"/work\"]\nedits = true\n\n[other]\nkey = 1\n";
        std::fs::write(&store.path, before).unwrap();
        store.add(Path::new("/new"), &Rule::Scripts).unwrap();
        let text = std::fs::read_to_string(&store.path).unwrap();
        assert_eq!(
            text,
            format!("{before}\n[workspaces.\"/new\"]\nscripts = true\n")
        );
    }

    #[test]
    fn removing_list_entries_keeps_the_lists_layout() {
        let store = store("list-layout");
        std::fs::create_dir_all(store.path.parent().unwrap()).unwrap();
        std::fs::write(
            &store.path,
            "[workspaces.\"/w\"]\n\
             hosts = [\"a.example\", \"b.example\", \"c.example\"]\n\
             commands = [\n    \"ls\", # about ls\n    \"pwd\", # about pwd\n    \"whoami\",\n]\n",
        )
        .unwrap();
        let revoke = |rule| {
            assert!(store
                .revoke(&SavedRule {
                    workspace: "/w".into(),
                    rule,
                })
                .unwrap());
        };
        revoke(Rule::Host("a.example".into()));
        revoke(Rule::Host("c.example".into()));
        revoke(Rule::Command("ls".into()));
        let text = std::fs::read_to_string(&store.path).unwrap();
        assert_eq!(
            text,
            "[workspaces.\"/w\"]\n\
             hosts = [\"b.example\"]\n\
             commands = [\n    \"pwd\", # about pwd\n    \"whoami\",\n]\n"
        );
        revoke(Rule::Command("whoami".into()));
        let text = std::fs::read_to_string(&store.path).unwrap();
        assert!(
            text.ends_with("commands = [\n    \"pwd\", # about pwd\n]\n"),
            "{text}"
        );
        revoke(Rule::Command("pwd".into()));
        let text = std::fs::read_to_string(&store.path).unwrap();
        assert!(!text.contains("about"), "{text}");
    }

    #[test]
    fn dotted_and_inline_workspaces_stay_valid() {
        for (name, before) in [
            ("dotted", "workspaces.\"/a\".scripts = true\n"),
            ("inline", "workspaces = { \"/a\" = { scripts = true } }\n"),
        ] {
            let store = store(name);
            std::fs::create_dir_all(store.path.parent().unwrap()).unwrap();
            std::fs::write(&store.path, before).unwrap();
            store.add(Path::new("/b"), &Rule::Edits).unwrap();
            let text = std::fs::read_to_string(&store.path).unwrap();
            let rules = store.list(None).unwrap();
            assert_eq!(rules.len(), 2, "{name}: {text}");
            assert!(text.parse::<toml_edit::DocumentMut>().is_ok(), "{text}");
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
