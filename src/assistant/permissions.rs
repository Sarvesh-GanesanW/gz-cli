//! Who decides whether a tool call may run, and what the user already
//! allowed.
//!
//! Policy: reads/searches ask once per folder (allowing a folder covers its
//! subfolders); every edit or write shows its diff and asks, unless edits
//! were allowed; every command asks, unless that exact command was allowed;
//! every script shows its code and asks, unless scripts were allowed.
//! "Allowed" means either for this chat (cleared on a new conversation) or
//! always (saved in `permissions.toml`, see [`crate::assistant::saved_rules`]).

use std::collections::BTreeSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use serde::{Deserialize, Serialize};

use crate::assistant::tools::{Access, Planned};

/// The user's answer to a permission prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    AllowOnce,
    /// Allow this and similar calls for the rest of this chat (same folder
    /// for reads, all edits for writes, the same command line for commands).
    AllowChat,
    /// Like [`Decision::AllowChat`], but saved so it survives new chats and
    /// restarts.
    AllowAlways,
    Deny,
}

impl Decision {
    /// How the audit log records the answer.
    pub fn kind(self) -> &'static str {
        match self {
            Decision::AllowOnce => "once",
            Decision::AllowChat => "chat",
            Decision::AllowAlways => "always",
            Decision::Deny => "deny",
        }
    }

    /// The chat backend's `remember` value for a server approval
    /// (gz-chat-eks `RememberChoice`).
    pub fn server_remember(self) -> &'static str {
        match self {
            Decision::AllowChat => "conversation",
            Decision::AllowAlways => "always",
            Decision::AllowOnce | Decision::Deny => "once",
        }
    }
}

/// Everything a front end needs to ask the user.
#[derive(Debug, Clone)]
pub struct PermissionRequest {
    pub access: Access,
    /// One line, e.g. `edit_file src/main.rs`.
    pub summary: String,
    /// Diff for edits/writes, command line for commands.
    pub preview: Option<String>,
    /// What "for this chat" would cover, phrased for the prompt.
    pub chat_scope: String,
    /// What "always" would cover; `None` when it can't be offered (e.g. a
    /// destructive server action, which the server never remembers forever).
    pub always_scope: Option<String>,
    /// For a server action: the tool the server wants to run
    /// (`submit_job`), used to title the prompt.
    pub action: Option<String>,
    /// The subagent whose work raised this request, by name.
    pub asked_by: Option<String>,
}

/// The result a refused call hands back, so the model knows to move on.
pub const DENIED: &str = "The user denied this action.";

/// The refusal text, carrying the user's note ("do it in Python instead")
/// when they gave one.
pub fn denial_message(note: Option<String>) -> String {
    match note.map(|n| n.trim().to_string()).filter(|n| !n.is_empty()) {
        Some(note) => format!("The user denied this action: {note}"),
        None => DENIED.to_string(),
    }
}

pub type DecisionFuture<'a> = Pin<Box<dyn Future<Output = Decision> + Send + 'a>>;

/// Asks the user. Implemented by the terminal REPL and (later) the TUI.
pub trait Approver: Send + Sync {
    fn decide<'a>(&'a self, request: &'a PermissionRequest) -> DecisionFuture<'a>;

    /// After a [`Decision::Deny`], what the user said to do instead, if
    /// anything. Taking it clears it.
    fn take_denial_note(&self) -> Option<String> {
        None
    }
}

/// One set of granted permissions: what a chat, or the saved rules for a
/// folder, already allows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rules {
    /// Absolute folders whose reads (and subfolders') are allowed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read_folders: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub edits: bool,
    /// Exact command lines.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub commands: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub scripts: bool,
}

impl Rules {
    pub fn allows(&self, planned: &Planned) -> bool {
        match planned.access {
            Access::Read => self
                .read_folders
                .iter()
                .any(|f| planned.scope.starts_with(f)),
            Access::Write => self.edits,
            Access::Command => self.commands.contains(command_of(planned)),
            Access::Script => self.scripts,
            Access::ServerAction => false,
        }
    }

