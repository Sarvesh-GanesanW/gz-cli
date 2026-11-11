//! Auto mode's query scan cap: before a `gz sql` command the model runs
//! goes to the review, gz estimates how much data its query reads (see
//! [`crate::sql::estimate`]). Above the cap it is blocked like any auto mode
//! block, so `/allow` lets it run once; within the cap, or when the
//! estimate fails, the review decides and is told what gz found.
//!
//! gz code reaches the lakehouse only through the gz CLI (the chat server
//! gives a gz code run no query tools of its own), so the command line is
//! where every query it starts can be seen.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::assistant::tools::Access;
use crate::assistant::PermissionRequest;
use crate::http::ApiClient;
use crate::sql::estimate::{self, exact_size, format_size, parse_size, ScanEstimate};

/// The cap when the user has set none.
pub const DEFAULT_CAP: u64 = 10 << 30;
/// An estimate reads metadata only; past this it is not worth the wait.
const ESTIMATE_TIMEOUT: Duration = Duration::from_secs(10);
/// The most of a SQL file gz reads to estimate it.
const MAX_SQL_FILE: u64 = 1 << 20;
const OFF: &str = "off";

/// The most a query may scan before auto mode blocks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanCap {
    Off,
    Bytes(u64),
}

impl Default for ScanCap {
    fn default() -> Self {
        ScanCap::Bytes(DEFAULT_CAP)
    }
}

impl ScanCap {
    /// `20GB`, `off`, or `default`.
    pub fn parse(text: &str) -> Result<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            OFF => Ok(ScanCap::Off),
            "default" => Ok(ScanCap::default()),
            _ => Ok(ScanCap::Bytes(parse_size(text)?)),
        }
    }

    /// The profile's `code_scan_cap`; absent is the default.
    pub fn from_saved(saved: Option<&str>) -> Result<Self> {
        saved.map_or(Ok(ScanCap::default()), Self::parse)
    }

    /// What the profile stores: nothing for the default.
    fn saved(self) -> Option<String> {
        match self {
            _ if self == ScanCap::default() => None,
            ScanCap::Off => Some(OFF.to_string()),
            ScanCap::Bytes(bytes) => Some(exact_size(bytes)),
        }
    }

    pub fn describe(self) -> String {
        match self {
            ScanCap::Off => "off: auto mode doesn't check how much a query scans".into(),
            ScanCap::Bytes(bytes) => format!(
                "{}: auto mode blocks a gz sql query estimated to scan more",
                format_size(bytes)
            ),
        }
    }
}

// ---------- the queries a command runs ----------

/// A query a `gz sql` command line runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandQuery {
    pub sql: String,
    /// `--warehouse`; `None` runs on the one `gz sql` remembers.
    pub warehouse: Option<String>,
}

/// The queries `command` runs through `gz sql`. `Err` says why one runs
/// but gz can't tell which (SQL built by the shell, piped from a program).
pub fn command_queries(command: &str, cwd: &Path) -> Result<Vec<CommandQuery>, String> {
    let segments = shell::segments(command);
    let mut queries = Vec::new();
    for (at, segment) in segments.iter().enumerate() {
        let Some(sql_args) = gz_sql_args(&segment.words) else {
            continue;
        };
        let previous = at.checked_sub(1).map(|i| &segments[i]);
        if let Some(query) = query_of(sql_args, segment, previous, cwd)? {
            queries.push(query);
        }
    }
    Ok(queries)
}

/// Flags of gz and `gz sql` that take a value.
const VALUE_FLAGS: &[&str] = &[
    "--profile",
    "--host",
    "--token",
    "-o",
    "--output",
    "--limit",
    "--warehouse",
    "--compute",
    "--max-scan",
    "--sql",
    "--session",
    "--page-size",
    "--data",
    "--query",
    "--page",
    "--format",
    "--out",
];
/// `gz sql` subcommands that run no query.
const NO_QUERY: &[&str] = &[
    "estimate",
    "status",
    "cancel",
    "results",
    "session-state",
    "export",
    "split",
    "query-time",
    "start-session",
    "stop",
    "stop-session",
    "session-status",
    "shutdown",
    "authorize",
    "saved",
    "test-connection",
    "max-scan",
    "help",
    "shell",
];

