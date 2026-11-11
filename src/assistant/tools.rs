//! The client-tool catalog the chat server may call (see the client-tools
//! contract). Each call is first *planned* — validated, sandboxed, and given
//! a human summary and, for writes, a diff — so the user can approve it
//! before anything touches the machine. Only then is it run.

use crate::assistant::jobs::Jobs;
use crate::assistant::notebook::{self, CellEdit};
use crate::assistant::search::Search;
use crate::assistant::web_fetch::Fetch;
use anyhow::{bail, Context, Result};
use globset::{Glob, GlobMatcher};
use ignore::WalkBuilder;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::assistant::sandbox::Sandbox;

/// Contract limit on one result's `output`.
/// Most characters one tool result sends the model. The chat server turns a
/// result over 64 KiB of JSON into a preview (gz-chat-eks agent/hooks.py), so
/// gz cuts well below that itself and says how to get the rest.
pub const MAX_OUTPUT_CHARS: usize = 40_000;
/// Room kept free for the note that ends a cut output.
const CUT_NOTE_CHARS: usize = 200;
const DEFAULT_READ_LINES: usize = 2000;
const MAX_LINE_CHARS: usize = 2000;
const MAX_LIST_ENTRIES: usize = 1000;
const MAX_FIND_RESULTS: usize = 100;
const BINARY_SNIFF_BYTES: usize = 8000;
const DEFAULT_COMMAND_SECS: u64 = 120;
const MAX_COMMAND_SECS: u64 = 600;
/// Contract limit on a script's source.
const MAX_SCRIPT_CHARS: usize = 65_536;
/// Results handed to scripts stay local, so they may be larger than what the
/// model sees, but still bounded.
const MAX_HELPER_READ_BYTES: u64 = 16 * 1024 * 1024;
const MAX_HELPER_RESULTS: usize = 10_000;

/// Tools gz can execute, in the contract's names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolName {
    ReadFile,
    ListDirectory,
    FindFiles,
    SearchFiles,
    EditFile,
    WriteFile,
    RunCommand,
    RunScript,
    NotebookEdit,
    WebFetch,
}

impl ToolName {
    pub const ALL: [ToolName; 8] = [
        ToolName::ReadFile,
        ToolName::ListDirectory,
        ToolName::FindFiles,
        ToolName::SearchFiles,
        ToolName::EditFile,
        ToolName::WriteFile,
        ToolName::RunCommand,
        ToolName::RunScript,
    ];
    /// More tools gz runs itself, offered only when the chat server
    /// lists them (`clientTools` in its capabilities).
    pub const EXTRA: [ToolName; 2] = [ToolName::NotebookEdit, ToolName::WebFetch];

    pub fn as_str(self) -> &'static str {
        match self {
            ToolName::ReadFile => "read_file",
            ToolName::ListDirectory => "list_directory",
            ToolName::FindFiles => "find_files",
            ToolName::SearchFiles => "search_files",
            ToolName::EditFile => "edit_file",
            ToolName::WriteFile => "write_file",
            ToolName::RunCommand => "run_command",
            ToolName::RunScript => "run_script",
            ToolName::NotebookEdit => "notebook_edit",
            ToolName::WebFetch => "web_fetch",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .chain(Self::EXTRA)
            .find(|t| t.as_str() == name)
    }

    /// The tool behind a display name the chat server shows for it
    /// ("Read file", "List directory", "run_script").
    pub fn from_display(name: &str) -> Option<Self> {
        let normalized = name.trim().to_ascii_lowercase().replace([' ', '-'], "_");
        Self::parse(&normalized)
    }

    pub fn access(self) -> Access {
        match self {
            ToolName::ReadFile
            | ToolName::ListDirectory
            | ToolName::FindFiles
            | ToolName::SearchFiles => Access::Read,
            ToolName::EditFile | ToolName::WriteFile | ToolName::NotebookEdit => Access::Write,
            ToolName::RunCommand => Access::Command,
            ToolName::RunScript => Access::Script,
            ToolName::WebFetch => Access::Fetch,
        }
    }
}

/// What kind of permission a call needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
    Command,
    /// A script: arbitrary code running as the user, approved after reading it.
    Script,
    /// Opening a folder outside the open ones (`change_directory`).
    Folder,
    /// An action the chat server itself wants to take (its own approval
    /// interrupt), not a local tool.
    ServerAction,
    /// Installing skills the user asked for (`/skills install`): confirmed
    /// by the user only, never by a mode, rule or review.
    Install,
    /// Trusting a team skill's version despite its warnings: confirmed by
    /// the user only, like an install.
    Trust,
    /// Fetching a web page (`web_fetch`); allowed per host.
    Fetch,
    /// Calling a tool on one of the user's local MCP servers
    /// (`local_mcp_call`); allowed per server and tool.
    LocalTool,
}

