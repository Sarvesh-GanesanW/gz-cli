//! `hooks.toml`: the user's own commands, one `[[event]]` list per event
//! (see [`HookEvent`]), each optionally narrowed by a `matcher` (a tool
//! name, `a|b`, or a regular expression), a `tools` list and a path glob.
//!
//! ```toml
//! [[post_tool_use]]
//! matcher = "edit_file|write_file"
//! path = "*.rs"
//! command = "rustfmt \"$GZ_HOOK_PATH\""
//! timeout = 20
//! ```

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use globset::{GlobBuilder, GlobMatcher};
use regex::Regex;
use serde::Deserialize;

use crate::assistant::local_context::ContextTool;
use crate::assistant::mcp_local::LocalMcpTool;
use crate::assistant::session_tools::SessionTool;
use crate::assistant::tools::ToolName;

/// How long a hook may run when its entry sets no `timeout`.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// The longest `timeout` an entry may ask for: a hook holds up the turn.
const MAX_TIMEOUT_SECS: u64 = 600;

/// When a hook runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookEvent {
    /// Before an allowed tool call runs (after the permission decision).
    BeforeTool,
    /// After a tool call, whether it worked or not.
    AfterTool,
    /// When the main agent's turn completed (report only).
    TurnEnd,
    /// The first message of a session: at startup, after `--continue` or
    /// `/resume`, or after `/clear`.
    SessionStart,
    /// gz code exits or `/clear` leaves the conversation.
    SessionEnd,
    /// The user sent a message, before it reaches the model.
    UserPromptSubmit,
    /// A tool call, before the permission check; may allow, deny or ask.
    PreToolUse,
    /// A tool call that worked.
    PostToolUse,
    /// A tool call that failed.
    PostToolUseFailure,
    /// The main agent finished its answer; may make it carry on.
    Stop,
    /// A subagent finished; may make a background subagent carry on.
    SubagentStop,
    /// Before the conversation is compacted.
    PreCompact,
    /// gz needs the user: a permission prompt or a question.
    Notification,
}

impl HookEvent {
    /// The first three events' hooks run one by one, in file order, as
    /// they always have; the rest run at once.
    pub fn runs_in_order(self) -> bool {
        matches!(
            self,
            HookEvent::BeforeTool | HookEvent::AfterTool | HookEvent::TurnEnd
        )
    }

    /// Every event, in the order `/hooks` lists them.
    pub const ALL: [HookEvent; 13] = [
        HookEvent::SessionStart,
        HookEvent::UserPromptSubmit,
        HookEvent::PreToolUse,
        HookEvent::BeforeTool,
        HookEvent::PostToolUse,
        HookEvent::PostToolUseFailure,
        HookEvent::AfterTool,
        HookEvent::Notification,
        HookEvent::SubagentStop,
        HookEvent::Stop,
        HookEvent::TurnEnd,
        HookEvent::PreCompact,
        HookEvent::SessionEnd,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            HookEvent::BeforeTool => "before_tool",
            HookEvent::AfterTool => "after_tool",
            HookEvent::TurnEnd => "turn_end",
            HookEvent::SessionStart => "session_start",
            HookEvent::SessionEnd => "session_end",
            HookEvent::UserPromptSubmit => "user_prompt_submit",
            HookEvent::PreToolUse => "pre_tool_use",
            HookEvent::PostToolUse => "post_tool_use",
            HookEvent::PostToolUseFailure => "post_tool_use_failure",
            HookEvent::Stop => "stop",
            HookEvent::SubagentStop => "subagent_stop",
            HookEvent::PreCompact => "pre_compact",
            HookEvent::Notification => "notification",
        }
    }

    /// Whether its hooks run per tool call (and take tools and a path).
    pub fn is_per_tool(self) -> bool {
        matches!(
            self,
            HookEvent::BeforeTool
                | HookEvent::AfterTool
                | HookEvent::PreToolUse
                | HookEvent::PostToolUse
                | HookEvent::PostToolUseFailure
        )
    }

    /// What a `matcher` is matched against, for events that take one.
    pub fn matcher_subject(self) -> Option<&'static str> {
        match self {
            HookEvent::BeforeTool
            | HookEvent::AfterTool
            | HookEvent::PreToolUse
            | HookEvent::PostToolUse
            | HookEvent::PostToolUseFailure => Some("the tool name"),
            HookEvent::SessionStart => Some("how it started: startup, resume or clear"),
            HookEvent::SessionEnd => Some("why it ended: exit or clear"),
            HookEvent::PreCompact => Some("what started it: manual or auto"),
            HookEvent::Notification => Some("its kind: permission or question"),
            HookEvent::SubagentStop => Some("the subagent's name"),
            HookEvent::TurnEnd | HookEvent::UserPromptSubmit | HookEvent::Stop => None,
        }
    }
}

