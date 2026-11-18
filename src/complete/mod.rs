//! Dynamic shell completion: `gz __complete <shell> <words…>`, called by
//! the scripts `gz completion <shell>` prints (see [`scripts`]).
//!
//! Completion must be fast and must never ask or fail: lists come from a
//! per-profile cache ([`cache`]); a stale one is served while a background
//! `gz __complete refresh` reads it again, and a missing one is read with a
//! short wait. Not signed in, or anything going wrong, completes nothing.

pub mod cache;
pub mod scripts;
pub mod source;
pub mod target;

use std::time::Duration;

use anyhow::Result;
use clap::{Arg, Command};

use crate::cmd::Runtime;
use crate::config::{self, Config};
use crate::output::OutputFormat;
use crate::resolve::lookup::Catalog;
use crate::resolve::roles::{Role, Text};
use crate::resolve::walk::{command_tree, Walk};
use crate::resolve::Kind;
use crate::table_history::TableName;
use cache::Cache;
use source::Source;
use target::{Item, Target};

/// The hidden first word that asks for completions.
pub const COMMAND: &str = "__complete";
/// How long a completion waits for a list it has never read.
const INLINE_WAIT: Duration = Duration::from_millis(1500);
const INLINE_HTTP_SECS: u64 = 2;
const REFRESH_HTTP_SECS: u64 = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shell {
    Bash,
    Zsh,
    Fish,
    Powershell,
}

impl Shell {
    fn parse(name: &str) -> Option<Self> {
        match name {
            "bash" => Some(Shell::Bash),
            "zsh" => Some(Shell::Zsh),
            "fish" => Some(Shell::Fish),
            "powershell" => Some(Shell::Powershell),
            _ => None,
        }
    }
}

/// `__complete <shell> <words…>` or `__complete refresh <profile> <key>`.
pub async fn run(args: &[String]) -> Result<()> {
    match args {
        [refresh, profile, key] if refresh == "refresh" => {
            refresh_now(profile, key).await;
            Ok(())
        }
        [shell, words @ ..] => {
            let Some(shell) = Shell::parse(shell) else {
                return Ok(());
            };
            let (before, current) = line_words(shell, words);
            let items = complete(&before, &current).await;
            print!("{}", render(shell, &current, &items));
            Ok(())
        }
        [] => Ok(()),
    }
}

/// The words left of the cursor (program name dropped) and the one under it.
fn line_words(shell: Shell, args: &[String]) -> (Vec<String>, String) {
    if shell == Shell::Bash {
        let line = std::env::var("GZ_COMPLETE_LINE").unwrap_or_default();
        return split_line(&line);
    }
    let current = std::env::var("GZ_COMPLETE_WORD").unwrap_or_default();
    (args.iter().map(|w| unquote(w)).collect(), unquote(&current))
}

/// Splits a bash line like the shell does; the last word is the one under
/// the cursor (empty after a space).
fn split_line(line: &str) -> (Vec<String>, String) {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for c in line.chars() {
        match (quote, c) {
            _ if escaped => {
                word.push(c);
                escaped = false;
            }
            (Some('\''), '\'') | (Some('"'), '"') => quote = None,
            (Some('"'), '\\') | (None, '\\') => escaped = true,
            (Some(_), _) => word.push(c),
            (None, '\'' | '"') => quote = Some(c),
            (None, c) if c.is_whitespace() => {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
            }
            (None, c) => word.push(c),
        }
    }
    (words.into_iter().skip(1).collect(), word)
}

/// A word as typed, without its quotes and backslashes.
fn unquote(word: &str) -> String {
    split_line(&format!("gz {word}")).1
}