/// A validated call, ready for the user's decision.
#[derive(Debug, Clone)]
pub struct Planned {
    pub access: Access,
    /// One line, e.g. `read_file src/main.rs`.
    pub summary: String,
    /// Folder a read covers (permission is remembered per folder).
    pub scope: PathBuf,
    /// Unified diff for writes, the command line for commands.
    pub preview: Option<String>,
    /// What the call touches: a write's file (relative to the folder), a
    /// fetch's host, or a local MCP tool as `server/tool`.
    pub target: Option<String>,
    action: Action,
}

#[derive(Debug, Clone)]
enum Action {
    Read {
        path: PathBuf,
        offset: usize,
        /// `None`: the text tool's default page, or the whole file for scripts.
        limit: Option<usize>,
    },
    List {
        path: PathBuf,
    },
    Find {
        matcher: GlobMatcher,
        match_names: bool,
        base: PathBuf,
    },
    Search(Box<Search>),
    Write {
        path: PathBuf,
        content: String,
        /// Set for edits: how many occurrences were replaced.
        replacements: Option<usize>,
    },
    Command {
        command: String,
        timeout: Duration,
        background: bool,
    },
    Fetch(Fetch),
    Script(ScriptSpec),
    /// Runs on its MCP server through [`crate::assistant::mcp_local`].
    LocalTool,
}

/// The language a `run_script` program is written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Python,
    Node,
}

impl Language {
    fn parse(name: &str) -> Result<Self> {
        match name {
            "python" => Ok(Language::Python),
            "node" => Ok(Language::Node),
            other => bail!("unsupported script language '{other}' (python or node)"),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Language::Python => "python",
            Language::Node => "node",
        }
    }
}

/// A validated `run_script` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptSpec {
    pub language: Language,
    pub code: String,
    pub timeout: Duration,
}

impl Planned {
    /// The command line, its timeout, and whether it asked for the
    /// background, when this is a `run_command` call.
    pub fn command(&self) -> Option<(&str, Duration, bool)> {
        match &self.action {
            Action::Command {
                command,
                timeout,
                background,
            } => Some((command, *timeout, *background)),
            _ => None,
        }
    }

    /// The file this call writes and the content it writes, when it is an
    /// edit, write or notebook edit.
    pub fn written(&self) -> Option<(&Path, &str)> {
        match &self.action {
            Action::Write { path, content, .. } => Some((path, content)),
            _ => None,
        }
    }

    /// The script to run, when this is an approved `run_script` call.
    pub fn script(&self) -> Option<&ScriptSpec> {
        match &self.action {
            Action::Script(spec) => Some(spec),
            _ => None,
        }
    }
}

/// The text a tool hands back to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub output: String,
    pub is_error: bool,
    pub truncated: bool,
}

impl ToolOutput {
    pub fn ok(text: String) -> Self {
        Self::capped(text, false)
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self::capped(text.into(), true)
    }

    /// Keeps a quarter of the room for the start of a long output and the
    /// rest for its end, where commands print errors and totals.
    fn capped(text: String, is_error: bool) -> Self {
        let total = text.chars().count();
        if total <= MAX_OUTPUT_CHARS {
            return Self {
                output: text,
                is_error,
                truncated: false,
            };
        }
        let room = MAX_OUTPUT_CHARS - CUT_NOTE_CHARS;
        let head_len = room / 4;
        let tail_len = room - head_len;
        let head: String = text.chars().take(head_len).collect();
        let tail: String = text.chars().skip(total - tail_len).collect();
        let cut = total - head_len - tail_len;
        Self {
            output: format!(
                "{head}\n… {cut} characters cut here by gz; narrow the request \
                 (a path, glob, offset, or filter) to see them …\n{tail}"
            ),
            is_error,
            truncated: true,
        }
    }
}

fn str_field<'a>(input: &'a Value, key: &str) -> Result<&'a str> {
    input[key]
        .as_str()
        .with_context(|| format!("missing string field '{key}'"))
}

fn opt_str<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input[key].as_str().filter(|s| !s.is_empty())
}

fn opt_usize(input: &Value, key: &str) -> Option<usize> {
    input[key].as_u64().map(|n| n as usize)
}

