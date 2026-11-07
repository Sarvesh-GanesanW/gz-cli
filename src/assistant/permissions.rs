//! Who decides whether a tool call may run, and what the user already
//! allowed this session.
//!
//! Policy: reads/searches ask once per folder (allowing a folder covers its
//! subfolders); every edit or write shows its diff and asks, unless edits
//! were allowed for the session; every command asks, unless that exact
//! command was allowed for the session.

use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use crate::assistant::tools::{Access, Planned};

/// The user's answer to a permission prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    AllowOnce,
    /// Allow this and similar calls for the rest of the session (same folder
    /// for reads, all edits for writes, the same command line for commands).
    AllowSession,
    Deny,
}

/// Everything a front end needs to ask the user.
#[derive(Debug, Clone)]
pub struct PermissionRequest {
    pub access: Access,
    /// One line, e.g. `edit_file src/main.rs`.
    pub summary: String,
    /// Diff for edits/writes, command line for commands.
    pub preview: Option<String>,
    /// What "this session" would cover, phrased for the prompt.
    pub session_scope: String,
}

pub type DecisionFuture<'a> = Pin<Box<dyn Future<Output = Decision> + Send + 'a>>;

/// Asks the user. Implemented by the terminal REPL and (later) the TUI.
pub trait Approver: Send + Sync {
    fn decide<'a>(&'a self, request: &'a PermissionRequest) -> DecisionFuture<'a>;
}

/// What has been allowed so far in this session.
#[derive(Debug, Default)]
pub struct Policy {
    read_folders: Vec<PathBuf>,
    edits_allowed: bool,
    commands: HashSet<String>,
}

impl Policy {
    /// `--yes-reads`: every read inside `root` is pre-approved.
    pub fn allow_reads_under(&mut self, root: &Path) {
        self.read_folders.push(root.to_path_buf());
    }

    pub fn is_preapproved(&self, planned: &Planned) -> bool {
        match planned.access {
            Access::Read => self
                .read_folders
                .iter()
                .any(|f| planned.scope.starts_with(f)),
            Access::Write => self.edits_allowed,
            Access::Command => self.commands.contains(command_of(planned)),
            Access::ServerAction => false,
        }
    }

    pub fn remember(&mut self, planned: &Planned) {
        match planned.access {
            Access::Read => self.read_folders.push(planned.scope.clone()),
            Access::Write => self.edits_allowed = true,
            Access::Command => {
                self.commands.insert(command_of(planned).to_string());
            }
            Access::ServerAction => {}
        }
    }

    pub fn request_for(planned: &Planned, scope_label: &str) -> PermissionRequest {
        let session_scope = match planned.access {
            Access::Read => format!("reads in {scope_label}/"),
            Access::Write => "all edits this session".to_string(),
            Access::Command => "this exact command".to_string(),
            Access::ServerAction => "this action in this conversation".to_string(),
        };
        PermissionRequest {
            access: planned.access,
            summary: planned.summary.clone(),
            preview: planned.preview.clone(),
            session_scope,
        }
    }
}

fn command_of(planned: &Planned) -> &str {
    planned.preview.as_deref().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant::sandbox::tests::temp_workspace;
    use crate::assistant::tools::{plan, ToolName};
    use serde_json::json;

    #[test]
    fn reads_are_remembered_per_folder_and_below() {
        let (dir, sandbox) = temp_workspace("policy-read");
        std::fs::create_dir_all(dir.join("src/deep")).unwrap();
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "").unwrap();
        std::fs::write(dir.join("src/deep/b.rs"), "").unwrap();
        std::fs::write(dir.join("docs/c.md"), "").unwrap();
        let read = |p: &str| plan(&sandbox, ToolName::ReadFile, &json!({"path": p})).unwrap();

        let mut policy = Policy::default();
        assert!(!policy.is_preapproved(&read("src/a.rs")));
        policy.remember(&read("src/a.rs"));
        assert!(policy.is_preapproved(&read("src/deep/b.rs")));
        assert!(!policy.is_preapproved(&read("docs/c.md")));
        policy.allow_reads_under(sandbox.root());
        assert!(policy.is_preapproved(&read("docs/c.md")));
    }

    #[test]
    fn edits_ask_until_allowed_for_the_session() {
        let (dir, sandbox) = temp_workspace("policy-edit");
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        let write = plan(
            &sandbox,
            ToolName::WriteFile,
            &json!({"path": "a.txt", "content": "y"}),
        )
        .unwrap();
        let mut policy = Policy::default();
        policy.allow_reads_under(sandbox.root());
        assert!(
            !policy.is_preapproved(&write),
            "reads must not cover writes"
        );
        policy.remember(&write);
        assert!(policy.is_preapproved(&write));
    }

    #[test]
    fn commands_are_remembered_exactly() {
        let (_dir, sandbox) = temp_workspace("policy-cmd");
        let cmd = |c: &str| plan(&sandbox, ToolName::RunCommand, &json!({"command": c})).unwrap();
        let mut policy = Policy::default();
        policy.remember(&cmd("cargo test"));
        assert!(policy.is_preapproved(&cmd("cargo test")));
        assert!(!policy.is_preapproved(&cmd("cargo test; rm -rf .")));
        let request = Policy::request_for(&cmd("ls"), "x");
        assert_eq!(request.session_scope, "this exact command");
    }
}
