//! The client-tool catalog the chat server may call (see the client-tools
//! contract). Each call is first *planned* — validated, sandboxed, and given
//! a human summary and, for writes, a diff — so the user can approve it
//! before anything touches the machine. Only then is it run.

use crate::assistant::edit_text;
use crate::assistant::jobs::Jobs;
use crate::assistant::notebook::{self, CellEdit};
use crate::assistant::read_ledger::{self, Fingerprint};
use crate::assistant::search::{self, Search};
use crate::assistant::web_fetch::Fetch;
use anyhow::{bail, Context, Result};
use globset::{Glob, GlobMatcher};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::assistant::sandbox::Sandbox;

/// Most bytes one tool result's `output` takes as UTF-8 JSON (escapes
/// included). The chat server collapses a result over 64 KiB of that JSON
/// into a short preview the model can't page through (gz-chat-eks
/// agent/hooks.py `MAX_MODEL_TOOL_RESULT_BYTES`), so gz cuts below it and
/// leaves room for the result's other fields.
pub const MAX_OUTPUT_BYTES: usize = 56 * 1024;
/// Room kept free for the note that ends a cut output.
const CUT_NOTE_BYTES: usize = 400;
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
        /// The file as the change was worked out against (`None`: it did
        /// not exist), so the write lands only on that same content.
        base: Option<Fingerprint>,
        /// For overwrites: what of the old file gz kept in the content
        /// (line endings, byte-order mark), told to the model.
        kept: Option<String>,
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

    /// Whether this call only reads or searches one output gz saved for
    /// this session (see [`OutputSpill`](crate::assistant::output_spill::OutputSpill)).
    /// gz told the model to page through that file, so reading it needs no
    /// permission; any other path, the folder around it included, does.
    pub fn reads_saved_output(&self, sandbox: &Sandbox) -> bool {
        let read = match &self.action {
            Action::Read { path, .. } => path.as_path(),
            Action::Search(search) => search.base(),
            _ => return false,
        };
        self.access == Access::Read && sandbox.spill().contains(read)
    }

    /// The tool's name: every summary starts with it (see [`plan`] and
    /// [`local_tool_call`]).
    pub fn tool_name(&self) -> &str {
        self.summary.split(' ').next().unwrap_or_default()
    }

    /// The real path a file tool reads, lists, searches or writes.
    pub fn path(&self) -> Option<&Path> {
        match &self.action {
            Action::Read { path, .. } | Action::List { path } | Action::Write { path, .. } => {
                Some(path)
            }
            Action::Find { base, .. } => Some(base),
            Action::Search(search) => Some(search.base()),
            Action::Command { .. } | Action::Fetch(_) | Action::Script(_) | Action::LocalTool => {
                None
            }
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

    /// Cuts the output again after notes were added to it, so the result
    /// still fits the cap.
    pub fn refit(self) -> Self {
        let truncated = self.truncated;
        let mut out = Self::capped(self.output, self.is_error);
        out.truncated |= truncated;
        out
    }

    /// Keeps a quarter of the room for the start of a long output and the
    /// rest for its end, where commands print errors and totals.
    fn capped(text: String, is_error: bool) -> Self {
        let cut = match fits_one_result(&text) {
            true => None,
            false => HeadTail::of(&text, MAX_OUTPUT_BYTES - CUT_NOTE_BYTES),
        };
        let Some(cut) = cut else {
            return Self {
                output: text,
                is_error,
                truncated: false,
            };
        };
        Self {
            output: cut.join(&format!(
                "… {} characters ({} bytes) cut here by gz; narrow the request (a path, \
                 glob, offset, or filter) to see them …",
                cut.cut_chars, cut.cut_bytes
            )),
            is_error,
            truncated: true,
        }
    }
}

/// Bytes `c` takes in a JSON string as the chat server encodes it (UTF-8,
/// non-ASCII left as is; quotes, backslashes and control characters
/// escaped).
fn json_len(c: char) -> usize {
    match c {
        '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
        c if (c as u32) < 0x20 => 6,
        c => c.len_utf8(),
    }
}

/// Bytes `text` takes inside a JSON string (without the quotes).
pub fn json_size(text: &str) -> usize {
    text.chars().map(json_len).sum()
}

/// Whether `text` fits one tool result without gz cutting it.
pub fn fits_one_result(text: &str) -> bool {
    json_size(text) <= MAX_OUTPUT_BYTES
}

/// A long text cut to its start and end, on character boundaries.
pub struct HeadTail<'a> {
    pub head: &'a str,
    pub tail: &'a str,
    pub cut_chars: usize,
    pub cut_bytes: usize,
}