/// Which file a hook came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    /// `<gz config folder>/hooks.toml`.
    User,
    /// `<open folder>/.gz/hooks.toml`; runs only once trusted.
    Project,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::User => "user",
            Source::Project => "project",
        }
    }
}

/// Names one hook across reloads: its file, event and place in that list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HookId {
    pub source: Source,
    pub event: HookEvent,
    /// 1-based, in file order.
    pub number: usize,
}

/// One loaded hook.
#[derive(Debug, Clone)]
pub struct Hook {
    pub id: HookId,
    pub command: String,
    /// Tool names it runs for; empty means every tool.
    pub tools: Vec<String>,
    matcher: Option<Matcher>,
    path: Option<PathGlob>,
    pub timeout: Duration,
}

/// A `matcher`: the whole subject must match (`edit_file`, `a|b`, `mcp_.*`).
#[derive(Debug, Clone)]
struct Matcher {
    pattern: String,
    regex: Regex,
}

impl Matcher {
    fn is_match(&self, subject: &str) -> bool {
        self.regex.is_match(subject)
    }
}

#[derive(Debug, Clone)]
struct PathGlob {
    pattern: String,
    matcher: GlobMatcher,
}

impl Hook {
    /// Whether this hook runs for an event about `subject` (the tool name,
    /// session source, subagent name, …).
    pub fn matches_subject(&self, subject: &str) -> bool {
        self.matcher.as_ref().is_none_or(|m| m.is_match(subject))
    }

    /// Whether this hook runs for a call to `tool` that touches `path`
    /// (relative patterns are taken from `cwd`). A hook with a path glob
    /// never runs for a call without a path.
    pub fn matches(&self, tool: &str, path: Option<&Path>, cwd: &Path) -> bool {
        if !self.tools.is_empty() && !self.tools.iter().any(|t| t == tool) {
            return false;
        }
        if !self.matches_subject(tool) {
            return false;
        }
        let Some(glob) = &self.path else {
            return true;
        };
        let Some(path) = path else {
            return false;
        };
        // `*.rs` means any Rust file, wherever it is; a pattern with a
        // slash is a path from the current folder.
        if !glob.pattern.contains('/') {
            return path
                .file_name()
                .is_some_and(|name| glob.matcher.is_match(name));
        }
        glob.matcher
            .is_match(path.strip_prefix(cwd).unwrap_or(path))
    }

    /// What it runs for: `edit_file,write_file *.rs`, or `every tool`.
    fn scope(&self) -> String {
        if !self.id.event.is_per_tool() {
            return match &self.matcher {
                Some(matcher) => matcher.pattern.clone(),
                None if matches!(self.id.event, HookEvent::TurnEnd | HookEvent::Stop) => {
                    "each turn".to_string()
                }
                None => "always".to_string(),
            };
        }
        let mut narrowed: Vec<String> = Vec::new();
        if !self.tools.is_empty() {
            narrowed.push(self.tools.join(","));
        }
        if let Some(matcher) = &self.matcher {
            narrowed.push(matcher.pattern.clone());
        }
        let tools = match narrowed.is_empty() {
            true => "every tool".to_string(),
            false => narrowed.join(" "),
        };
        match &self.path {
            Some(glob) => format!("{tools} {}", glob.pattern),
            None => tools,
        }
    }

