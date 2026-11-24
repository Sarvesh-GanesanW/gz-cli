//! Per-run controls a front end sets on a [`Session`](super::Session) for
//! scripted runs (`gz code -p`): which client tools the model is offered,
//! extra user instructions, `--bare`, and a [`StepGuard`] that may stop the
//! run before its next model turn (`--max-turns`, `--max-budget-usd`).
//!
//! The session calls into this module from a few single lines; the logic
//! lives here so the session loop stays as it is.

use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};

use super::local_context::Limits;

/// Every client tool name the chat server accepts in `clientTools`
/// (gz-chat `agent/client_tools.py:75-97`, `ClientToolName`); `--tools`
/// names must come from this list.
pub const SERVER_TOOL_NAMES: [&str; 21] = [
    "read_file",
    "list_directory",
    "find_files",
    "search_files",
    "edit_file",
    "write_file",
    "run_command",
    "run_script",
    "change_directory",
    "load_skill",
    "memory",
    "command_output",
    "stop_command",
    "notebook_edit",
    "web_fetch",
    "ask_user_question",
    "enter_plan_mode",
    "exit_plan_mode",
    "todo_write",
    "local_mcp_tools",
    "local_mcp_call",
];

/// Tools `--bare` never offers: they read the user's skills, memories and
/// MCP servers.
const BARE_LEFT_OUT: [&str; 4] = ["load_skill", "memory", "local_mcp_tools", "local_mcp_call"];

/// Longest instruction text the server takes in one entry
/// (gz-chat `agent/client_context.py:38`, `INSTRUCTION_FILE_MAX_CHARS`).
pub const INSTRUCTION_MAX_CHARS: usize = 20_000;
/// Longest `path` the server takes (gz-chat `agent/client_context.py:46`).
const PATH_MAX_CHARS: usize = 512;

/// Decides, before each model turn of a run, whether it may go ahead.
pub trait StepGuard: Send + Sync {
    /// Called before the run starts and before every resume; `Some(why)`
    /// ends the turn as failed with `why` instead of continuing.
    fn before_leg(&self) -> Option<String>;
    /// A client tool call the model asked for, with its input, before gz
    /// decides whether to run it.
    fn tool_requested(&self, _tool: &str, _input: &Value) {}
}

/// One extra user-scope instruction entry (`--append-system-prompt`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtraInstruction {
    /// What the model sees as the entry's path: the file's, or the flag's
    /// name for inline text.
    pub path: String,
    pub text: String,
}

impl ExtraInstruction {
    /// Checks the entry against the server's limits, so a run fails before
    /// it starts rather than with a 422.
    pub fn new(path: &str, text: &str) -> Result<Self> {
        if text.trim().is_empty() {
            anyhow::bail!("the appended instructions from {path} are empty; pass some text");
        }
        let chars = text.chars().count();
        if chars > INSTRUCTION_MAX_CHARS {
            anyhow::bail!(
                "the appended instructions from {path} are {chars} characters; the chat takes at \
                 most {INSTRUCTION_MAX_CHARS} per entry, so shorten them or split them across \
                 --append-system-prompt-file flags"
            );
        }
        let path = sendable_path(path);
        Ok(Self {
            path,
            text: text.to_string(),
        })
    }
}

/// A display path the server accepts: no control characters, no `..`
/// segment, at most 512 characters.
fn sendable_path(path: &str) -> String {
    let cleaned: String = path.chars().filter(|c| !c.is_control()).collect();
    let segments: Vec<&str> = cleaned
        .split(['/', '\\'])
        .filter(|segment| *segment != "..")
        .collect();
    let joined = segments.join("/");
    let shortened: String = joined.chars().take(PATH_MAX_CHARS).collect();
    if shortened.trim().is_empty() {
        "--append-system-prompt".to_string()
    } else {
        shortened
    }
}

/// The controls; the default changes nothing.
#[derive(Clone, Default)]
pub struct RunControls {
    /// `--tools`: the only client tools offered; `None` offers all.
    pub tools: Option<Vec<String>>,
    /// Sent first in `clientContext.instructions`, as the user's own.
    pub extra_instructions: Vec<ExtraInstruction>,
    /// `--bare`: no instruction files, memories, skills or MCP tools.
    pub bare: bool,
    pub guard: Option<Arc<dyn StepGuard>>,
}

impl RunControls {
    /// Whether the model may be offered (and gz may run) `tool`.
    pub fn allows(&self, tool: &str) -> bool {
        if self.bare && BARE_LEFT_OUT.contains(&tool) {
            return false;
        }
        match &self.tools {
            Some(allowed) => allowed.iter().any(|name| name == tool),
            None => true,
        }
    }