impl<'a> HeadTail<'a> {
    /// `None` when `text` fits `room` JSON bytes; otherwise its first
    /// quarter of the room and its last three quarters.
    pub fn of(text: &'a str, room: usize) -> Option<Self> {
        if json_size(text) <= room {
            return None;
        }
        let head_room = room / 4;
        let head_end = prefix_within(text, head_room);
        let tail_start = suffix_within(&text[head_end..], room - head_room) + head_end;
        let middle = &text[head_end..tail_start];
        Some(Self {
            head: &text[..head_end],
            tail: &text[tail_start..],
            cut_chars: middle.chars().count(),
            cut_bytes: middle.len(),
        })
    }

    /// The head, `note` on a line of its own, and the tail.
    pub fn join(&self, note: &str) -> String {
        format!("{}\n{note}\n{}", self.head, self.tail)
    }
}

/// Byte index where the longest prefix of `text` within `room` JSON bytes
/// ends.
fn prefix_within(text: &str, room: usize) -> usize {
    let mut used = 0;
    for (index, c) in text.char_indices() {
        used += json_len(c);
        if used > room {
            return index;
        }
    }
    text.len()
}

/// Byte index where the longest suffix of `text` within `room` JSON bytes
/// starts.
fn suffix_within(text: &str, room: usize) -> usize {
    let mut used = 0;
    for (index, c) in text.char_indices().rev() {
        used += json_len(c);
        if used > room {
            return index + c.len_utf8();
        }
    }
    0
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
            let path = sandbox.resolve_readable(str_field(input, "path")?)?;
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
            let bytes = known_content(sandbox, &path)?;
            let base = Some(Fingerprint::of(&bytes));
            let old = edit_text::editable(bytes, &sandbox.display(&path), "edit_file")?;
            let (content, replaced) = edit_text::apply_edit(
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
                    base,
                    kept: None,
                },
            )
        }
        ToolName::WriteFile => {
            let path = sandbox.resolve(str_field(input, "path")?)?;
            if path.is_dir() {
                bail!("{} is a folder", sandbox.display(&path));
            }
            let existing = match path.exists() {
                true => Some(known_content(sandbox, &path)?),
                false => None,
            };
            let shown = sandbox.display(&path);
            let edit_text::Replacement { content, kept } =
                edit_text::replacement(existing.as_deref(), str_field(input, "content")?, &shown)?;
            let base = existing.as_deref().map(Fingerprint::of);
            // The diff shows a file that isn't UTF-8 as read_file does.
            let old = existing.map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
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
                    base,
                    kept,
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
            let bytes = known_content(sandbox, &path)?;
            let base = Some(Fingerprint::of(&bytes));
            let old = edit_text::editable(bytes, &sandbox.display(&path), "notebook_edit")?;
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
                    base,
                    kept: None,
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

/// The bytes of an existing file the model wants to change, refused
/// unless the model read it as it is now (see [`read_ledger`]).
fn known_content(sandbox: &Sandbox, path: &Path) -> Result<Vec<u8>> {
    let shown = sandbox.display(path);
    let bytes = std::fs::read(path).with_context(|| format!("cannot read {shown}"))?;
    sandbox.reads().check_known(path, &bytes, &shown)?;
    Ok(bytes)
}

fn parent(sandbox: &Sandbox, path: &Path) -> PathBuf {
    path.parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| sandbox.cwd())
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
        } => read_file(sandbox, path, *offset, limit.unwrap_or(DEFAULT_READ_LINES)),
        Action::List { path } => list_directory(sandbox, path),
        Action::Find {
            matcher,
            match_names,
            base,
        } => Ok(find_files(sandbox, matcher, *match_names, base)),
        Action::Search(search) => Ok(search.run(sandbox)),
        Action::Write {
            path,
            content,
            base,
            kept,
            ..
        } => write_file(sandbox, path, content, *base).map(|wrote| match kept {
            Some(kept) => format!("{wrote}; {kept}"),
            None => wrote,
        }),
        Action::Command {
            command, timeout, ..
        } => {
            // Without the session's background commands, one runs to its end
            // or its timeout here.
            let (jobs, _notices) = Jobs::new();
            let jobs = jobs.with_spill(sandbox.spill().clone());
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
        } => Ok(Value::String(read_text(sandbox, path, *offset, *limit)?)),
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
            base,
            kept,
        } => {
            write_file(sandbox, path, content, *base)?;
            let mut result = match replacements {
                Some(n) => json!({"replacements": n}),
                None => json!({"bytes": content.len()}),
            };
            if let Some(kept) = kept {
                result["kept"] = json!(kept);
            }
            Ok(result)
        }
        Action::Command { .. } | Action::Script(_) | Action::Fetch(_) => {
            bail!("{} is not available inside scripts", planned.summary)
        }
        Action::LocalTool => bail!("{} is not available inside scripts", planned.summary),
    }
}