    /// One line for `/hooks`: `edit_file *.rs → cargo fmt (30s)`, a
    /// multi-line command joined with `↵`.
    pub fn action(&self) -> String {
        let command: Vec<&str> = self
            .command
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();
        format!(
            "{} → {} ({}s)",
            self.scope(),
            command.join(" ↵ "),
            self.timeout.as_secs()
        )
    }

    /// For the trust prompt: a heading line, then the command exactly as
    /// written, indented, so nothing in it is hidden.
    pub fn describe(&self) -> Vec<String> {
        let heading = format!(
            "{} {} ({}s):",
            self.id.event.as_str(),
            self.scope(),
            self.timeout.as_secs()
        );
        std::iter::once(heading)
            .chain(
                self.command
                    .trim_end()
                    .lines()
                    .map(|line| format!("  {line}")),
            )
            .collect()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileSpec {
    #[serde(default)]
    before_tool: Vec<HookSpec>,
    #[serde(default)]
    after_tool: Vec<HookSpec>,
    #[serde(default)]
    turn_end: Vec<HookSpec>,
    #[serde(default)]
    session_start: Vec<HookSpec>,
    #[serde(default)]
    session_end: Vec<HookSpec>,
    #[serde(default)]
    user_prompt_submit: Vec<HookSpec>,
    #[serde(default)]
    pre_tool_use: Vec<HookSpec>,
    #[serde(default)]
    post_tool_use: Vec<HookSpec>,
    #[serde(default)]
    post_tool_use_failure: Vec<HookSpec>,
    #[serde(default)]
    stop: Vec<HookSpec>,
    #[serde(default)]
    subagent_stop: Vec<HookSpec>,
    #[serde(default)]
    pre_compact: Vec<HookSpec>,
    #[serde(default)]
    notification: Vec<HookSpec>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HookSpec {
    command: String,
    #[serde(default)]
    tools: Vec<String>,
    matcher: Option<String>,
    path: Option<String>,
    /// Seconds.
    timeout: Option<u64>,
}

/// Every hook in a `hooks.toml`'s text, in file order per event.
pub fn parse(text: &str, source: Source) -> Result<Vec<Hook>> {
    let spec: FileSpec = toml::from_str(text)?;
    let lists = [
        (HookEvent::BeforeTool, spec.before_tool),
        (HookEvent::AfterTool, spec.after_tool),
        (HookEvent::TurnEnd, spec.turn_end),
        (HookEvent::SessionStart, spec.session_start),
        (HookEvent::SessionEnd, spec.session_end),
        (HookEvent::UserPromptSubmit, spec.user_prompt_submit),
        (HookEvent::PreToolUse, spec.pre_tool_use),
        (HookEvent::PostToolUse, spec.post_tool_use),
        (HookEvent::PostToolUseFailure, spec.post_tool_use_failure),
        (HookEvent::Stop, spec.stop),
        (HookEvent::SubagentStop, spec.subagent_stop),
        (HookEvent::PreCompact, spec.pre_compact),
        (HookEvent::Notification, spec.notification),
    ];
    let mut hooks = Vec::new();
    for (event, specs) in lists {
        for (i, spec) in specs.into_iter().enumerate() {
            let id = HookId {
                source,
                event,
                number: i + 1,
            };
            let hook = build(id, spec)
                .with_context(|| format!("[[{}]] entry {}", event.as_str(), i + 1))?;
            hooks.push(hook);
        }
    }
    Ok(hooks)
}

fn build(id: HookId, spec: HookSpec) -> Result<Hook> {
    if spec.command.trim().is_empty() {
        bail!("command is empty");
    }
    let event = id.event;
    if !event.is_per_tool() && (!spec.tools.is_empty() || spec.path.is_some()) {
        bail!(
            "{} hooks are not about one tool call, so they take no tools or path",
            event.as_str()
        );
    }
    if spec.matcher.is_some() && event.matcher_subject().is_none() {
        bail!("{} hooks take no matcher", event.as_str());
    }
    if let Some(unknown) = spec.tools.iter().find(|t| !is_known_tool(t, event)) {
        bail!(
            "unknown tool '{unknown}' (tools are {})",
            known_tools(event)
        );
    }
    let matcher = spec
        .matcher
        .as_deref()
        .filter(|pattern| !matches!(pattern.trim(), "" | "*"))
        .map(|pattern| matcher(pattern, event))
        .transpose()?;
    let timeout = match spec.timeout {
        None => DEFAULT_TIMEOUT,
        Some(secs @ 1..=MAX_TIMEOUT_SECS) => Duration::from_secs(secs),
        Some(secs) => bail!("timeout {secs} is out of range (1 to {MAX_TIMEOUT_SECS} seconds)"),
    };
    let path = spec.path.map(|pattern| path_glob(&pattern)).transpose()?;
    Ok(Hook {
        id,
        command: spec.command,
        tools: spec.tools,
        matcher,
        path,
        timeout,
    })
}

/// Whether `event` hooks can run for the tool `name`. The tools answered
/// outside the usual permission path (folders, memory, skills, the
/// session's own tools) have only the newer events, never `before_tool`
/// or `after_tool`.
fn is_known_tool(name: &str, event: HookEvent) -> bool {
    let is_side_tool = ContextTool::parse(name).is_some() || SessionTool::parse(name).is_some();
    ToolName::parse(name).is_some()
        || LocalMcpTool::parse(name) == Some(LocalMcpTool::Call)
        || (is_side_tool && !event.runs_in_order())
}

fn known_tools(event: HookEvent) -> String {
    let side_tools = ContextTool::ALL
        .map(ContextTool::as_str)
        .into_iter()
        .chain(SessionTool::ALL.map(SessionTool::as_str))
        .filter(|_| !event.runs_in_order());
    let known: Vec<&str> = ToolName::ALL
        .into_iter()
        .chain(ToolName::EXTRA)
        .map(ToolName::as_str)
        .chain([LocalMcpTool::Call.as_str()])
        .chain(side_tools)
        .collect();
    known.join(", ")
}

/// Compiles `pattern` to match a whole subject. For tool events, a plain
/// `a|b` list must name real tools, so a typo fails loudly instead of the
/// hook never running.
fn matcher(pattern: &str, event: HookEvent) -> Result<Matcher> {
    let pattern = pattern.trim();
    let is_plain_list = pattern.split('|').all(|part| {
        !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    });
    if event.is_per_tool() && is_plain_list {
        if let Some(unknown) = pattern.split('|').find(|t| !is_known_tool(t, event)) {
            bail!(
                "matcher names an unknown tool '{unknown}' (tools are {})",
                known_tools(event)
            );
        }
    }
    let regex = Regex::new(&format!("^(?:{pattern})$"))
        .with_context(|| format!("matcher '{pattern}' is not a valid regular expression"))?;
    Ok(Matcher {
        pattern: pattern.to_string(),
        regex,
    })
}

fn path_glob(pattern: &str) -> Result<PathGlob> {
    let matcher = GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .with_context(|| format!("path '{pattern}' is not a valid glob"))?
        .compile_matcher();
    Ok(PathGlob {
        pattern: pattern.to_string(),
        matcher,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(text: &str) -> Hook {
        parse(text, Source::User).unwrap().remove(0)
    }

    #[test]
    fn tools_and_globs_narrow_what_a_hook_runs_for() {
        let cwd = Path::new("/work/app");
        let rust = one("[[after_tool]]\ntools = [\"edit_file\"]\npath = \"*.rs\"\ncommand = \"x\"");
        let src_rs = Path::new("/work/app/src/main.rs");
        assert!(rust.matches("edit_file", Some(src_rs), cwd));
        assert!(!rust.matches("write_file", Some(src_rs), cwd), "other tool");
        assert!(
            !rust.matches("edit_file", Some(Path::new("/work/app/a.py")), cwd),
            "other file type"
        );
        assert!(!rust.matches("edit_file", None, cwd), "no path, no match");

        let nested = one("[[before_tool]]\npath = \"src/**/*.rs\"\ncommand = \"x\"");
        assert!(nested.matches("read_file", Some(src_rs), cwd));
        assert!(!nested.matches("read_file", Some(Path::new("/work/app/main.rs")), cwd));

        let every = one("[[before_tool]]\ncommand = \"x\"");
        assert!(every.matches("run_command", None, cwd));
        assert_eq!(every.timeout, DEFAULT_TIMEOUT);
    }

    #[test]
    fn mistakes_name_the_entry_and_the_problem() {
        let err = |text: &str| format!("{:#}", parse(text, Source::User).unwrap_err());
        assert!(err("[[before_tool]]\ntools = [\"edit\"]\ncommand = \"x\"")
            .contains("[[before_tool]] entry 1: unknown tool 'edit'"));
        assert!(err("[[turn_end]]\npath = \"*\"\ncommand = \"x\"").contains("no tools or path"));
        assert!(err("[[after_tool]]\ncommand = \"x\"\ntimeout = 0").contains("out of range"));
        assert!(err("[[after_tool]]\ncommand = \"x\"\npaths = \"*\"").contains("unknown field"));
        assert!(err("[[after_tool]]\ncommand = \"  \"").contains("command is empty"));
    }

    #[test]
    fn matchers_take_a_name_a_list_or_a_regex() {
        let cwd = Path::new("/w");
        let list = one("[[pre_tool_use]]\nmatcher = \"edit_file|write_file\"\ncommand = \"x\"");
        assert!(list.matches("write_file", None, cwd));
        assert!(!list.matches("read_file", None, cwd));
        let regex = one("[[post_tool_use]]\nmatcher = \"local_mcp_.*\"\ncommand = \"x\"");
        assert!(regex.matches("local_mcp_call", None, cwd));
        assert!(!regex.matches("read_file", None, cwd), "whole name only");
        let star = one("[[post_tool_use]]\nmatcher = \"*\"\ncommand = \"x\"");
        assert!(star.matches("run_command", None, cwd));
        let source = one("[[session_start]]\nmatcher = \"resume\"\ncommand = \"x\"");
        assert!(source.matches_subject("resume"));
        assert!(!source.matches_subject("startup"));
    }

    #[test]
    fn every_event_parses_and_bad_matchers_are_named() {
        for event in HookEvent::ALL {
            let hook = one(&format!("[[{}]]\ncommand = \"x\"", event.as_str()));
            assert_eq!(hook.id.event, event);
        }
        let err = |text: &str| format!("{:#}", parse(text, Source::User).unwrap_err());
        assert!(
            err("[[pre_tool_use]]\nmatcher = \"edit|write_file\"\ncommand = \"x\"")
                .contains("matcher names an unknown tool 'edit'")
        );
        assert!(err("[[pre_tool_use]]\nmatcher = \"(\"\ncommand = \"x\"")
            .contains("not a valid regular expression"));
        let side =
            one("[[pre_tool_use]]\nmatcher = \"change_directory|todo_write\"\ncommand = \"x\"");
        assert!(side.matches("change_directory", None, Path::new("/")));
        assert!(
            err("[[before_tool]]\nmatcher = \"change_directory\"\ncommand = \"x\"")
                .contains("matcher names an unknown tool 'change_directory'"),
            "before_tool never runs for it, so naming it is a mistake"
        );
        assert!(err("[[stop]]\nmatcher = \"x\"\ncommand = \"x\"")
            .contains("stop hooks take no matcher"));
        assert!(
            err("[[session_start]]\ntools = [\"read_file\"]\ncommand = \"x\"")
                .contains("take no tools or path")
        );
    }
}