fn glob(pattern: &str) -> Result<GlobMatcher> {
    Ok(Glob::new(pattern)
        .with_context(|| format!("invalid glob '{pattern}'"))?
        .compile_matcher())
}

/// Validates a call and works out exactly what it would do.
pub fn plan(sandbox: &Sandbox, tool: ToolName, input: &Value) -> Result<Planned> {
    let path_arg = opt_str(input, "path").unwrap_or(".");
    let (summary, scope, preview, action) = match tool {
        ToolName::ReadFile => {
            let path = sandbox.resolve(str_field(input, "path")?)?;
            if !path.is_file() {
                bail!("{} is not a file", sandbox.display(&path));
            }
            let offset = opt_usize(input, "offset").unwrap_or(1).max(1);
            let limit = opt_usize(input, "limit").map(|l| l.clamp(1, DEFAULT_READ_LINES));
            let scope = parent(sandbox, &path);
            (
                sandbox.display(&path),
                scope,
                None,
                Action::Read {
                    path,
                    offset,
                    limit,
                },
            )
        }
        ToolName::ListDirectory => {
            let path = existing_dir(sandbox, path_arg)?;
            (
                sandbox.display(&path),
                path.clone(),
                None,
                Action::List { path },
            )
        }
        ToolName::FindFiles => {
            let pattern = str_field(input, "pattern")?;
            let base = existing_dir(sandbox, path_arg)?;
            let action = Action::Find {
                matcher: glob(pattern)?,
                match_names: !pattern.contains('/'),
                base: base.clone(),
            };
            (
                format!("{pattern} in {}", sandbox.display(&base)),
                base,
                None,
                action,
            )
        }
        ToolName::SearchFiles => {
            let search = Search::parse(sandbox, input)?;
            let base = search.base();
            let scope = if base.is_file() {
                parent(sandbox, base)
            } else {
                base.to_path_buf()
            };
            (
                search.summary(sandbox),
                scope,
                None,
                Action::Search(Box::new(search)),
            )
        }
        ToolName::EditFile => {
            let path = sandbox.resolve(str_field(input, "path")?)?;
            let old = std::fs::read_to_string(&path)
                .with_context(|| format!("cannot read {}", sandbox.display(&path)))?;
            let (content, replaced) = apply_edit(
                &old,
                str_field(input, "old_text")?,
                str_field(input, "new_text")?,
                input["replace_all"].as_bool().unwrap_or(false),
            )?;
            let diff = unified_diff(&sandbox.display(&path), Some(&old), &content);
            let scope = parent(sandbox, &path);
            (
                sandbox.display(&path),
                scope,
                Some(diff),
                Action::Write {
                    path,
                    content,
                    replacements: Some(replaced),
                },
            )
        }
        ToolName::WriteFile => {
            let path = sandbox.resolve(str_field(input, "path")?)?;
            if path.is_dir() {
                bail!("{} is a folder", sandbox.display(&path));
            }
            let content = str_field(input, "content")?.to_string();
            let old = std::fs::read_to_string(&path).ok();
            let diff = unified_diff(&sandbox.display(&path), old.as_deref(), &content);
            let scope = parent(sandbox, &path);
            (
                sandbox.display(&path),
                scope,
                Some(diff),
                Action::Write {
                    path,
                    content,
                    replacements: None,
                },
            )
        }
        ToolName::RunCommand => {
            let command = str_field(input, "command")?.trim().to_string();
            if command.is_empty() {
                bail!("the command is empty");
            }
            let secs = input["timeout_seconds"]
                .as_u64()
                .unwrap_or(DEFAULT_COMMAND_SECS)
                .clamp(1, MAX_COMMAND_SECS);
            let action = Action::Command {
                command: command.clone(),
                timeout: Duration::from_secs(secs),
                background: input["run_in_background"].as_bool().unwrap_or(false),
            };
            (command.clone(), sandbox.cwd(), Some(command), action)
        }
        ToolName::NotebookEdit => {
            let path = sandbox.resolve(str_field(input, "path")?)?;
            if path.extension().and_then(|e| e.to_str()) != Some("ipynb") {
                bail!("{} is not a notebook (.ipynb)", sandbox.display(&path));
            }
            let old = std::fs::read_to_string(&path)
                .with_context(|| format!("cannot read {}", sandbox.display(&path)))?;
            let edit = CellEdit::parse(input)?;
            let content = edit.apply(&old)?;
            // The diff compares cells as the model reads them, not raw JSON.
            let label = sandbox.display(&path);
            let diff = unified_diff(
                &label,
                Some(&notebook::render(&old)?),
                &notebook::render(&content)?,
            );
            (
                format!("{label} ({})", edit.describe()),
                parent(sandbox, &path),
                Some(diff),
                Action::Write {
                    path,
                    content,
                    replacements: None,
                },
            )
        }
        ToolName::WebFetch => {
            let fetch = Fetch::parse(input)?;
            (
                fetch.url.to_string(),
                sandbox.cwd(),
                None,
                Action::Fetch(fetch),
            )
        }
        ToolName::RunScript => {
            let spec = plan_script(input)?;
            let lines = spec.code.lines().count();
            (
                format!("{} ({lines} lines)", spec.language.as_str()),
                sandbox.cwd(),
                Some(spec.code.clone()),
                Action::Script(spec),
            )
        }
    };
    let target = match &action {
        Action::Write { path, .. } => Some(sandbox.display(path)),
        Action::Fetch(fetch) => Some(fetch.host()),
        _ => None,
    };
    Ok(Planned {
        access: tool.access(),
        summary: format!("{} {summary}", tool.as_str()),
        scope,
        preview,
        target,
        action,
    })
}

