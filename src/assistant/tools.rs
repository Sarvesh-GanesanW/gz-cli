//! The client-tool catalog the chat server may call (see the client-tools
//! contract). Each call is first *planned* — validated, sandboxed, and given
//! a human summary and, for writes, a diff — so the user can approve it
//! before anything touches the machine. Only then is it run.

use anyhow::{bail, Context, Result};
use globset::{Glob, GlobMatcher};
use ignore::WalkBuilder;
use regex::Regex;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::assistant::sandbox::Sandbox;

/// Contract limit on one result's `output`.
pub const MAX_OUTPUT_CHARS: usize = 262_144;
const DEFAULT_READ_LINES: usize = 2000;
const MAX_LINE_CHARS: usize = 2000;
const MAX_LIST_ENTRIES: usize = 1000;
const MAX_FIND_RESULTS: usize = 500;
const MAX_SEARCH_MATCHES: usize = 500;
const MAX_SEARCH_LINE_CHARS: usize = 300;
const MAX_SEARCH_FILE_BYTES: u64 = 2 * 1024 * 1024;
const BINARY_SNIFF_BYTES: usize = 8000;
const DEFAULT_COMMAND_SECS: u64 = 120;
const MAX_COMMAND_SECS: u64 = 600;

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
}

impl ToolName {
    pub const ALL: [ToolName; 7] = [
        ToolName::ReadFile,
        ToolName::ListDirectory,
        ToolName::FindFiles,
        ToolName::SearchFiles,
        ToolName::EditFile,
        ToolName::WriteFile,
        ToolName::RunCommand,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ToolName::ReadFile => "read_file",
            ToolName::ListDirectory => "list_directory",
            ToolName::FindFiles => "find_files",
            ToolName::SearchFiles => "search_files",
            ToolName::EditFile => "edit_file",
            ToolName::WriteFile => "write_file",
            ToolName::RunCommand => "run_command",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.as_str() == name)
    }

    pub fn access(self) -> Access {
        match self {
            ToolName::ReadFile
            | ToolName::ListDirectory
            | ToolName::FindFiles
            | ToolName::SearchFiles => Access::Read,
            ToolName::EditFile | ToolName::WriteFile => Access::Write,
            ToolName::RunCommand => Access::Command,
        }
    }
}

/// What kind of permission a call needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
    Command,
    /// An action the chat server itself wants to take (its own approval
    /// interrupt), not a local tool.
    ServerAction,
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
    action: Action,
}

#[derive(Debug, Clone)]
enum Action {
    Read {
        path: PathBuf,
        offset: usize,
        limit: usize,
    },
    List {
        path: PathBuf,
    },
    Find {
        matcher: GlobMatcher,
        match_names: bool,
        base: PathBuf,
    },
    Search {
        regex: Regex,
        file_glob: Option<GlobMatcher>,
        base: PathBuf,
    },
    Write {
        path: PathBuf,
        content: String,
    },
    Command {
        command: String,
        timeout: Duration,
    },
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