    /// Grants what `planned` needs (its folder, edits, its command, scripts).
    pub fn grant(&mut self, planned: &Planned) {
        match planned.access {
            Access::Read => {
                if !self.read_folders.contains(&planned.scope) {
                    self.read_folders.push(planned.scope.clone());
                }
            }
            Access::Write => self.edits = true,
            Access::Command => {
                self.commands.insert(command_of(planned).to_string());
            }
            Access::Script => self.scripts = true,
            Access::ServerAction => {}
        }
    }

    pub fn merge(&mut self, other: &Rules) {
        for folder in &other.read_folders {
            if !self.read_folders.contains(folder) {
                self.read_folders.push(folder.clone());
            }
        }
        self.edits |= other.edits;
        self.commands.extend(other.commands.iter().cloned());
        self.scripts |= other.scripts;
    }

    pub fn is_empty(&self) -> bool {
        self == &Rules::default()
    }
}

/// What the user allowed: for this chat, and always (loaded from the saved
/// rules when the session starts).
#[derive(Debug, Default)]
pub struct Policy {
    /// `--yes-reads`: kept for the whole session, across chats.
    session_reads: Vec<PathBuf>,
    chat: Rules,
    always: Rules,
}

impl Policy {
    /// A policy that starts with the user's saved "always" rules.
    pub fn with_saved(always: Rules) -> Self {
        Self {
            always,
            ..Self::default()
        }
    }

    /// `--yes-reads`: every read inside `root` is pre-approved.
    pub fn allow_reads_under(&mut self, root: &Path) {
        self.session_reads.push(root.to_path_buf());
    }

    pub fn is_preapproved(&self, planned: &Planned) -> bool {
        let session_read = planned.access == Access::Read
            && self
                .session_reads
                .iter()
                .any(|f| planned.scope.starts_with(f));
        session_read || self.chat.allows(planned) || self.always.allows(planned)
    }

    /// Records an approval: chat-scoped for [`Decision::AllowChat`], saved
    /// (in memory; the caller writes the file) for [`Decision::AllowAlways`].
    pub fn remember(&mut self, planned: &Planned, decision: Decision) {
        match decision {
            Decision::AllowChat => self.chat.grant(planned),
            Decision::AllowAlways => self.always.grant(planned),
            Decision::AllowOnce | Decision::Deny => {}
        }
    }

    /// Forgets everything allowed "for this chat" (a new conversation).
    pub fn clear_chat(&mut self) {
        self.chat = Rules::default();
    }

    /// Replaces the saved rules (after a revoke elsewhere).
    pub fn set_saved(&mut self, always: Rules) {
        self.always = always;
    }

    pub fn request_for(planned: &Planned, scope_label: &str) -> PermissionRequest {
        let (chat_scope, always_scope) = match planned.access {
            Access::Read => (
                format!("reads in {scope_label}/"),
                format!("reads in {scope_label}/"),
            ),
            Access::Write => (
                "all edits in this chat".to_string(),
                "all edits in this folder".to_string(),
            ),
            Access::Command => (
                "this exact command".to_string(),
                "this exact command in this folder".to_string(),
            ),
            Access::Script => (
                "scripts in this chat".to_string(),
                "scripts in this folder".to_string(),
            ),
            Access::ServerAction => (
                "this action in this chat".to_string(),
                "this action in every chat".to_string(),
            ),
        };
        PermissionRequest {
            access: planned.access,
            summary: planned.summary.clone(),
            preview: planned.preview.clone(),
            chat_scope,
            always_scope: Some(always_scope),
            action: None,
            asked_by: None,
        }
    }

    pub fn edits_allowed(&self) -> bool {
        self.chat.edits || self.always.edits
    }
}

/// Per-script edit permission, so "allow all edits from this script" ends
/// with the script.
#[derive(Debug, Default)]
pub struct ScriptEdits {
    allowed: bool,
}