/// A `local_mcp_call`, checked by the session against the server's tools:
/// `arguments` is what the user approves (and auto mode reviews), `folder`
/// where an "always" answer holds.
pub fn local_tool_call(server: &str, tool: &str, arguments: String, folder: PathBuf) -> Planned {
    let target = format!("{server}/{tool}");
    Planned {
        access: Access::LocalTool,
        summary: format!("local_mcp_call {target}"),
        scope: folder,
        preview: Some(arguments),
        target: Some(target),
        action: Action::LocalTool,
    }
}

fn plan_script(input: &Value) -> Result<ScriptSpec> {
    let language = Language::parse(str_field(input, "language")?)?;
    let code = str_field(input, "code")?;
    if code.trim().is_empty() {
        bail!("the script is empty");
    }
    if code.chars().count() > MAX_SCRIPT_CHARS {
        bail!("the script is longer than {MAX_SCRIPT_CHARS} characters");
    }
    let secs = input["timeout_seconds"]
        .as_u64()
        .unwrap_or(DEFAULT_COMMAND_SECS)
        .clamp(1, MAX_COMMAND_SECS);
    Ok(ScriptSpec {
        language,
        code: code.to_string(),
        timeout: Duration::from_secs(secs),
    })
}

fn existing_dir(sandbox: &Sandbox, requested: &str) -> Result<PathBuf> {
    let path = sandbox.resolve(requested)?;
    if !path.is_dir() {
        bail!("{} is not a folder", sandbox.display(&path));
    }
    Ok(path)
}

fn parent(sandbox: &Sandbox, path: &Path) -> PathBuf {
    path.parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| sandbox.cwd())
}

/// Exact-string replacement; ambiguity is an error so the model must be
/// specific (or ask for `replace_all`).
fn apply_edit(old: &str, find: &str, replace: &str, replace_all: bool) -> Result<(String, usize)> {
    if find.is_empty() {
        bail!("old_text is empty");
    }
    match old.matches(find).count() {
        0 => bail!("old_text was not found in the file"),
        1 => Ok((old.replacen(find, replace, 1), 1)),
        n if replace_all => Ok((old.replace(find, replace), n)),
        n => bail!("old_text appears {n} times; include more context or set replace_all"),
    }
}

/// Unified diff for the approval prompt; `old: None` is a new file.
pub fn unified_diff(name: &str, old: Option<&str>, new: &str) -> String {
    let before = old.unwrap_or("");
    let from = if old.is_some() {
        format!("a/{name}")
    } else {
        "/dev/null".to_string()
    };
    similar::TextDiff::from_lines(before, new)
        .unified_diff()
        .context_radius(3)
        .header(&from, &format!("b/{name}"))
        .to_string()
}