/// The words after `gz [global flags] sql`, when the segment runs `gz sql`.
fn gz_sql_args(words: &[shell::Word]) -> Option<&[shell::Word]> {
    let start = words.iter().position(|w| !is_assignment(&w.text))?;
    let program = &words[start].text;
    if program != "gz" && !program.ends_with("/gz") {
        return None;
    }
    let mut at = start + 1;
    while let Some(word) = words.get(at) {
        match word.text.as_str() {
            "sql" => return Some(&words[at + 1..]),
            flag if flag.starts_with('-') => at += flag_width(flag),
            _ => return None,
        }
    }
    None
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_')
    })
}

fn flag_width(flag: &str) -> usize {
    if VALUE_FLAGS.contains(&flag) {
        2
    } else {
        1
    }
}

/// The value of `--name VALUE` or `--name=VALUE`.
fn flag_value<'a>(args: &'a [shell::Word], name: &str) -> Option<&'a shell::Word> {
    args.iter().enumerate().find_map(|(at, word)| {
        if word.text == name {
            return args.get(at + 1);
        }
        None
    })
}

fn inline_flag(args: &[shell::Word], name: &str) -> Option<shell::Word> {
    let prefix = format!("{name}=");
    args.iter().find_map(|word| {
        word.text.strip_prefix(&prefix).map(|value| shell::Word {
            text: value.to_string(),
            expands: word.expands,
        })
    })
}

fn named(args: &[shell::Word], name: &str) -> Option<shell::Word> {
    flag_value(args, name)
        .cloned()
        .or_else(|| inline_flag(args, name))
}

/// The subcommand and its positional words.
fn positionals(args: &[shell::Word]) -> Vec<&shell::Word> {
    let mut found = Vec::new();
    let mut at = 0;
    while let Some(word) = args.get(at) {
        if word.text.starts_with('-') {
            at += if word.text.contains('=') {
                1
            } else {
                flag_width(&word.text)
            };
            continue;
        }
        found.push(word);
        at += 1;
    }
    found
}

fn query_of(
    args: &[shell::Word],
    segment: &shell::Segment,
    previous: Option<&shell::Segment>,
    cwd: &Path,
) -> Result<Option<CommandQuery>, String> {
    let warehouse = named(args, "--warehouse").map(|w| w.text);
    let words = positionals(args);
    let subcommand = words.first().map(|w| w.text.as_str());
    let sql = match subcommand {
        Some(name) if NO_QUERY.contains(&name) => return Ok(None),
        None => stdin_sql(segment, previous, cwd)?,
        Some("run" | "submit") => match named(args, "--sql") {
            Some(word) => sql_argument(&word, cwd)?,
            None => return Err("`gz sql run` takes its query from --data here".into()),
        },
        Some("run-file") => {
            let file = words.get(1).ok_or("`gz sql run-file` names no file")?;
            literal(file)?;
            read_sql_file(&file.text, cwd)?
        }
        Some(other) => return Err(format!("`gz sql {other}` is not a command gz knows")),
    };
    Ok(Some(CommandQuery { sql, warehouse }))
}

/// A word the shell passes as it is written.
fn literal(word: &shell::Word) -> Result<&str, String> {
    if word.expands {
        return Err("the shell builds this query from variables or commands".into());
    }
    Ok(&word.text)
}

/// `--sql TEXT` or `--sql @file.sql`.
fn sql_argument(word: &shell::Word, cwd: &Path) -> Result<String, String> {
    let text = literal(word)?;
    match text.strip_prefix('@') {
        Some(path) => read_sql_file(path, cwd),
        None => Ok(text.to_string()),
    }
}