    fn capped(text: String, is_error: bool) -> Self {
        if text.chars().count() <= MAX_OUTPUT_CHARS {
            return Self {
                output: text,
                is_error,
                truncated: false,
            };
        }
        let mut kept: String = text.chars().take(MAX_OUTPUT_CHARS - 64).collect();
        kept.push_str("\n… output truncated by gz");
        Self {
            output: kept,
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
            let limit = opt_usize(input, "limit")
                .unwrap_or(DEFAULT_READ_LINES)
                .clamp(1, DEFAULT_READ_LINES);
            let scope = path.parent().unwrap_or(sandbox.root()).to_path_buf();
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
            let pattern = str_field(input, "pattern")?;
            let base = existing_dir(sandbox, path_arg)?;
            let regex =
                Regex::new(pattern).with_context(|| format!("invalid regex '{pattern}'"))?;
            let file_glob = opt_str(input, "glob").map(glob).transpose()?;
            let action = Action::Search {
                regex,
                file_glob,
                base: base.clone(),
            };
            (
                format!("/{pattern}/ in {}", sandbox.display(&base)),
                base,
                None,
                action,
            )
        }
        ToolName::EditFile => {
            let path = sandbox.resolve(str_field(input, "path")?)?;
            let old = std::fs::read_to_string(&path)
                .with_context(|| format!("cannot read {}", sandbox.display(&path)))?;
            let content = apply_edit(
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
                Action::Write { path, content },
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
                Action::Write { path, content },
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
            };
            (
                command.clone(),
                sandbox.root().to_path_buf(),
                Some(command),
                action,
            )
        }
    };
    Ok(Planned {
        access: tool.access(),
        summary: format!("{} {summary}", tool.as_str()),
        scope,
        preview,
        action,
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
    path.parent().unwrap_or(sandbox.root()).to_path_buf()
}

/// Exact-string replacement; ambiguity is an error so the model must be
/// specific (or ask for `replace_all`).
fn apply_edit(old: &str, find: &str, replace: &str, replace_all: bool) -> Result<String> {
    if find.is_empty() {
        bail!("old_text is empty");
    }
    match old.matches(find).count() {
        0 => bail!("old_text was not found in the file"),
        1 => Ok(old.replacen(find, replace, 1)),
        _ if replace_all => Ok(old.replace(find, replace)),
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
        } => read_file(path, *offset, *limit),
        Action::List { path } => list_directory(sandbox, path),
        Action::Find {
            matcher,
            match_names,
            base,
        } => Ok(find_files(sandbox, matcher, *match_names, base)),
        Action::Search {
            regex,
            file_glob,
            base,
        } => Ok(search_files(sandbox, regex, file_glob.as_ref(), base)),
        Action::Write { path, content } => write_file(sandbox, path, content),
        Action::Command { command, timeout } => {
            return run_command(sandbox.root(), command, *timeout).await
        }
    };
    match result {
        Ok(text) => ToolOutput::ok(text),
        Err(err) => ToolOutput::error(format!("{err:#}")),
    }
}

fn looks_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(BINARY_SNIFF_BYTES)].contains(&0)
}