/// Runs an approved call.
pub async fn run(sandbox: &Sandbox, planned: &Planned) -> ToolOutput {
    let result = match &planned.action {
        Action::Read {
            path,
            offset,
            limit,
        } => read_file(path, *offset, limit.unwrap_or(DEFAULT_READ_LINES)),
        Action::List { path } => list_directory(sandbox, path),
        Action::Find {
            matcher,
            match_names,
            base,
        } => Ok(find_files(sandbox, matcher, *match_names, base)),
        Action::Search(search) => Ok(search.run(sandbox)),
        Action::Write { path, content, .. } => write_file(sandbox, path, content),
        Action::Command {
            command, timeout, ..
        } => {
            // Without the session's background commands, one runs to its end
            // or its timeout here.
            let (jobs, _notices) = Jobs::new();
            return jobs
                .run(command, &sandbox.cwd(), *timeout, false, false)
                .await;
        }
        Action::Fetch(fetch) => fetch.run().await,
        Action::Script(_) => {
            return ToolOutput::error("scripts run through the script runner, not here")
        }
        Action::LocalTool => {
            return ToolOutput::error("local MCP tools run through their server, not here")
        }
    };
    match result {
        Ok(text) => ToolOutput::ok(text),
        Err(err) => ToolOutput::error(format!("{err:#}")),
    }
}

/// Runs an approved call for a script helper and returns the structured
/// shape the helper API promises (see the run_script contract) instead of
/// the model-facing text.
pub fn run_helper(sandbox: &Sandbox, planned: &Planned) -> Result<Value> {
    match &planned.action {
        Action::Read {
            path,
            offset,
            limit,
        } => Ok(Value::String(read_text(path, *offset, *limit)?)),
        Action::List { path } => Ok(json!(list_entries(sandbox, path)?)),
        Action::Find {
            matcher,
            match_names,
            base,
        } => {
            let mut found = find_paths(sandbox, matcher, *match_names, base);
            found.truncate(MAX_HELPER_RESULTS);
            Ok(json!(found))
        }
        Action::Search(search) => {
            let (lines, _) = search.matching_lines(sandbox, MAX_HELPER_RESULTS);
            Ok(json!(lines
                .into_iter()
                .map(|l| json!({"path": l.path, "line": l.number, "text": l.text.trim()}))
                .collect::<Vec<_>>()))
        }
        Action::Write {
            path,
            content,
            replacements,
        } => {
            write_file(sandbox, path, content)?;
            Ok(match replacements {
                Some(n) => json!({"replacements": n}),
                None => json!({"bytes": content.len()}),
            })
        }
        Action::Command { .. } | Action::Script(_) | Action::Fetch(_) => {
            bail!("{} is not available inside scripts", planned.summary)
        }
        Action::LocalTool => bail!("{} is not available inside scripts", planned.summary),
    }
}

/// File text for a script: no line numbers; `offset`/`limit` select lines
/// only when the script asks for them.
fn read_text(path: &Path, offset: usize, limit: Option<usize>) -> Result<String> {
    if path.metadata().map(|m| m.len()).unwrap_or(0) > MAX_HELPER_READ_BYTES {
        bail!("the file is larger than {MAX_HELPER_READ_BYTES} bytes");
    }
    let bytes = std::fs::read(path).context("cannot read the file")?;
    if looks_binary(&bytes) {
        bail!("this is a binary file");
    }
    let text = String::from_utf8_lossy(&bytes);
    if offset == 1 && limit.is_none() {
        return Ok(text.into_owned());
    }
    let selected: Vec<&str> = text
        .lines()
        .skip(offset - 1)
        .take(limit.unwrap_or(usize::MAX))
        .collect();
    Ok(selected.join("\n"))
}

pub(crate) fn looks_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(BINARY_SNIFF_BYTES)].contains(&0)
}

fn read_file(path: &Path, offset: usize, limit: usize) -> Result<String> {
    let bytes = std::fs::read(path).context("cannot read the file")?;
    if looks_binary(&bytes) {
        bail!("this is a binary file");
    }
    let text = String::from_utf8_lossy(&bytes);
    // A notebook reads as its cells, with the ids notebook_edit targets.
    let text = if path.extension().and_then(|e| e.to_str()) == Some("ipynb") {
        std::borrow::Cow::Owned(notebook::render(&text)?)
    } else {
        text
    };
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return Ok("(empty file)".to_string());
    }
    let start = offset - 1;
    if start >= lines.len() {
        bail!("offset {offset} is past the end ({} lines)", lines.len());
    }
    // Stop at a whole line before the output cap, so the note below says
    // exactly where to continue instead of the cap cutting a line.
    let last = (start + limit).min(lines.len());
    let mut out = String::new();
    let mut out_chars = 0;
    let mut end = start;
    for line in &lines[start..last] {
        let shown: String = line.chars().take(MAX_LINE_CHARS).collect();
        let row = format!("{:>6}\t{shown}\n", end + 1);
        out_chars += row.chars().count();
        if out_chars > MAX_OUTPUT_CHARS - CUT_NOTE_CHARS && end > start {
            break;
        }
        out.push_str(&row);
        end += 1;
    }
    if end < lines.len() {
        out.push_str(&format!(
            "… {} more lines; read again with offset {}\n",
            lines.len() - end,
            end + 1
        ));
    }
    Ok(out)
}