/// File text for a script: no line numbers; `offset`/`limit` select lines
/// only when the script asks for them.
fn read_text(
    sandbox: &Sandbox,
    path: &Path,
    offset: usize,
    limit: Option<usize>,
) -> Result<String> {
    if path.metadata().map(|m| m.len()).unwrap_or(0) > MAX_HELPER_READ_BYTES {
        bail!("the file is larger than {MAX_HELPER_READ_BYTES} bytes");
    }
    let bytes = std::fs::read(path).context("cannot read the file")?;
    sandbox.reads().record(path, &bytes);
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

fn read_file(sandbox: &Sandbox, path: &Path, offset: usize, limit: usize) -> Result<String> {
    let bytes = std::fs::read(path).context("cannot read the file")?;
    // Any read counts, a page or a binary file refused below included: the
    // model knows what the file is now and may change or replace it.
    sandbox.reads().record(path, &bytes);
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
    let mut out = match edit_text::read_note(&bytes) {
        Some(note) => format!("{note}\n"),
        None => String::new(),
    };
    let mut out_bytes = 0;
    let mut end = start;
    for line in &lines[start..last] {
        let shown: String = line.chars().take(MAX_LINE_CHARS).collect();
        let row = format!("{:>6}\t{shown}\n", end + 1);
        out_bytes += json_size(&row);
        if out_bytes > MAX_OUTPUT_BYTES - CUT_NOTE_BYTES && end > start {
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

/// Files under `base` matching the glob, in path order, honoring
/// .gitignore and skipping secrets and `.git`.
fn matching_files(
    sandbox: &Sandbox,
    matcher: &GlobMatcher,
    match_names: bool,
    base: &Path,
) -> Vec<PathBuf> {
    search::walk_files(base, None, |path| {
        !sandbox.is_secret(path)
            && ((match_names && path.file_name().is_some_and(|n| matcher.is_match(n)))
                || matcher.is_match(sandbox.display(path)))
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
        .iter()
        .map(|path| sandbox.display(path))
        .collect();
    found.sort();
    found
}

/// The tool result: matching paths newest first, since recently changed
/// files are usually the ones in play.
fn find_files(sandbox: &Sandbox, matcher: &GlobMatcher, match_names: bool, base: &Path) -> String {
    let found = matching_files(sandbox, matcher, match_names, base);
    // Dating needs a stat per match; done on several threads.
    let modified = search::parallel_map(&found, |path| {
        path.metadata().and_then(|m| m.modified()).ok()
    });
    let mut dated: Vec<(Option<std::time::SystemTime>, String)> = modified
        .into_iter()
        .zip(&found)
        .map(|(modified, path)| (modified, sandbox.display(path)))
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

/// Writes an approved change, only onto the content it was planned
/// against (`base`), and notes the result as read: the model knows it.
fn write_file(
    sandbox: &Sandbox,
    path: &Path,
    content: &str,
    base: Option<Fingerprint>,
) -> Result<String> {
    read_ledger::check_unchanged(path, base, &sandbox.display(path))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).context("cannot create the folder")?;
    }
    std::fs::write(path, content).context("cannot write the file")?;
    sandbox.reads().record(path, content.as_bytes());
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

    /// Reads `path` with read_file, as the model would before changing it.
    async fn read_as_model(sandbox: &Sandbox, path: &str) {
        let planned = plan_ok(sandbox, ToolName::ReadFile, json!({"path": path}));
        let out = run(sandbox, &planned).await;
        assert!(!out.is_error, "{}", out.output);
    }

    fn plan_err(sandbox: &Sandbox, tool: ToolName, input: Value) -> String {
        format!("{:#}", plan(sandbox, tool, &input).unwrap_err())
    }

    #[tokio::test]
    async fn changes_need_a_read_of_the_current_content() {
        let (dir, sandbox) = temp_workspace("read-first");
        std::fs::write(dir.join("a.rs"), "let x = 1;\n").unwrap();
        let change = json!({"path": "a.rs", "old_text": "1", "new_text": "2"});
        let overwrite = json!({"path": "a.rs", "content": "new\n"});

        let unread = plan_err(&sandbox, ToolName::EditFile, change.clone());
        assert!(unread.contains("read it with read_file first"), "{unread}");
        let unread = plan_err(&sandbox, ToolName::WriteFile, overwrite.clone());
        assert!(unread.contains("read it with read_file first"), "{unread}");

        // A page of the file counts as a read.
        let page = plan_ok(
            &sandbox,
            ToolName::ReadFile,
            json!({"path": "a.rs", "offset": 1, "limit": 1}),
        );
        run(&sandbox, &page).await;
        plan_ok(&sandbox, ToolName::EditFile, change.clone());
        plan_ok(&sandbox, ToolName::WriteFile, overwrite);

        std::fs::write(dir.join("a.rs"), "let x = 1; // by the user\n").unwrap();
        let changed = plan_err(&sandbox, ToolName::EditFile, change);
        assert!(
            changed.contains("changed on disk since you last read it")
                && changed.contains("read it again with read_file before editing"),
            "{changed}"
        );
    }

    #[tokio::test]
    async fn new_files_need_no_read_and_own_writes_count_as_read() {
        let (dir, sandbox) = temp_workspace("own-writes");
        let create = plan_ok(
            &sandbox,
            ToolName::WriteFile,
            json!({"path": "n.txt", "content": "one\n"}),
        );
        assert!(!run(&sandbox, &create).await.is_error);
        let edit = plan_ok(
            &sandbox,
            ToolName::EditFile,
            json!({"path": "n.txt", "old_text": "one", "new_text": "two"}),
        );
        assert!(!run(&sandbox, &edit).await.is_error);
        plan_ok(
            &sandbox,
            ToolName::EditFile,
            json!({"path": "n.txt", "old_text": "two", "new_text": "three"}),
        );
        assert_eq!(std::fs::read_to_string(dir.join("n.txt")).unwrap(), "two\n");
    }

    #[tokio::test]
    async fn a_forked_sandbox_reads_for_itself() {
        let (dir, sandbox) = temp_workspace("fork-reads");
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        read_as_model(&sandbox, "a.txt").await;
        let fork = sandbox.fork();
        let change = json!({"path": "a.txt", "old_text": "one", "new_text": "two"});
        assert!(plan(&fork, ToolName::EditFile, &change).is_err());
        assert!(plan(&sandbox.clone(), ToolName::EditFile, &change).is_ok());
    }

    #[tokio::test]
    async fn notebook_edits_need_a_read_too() {
        let (dir, sandbox) = temp_workspace("nb-read");
        let notebook = json!({
            "cells": [{"cell_type": "code", "id": "c1", "metadata": {}, "source": ["x = 1"],
                       "outputs": [], "execution_count": null}],
            "metadata": {}, "nbformat": 4, "nbformat_minor": 5
        });
        std::fs::write(dir.join("n.ipynb"), notebook.to_string()).unwrap();
        let edit = json!({"path": "n.ipynb", "cell_id": "c1", "new_source": "x = 2"});
        let err = plan_err(&sandbox, ToolName::NotebookEdit, edit.clone());
        assert!(err.contains("read it with read_file first"), "{err}");
        read_as_model(&sandbox, "n.ipynb").await;
        plan_ok(&sandbox, ToolName::NotebookEdit, edit);
    }

    #[tokio::test]
    async fn an_approved_change_never_lands_on_newer_content() {
        let (dir, sandbox) = temp_workspace("stale-diff");
        std::fs::write(dir.join("a.rs"), "let x = 1;\n").unwrap();
        read_as_model(&sandbox, "a.rs").await;
        let planned = plan_ok(
            &sandbox,
            ToolName::EditFile,
            json!({"path": "a.rs", "old_text": "1", "new_text": "2"}),
        );
        // While the user looks at the diff, a formatter rewrites the file.
        std::fs::write(dir.join("a.rs"), "let x = 1; // formatted\n").unwrap();
        let out = run(&sandbox, &planned).await;
        assert!(out.is_error);
        assert!(
            out.output
                .contains("changed on disk after this change was planned")
                && out.output.contains("read it again with read_file"),
            "{}",
            out.output
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("a.rs")).unwrap(),
            "let x = 1; // formatted\n"
        );

        let create = plan_ok(
            &sandbox,
            ToolName::WriteFile,
            json!({"path": "b.rs", "content": "mine\n"}),
        );
        std::fs::write(dir.join("b.rs"), "theirs\n").unwrap();
        let out = run(&sandbox, &create).await;
        assert!(out.is_error && out.output.contains("created by something else"));
        assert_eq!(
            std::fs::read_to_string(dir.join("b.rs")).unwrap(),
            "theirs\n"
        );
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
        read_as_model(&sandbox, "a.rs").await;
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

    #[tokio::test]
    async fn crlf_files_take_lf_edits_and_stay_crlf_on_disk() {
        let (dir, sandbox) = temp_workspace("crlf");
        std::fs::write(dir.join("w.bat"), "@echo off\r\necho one\r\n").unwrap();
        read_as_model(&sandbox, "w.bat").await;
        let planned = plan_ok(
            &sandbox,
            ToolName::EditFile,
            json!({"path": "w.bat", "old_text": "off\necho one", "new_text": "off\necho two\necho three"}),
        );
        assert!(!run(&sandbox, &planned).await.is_error);
        assert_eq!(
            std::fs::read(dir.join("w.bat")).unwrap(),
            b"@echo off\r\necho two\r\necho three\r\n"
        );
        // An overwrite keeps the file's endings too; its own write counts
        // as read.
        let planned = plan_ok(
            &sandbox,
            ToolName::WriteFile,
            json!({"path": "w.bat", "content": "@echo on\n"}),
        );
        let out = run(&sandbox, &planned).await;
        assert!(!out.is_error);
        assert!(
            out.output.ends_with(
                "; kept the file's CRLF line endings; to change that, rewrite it with run_script"
            ),
            "the model is told its LF endings were not written: {}",
            out.output
        );
        assert_eq!(std::fs::read(dir.join("w.bat")).unwrap(), b"@echo on\r\n");
    }

    #[tokio::test]
    async fn files_that_are_not_utf8_never_get_replacement_characters() {
        let (dir, sandbox) = temp_workspace("latin1");
        let latin1 = b"caf\xe9 menu\n".to_vec();
        std::fs::write(dir.join("menu.txt"), &latin1).unwrap();
        let read = plan_ok(&sandbox, ToolName::ReadFile, json!({"path": "menu.txt"}));
        let shown = run(&sandbox, &read).await.output;
        assert!(
            shown.starts_with("(this file is not valid UTF-8"),
            "{shown}"
        );
        assert!(shown.contains("caf\u{FFFD} menu"));

        let edit = plan_err(
            &sandbox,
            ToolName::EditFile,
            json!({"path": "menu.txt", "old_text": "menu", "new_text": "list"}),
        );
        assert!(
            edit.contains("not valid UTF-8") && edit.contains("run_script"),
            "{edit}"
        );
        let copied = plan_err(
            &sandbox,
            ToolName::WriteFile,
            json!({"path": "menu.txt", "content": "caf\u{FFFD} list\n"}),
        );
        assert!(copied.contains("would destroy those bytes"), "{copied}");
        assert_eq!(std::fs::read(dir.join("menu.txt")).unwrap(), latin1);

        let replaced = plan_ok(
            &sandbox,
            ToolName::WriteFile,
            json!({"path": "menu.txt", "content": "café list\n"}),
        );
        assert!(replaced.preview.unwrap().contains("-caf\u{FFFD} menu"));
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

    /// The size the chat server measures: the result as UTF-8 JSON.
    fn server_size(out: &ToolOutput) -> usize {
        serde_json::to_string(
            &json!({"output": out.output, "isError": out.is_error, "truncated": out.truncated}),
        )
        .unwrap()
        .len()
    }

    #[test]
    fn long_output_keeps_its_start_and_end() {
        let text = format!("first{}last", "x".repeat(MAX_OUTPUT_BYTES));
        let out = ToolOutput::ok(text);
        assert!(out.truncated);
        assert!(json_size(&out.output) <= MAX_OUTPUT_BYTES);
        assert!(out.output.starts_with("first"));
        assert!(out.output.ends_with("last"), "errors and totals come last");
        assert!(out.output.contains("characters ("));
        assert!(out.output.contains("bytes) cut here by gz"));
    }

    #[test]
    fn output_is_capped_by_its_json_bytes_not_its_characters() {
        // 30,000 characters: under any character cap, but each is 3 UTF-8
        // bytes (90 KB), and quotes or newlines double when escaped.
        for unit in ["界", "\"", "\n", "\u{1}", "🙂"] {
            let text = unit.repeat(30_000);
            let out = ToolOutput::ok(text.clone());
            assert!(out.truncated, "{unit:?}");
            assert!(
                server_size(&out) < 64 * 1024,
                "{unit:?}: {}",
                server_size(&out)
            );
            assert!(json_size(&out.output) <= MAX_OUTPUT_BYTES, "{unit:?}");
        }
        let fits = "界".repeat(10_000);
        assert!(!ToolOutput::ok(fits.clone()).truncated);
        assert_eq!(ToolOutput::ok(fits.clone()).output, fits);
    }

    #[test]
    fn notes_added_after_the_cut_are_cut_to_fit_too() {
        let mut out = ToolOutput::ok("x".repeat(MAX_OUTPUT_BYTES));
        assert!(!out.truncated);
        out.output
            .push_str("\n[after_tool hook]\nformatted 3 files");
        let out = out.refit();
        assert!(out.truncated && json_size(&out.output) <= MAX_OUTPUT_BYTES);
        assert!(out.output.ends_with("formatted 3 files"));
    }

    #[test]
    fn json_size_matches_the_servers_encoding() {
        let text = "a\"b\\c\nd\te\u{1}f界🙂\u{7f}";
        let encoded = serde_json::to_string(text).unwrap();
        assert_eq!(json_size(text), encoded.len() - 2);
    }

    #[tokio::test]
    async fn wide_character_reads_page_within_the_byte_cap() {
        let (dir, sandbox) = temp_workspace("read-wide");
        let body: String = (1..=2_000)
            .map(|i| format!("{i} {}\n", "漢".repeat(60)))
            .collect();
        std::fs::write(dir.join("wide.txt"), body).unwrap();
        let planned = plan_ok(&sandbox, ToolName::ReadFile, json!({"path": "wide.txt"}));
        let out = run(&sandbox, &planned).await;
        assert!(!out.truncated, "paging, not the generic cut");
        assert!(json_size(&out.output) <= MAX_OUTPUT_BYTES);
        assert!(out.output.contains("read again with offset"));
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
        assert!(json_size(&out.output) <= MAX_OUTPUT_BYTES);
        let last_row = out.output.lines().rev().nth(1).unwrap();
        let shown: usize = last_row.split('\t').next().unwrap().trim().parse().unwrap();
        assert!(out
            .output
            .ends_with(&format!("read again with offset {}\n", shown + 1)));
    }

    /// The saved-output path a cut result names.
    fn saved_path(output: &str) -> String {
        let rest = output.split("is saved in ").nth(1).expect(output);
        rest.split(':').next().unwrap().to_string()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn long_command_output_is_saved_and_can_be_paged_and_searched() {
        let (_dir, sandbox) = temp_workspace("cmd-spill");
        let command = "i=0; while [ $i -lt 20000 ]; do echo \"row $i\"; i=$((i+1)); done";
        let planned = plan_ok(&sandbox, ToolName::RunCommand, json!({"command": command}));
        let out = run(&sandbox, &planned).await;
        assert!(out.truncated && !out.is_error, "{}", out.output);
        assert!(json_size(&out.output) <= MAX_OUTPUT_BYTES);
        assert!(out
            .output
            .starts_with("[The whole output (20001 lines) is saved in "));
        assert!(out.output.contains("]\nexit code 0\nrow 0\n"));
        assert!(out.output.ends_with("row 19999\n"));
        let path = saved_path(&out.output);

        let page = plan_ok(
            &sandbox,
            ToolName::ReadFile,
            json!({"path": path, "offset": 10_001, "limit": 2}),
        );
        let page = run(&sandbox, &page).await;
        assert!(
            page.output
                .starts_with(" 10001\trow 9999\n 10002\trow 10000\n"),
            "{}",
            page.output
        );

        let search = plan_ok(
            &sandbox,
            ToolName::SearchFiles,
            json!({"pattern": "^row 12345$", "path": path, "output_mode": "content"}),
        );
        let found = run(&sandbox, &search).await;
        assert!(found.output.contains("row 12345"), "{}", found.output);
    }

    #[test]
    fn only_saved_outputs_open_outside_the_folders() {
        let (_dir, sandbox) = temp_workspace("spill-access");
        let saved = sandbox.spill().save("the whole log").unwrap();
        let shown = saved.to_string_lossy().to_string();
        assert_eq!(sandbox.resolve_readable(&shown).unwrap(), saved);
        // Saved outputs are for reading and searching, never for changes.
        assert!(sandbox.resolve(&shown).is_err());
        let write = plan(
            &sandbox,
            ToolName::WriteFile,
            &json!({"path": shown, "content": "x"}),
        );
        assert!(write.is_err());
        let listing = plan(
            &sandbox,
            ToolName::ListDirectory,
            &json!({"path": saved.parent().unwrap()}),
        );
        assert!(listing.is_err(), "the folder itself stays closed");

        let sibling = saved.with_file_name("planted.txt");
        std::fs::write(&sibling, "not gz's").unwrap();
        assert!(sandbox
            .resolve_readable(&sibling.to_string_lossy())
            .is_err());
        assert!(plan(&sandbox, ToolName::ReadFile, &json!({"path": "/etc/hosts"})).is_err());
        let other =
            Sandbox::new(&crate::assistant::sandbox::tests::temp_dir("spill-other")).unwrap();
        assert!(
            other.resolve_readable(&shown).is_err(),
            "another session can't open it"
        );
    }

    #[test]
    fn catalog_names_round_trip() {
        for tool in ToolName::ALL {
            assert_eq!(ToolName::parse(tool.as_str()), Some(tool));
        }
        assert_eq!(ToolName::parse("rm_rf"), None);
    }
}