fn read_file(path: &Path, offset: usize, limit: usize) -> Result<String> {
    let bytes = std::fs::read(path).context("cannot read the file")?;
    if looks_binary(&bytes) {
        bail!("this is a binary file");
    }
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return Ok("(empty file)".to_string());
    }
    let start = offset - 1;
    if start >= lines.len() {
        bail!("offset {offset} is past the end ({} lines)", lines.len());
    }
    let end = (start + limit).min(lines.len());
    let mut out = String::new();
    for (i, line) in lines[start..end].iter().enumerate() {
        let shown: String = line.chars().take(MAX_LINE_CHARS).collect();
        out.push_str(&format!("{:>6}\t{shown}\n", start + i + 1));
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

fn list_directory(sandbox: &Sandbox, path: &Path) -> Result<String> {
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
    if entries.is_empty() {
        return Ok("(empty folder)".to_string());
    }
    entries.sort();
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

fn find_files(sandbox: &Sandbox, matcher: &GlobMatcher, match_names: bool, base: &Path) -> String {
    let mut found: Vec<String> = workspace_files(sandbox, base)
        .filter(|path| {
            let rel = sandbox.display(path);
            matcher.is_match(&rel)
                || (match_names && path.file_name().is_some_and(|n| matcher.is_match(n)))
        })
        .map(|path| sandbox.display(&path))
        .collect();
    if found.is_empty() {
        return "(no matching files)".to_string();
    }
    found.sort();
    let total = found.len();
    found.truncate(MAX_FIND_RESULTS);
    let mut out = found.join("\n");
    if total > MAX_FIND_RESULTS {
        out.push_str(&format!("\n… {} more files", total - MAX_FIND_RESULTS));
    }
    out
}

fn search_files(
    sandbox: &Sandbox,
    regex: &Regex,
    file_glob: Option<&GlobMatcher>,
    base: &Path,
) -> String {
    let mut matches = Vec::new();
    let mut more = false;
    for path in workspace_files(sandbox, base) {
        let rel = sandbox.display(&path);
        if let Some(glob) = file_glob {
            let name_matches = path.file_name().is_some_and(|n| glob.is_match(n));
            if !glob.is_match(&rel) && !name_matches {
                continue;
            }
        }
        if path.metadata().map(|m| m.len()).unwrap_or(u64::MAX) > MAX_SEARCH_FILE_BYTES {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        if looks_binary(&bytes) {
            continue;
        }
        for (number, line) in String::from_utf8_lossy(&bytes).lines().enumerate() {
            if !regex.is_match(line) {
                continue;
            }
            if matches.len() == MAX_SEARCH_MATCHES {
                more = true;
                break;
            }
            let shown: String = line.trim().chars().take(MAX_SEARCH_LINE_CHARS).collect();
            matches.push(format!("{rel}:{}: {shown}", number + 1));
        }
        if more {
            break;
        }
    }
    if matches.is_empty() {
        return "(no matches)".to_string();
    }
    let mut out = matches.join("\n");
    if more {
        out.push_str(&format!(
            "\n… stopped at {MAX_SEARCH_MATCHES} matches; narrow the search"
        ));
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

async fn run_command(root: &Path, command: &str, timeout: Duration) -> ToolOutput {
    let mut shell = if cfg!(windows) {
        let mut c = tokio::process::Command::new("cmd");
        c.args(["/C", command]);
        c
    } else {
        let mut c = tokio::process::Command::new("sh");
        c.args(["-c", command]);
        c
    };
    shell
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let output = match tokio::time::timeout(timeout, shell.output()).await {
        Err(_) => {
            return ToolOutput::error(format!(
                "the command timed out after {}s and was stopped",
                timeout.as_secs()
            ))
        }
        Ok(Err(err)) => return ToolOutput::error(format!("cannot start the command: {err}")),
        Ok(Ok(output)) => output,
    };
    let code = output
        .status
        .code()
        .map(|c| c.to_string())
        .unwrap_or_else(|| "signal".to_string());
    let mut text = format!("exit code {code}\n");
    text.push_str(&String::from_utf8_lossy(&output.stdout));
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.trim().is_empty() {
        text.push_str("\n[stderr]\n");
        text.push_str(&stderr);
    }
    if output.status.success() {
        ToolOutput::ok(text)
    } else {
        ToolOutput::error(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert_eq!(
            run(&sandbox, &found).await.output,
            "src/main.rs\nsrc/nested/lib.rs"
        );

        let search = plan_ok(&sandbox, ToolName::SearchFiles, json!({"pattern": "TODO"}));
        assert_eq!(
            run(&sandbox, &search).await.output,
            "src/nested/lib.rs:1: // TODO: fix"
        );

        let scoped = plan_ok(
            &sandbox,
            ToolName::SearchFiles,
            json!({"pattern": "fn", "glob": "main.rs"}),
        );
        assert_eq!(
            run(&sandbox, &scoped).await.output,
            "src/main.rs:1: fn main() {}"
        );
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
        assert_eq!(apply_edit("a a", "a", "b", true).unwrap(), "b b");
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

    #[test]
    fn caps_long_output() {
        let out = ToolOutput::ok("x".repeat(MAX_OUTPUT_CHARS + 10));
        assert!(out.truncated);
        assert!(out.output.chars().count() <= MAX_OUTPUT_CHARS);
    }

    #[test]
    fn catalog_names_round_trip() {
        for tool in ToolName::ALL {
            assert_eq!(ToolName::parse(tool.as_str()), Some(tool));
        }
        assert_eq!(ToolName::parse("rm_rf"), None);
    }
}