/// Sorted entry names, folders suffixed with `/`, secrets left out.
fn list_entries(sandbox: &Sandbox, path: &Path) -> Result<Vec<String>> {
    let mut entries: Vec<String> = std::fs::read_dir(path)
        .context("cannot list the folder")?
        .filter_map(|e| e.ok())
        .filter(|e| !sandbox.is_secret(&e.path()))
        .map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if e.path().is_dir() {
                format!("{name}/")
            } else {
                name
            }
        })
        .collect();
    entries.sort();
    Ok(entries)
}

fn list_directory(sandbox: &Sandbox, path: &Path) -> Result<String> {
    let mut entries = list_entries(sandbox, path)?;
    if entries.is_empty() {
        return Ok("(empty folder)".to_string());
    }
    let total = entries.len();
    entries.truncate(MAX_LIST_ENTRIES);
    let mut out = entries.join("\n");
    if total > MAX_LIST_ENTRIES {
        out.push_str(&format!("\n… {} more entries", total - MAX_LIST_ENTRIES));
    }
    Ok(out)
}

/// Files under `base`, honoring .gitignore and skipping secrets and `.git`.
fn workspace_files<'a>(sandbox: &'a Sandbox, base: &Path) -> impl Iterator<Item = PathBuf> + 'a {
    WalkBuilder::new(base)
        .hidden(false)
        .require_git(false)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_some_and(|t| t.is_file()))
        .map(|entry| entry.into_path())
        .filter(move |path| !sandbox.is_secret(path))
}

/// Files under `base` matching the glob, in walk order.
fn matching_files<'a>(
    sandbox: &'a Sandbox,
    matcher: &'a GlobMatcher,
    match_names: bool,
    base: &Path,
) -> impl Iterator<Item = PathBuf> + 'a {
    workspace_files(sandbox, base).filter(move |path| {
        matcher.is_match(sandbox.display(path))
            || (match_names && path.file_name().is_some_and(|n| matcher.is_match(n)))
    })
}

/// Sorted workspace-relative paths matching the glob.
fn find_paths(
    sandbox: &Sandbox,
    matcher: &GlobMatcher,
    match_names: bool,
    base: &Path,
) -> Vec<String> {
    let mut found: Vec<String> = matching_files(sandbox, matcher, match_names, base)
        .map(|path| sandbox.display(&path))
        .collect();
    found.sort();
    found
}

/// The tool result: matching paths newest first, since recently changed
/// files are usually the ones in play.
fn find_files(sandbox: &Sandbox, matcher: &GlobMatcher, match_names: bool, base: &Path) -> String {
    let mut dated: Vec<(Option<std::time::SystemTime>, String)> =
        matching_files(sandbox, matcher, match_names, base)
            .map(|path| {
                let modified = path.metadata().and_then(|m| m.modified()).ok();
                (modified, sandbox.display(&path))
            })
            .collect();
    dated.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let mut found: Vec<String> = dated.into_iter().map(|(_, path)| path).collect();
    if found.is_empty() {
        return "(no matching files)".to_string();
    }
    let total = found.len();
    found.truncate(MAX_FIND_RESULTS);
    let mut out = found.join("\n");
    if total > MAX_FIND_RESULTS {
        out.push_str(&format!("\n… {} more files", total - MAX_FIND_RESULTS));
    }
    out
}