/// What `gz sql` reads on stdin: a here-document, a `<` file, or what an
/// `echo`, `printf` or `cat` before the pipe writes.
fn stdin_sql(
    segment: &shell::Segment,
    previous: Option<&shell::Segment>,
    cwd: &Path,
) -> Result<String, String> {
    match &segment.stdin {
        Some(shell::Stdin::Text(word)) => return literal(word).map(str::to_string),
        Some(shell::Stdin::File(word)) => return read_sql_file(literal(word)?, cwd),
        None => {}
    }
    let producer = previous
        .filter(|p| p.pipes_to_next)
        .ok_or("gz sql reads its query from the terminal here")?;
    let Some((program, args)) = producer.words.split_first() else {
        return Err("gz sql reads its query from an empty command".into());
    };
    match program.text.as_str() {
        "echo" => {
            let args: Vec<&shell::Word> =
                args.iter().filter(|w| !w.text.starts_with('-')).collect();
            let texts = args
                .iter()
                .map(|w| literal(w))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(texts.join(" "))
        }
        "printf" => args
            .first()
            .ok_or_else(|| "printf writes nothing".to_string())
            .and_then(|w| literal(w).map(|t| t.replace("\\n", "\n"))),
        "cat" => args
            .iter()
            .map(|w| read_sql_file(literal(w)?, cwd))
            .collect::<Result<Vec<_>, _>>()
            .map(|texts| texts.join(";\n")),
        other => Err(format!("gz sql reads its query from `{other}`")),
    }
}

fn read_sql_file(path: &str, cwd: &Path) -> Result<String, String> {
    let full = cwd.join(path);
    let size = std::fs::metadata(&full)
        .map_err(|err| format!("cannot read {path}: {err}"))?
        .len();
    if size > MAX_SQL_FILE {
        return Err(format!("{path} is larger than 1 MB"));
    }
    std::fs::read_to_string(&full).map_err(|err| format!("cannot read {path}: {err}"))
}

// ---------- estimating ----------

pub type EstimateFuture<'a> = Pin<Box<dyn Future<Output = Result<ScanEstimate>> + Send + 'a>>;

/// Estimates a query's scan: the lakehouse in use, a script in tests.
pub trait QueryEstimator: Send + Sync {
    fn estimate<'a>(&'a self, warehouse: Option<&'a str>, sql: &'a str) -> EstimateFuture<'a>;
}

/// Estimates on the profile's lakehouse.
pub struct HttpEstimator {
    http: ApiClient,
}

impl QueryEstimator for HttpEstimator {
    fn estimate<'a>(&'a self, warehouse: Option<&'a str>, sql: &'a str) -> EstimateFuture<'a> {
        Box::pin(async move {
            let remembered = self.http.resolved().profile.sql_warehouse.clone();
            let warehouse = warehouse
                .map(str::to_string)
                .or(remembered)
                .context("the command names no warehouse and gz sql remembers none")?;
            estimate::estimate(&self.http, &warehouse, sql).await
        })
    }
}

/// What the cap says about an action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanCheck {
    /// It runs no query (or the cap is off).
    NotAQuery,
    /// Within the cap; the note goes to the review.
    Within(String),
    /// Over the cap: the reason for the block.
    Over(String),
    /// A query whose scan gz couldn't estimate; the note goes to the review.
    Unknown(String),
}

type Save = Arc<dyn Fn(ScanCap) -> Result<()> + Send + Sync>;

/// The session's cap and how to estimate; clones share the cap, so a
/// change applies to the main agent and every subagent.
#[derive(Clone)]
pub struct ScanGate {
    cap: Arc<RwLock<ScanCap>>,
    save: Save,
    estimator: Arc<dyn QueryEstimator>,
}

impl std::fmt::Debug for ScanGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScanGate")
            .field("cap", &self.cap())
            .finish()
    }
}

impl ScanGate {
    pub fn new(cap: ScanCap, estimator: Arc<dyn QueryEstimator>, save: Save) -> Self {
        Self {
            cap: Arc::new(RwLock::new(cap)),
            save,
            estimator,
        }
    }