impl ScriptEdits {
    pub fn is_allowed(&self, policy: &Policy) -> bool {
        self.allowed || policy.edits_allowed()
    }

    pub fn allow_rest_of_script(&mut self) {
        self.allowed = true;
    }

    pub fn request_for(planned: &Planned) -> PermissionRequest {
        PermissionRequest {
            access: planned.access,
            summary: format!("script wants to {}", planned.summary),
            preview: planned.preview.clone(),
            chat_scope: "all edits from this script".to_string(),
            always_scope: Some("all edits in this folder".to_string()),
            action: None,
            asked_by: None,
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
        policy.remember(&read("src/a.rs"), Decision::AllowChat);
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
        policy.remember(&write, Decision::AllowChat);
        assert!(policy.is_preapproved(&write));
    }

    #[test]
    fn scripts_ask_until_allowed_for_the_session() {
        let (_dir, sandbox) = temp_workspace("policy-script");
        let script = plan(
            &sandbox,
            ToolName::RunScript,
            &json!({"language": "python", "code": "print(1)"}),
        )
        .unwrap();
        let mut policy = Policy::default();
        assert!(!policy.is_preapproved(&script));
        assert_eq!(
            Policy::request_for(&script, ".").chat_scope,
            "scripts in this chat"
        );
        policy.remember(&script, Decision::AllowChat);
        assert!(policy.is_preapproved(&script));
    }

    #[test]
    fn chat_grants_clear_but_saved_rules_stay() {
        let (dir, sandbox) = temp_workspace("policy-scopes");
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        let write = plan(
            &sandbox,
            ToolName::WriteFile,
            &json!({"path": "a.txt", "content": "y"}),
        )
        .unwrap();
        let cmd = plan(&sandbox, ToolName::RunCommand, &json!({"command": "ls"})).unwrap();
        let mut policy = Policy::default();
        policy.remember(&write, Decision::AllowChat);
        policy.remember(&cmd, Decision::AllowAlways);
        policy.clear_chat();
        assert!(
            !policy.is_preapproved(&write),
            "chat grants end with the chat"
        );
        assert!(
            policy.is_preapproved(&cmd),
            "always grants survive a new chat"
        );
        policy.remember(&write, Decision::AllowOnce);
        assert!(!policy.is_preapproved(&write), "once grants nothing");
    }

    #[test]
    fn decisions_map_to_audit_and_server_values() {
        assert_eq!(Decision::AllowChat.server_remember(), "conversation");
        assert_eq!(Decision::AllowAlways.server_remember(), "always");
        assert_eq!(Decision::AllowOnce.server_remember(), "once");
        let kinds: Vec<_> = [
            Decision::AllowOnce,
            Decision::AllowChat,
            Decision::AllowAlways,
            Decision::Deny,
        ]
        .iter()
        .map(|d| d.kind())
        .collect();
        assert_eq!(kinds, vec!["once", "chat", "always", "deny"]);
    }

    #[test]
    fn script_edit_permission_ends_with_the_script() {
        let policy = Policy::default();
        let mut edits = ScriptEdits::default();
        assert!(!edits.is_allowed(&policy));
        edits.allow_rest_of_script();
        assert!(edits.is_allowed(&policy));
        assert!(!ScriptEdits::default().is_allowed(&policy));
    }

    #[test]
    fn commands_are_remembered_exactly() {
        let (_dir, sandbox) = temp_workspace("policy-cmd");
        let cmd = |c: &str| plan(&sandbox, ToolName::RunCommand, &json!({"command": c})).unwrap();
        let mut policy = Policy::default();
        policy.remember(&cmd("cargo test"), Decision::AllowChat);
        assert!(policy.is_preapproved(&cmd("cargo test")));
        assert!(!policy.is_preapproved(&cmd("cargo test; rm -rf .")));
        let request = Policy::request_for(&cmd("ls"), "x");
        assert_eq!(request.chat_scope, "this exact command");
    }
}