/// The candidates for `current`, best effort.
async fn complete(before: &[String], current: &str) -> Vec<Item> {
    let root = command_tree();
    let (walked, found) = target::target(&root, before, current);
    let items = match found {
        Target::Subcommand(command, positional) => {
            let mut items = target::subcommands(command);
            if let Some(arg) = positional {
                items.extend(values(&root, &walked, arg, current).await);
            }
            items
        }
        Target::Flag(command) => target::flags(command),
        Target::Value { arg, prefix, .. } => {
            let typed = current.strip_prefix(prefix.as_str()).unwrap_or(current);
            values(&root, &walked, arg, typed)
                .await
                .into_iter()
                .map(|item| Item::new(format!("{prefix}{}", item.value), item.about))
                .collect()
        }
        Target::Nothing => Vec::new(),
    };
    matching(items, current)
}

async fn values(root: &Command, walked: &Walk, arg: &Arg, typed: &str) -> Vec<Item> {
    let fixed = target::choices(arg);
    if !fixed.is_empty() {
        return fixed;
    }
    let Some(role) = target::role(walked, root, arg) else {
        return Vec::new();
    };
    let Some(mut context) = Context::open(walked) else {
        return Vec::new();
    };
    context.values(walked, arg, role, typed).await
}

/// Items starting with `typed`; in any case when none match in the same case.
fn matching(items: Vec<Item>, typed: &str) -> Vec<Item> {
    let same: Vec<Item> = items
        .iter()
        .filter(|i| i.value.starts_with(typed))
        .cloned()
        .collect();
    if !same.is_empty() {
        return dedup(same);
    }
    let lower = typed.to_lowercase();
    dedup(
        items
            .into_iter()
            .filter(|i| i.value.to_lowercase().starts_with(&lower))
            .collect(),
    )
}

fn dedup(items: Vec<Item>) -> Vec<Item> {
    let mut seen = std::collections::HashSet::new();
    items
        .into_iter()
        .filter(|i| seen.insert(i.value.clone()))
        .collect()
}