    /// The profile's cap, saved back there when changed, estimated on its
    /// lakehouse. A cap the profile holds but gz can't read is the default,
    /// with a warning.
    pub fn for_profile(http: &ApiClient) -> Self {
        let saved = http.resolved().profile.code_scan_cap.clone();
        let cap = ScanCap::from_saved(saved.as_deref()).unwrap_or_else(|err| {
            crate::output::warn(&format!("code_scan_cap: {err:#}; using the default cap"));
            ScanCap::default()
        });
        let profile = http.resolved().profile_name.clone();
        let save: Save = Arc::new(move |cap: ScanCap| {
            crate::sql::remember_in_profile(&profile, |p| p.code_scan_cap = cap.saved())
        });
        let estimator = Arc::new(HttpEstimator { http: http.clone() });
        Self::new(cap, estimator, save)
    }

    pub fn cap(&self) -> ScanCap {
        *self.cap.read().unwrap_or_else(|p| p.into_inner())
    }

    /// Applies `cap` now and saves it for later sessions.
    pub fn set_cap(&self, cap: ScanCap) -> Result<()> {
        *self.cap.write().unwrap_or_else(|p| p.into_inner()) = cap;
        (self.save)(cap).context("the cap applies to this session but could not be saved")
    }

    /// Checks the queries `request` runs, if it runs any.
    pub async fn check(&self, request: &PermissionRequest, cwd: &Path) -> ScanCheck {
        let ScanCap::Bytes(cap) = self.cap() else {
            return ScanCheck::NotAQuery;
        };
        let Some(command) = request
            .preview
            .as_deref()
            .filter(|_| request.access == Access::Command)
        else {
            return ScanCheck::NotAQuery;
        };
        let queries = match command_queries(command, cwd) {
            Ok(queries) if queries.is_empty() => return ScanCheck::NotAQuery,
            Ok(queries) => queries,
            Err(why) => return ScanCheck::Unknown(unknown_note(&why)),
        };
        let mut largest: Option<ScanEstimate> = None;
        for query in &queries {
            let estimate = match self.estimate(query).await {
                Ok(estimate) => estimate,
                Err(err) => return ScanCheck::Unknown(unknown_note(&format!("{err:#}"))),
            };
            if largest
                .as_ref()
                .is_none_or(|l| estimate.read.bytes > l.read.bytes)
            {
                largest = Some(estimate);
            }
        }
        let Some(estimate) = largest else {
            return ScanCheck::NotAQuery;
        };
        if estimate.read.bytes > cap {
            return ScanCheck::Over(format!(
                "this query reads {}, over auto mode's scan cap of {}. Narrow the query \
                 (a filter on a partition column, fewer tables), or ask the user to /allow it \
                 or raise the cap with /auto scan cap <size>.",
                estimate.amount(),
                format_size(cap)
            ));
        }
        ScanCheck::Within(format!(
            "scan estimate: this query reads {}, within auto mode's scan cap of {}.",
            estimate.amount(),
            format_size(cap)
        ))
    }

    async fn estimate(&self, query: &CommandQuery) -> Result<ScanEstimate> {
        let warehouse = query.warehouse.as_deref();
        tokio::time::timeout(
            ESTIMATE_TIMEOUT,
            self.estimator.estimate(warehouse, &query.sql),
        )
        .await
        .context("the estimate took too long")?
    }
}

fn unknown_note(why: &str) -> String {
    format!(
        "This command runs a lakehouse query whose scan gz could not estimate ({why}), so the \
         scan cap was not checked; judge how much data it may read."
    )
}

/// The request the review sees: the action with gz's note after its
/// detail. Blocks and `/allow` still key on the action itself.
pub fn with_review_note(request: &PermissionRequest, note: &str) -> PermissionRequest {
    let detail = match request.preview.as_deref() {
        Some(detail) => format!("{detail}\n\n[gz: {note}]"),
        None => format!("[gz: {note}]"),
    };
    PermissionRequest {
        preview: Some(detail),
        ..request.clone()
    }
}