fn write_file(sandbox: &Sandbox, path: &Path, content: &str) -> Result<String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).context("cannot create the folder")?;
    }
    std::fs::write(path, content).context("cannot write the file")?;
    Ok(format!(
        "wrote {} ({} lines)",
        sandbox.display(path),
        content.lines().count()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_the_servers_display_names_for_client_tools() {
        assert_eq!(
            ToolName::from_display("Read file"),
            Some(ToolName::ReadFile)
        );
        assert_eq!(
            ToolName::from_display("List directory"),
            Some(ToolName::ListDirectory)
        );
        assert_eq!(
            ToolName::from_display("run_script"),
            Some(ToolName::RunScript)
        );
        assert_eq!(ToolName::from_display("Query lakehouse"), None);
    }
    use crate::assistant::sandbox::tests::temp_workspace;
    use serde_json::json;

    fn plan_ok(sandbox: &Sandbox, tool: ToolName, input: Value) -> Planned {
        plan(sandbox, tool, &input).unwrap()
    }

    #[tokio::test]
    async fn reads_with_line_numbers_and_paging() {
        let (dir, sandbox) = temp_workspace("read");
        std::fs::write(dir.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let planned = plan_ok(
            &sandbox,
            ToolName::ReadFile,
            json!({"path": "a.txt", "offset": 2, "limit": 1}),
        );
        assert_eq!(planned.summary, "read_file a.txt");
        let out = run(&sandbox, &planned).await;
        assert!(!out.is_error);
        assert!(out.output.starts_with("     2\ttwo\n"));
        assert!(out
            .output
            .contains("1 more lines; read again with offset 3"));
    }

    #[tokio::test]
    async fn refuses_binary_files() {
        let (dir, sandbox) = temp_workspace("binary");
        std::fs::write(dir.join("b.bin"), [0u8, 1, 2]).unwrap();
        let planned = plan_ok(&sandbox, ToolName::ReadFile, json!({"path": "b.bin"}));
        assert!(run(&sandbox, &planned).await.is_error);
    }

    #[tokio::test]
    async fn lists_folders_without_secrets() {
        let (dir, sandbox) = temp_workspace("list");
        std::fs::create_dir(dir.join("src")).unwrap();
        std::fs::write(dir.join("README.md"), "").unwrap();
        std::fs::write(dir.join(".env"), "SECRET=1").unwrap();
        let planned = plan_ok(&sandbox, ToolName::ListDirectory, json!({}));
        assert_eq!(run(&sandbox, &planned).await.output, "README.md\nsrc/");
    }

    #[tokio::test]
    async fn finds_and_searches_respecting_gitignore() {
        let (dir, sandbox) = temp_workspace("find");
        std::fs::create_dir_all(dir.join("src/nested")).unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join("src/nested/lib.rs"), "// TODO: fix\nfn lib() {}\n").unwrap();
        std::fs::write(dir.join("target/gen.rs"), "// TODO: generated\n").unwrap();
        std::fs::write(dir.join("server.key"), "TODO secret").unwrap();

        let found = plan_ok(&sandbox, ToolName::FindFiles, json!({"pattern": "*.rs"}));
        let mut paths: Vec<String> = run(&sandbox, &found)
            .await
            .output
            .lines()
            .map(str::to_string)
            .collect();
        paths.sort();
        assert_eq!(paths, vec!["src/main.rs", "src/nested/lib.rs"]);

        let search = plan_ok(&sandbox, ToolName::SearchFiles, json!({"pattern": "TODO"}));
        assert_eq!(run(&sandbox, &search).await.output, "src/nested/lib.rs");
        let lines = plan_ok(
            &sandbox,
            ToolName::SearchFiles,
            json!({"pattern": "TODO", "output_mode": "content"}),
        );
        assert_eq!(
            run(&sandbox, &lines).await.output,
            "src/nested/lib.rs:1: // TODO: fix"
        );

        let scoped = plan_ok(
            &sandbox,
            ToolName::SearchFiles,
            json!({"pattern": "fn", "glob": "main.rs"}),
        );
        assert_eq!(run(&sandbox, &scoped).await.output, "src/main.rs");
    }

    #[tokio::test]
    async fn edits_show_a_diff_before_writing() {
        let (dir, sandbox) = temp_workspace("edit");
        std::fs::write(dir.join("a.rs"), "let x = 1;\nlet y = 2;\n").unwrap();
        let planned = plan_ok(
            &sandbox,
            ToolName::EditFile,
            json!({"path": "a.rs", "old_text": "x = 1", "new_text": "x = 10"}),
        );
        let diff = planned.preview.clone().unwrap();
        assert!(diff.contains("-let x = 1;") && diff.contains("+let x = 10;"));
        assert_eq!(
            std::fs::read_to_string(dir.join("a.rs")).unwrap(),
            "let x = 1;\nlet y = 2;\n"
        );
        assert!(!run(&sandbox, &planned).await.is_error);
        assert_eq!(
            std::fs::read_to_string(dir.join("a.rs")).unwrap(),
            "let x = 10;\nlet y = 2;\n"
        );
    }

    #[test]
    fn edit_requires_a_unique_match() {
        assert!(apply_edit("a a", "a", "b", false).is_err());
        assert_eq!(
            apply_edit("a a", "a", "b", true).unwrap(),
            ("b b".to_string(), 2)
        );
        assert!(apply_edit("abc", "zzz", "b", false).is_err());
    }

    #[tokio::test]
    async fn writes_new_files_with_a_creation_diff() {
        let (dir, sandbox) = temp_workspace("write");
        let planned = plan_ok(
            &sandbox,
            ToolName::WriteFile,
            json!({"path": "docs/new.md", "content": "# Hi\n"}),
        );
        assert!(planned.preview.as_ref().unwrap().contains("--- /dev/null"));
        run(&sandbox, &planned).await;
        assert_eq!(
            std::fs::read_to_string(dir.join("docs/new.md")).unwrap(),
            "# Hi\n"
        );
    }

    #[test]
    fn plans_reject_sandbox_violations() {
        let (_dir, sandbox) = temp_workspace("violations");
        assert!(plan(&sandbox, ToolName::ReadFile, &json!({"path": "../x"})).is_err());
        assert!(plan(
            &sandbox,
            ToolName::WriteFile,
            &json!({"path": ".env", "content": ""})
        )
        .is_err());
        assert!(plan(&sandbox, ToolName::ReadFile, &json!({})).is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runs_commands_in_the_workspace_with_a_timeout() {
        let (dir, sandbox) = temp_workspace("cmd");
        std::fs::write(dir.join("marker"), "").unwrap();
        let planned = plan_ok(&sandbox, ToolName::RunCommand, json!({"command": "ls"}));
        let out = run(&sandbox, &planned).await;
        assert!(out.output.starts_with("exit code 0") && out.output.contains("marker"));
        let failing = plan_ok(&sandbox, ToolName::RunCommand, json!({"command": "exit 3"}));
        let out = run(&sandbox, &failing).await;
        assert!(out.is_error && out.output.starts_with("exit code 3"));
        let slow = plan_ok(
            &sandbox,
            ToolName::RunCommand,
            json!({"command": "sleep 5", "timeout_seconds": 1}),
        );
        assert!(run(&sandbox, &slow)
            .await
            .output
            .contains("timed out after 1s"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn commands_run_in_the_current_folder_not_the_start_folder() {
        let (dir, sandbox) = temp_workspace("cmd-cwd");
        std::fs::create_dir(dir.join("app")).unwrap();
        std::fs::write(dir.join("app/inner-marker"), "").unwrap();
        sandbox.set_cwd(&dir.join("app")).unwrap();
        let planned = plan_ok(&sandbox, ToolName::RunCommand, json!({"command": "ls"}));
        assert_eq!(
            planned.scope,
            dir.join("app"),
            "permission is asked for this folder"
        );
        let out = run(&sandbox, &planned).await;
        assert!(out.output.contains("inner-marker"), "{}", out.output);
    }

    #[test]
    fn long_output_keeps_its_start_and_end() {
        let text = format!("first{}last", "x".repeat(MAX_OUTPUT_CHARS));
        let out = ToolOutput::ok(text);
        assert!(out.truncated);
        assert!(out.output.chars().count() <= MAX_OUTPUT_CHARS);
        assert!(out.output.starts_with("first"));
        assert!(out.output.ends_with("last"), "errors and totals come last");
        assert!(out.output.contains("characters cut here by gz"));
    }

    #[tokio::test]
    async fn large_reads_stop_at_a_line_and_say_where_to_continue() {
        let (dir, sandbox) = temp_workspace("read-large");
        let body: String = (1..=3_000)
            .map(|i| format!("line {i}: {}\n", "y".repeat(40)))
            .collect();
        std::fs::write(dir.join("big.txt"), body).unwrap();
        let planned = plan_ok(&sandbox, ToolName::ReadFile, json!({"path": "big.txt"}));
        let out = run(&sandbox, &planned).await;
        assert!(!out.truncated, "paging, not the generic cut");
        assert!(out.output.chars().count() <= MAX_OUTPUT_CHARS);
        let last_row = out.output.lines().rev().nth(1).unwrap();
        let shown: usize = last_row.split('\t').next().unwrap().trim().parse().unwrap();
        assert!(out
            .output
            .ends_with(&format!("read again with offset {}\n", shown + 1)));
    }

    #[test]
    fn catalog_names_round_trip() {
        for tool in ToolName::ALL {
            assert_eq!(ToolName::parse(tool.as_str()), Some(tool));
        }
        assert_eq!(ToolName::parse("rm_rf"), None);
    }
}