fn render(shell: Shell, current: &str, items: &[Item]) -> String {
    let mut out = String::new();
    for item in items {
        let line = match shell {
            Shell::Bash => bash_word(current, &item.value),
            _ => format!("{}\t{}", item.value, one_line(&item.about)),
        };
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// Bash replaces only the part of the word after its last `=` or `:`,
/// and inserts the text as is, so spaces are escaped.
fn bash_word(current: &str, value: &str) -> String {
    let cut = current.rfind(['=', ':']).map(|at| at + 1).unwrap_or(0);
    let tail = value.get(cut..).unwrap_or(value);
    let mut escaped = String::new();
    for c in tail.chars() {
        if c.is_whitespace() || "'\"\\$`()&;|<>*?![]{}#".contains(c) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

fn one_line(text: &str) -> String {
    text.replace(['\t', '\n', '\r'], " ")
}

/// The profile and its sign-in, for the lists that need the server.
struct Context {
    profile: String,
    config: Config,
    cache: Option<Cache>,
    /// `None` when the profile has no sign-in.
    catalog: Option<Catalog>,
}

impl Context {
    fn open(walked: &Walk) -> Option<Self> {
        let config = config::load_config().ok()?;
        let profile = config.active_name(walked.first_value("profile"));
        let host = walked
            .first_value("host")
            .map(String::from)
            .or_else(|| std::env::var("GZ_HOST").ok());
        let catalog =
            runtime(&config, &profile, host, INLINE_HTTP_SECS).map(|rt| Catalog::new(&rt));
        Some(Self {
            cache: Cache::for_profile(&profile).ok(),
            profile,
            config,
            catalog,
        })
    }

    async fn values(&mut self, walked: &Walk, arg: &Arg, role: Role, typed: &str) -> Vec<Item> {
        match role {
            Role::Id(Kind::Project) if takes_job_alone(walked, arg) => {
                let mut items = self.names(walked, Kind::Project).await;
                items.extend(self.list(Source::Names(Kind::Job, None)).await);
                items
            }
            Role::Id(kind) | Role::Name(kind) => self.names(walked, kind).await,
            Role::Ids(kinds) => {
                let nth = walked.values_of(arg.get_id().as_str()).len();
                match kinds.get(nth).copied().flatten() {
                    Some(kind) => self.names(walked, kind).await,
                    None => Vec::new(),
                }
            }
            Role::DesignerId => match designer_kind(walked) {
                Some(kind) => self.names(walked, kind).await,
                None => Vec::new(),
            },
            Role::Text(text) => self.text(walked, text, typed).await,
        }
    }

    async fn text(&mut self, walked: &Walk, text: Text, typed: &str) -> Vec<Item> {
        match text {
            Text::Profile => {
                let mut names: Vec<&String> = self.config.profiles.keys().collect();
                names.sort();
                names.into_iter().map(|n| Item::new(n, "profile")).collect()
            }
            Text::Runtime => self.list(Source::Runtimes).await,
            Text::Namespace => match walked.first_value("warehouse") {
                Some(warehouse) => databases(&self.list(schema(warehouse)).await),
                None => Vec::new(),
            },
            Text::TableIn => {
                let database = walked
                    .first_value("namespace")
                    .or_else(|| walked.first_value("database"));
                match (walked.first_value("warehouse"), database) {
                    (Some(warehouse), Some(database)) => {
                        let items = self.list(schema(warehouse)).await;
                        tables_in(&items, database)
                    }
                    _ => Vec::new(),
                }
            }
            Text::Table => match walked.first_value("warehouse") {
                Some(warehouse) => self.database_tables(warehouse, typed).await,
                None => self.qualified_tables(typed).await,
            },
            Text::DatabaseTable => match self.warehouse(walked) {
                Some(warehouse) => self.database_tables(&warehouse, typed).await,
                None => Vec::new(),
            },
            Text::Ref => match self.table_on_line(walked) {
                Some((warehouse, table)) => self.list(Source::Refs { warehouse, table }).await,
                None => Vec::new(),
            },
        }
    }

    /// Names of `kind`; jobs and notebooks of the project on the line.
    async fn names(&mut self, walked: &Walk, kind: Kind) -> Vec<Item> {
        let project = match kind.in_project() {
            true => self.project_on_line(walked).await,
            false => None,
        };
        self.list(Source::Names(kind, project)).await
    }

    /// The project given on the line, by id or by a name in the cache.
    async fn project_on_line(&mut self, walked: &Walk) -> Option<i64> {
        let root = command_tree();
        let command = crate::resolve::walk::leaf(&root, &walked.path);
        let given = walked.filled.iter().find(|filled| {
            command
                .get_arguments()
                .find(|arg| arg.get_id() == &filled.arg)
                .and_then(|arg| crate::resolve::roles::role_of(&walked.key(), command, arg))
                .is_some_and(|role| is_project(role, filled.nth))
        })?;
        if let Ok(id) = given.value.parse() {
            return Some(id);
        }
        let projects = self.list(Source::Names(Kind::Project, None)).await;
        projects
            .iter()
            .find(|p| p.value.eq_ignore_ascii_case(&given.value))
            .and_then(|p| source::id_in(&p.about))
    }

    /// The warehouse for `DATABASE.TABLE`: `--warehouse`, else `gz sql`'s.
    fn warehouse(&self, walked: &Walk) -> Option<String> {
        walked
            .first_value("warehouse")
            .map(String::from)
            .or_else(|| self.config.profile(&self.profile).sql_warehouse)
    }

    /// The table a `--from`/`--to`/`--at` belongs to.
    fn table_on_line(&self, walked: &Walk) -> Option<(String, TableName)> {
        let typed = walked.first_value("table")?;
        let parts: Vec<&str> = typed.split('.').collect();
        let (warehouse, database, table) = match parts.as_slice() {
            [warehouse, database, table] => (warehouse.to_string(), *database, *table),
            [database, table] => (self.warehouse(walked)?, *database, *table),
            _ => return None,
        };
        Some((
            warehouse,
            TableName {
                database: database.to_string(),
                table: table.to_string(),
            },
        ))
    }

    /// `WAREHOUSE.`, then `WAREHOUSE.DATABASE.`, then the tables.
    async fn qualified_tables(&mut self, typed: &str) -> Vec<Item> {
        let Some((warehouse, rest)) = typed.split_once('.') else {
            let warehouses = self.list(Source::Names(Kind::Warehouse, None)).await;
            return warehouses
                .into_iter()
                .map(|w| Item::new(format!("{}.", w.value), "warehouse"))
                .collect();
        };
        let warehouse = warehouse.to_string();
        self.database_tables(&warehouse, rest)
            .await
            .into_iter()
            .map(|item| Item::new(format!("{warehouse}.{}", item.value), item.about))
            .collect()
    }

    /// `DATABASE.`, then the database's tables.
    async fn database_tables(&mut self, warehouse: &str, typed: &str) -> Vec<Item> {
        let items = self.list(schema(warehouse)).await;
        match typed.split_once('.') {
            None => databases(&items)
                .into_iter()
                .map(|d| Item::new(format!("{}.", d.value), d.about))
                .collect(),
            Some((database, _)) => tables_in(&items, database)
                .into_iter()
                .map(|t| Item::new(format!("{database}.{}", t.value), t.about))
                .collect(),
        }
    }

    /// A list from the cache, read again when stale (in the background) or
    /// missing (with a short wait). Empty when not signed in.
    async fn list(&mut self, source: Source) -> Vec<Item> {
        let (Some(catalog), Some(cache)) = (&mut self.catalog, &self.cache) else {
            return Vec::new();
        };
        let key = source.key();
        let now = config::now_unix();
        if let Some(cached) = cache.get(&key, now) {
            if !cached.fresh {
                refresh_later(cache, &self.profile, &key);
            }
            return cached.items;
        }
        match tokio::time::timeout(INLINE_WAIT, source.fetch(catalog)).await {
            Ok(Ok(items)) => {
                cache.put(&key, &items, now).ok();
                items
            }
            _ => {
                refresh_later(cache, &self.profile, &key);
                Vec::new()
            }
        }
    }
}

/// `gz jobs run <TAB>`: the first value may be a project or a job alone.
fn takes_job_alone(walked: &Walk, arg: &Arg) -> bool {
    let root = command_tree();
    let command = crate::resolve::walk::leaf(&root, &walked.path);
    crate::resolve::roles::project_before_job(&walked.key(), command)
        .is_some_and(|project| project.get_id() == arg.get_id())
}

/// Whether the `nth` value of an argument with `role` is a project. While
/// completing, listed values are read left to right (`PROJECT JOB …`).
fn is_project(role: Role, nth: usize) -> bool {
    match role {
        Role::Id(kind) => kind == Kind::Project,
        Role::Ids(kinds) => kinds.get(nth).copied().flatten() == Some(Kind::Project),
        _ => false,
    }
}

fn designer_kind(walked: &Walk) -> Option<Kind> {
    match walked.first_value("kind")? {
        "dataset" => Some(Kind::Dataset),
        "dashboard" => Some(Kind::Dashboard),
        "visualization" => Some(Kind::Visualization),
        "filter" => Some(Kind::Filter),
        _ => None,
    }
}

fn schema(warehouse: &str) -> Source {
    Source::Schema(warehouse.to_string())
}

fn databases(items: &[Item]) -> Vec<Item> {
    items
        .iter()
        .filter(|i| !i.value.contains('.'))
        .cloned()
        .collect()
}

/// The tables of `database`, by table name alone.
fn tables_in(items: &[Item], database: &str) -> Vec<Item> {
    items
        .iter()
        .filter_map(|i| {
            let (db, table) = i.value.split_once('.')?;
            (db == database).then(|| Item::new(table, &i.about))
        })
        .collect()
}

/// A client for `profile`, or `None` when it has no sign-in.
fn runtime(config: &Config, profile: &str, host: Option<String>, secs: u64) -> Option<Runtime> {
    let resolved = config::resolve(config, Some(profile), host, None);
    resolved.token()?;
    let http = crate::http::ApiClient::new(resolved, OutputFormat::Json, true, 0, secs).ok()?;
    Some(Runtime {
        http,
        format: OutputFormat::Json,
        limit: None,
        quiet: true,
        asked_quiet: true,
        no_input: true,
    })
}

/// Starts `gz __complete refresh` for `key`, detached, unless one is running.
fn refresh_later(cache: &Cache, profile: &str, key: &str) {
    if !cache.claim_refresh(key) {
        return;
    }
    let Ok(program) = std::env::current_exe() else {
        cache.release_refresh(key);
        return;
    };
    let started = std::process::Command::new(program)
        .args([COMMAND, "refresh", profile, key])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    if started.is_err() {
        cache.release_refresh(key);
    }
}

/// The background half of [`refresh_later`]: read the list and store it.
async fn refresh_now(profile: &str, key: &str) {
    let (Ok(config), Ok(cache), Some(source)) = (
        config::load_config(),
        Cache::for_profile(profile),
        Source::parse(key),
    ) else {
        return;
    };
    let host = std::env::var("GZ_HOST").ok();
    if let Some(rt) = runtime(&config, profile, host, REFRESH_HTTP_SECS) {
        if let Ok(items) = source.fetch(&mut Catalog::new(&rt)).await {
            cache.put(key, &items, config::now_unix()).ok();
        }
    }
    cache.release_refresh(key);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(values: &[&str]) -> Vec<Item> {
        values.iter().map(|v| Item::new(*v, "")).collect()
    }

    fn values(items: &[Item]) -> Vec<&str> {
        items.iter().map(|i| i.value.as_str()).collect()
    }

    #[test]
    fn splits_a_bash_line_at_the_cursor() {
        let (before, current) = split_line("gz jobs run 'Home Health' DQ");
        assert_eq!(before, vec!["jobs", "run", "Home Health"]);
        assert_eq!(current, "DQ");
        let (before, current) = split_line("gz projects get ");
        assert_eq!(before, vec!["projects", "get"]);
        assert_eq!(current, "");
        assert_eq!(unquote(r#""Home He"#), "Home He");
        assert_eq!(unquote(r"Home\ He"), "Home He");
    }

    #[test]
    fn matches_by_prefix_same_case_first() {
        let list = items(&["Sales", "sales-eu", "Ops"]);
        assert_eq!(values(&matching(list.clone(), "Sa")), vec!["Sales"]);
        assert_eq!(
            values(&matching(list.clone(), "SA")),
            vec!["Sales", "sales-eu"]
        );
        assert_eq!(values(&matching(list, "")).len(), 3);
    }

    #[test]
    fn bash_gets_the_tail_after_a_word_break_escaped() {
        assert_eq!(
            bash_word("--project=Ho", "--project=Home Health"),
            r"Home\ Health"
        );
        assert_eq!(bash_word("Lin", "Lineage."), "Lineage.");
    }

    #[test]
    fn schema_items_split_into_databases_and_tables() {
        let list = vec![
            Item::new("sales", "database"),
            Item::new("sales.orders", "table"),
            Item::new("ops", "database"),
        ];
        assert_eq!(values(&databases(&list)), vec!["sales", "ops"]);
        assert_eq!(values(&tables_in(&list, "sales")), vec!["orders"]);
    }

    #[test]
    fn renders_descriptions_for_shells_that_show_them() {
        let list = vec![Item::new("Sales", "id 1999")];
        assert_eq!(render(Shell::Zsh, "", &list), "Sales\tid 1999\n");
        assert_eq!(render(Shell::Bash, "", &list), "Sales\n");
    }
}