/// Just enough of the shell's grammar to find what a command pipes into
/// `gz sql`: words with their quotes removed, pipes and command
/// separators, `<` and `<<<` input, and here-documents.
mod shell {
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Word {
        pub text: String,
        /// Holds a `$` or backtick the shell would expand.
        pub expands: bool,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Stdin {
        File(Word),
        Text(Word),
    }

    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub struct Segment {
        pub words: Vec<Word>,
        pub stdin: Option<Stdin>,
        /// Ends in `|`: the next segment reads its output.
        pub pipes_to_next: bool,
    }

    enum Redirect {
        File,
        Text,
        HereDoc,
    }

    pub fn segments(command: &str) -> Vec<Segment> {
        let chars: Vec<char> = command.chars().collect();
        let mut segments = Vec::new();
        let mut current = Segment::default();
        let mut pending: Option<Redirect> = None;
        let mut heredocs: Vec<(usize, String, bool)> = Vec::new();
        let mut at = 0;
        while at < chars.len() {
            let ch = chars[at];
            match ch {
                '\n' => {
                    at = read_heredocs(&chars, at + 1, &mut heredocs, &mut segments, &mut current);
                    finish(&mut segments, &mut current, false);
                    continue;
                }
                _ if ch.is_whitespace() => at += 1,
                '|' if chars.get(at + 1) == Some(&'|') => {
                    finish(&mut segments, &mut current, false);
                    at += 2;
                }
                '|' => {
                    finish(&mut segments, &mut current, true);
                    at += 1;
                }
                '&' | ';' => {
                    finish(&mut segments, &mut current, false);
                    at += if chars.get(at + 1) == Some(&'&') {
                        2
                    } else {
                        1
                    };
                }
                '<' => {
                    let count = chars[at..].iter().take_while(|c| **c == '<').count();
                    at += count;
                    pending = Some(match count {
                        1 => Redirect::File,
                        2 => {
                            if chars.get(at) == Some(&'-') {
                                at += 1;
                            }
                            Redirect::HereDoc
                        }
                        _ => Redirect::Text,
                    });
                }
                '>' => {
                    // Output goes elsewhere; its target is no argument.
                    at += 1;
                    if chars.get(at) == Some(&'&') {
                        at += 1;
                    }
                    while chars.get(at).is_some_and(|c| c.is_whitespace()) {
                        at += 1;
                    }
                    at = read_word(&chars, at).2;
                }
                _ => {
                    let (word, quoted, end) = read_word(&chars, at);
                    at = end;
                    match pending.take() {
                        Some(Redirect::File) => current.stdin = Some(Stdin::File(word)),
                        Some(Redirect::Text) => current.stdin = Some(Stdin::Text(word)),
                        Some(Redirect::HereDoc) => {
                            heredocs.push((segments.len(), word.text, quoted));
                        }
                        None => current.words.push(word),
                    }
                }
            }
        }
        finish(&mut segments, &mut current, false);
        segments
    }

    fn finish(segments: &mut Vec<Segment>, current: &mut Segment, pipes: bool) {
        if current.words.is_empty() && current.stdin.is_none() {
            if let Some(last) = segments.last_mut() {
                last.pipes_to_next |= pipes;
            }
            return;
        }
        current.pipes_to_next = pipes;
        segments.push(std::mem::take(current));
    }

    /// Reads the bodies of the here-documents opened on the line just
    /// ended, from `at`; returns where the next line starts.
    fn read_heredocs(
        chars: &[char],
        mut at: usize,
        heredocs: &mut Vec<(usize, String, bool)>,
        segments: &mut [Segment],
        current: &mut Segment,
    ) -> usize {
        for (index, delimiter, quoted) in heredocs.drain(..) {
            let mut body = Vec::new();
            while at < chars.len() {
                let end = chars[at..]
                    .iter()
                    .position(|c| *c == '\n')
                    .map_or(chars.len(), |p| at + p);
                let line: String = chars[at..end].iter().collect();
                at = (end + 1).min(chars.len());
                if line.trim() == delimiter {
                    break;
                }
                body.push(line);
            }
            let text = body.join("\n");
            let word = Word {
                expands: !quoted && (text.contains('$') || text.contains('`')),
                text,
            };
            let target = if index < segments.len() {
                &mut segments[index]
            } else {
                &mut *current
            };
            target.stdin = Some(Stdin::Text(word));
        }
        at
    }

    /// The word at `start`, whether any part of it was quoted, and where
    /// it ends.
    fn read_word(chars: &[char], start: usize) -> (Word, bool, usize) {
        let mut text = String::new();
        let mut expands = false;
        let mut quoted = false;
        let mut at = start;
        while let Some(&ch) = chars.get(at) {
            match ch {
                _ if ch.is_whitespace() => break,
                '|' | '&' | ';' | '<' | '>' => break,
                '\'' => {
                    quoted = true;
                    at += 1;
                    while let Some(&inner) = chars.get(at) {
                        at += 1;
                        if inner == '\'' {
                            break;
                        }
                        text.push(inner);
                    }
                }
                '"' => {
                    quoted = true;
                    at += 1;
                    while let Some(&inner) = chars.get(at) {
                        at += 1;
                        match inner {
                            '"' => break,
                            '\\' => {
                                if let Some(&next) = chars.get(at) {
                                    if !matches!(next, '"' | '\\' | '$' | '`') {
                                        text.push('\\');
                                    }
                                    text.push(next);
                                    at += 1;
                                }
                            }
                            '$' | '`' => {
                                expands = true;
                                text.push(inner);
                            }
                            _ => text.push(inner),
                        }
                    }
                }
                '\\' => {
                    if let Some(&next) = chars.get(at + 1) {
                        text.push(next);
                    }
                    at += 2;
                }
                '$' | '`' => {
                    expands = true;
                    text.push(ch);
                    at += 1;
                }
                _ => {
                    text.push(ch);
                    at += 1;
                }
            }
        }
        (Word { text, expands }, quoted, at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cwd() -> std::path::PathBuf {
        std::env::temp_dir()
    }

    fn queries(command: &str) -> Vec<CommandQuery> {
        command_queries(command, &cwd()).unwrap()
    }

    #[test]
    fn a_query_passed_with_sql_or_piped_in_is_found() {
        let found = queries(r#"gz sql submit --sql "SELECT * FROM a.b" --session s --warehouse W"#);
        assert_eq!(
            found,
            [CommandQuery {
                sql: "SELECT * FROM a.b".into(),
                warehouse: Some("W".into())
            }]
        );
        let piped =
            queries("cd x && echo 'SELECT 1 FROM a.b;' | gz --profile p sql --warehouse=W -o json");
        assert_eq!(piped[0].sql, "SELECT 1 FROM a.b;");
        assert_eq!(piped[0].warehouse.as_deref(), Some("W"));
    }

    #[test]
    fn a_here_document_is_the_query() {
        let found = queries("gz sql --warehouse W <<'EOF'\nSELECT *\nFROM a.b\nEOF\necho done");
        assert_eq!(found[0].sql, "SELECT *\nFROM a.b");
        let text = queries("gz sql <<< \"SELECT 2 FROM c.d\"");
        assert_eq!(text[0].sql, "SELECT 2 FROM c.d");
    }

    #[test]
    fn a_file_is_read_from_the_folder() {
        let dir = std::env::temp_dir().join(format!("gz-scan-cap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("q.sql"), "SELECT * FROM x.y").unwrap();
        for command in [
            "gz sql run-file q.sql",
            "gz sql < q.sql",
            "cat q.sql | gz sql",
            "gz sql run --sql @q.sql --wait",
        ] {
            let found = command_queries(command, &dir).unwrap();
            assert_eq!(found[0].sql, "SELECT * FROM x.y", "{command}");
        }
    }

    #[test]
    fn commands_that_run_no_query_are_not_checked() {
        for command in [
            "gz sql estimate 'SELECT * FROM a.b'",
            "gz sql status s e",
            "gz table versions a.b",
            "ls | grep sql",
            "echo gz sql",
        ] {
            assert!(queries(command).is_empty(), "{command}");
        }
    }

    #[test]
    fn a_query_the_shell_builds_cannot_be_estimated() {
        for command in [
            "gz sql submit --sql \"SELECT * FROM $T\" --session s --warehouse w",
            "python make_query.py | gz sql",
            "gz sql --warehouse w",
            "gz sql <<EOF\nSELECT * FROM $T\nEOF",
        ] {
            assert!(command_queries(command, &cwd()).is_err(), "{command}");
        }
    }

    #[test]
    fn caps_parse_and_save_only_when_not_the_default() {
        assert_eq!(ScanCap::parse("20GB").unwrap(), ScanCap::Bytes(20 << 30));
        assert_eq!(ScanCap::parse("OFF").unwrap(), ScanCap::Off);
        assert_eq!(
            ScanCap::from_saved(None).unwrap(),
            ScanCap::Bytes(DEFAULT_CAP)
        );
        assert_eq!(ScanCap::Bytes(20 << 30).saved().as_deref(), Some("20GB"));
        assert_eq!(ScanCap::default().saved(), None);
        assert!(ScanCap::parse("big").is_err());
    }
}

/// Auto mode with the cap, end to end through the approver.
#[cfg(test)]
mod approver_tests {
    use super::*;
    use crate::assistant::action_review::{ActionReviewer, ReviewFuture, ReviewRequest, Verdict};
    use crate::assistant::auto_rules::AutoStore;
    use crate::assistant::{Approver, Decision};
    use crate::code_ui::approver::{AutoReview, ChannelApprover, Mode};
    use crate::code_ui::auto_mode::AutoContext;
    use crate::sql::estimate::Totals;
    use crate::sql::tables::TableRef;
    use std::sync::Mutex;

    const GB: u64 = 1 << 30;
    const BIG: &str =
        "gz sql submit --sql 'SELECT * FROM DemoDB.inventory' --session s --warehouse W";
    const SMALL: &str = "echo 'SELECT * FROM DemoDB.sales;' | gz sql --warehouse W";

    /// Sizes each query by the table it names; fails for any other.
    struct Sizes {
        asked: Mutex<Vec<(Option<String>, String)>>,
    }

    impl QueryEstimator for Sizes {
        fn estimate<'a>(&'a self, warehouse: Option<&'a str>, sql: &'a str) -> EstimateFuture<'a> {
            self.asked
                .lock()
                .unwrap()
                .push((warehouse.map(str::to_string), sql.to_string()));
            let bytes = if sql.contains("inventory") {
                Some(9 * GB)
            } else if sql.contains("sales") {
                Some(25_116)
            } else {
                None
            };
            Box::pin(async move {
                let bytes = bytes.context("the lakehouse is unreachable")?;
                let table = TableRef {
                    database: "DemoDB".into(),
                    table: "t".into(),
                };
                let totals = Totals {
                    bytes,
                    files: 1,
                    rows: 1,
                };
                Ok(ScanEstimate::upper_bound(vec![(table, totals)], Vec::new()))
            })
        }
    }

    /// Allows everything it reviews and keeps what it saw.
    struct Reviewer {
        seen: Mutex<Vec<ReviewRequest>>,
    }

    impl ActionReviewer for Reviewer {
        fn review<'a>(&'a self, request: &'a ReviewRequest) -> ReviewFuture<'a> {
            self.seen.lock().unwrap().push(request.clone());
            Box::pin(async { Ok(Verdict::Allow) })
        }
    }

    struct Setup {
        approver: ChannelApprover,
        context: Arc<Mutex<AutoContext>>,
        reviewer: Arc<Reviewer>,
        sizes: Arc<Sizes>,
        saved: Arc<Mutex<Vec<ScanCap>>>,
    }

    fn setup(name: &str, cap: ScanCap) -> Setup {
        let sizes = Arc::new(Sizes {
            asked: Mutex::new(Vec::new()),
        });
        let saved = Arc::new(Mutex::new(Vec::new()));
        let record = saved.clone();
        let gate = ScanGate::new(
            cap,
            sizes.clone(),
            Arc::new(move |cap| {
                record.lock().unwrap().push(cap);
                Ok(())
            }),
        );
        let mut context = AutoContext::default();
        context.user_said("look at the inventory");
        context.set_scan_gate(gate);
        let context = Arc::new(Mutex::new(context));
        let reviewer = Arc::new(Reviewer {
            seen: Mutex::new(Vec::new()),
        });
        let (_, sandbox) = crate::assistant::sandbox::tests::temp_workspace(name);
        let store_dir = crate::assistant::sandbox::tests::temp_dir(&format!("{name}-store"));
        let (asks, _) = tokio::sync::mpsc::unbounded_channel();
        let approver =
            ChannelApprover::new(asks, Arc::new(Mutex::new(Mode::Auto))).with_auto(AutoReview {
                reviewer: reviewer.clone(),
                context: context.clone(),
                store: AutoStore::in_folder(&store_dir),
                sandbox,
            });
        Setup {
            approver,
            context,
            reviewer,
            sizes,
            saved,
        }
    }

    fn command(line: &str) -> PermissionRequest {
        PermissionRequest {
            access: Access::Command,
            summary: format!("run_command {line}"),
            preview: Some(line.into()),
            chat_scope: String::new(),
            always_scope: None,
            action: None,
            asked_by: None,
            target: None,
            note: None,
        }
    }

    #[tokio::test]
    async fn a_query_over_the_cap_is_blocked_without_a_review_and_allow_runs_it_once() {
        let s = setup("over", ScanCap::Bytes(GB));
        let big = command(BIG);

        assert_eq!(s.approver.decide(&big).await, Decision::Deny);
        let told = s.approver.take_refusal().message();
        assert!(
            told.contains("reads up to 9.0 GB, over auto mode's scan cap of 1.0 GB"),
            "{told}"
        );
        assert!(
            s.reviewer.seen.lock().unwrap().is_empty(),
            "the cap decided, not the review"
        );
        assert_eq!(
            s.sizes.asked.lock().unwrap()[0],
            (Some("W".into()), "SELECT * FROM DemoDB.inventory".into())
        );

        let allowed = s.context.lock().unwrap().allow_blocked(None).unwrap();
        assert_eq!(allowed.summary, big.summary);
        assert_eq!(s.approver.decide(&big).await, Decision::AllowOnce);
        assert_eq!(
            s.approver.decide(&big).await,
            Decision::Deny,
            "allowed once only"
        );
    }

    #[tokio::test]
    async fn a_query_within_the_cap_goes_to_the_review_with_the_estimate() {
        let s = setup("within", ScanCap::Bytes(GB));

        assert_eq!(
            s.approver.decide(&command(SMALL)).await,
            Decision::AllowOnce
        );
        let seen = s.reviewer.seen.lock().unwrap();
        let detail = seen[0].action.detail.clone().unwrap();
        assert!(detail.starts_with(SMALL), "{detail}");
        assert!(
            detail.contains("[gz: scan estimate: this query reads up to 24.5 KB, within auto mode's scan cap of 1.0 GB.]"),
            "{detail}"
        );
    }

    #[tokio::test]
    async fn a_failed_estimate_does_not_block_and_the_review_is_told() {
        let s = setup("unknown", ScanCap::Bytes(GB));
        let unreachable = command("gz sql run-file missing.sql --warehouse W");

        assert_eq!(s.approver.decide(&unreachable).await, Decision::AllowOnce);
        let seen = s.reviewer.seen.lock().unwrap();
        let detail = seen[0].action.detail.clone().unwrap();
        assert!(
            detail.contains("could not estimate (cannot read missing.sql"),
            "{detail}"
        );
    }

    #[tokio::test]
    async fn with_the_cap_off_nothing_is_estimated() {
        let s = setup("off", ScanCap::Off);

        assert_eq!(s.approver.decide(&command(BIG)).await, Decision::AllowOnce);
        assert!(s.sizes.asked.lock().unwrap().is_empty());
        assert!(s.saved.lock().unwrap().is_empty());
    }
}
