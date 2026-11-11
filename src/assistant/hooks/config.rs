//! `hooks.toml`: the user's own commands for `before_tool`, `after_tool`
//! and `turn_end`, each optionally narrowed to some tools and a path glob.
//!
//! ```toml
//! [[after_tool]]
//! tools = ["edit_file", "write_file"]
//! path = "*.rs"
//! command = "rustfmt \"$GZ_HOOK_PATH\""
//! timeout = 20
//! ```

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use globset::{GlobBuilder, GlobMatcher};
use serde::Deserialize;

use crate::assistant::mcp_local::LocalMcpTool;
use crate::assistant::tools::ToolName;

/// How long a hook may run when its entry sets no `timeout`.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// The longest `timeout` an entry may ask for: a hook holds up the turn.
const MAX_TIMEOUT_SECS: u64 = 600;

/// When a hook runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookEvent {
    BeforeTool,
    AfterTool,
    TurnEnd,
}

impl HookEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            HookEvent::BeforeTool => "before_tool",
            HookEvent::AfterTool => "after_tool",
            HookEvent::TurnEnd => "turn_end",
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
    path: Option<PathGlob>,
    pub timeout: Duration,
}

#[derive(Debug, Clone)]
struct PathGlob {
    pattern: String,
    matcher: GlobMatcher,
}

impl Hook {
    /// Whether this hook runs for a call to `tool` that touches `path`
    /// (relative patterns are taken from `cwd`). A hook with a path glob
    /// never runs for a call without a path.
    pub fn matches(&self, tool: &str, path: Option<&Path>, cwd: &Path) -> bool {
        if !self.tools.is_empty() && !self.tools.iter().any(|t| t == tool) {
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
        if self.id.event == HookEvent::TurnEnd {
            return "each turn".to_string();
        }
        let tools = match self.tools.is_empty() {
            true => "every tool".to_string(),
            false => self.tools.join(","),
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
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HookSpec {
    command: String,
    #[serde(default)]
    tools: Vec<String>,
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
    if id.event == HookEvent::TurnEnd && (!spec.tools.is_empty() || spec.path.is_some()) {
        bail!("turn_end hooks run once per turn, so they take no tools or path");
    }
    let is_known = |name: &String| {
        ToolName::parse(name).is_some() || LocalMcpTool::parse(name) == Some(LocalMcpTool::Call)
    };
    if let Some(unknown) = spec.tools.iter().find(|t| !is_known(t)) {
        let known: Vec<&str> = ToolName::ALL
            .into_iter()
            .chain(ToolName::EXTRA)
            .map(ToolName::as_str)
            .chain([LocalMcpTool::Call.as_str()])
            .collect();
        bail!("unknown tool '{unknown}' (tools are {})", known.join(", "));
    }
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
        path,
        timeout,
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
}