    /// Drops the tools these controls leave out.
    pub fn filter_tools(&self, names: &mut Vec<&'static str>) {
        names.retain(|name| self.allows(name));
    }

    /// Fits the start body to the controls: `--bare` strips the found
    /// instructions, memories and skills; extra instructions go first; and
    /// a run with no client tools sends no `clientContext`, which the
    /// server only takes with tools (gz-chat
    /// `api/routes/chat/request_models.py:315-316`).
    /// `limits` are the server's (`None` keeps the contract's).
    pub fn apply_to_body(&self, body: &mut Value, limits: Option<Limits>) {
        let has_tools = body["clientTools"]
            .as_array()
            .is_some_and(|tools| !tools.is_empty());
        if !has_tools {
            if let Some(body) = body.as_object_mut() {
                body.remove("clientContext");
            }
            return;
        }
        let Some(context) = body.get_mut("clientContext").and_then(Value::as_object_mut) else {
            return;
        };
        if self.bare {
            context.remove("instructions");
            context.remove("memories");
            context.remove("skills");
        }
        if self.extra_instructions.is_empty() {
            return;
        }
        let found = context
            .remove("instructions")
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default();
        let extra = self
            .extra_instructions
            .iter()
            .map(|entry| json!({"source": "user", "path": entry.path, "text": entry.text}));
        // The extra entries were asked for on this run, so they win a place
        // over the farthest instruction files when there are too many.
        let most = limits.unwrap_or_default().instruction_files;
        let instructions: Vec<Value> = extra.chain(found).take(most).collect();
        context.insert("instructions".into(), Value::Array(instructions));
    }

    /// See [`StepGuard::before_leg`].
    pub fn before_leg(&self) -> Option<String> {
        self.guard.as_ref()?.before_leg()
    }

    /// See [`StepGuard::tool_requested`].
    pub fn tool_requested(&self, tool: &str, input: &Value) {
        if let Some(guard) = &self.guard {
            guard.tool_requested(tool, input);
        }
    }

    /// Refuses extra instructions the server would not take, rather than
    /// dropping some or failing the run with a 422.
    pub fn check_limits(&self, limits: &Limits) -> Result<()> {
        let count = self.extra_instructions.len();
        if count > limits.instruction_files {
            anyhow::bail!(
                "{count} appended instruction entries were given, but the chat takes at most {} \
                 instruction entries; drop --append-system-prompt-file flags or join the files \
                 into one",
                limits.instruction_files
            );
        }
        for entry in &self.extra_instructions {
            let chars = entry.text.chars().count();
            if chars > limits.instruction_chars {
                anyhow::bail!(
                    "the appended instructions from {} are {chars} characters; the chat takes at \
                     most {} per entry, so shorten them or split them across \
                     --append-system-prompt-file flags",
                    entry.path,
                    limits.instruction_chars
                );
            }
        }
        Ok(())
    }

    /// Whether any extra instructions need the server's local context.
    pub fn needs_local_context(&self) -> bool {
        !self.extra_instructions.is_empty()
    }
}

/// Parses `--tools`: names separated by commas or spaces; `""` or `none`
/// offers no client tools, `all` or `default` every one.
pub fn parse_tools(text: &str) -> Result<Option<Vec<String>>> {
    let trimmed = text.trim();
    match trimmed {
        "all" | "default" => return Ok(None),
        "" | "none" => return Ok(Some(Vec::new())),
        _ => {}
    }
    let mut names: Vec<String> = Vec::new();
    for name in trimmed
        .split([',', ' '])
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        if !SERVER_TOOL_NAMES.contains(&name) {
            anyhow::bail!(
                "--tools: unknown tool '{name}'; use names from: {}, or `none`/`all`",
                SERVER_TOOL_NAMES.join(", ")
            );
        }
        if !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
    }
    Ok(Some(names))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_parse_lists_and_keywords() {
        assert_eq!(parse_tools("all").unwrap(), None);
        assert_eq!(parse_tools(" none ").unwrap(), Some(vec![]));
        assert_eq!(parse_tools("").unwrap(), Some(vec![]));
        assert_eq!(
            parse_tools("read_file, search_files read_file").unwrap(),
            Some(vec!["read_file".to_string(), "search_files".to_string()])
        );
        let err = parse_tools("read_file,rm").unwrap_err().to_string();
        assert!(err.contains("unknown tool 'rm'") && err.contains("run_command"));
    }

    #[test]
    fn filtering_keeps_only_allowed_tools_and_bare_drops_personal_ones() {
        let controls = RunControls {
            tools: Some(vec!["read_file".into(), "memory".into()]),
            ..Default::default()
        };
        let mut names = vec!["read_file", "run_command", "memory"];
        controls.filter_tools(&mut names);
        assert_eq!(names, ["read_file", "memory"]);
        let bare = RunControls {
            bare: true,
            ..controls
        };
        let mut names = vec!["read_file", "memory", "load_skill", "local_mcp_call"];
        bare.filter_tools(&mut names);
        assert_eq!(names, ["read_file"]);
    }

    #[test]
    fn extra_instructions_go_first_and_bare_strips_found_context() {
        let controls = RunControls {
            extra_instructions: vec![
                ExtraInstruction::new("--append-system-prompt", "Be brief.").unwrap()
            ],
            bare: true,
            ..Default::default()
        };
        let mut body = json!({
            "clientTools": ["read_file"],
            "clientContext": {
                "cwd": "~/p",
                "instructions": [{"source": "project", "path": "~/p/GZ.md", "text": "x"}],
                "memories": [{"id": "m", "text": "y"}],
                "skills": [{"name": "s", "description": "d"}],
            }
        });
        controls.apply_to_body(&mut body, None);
        assert_eq!(
            body["clientContext"],
            json!({"cwd": "~/p", "instructions": [
                {"source": "user", "path": "--append-system-prompt", "text": "Be brief."}
            ]})
        );
    }

    #[test]
    fn extra_instructions_take_the_place_of_the_farthest_files() {
        let controls = RunControls {
            extra_instructions: vec![ExtraInstruction::new("a.md", "A").unwrap()],
            ..Default::default()
        };
        let found: Vec<Value> = (0..8)
            .map(|i| json!({"source": "project", "path": format!("f{i}"), "text": "t"}))
            .collect();
        let mut body =
            json!({"clientTools": ["read_file"], "clientContext": {"instructions": found}});
        controls.apply_to_body(&mut body, None);
        let sent = body["clientContext"]["instructions"].as_array().unwrap();
        assert_eq!(sent.len(), 8);
        assert_eq!(sent[0]["path"], "a.md");
        assert_eq!(sent[7]["path"], "f6");
    }

    #[test]
    fn extra_instructions_fit_the_servers_own_entry_limit() {
        let controls = RunControls {
            extra_instructions: vec![ExtraInstruction::new("a.md", "A").unwrap()],
            ..Default::default()
        };
        let found: Vec<Value> = (0..4)
            .map(|i| json!({"source": "project", "path": format!("f{i}"), "text": "t"}))
            .collect();
        let mut body =
            json!({"clientTools": ["read_file"], "clientContext": {"instructions": found}});
        let limits = Limits {
            instruction_files: 3,
            ..Limits::default()
        };
        controls.apply_to_body(&mut body, Some(limits));
        let sent = body["clientContext"]["instructions"].as_array().unwrap();
        assert_eq!(sent.len(), 3);
        assert_eq!(sent[0]["path"], "a.md");
    }

    #[test]
    fn too_many_or_too_long_extra_instructions_are_refused() {
        let entries = |n: usize, text: &str| RunControls {
            extra_instructions: (0..n)
                .map(|i| ExtraInstruction::new(&format!("r{i}.md"), text).unwrap())
                .collect(),
            ..Default::default()
        };
        let contract = Limits::default();
        assert!(entries(8, "t").check_limits(&contract).is_ok());
        let err = entries(9, "t")
            .check_limits(&contract)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("9 appended") && err.contains("at most 8"),
            "{err}"
        );
        let small = Limits {
            instruction_chars: 10,
            ..Limits::default()
        };
        let err = entries(1, "eleven char")
            .check_limits(&small)
            .unwrap_err()
            .to_string();
        assert!(err.contains("r0.md") && err.contains("at most 10"), "{err}");
    }

    #[test]
    fn no_tools_means_no_client_context() {
        let mut body = json!({"clientTools": [], "clientContext": {"cwd": "~"}});
        RunControls::default().apply_to_body(&mut body, None);
        assert!(body.get("clientContext").is_none());
    }

    #[test]
    fn instructions_are_checked_against_the_server_limits() {
        assert!(ExtraInstruction::new("x", "  ").is_err());
        let long = "a".repeat(INSTRUCTION_MAX_CHARS + 1);
        let err = ExtraInstruction::new("x", &long).unwrap_err().to_string();
        assert!(err.contains("at most 20000"), "{err}");
        let entry = ExtraInstruction::new("../secret/../notes.md", "t").unwrap();
        assert_eq!(entry.path, "secret/notes.md");
    }
}
