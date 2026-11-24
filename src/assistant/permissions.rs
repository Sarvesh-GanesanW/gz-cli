//! Who decides whether a tool call may run, and what the user already
//! allowed.
//!
//! Policy: reads/searches inside the open folders run without asking (see
//! [`crate::assistant::read_only`], which also lets read-only commands and
//! scripts run under a read-only OS sandbox); every edit or write shows its
//! diff and asks, unless edits were allowed; every other command asks,
//! unless that exact command was allowed; every other script shows its code
//! and asks, unless scripts were allowed.
//! "Allowed" means either for this chat (cleared on a new conversation) or
//! always (saved in `permissions.toml`, see [`crate::assistant::saved_rules`]).
//! A background subagent's session shares its main session's grants (see
//! [`Policy::shared`]), since to the user it works in the same chat.

use std::collections::BTreeSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

use crate::assistant::chat_grants::{ChatGrantStore, ChatGrants};
use crate::assistant::questions::{Query, Reply};
use crate::assistant::rules::{RuleSet, RuleVerdict};
use crate::assistant::sandbox::Sandbox;
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
    /// What "for this chat" would cover, phrased for the prompt; empty
    /// when it can't be offered (an ask rule asks every time).
    pub chat_scope: String,
    /// What "always" would cover; `None` when it can't be offered (e.g. a
    /// destructive server action, which the server never remembers forever).
    pub always_scope: Option<String>,
    /// For a server action: the tool the server wants to run
    /// (`submit_job`), used to title the prompt.
    pub action: Option<String>,
    /// The subagent whose work raised this request, by name.
    pub asked_by: Option<String>,
    /// The file a write touches, relative to the folder.
    pub target: Option<String>,
    /// A line shown above the choices, e.g. why auto mode asks after all.
    pub note: Option<String>,
}

/// The result a refused call hands back, so the model knows to move on.
pub const DENIED: &str = "The user denied this action.";

/// Who let a call go ahead, for the audit log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecidedBy {
    /// The user answered a prompt.
    User,
    /// Auto mode ran it: routine, or its review allowed it.
    AutoMode,
    /// Accept-edits mode let an edit through.
    AcceptEdits,
    /// Auto mode had blocked it and the user allowed it by hand.
    UserAfterBlock,
}

impl DecidedBy {
    /// The audit log's `decision` for a call that went ahead.
    pub fn audit_label(self, decision: Decision) -> &'static str {
        match self {
            DecidedBy::User => decision.kind(),
            DecidedBy::AutoMode => "auto",
            DecidedBy::AcceptEdits => "accept-edits",
            DecidedBy::UserAfterBlock => "allowed-after-block",
        }
    }
}

/// Why an action was refused: the user said no, or a mode blocked it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The user denied it, with what they said to do instead, if anything.
    User { note: Option<String> },
    /// A mode refused it without asking: plan mode, or auto mode's review.
    Blocked {
        by: &'static str,
        reason: String,
        /// The command that lets the user run the action after all (auto
        /// mode's `/allow`); `None` when the block is final (plan mode).
        allow_with: Option<&'static str>,
    },
}

impl Refusal {
    /// The tool result the model receives.
    pub fn message(&self) -> String {
        match self {
            Refusal::User { note } => {
                match note.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
                    Some(note) => format!("The user denied this action: {note}"),
                    None => DENIED.to_string(),
                }
            }
            Refusal::Blocked {
                by,
                reason,
                allow_with: None,
            } => format!(
                "Blocked by {by}: {reason} Don't retry it unchanged. Take a different \
                 approach within what the user asked, or ask the user."
            ),
            // Without the last sentence the model treats the block as final
            // and refuses even after the user allows the action.
            Refusal::Blocked {
                by,
                reason,
                allow_with: Some(command),
            } => format!(
                "Blocked by {by}: {reason} Don't retry it on your own, unchanged or by \
                 another route. Take a different approach within what the user asked, or \
                 tell the user what was blocked and why, and that {command} lets it run. \
                 The block is not final: once the user allows or authorizes this action, \
                 call it again exactly as it was."
            ),
        }
    }

    /// The audit log's `decision`: `deny` from the user, `blocked` by a mode.
    pub fn audit_label(&self) -> &'static str {
        match self {
            Refusal::User { .. } => "deny",
            Refusal::Blocked { .. } => "blocked",
        }
    }

    /// For the audit log: which mode blocked it and why.
    pub fn audit_reason(&self) -> Option<String> {
        match self {
            Refusal::User { .. } => None,
            Refusal::Blocked { by, reason, .. } => Some(format!("{by}: {reason}")),
        }
    }

    /// The dim line the user sees.
    pub fn notice(&self, summary: &str) -> String {
        match self {
            Refusal::User { .. } => format!("denied: {summary}"),
            Refusal::Blocked {
                by,
                reason,
                allow_with,
            } => {
                let hint = allow_with
                    .map(|command| format!(" · {command} lets it run"))
                    .unwrap_or_default();
                format!("{by} blocked: {summary} · {reason}{hint}")
            }
        }
    }
}

pub type DecisionFuture<'a> = Pin<Box<dyn Future<Output = Decision> + Send + 'a>>;
pub type ReplyFuture<'a> = Pin<Box<dyn Future<Output = Reply> + Send + 'a>>;

/// Asks the user. Implemented by the terminal REPL and (later) the TUI.
pub trait Approver: Send + Sync {
    fn decide<'a>(&'a self, request: &'a PermissionRequest) -> DecisionFuture<'a>;

    /// After a [`Decision::Deny`], why. Taking it clears it.
    fn take_refusal(&self) -> Refusal {
        Refusal::User { note: None }
    }

    /// After a call was allowed, who allowed it. Taking it clears it.
    fn take_decided_by(&self) -> DecidedBy {
        DecidedBy::User
    }

    /// Shows the user the model's questions or plan (`ask_user_question`,
    /// `exit_plan_mode`). Front ends that can't ask reply
    /// [`Reply::NoAnswer`].
    fn ask<'a>(&'a self, _query: &'a Query) -> ReplyFuture<'a> {
        Box::pin(async { Reply::NoAnswer })
    }

    /// A rule refused `request` without asking anyone; front ends that
    /// report decisions record it.
    fn note_refusal(&self, _request: &PermissionRequest, _refusal: &Refusal) {}

    /// Asks the user even when a mode would decide without asking: an ask
    /// rule matched. Front ends without modes just ask.
    fn ask_anyway<'a>(&'a self, request: &'a PermissionRequest) -> DecisionFuture<'a> {
        self.decide(request)
    }

    /// Whether plan mode is on, so actions are refused and the model
    /// should finish with `exit_plan_mode`.
    fn in_plan_mode(&self) -> bool {
        false
    }

    /// `enter_plan_mode`: turns plan mode on; false when this front end
    /// has none.
    fn enter_plan_mode(&self) -> bool {
        false
    }

    /// Whether accept-edits mode is on, so safe file commands run without
    /// asking (see [`crate::assistant::read_only`]).
    fn accepts_edits(&self) -> bool {
        false
    }

    /// Whether don't-ask mode is on: what no rule or grant allows is
    /// refused, gz's automatic approvals of commands and scripts included.
    fn refuses_unasked(&self) -> bool {
        false
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
    /// How allowed commands start (`git status`): the start alone, or with
    /// more arguments after it. See [`command_starts_with`].
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub command_starts: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub scripts: bool,
    /// Hosts whose pages `web_fetch` may read.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub hosts: BTreeSet<String>,
    /// Local MCP tools `local_mcp_call` may call, as `server/tool`.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub local_tools: BTreeSet<String>,
}

impl Rules {
    pub fn allows(&self, planned: &Planned) -> bool {
        match planned.access {
            Access::Read => self
                .read_folders
                .iter()
                .any(|f| planned.scope.starts_with(f)),
            Access::Write => self.edits,
            Access::Command => {
                let command = command_of(planned);
                self.commands.contains(command)
                    || self
                        .command_starts
                        .iter()
                        .any(|start| command_starts_with(command, start))
            }
            Access::Script => self.scripts,
            Access::Fetch => planned
                .target
                .as_ref()
                .is_some_and(|h| self.hosts.contains(h)),
            Access::LocalTool => planned
                .target
                .as_ref()
                .is_some_and(|t| self.local_tools.contains(t)),
            Access::Folder | Access::ServerAction | Access::Install | Access::Trust => false,
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
            Access::Fetch => {
                if let Some(host) = &planned.target {
                    self.hosts.insert(host.clone());
                }
            }
            Access::LocalTool => {
                if let Some(tool) = &planned.target {
                    self.local_tools.insert(tool.clone());
                }
            }
            Access::Folder | Access::ServerAction | Access::Install | Access::Trust => {}
        }
    }

    pub fn is_empty(&self) -> bool {
        self == &Rules::default()
    }
}

/// The saved "always" rules, each with the folder it was granted in. Reads
/// name their own (absolute) folders; edits, commands and scripts count
/// only inside the folder they were granted in, wherever the session's
/// current folder is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SavedRules(Vec<(PathBuf, Rules)>);

impl SavedRules {
    pub fn new(folders: Vec<(PathBuf, Rules)>) -> Self {
        Self(folders)
    }

    pub fn allows(&self, planned: &Planned) -> bool {
        self.0.iter().any(|(folder, rules)| {
            rules.allows(planned)
                && (planned.access == Access::Read || planned.scope.starts_with(folder))
        })
    }

    /// Grants what `planned` needs under `folder`.
    pub fn grant(&mut self, folder: &Path, planned: &Planned) {
        match self.0.iter_mut().find(|(f, _)| f == folder) {
            Some((_, rules)) => rules.grant(planned),
            None => {
                let mut rules = Rules::default();
                rules.grant(planned);
                self.0.push((folder.to_path_buf(), rules));
            }
        }
    }
}

/// What the user allowed: for this chat, and always (loaded from the saved
/// rules when the session starts).
#[derive(Debug, Default)]
pub struct Policy {
    /// `--yes-reads`: kept for the whole session, across chats.
    session_reads: Vec<PathBuf>,
    grants: Arc<Mutex<Grants>>,
}

/// The grants sessions of one chat share.
#[derive(Debug, Default)]
struct Grants {
    chat: Rules,
    always: SavedRules,
    /// Server tools allowed for this chat, by name (the server keys its
    /// conversation grants the same way).
    chat_actions: BTreeSet<String>,
    /// Scripts (by hash) that tried to write while running read-only
    /// without asking in this chat; they ask from then on.
    scripts_wrote: BTreeSet<String>,
    /// Where this chat's grants are kept, and the chat's id; `None` keeps
    /// them in memory only.
    saved_chat: Option<(ChatGrantStore, String)>,
    /// The allow/ask/deny rules (see [`crate::assistant::rules`]).
    rules: RuleSet,
}

impl Grants {
    fn chat_grants(&self) -> ChatGrants {
        ChatGrants {
            rules: self.chat.clone(),
            actions: self.chat_actions.clone(),
            scripts_wrote: self.scripts_wrote.clone(),
        }
    }

    /// Writes this chat's grants to its file, when it has one.
    fn save_chat(&self) {
        let Some((store, conversation)) = &self.saved_chat else {
            return;
        };
        if let Err(err) = store.save(conversation, &self.chat_grants()) {
            crate::output::warn(&format!(
                "cannot keep this chat's permissions for --continue: {err:#}"
            ));
        }
    }
}

impl Policy {
    /// A policy that starts with the user's saved "always" rules.
    pub fn with_saved(always: SavedRules) -> Self {
        let grants = Grants {
            always,
            ..Grants::default()
        };
        Self {
            grants: Arc::new(Mutex::new(grants)),
            ..Self::default()
        }
    }

    /// A policy for another session of the same chat (a background
    /// subagent's): an answer either one gets counts for both.
    pub fn shared(&self) -> Self {
        Self {
            session_reads: self.session_reads.clone(),
            grants: self.grants.clone(),
        }
    }

    fn grants(&self) -> MutexGuard<'_, Grants> {
        // The grants are plain data, valid even if a holder panicked.
        self.grants
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Whether the server tool `tool` was allowed for this chat.
    pub fn allows_action(&self, tool: &str) -> bool {
        self.grants().chat_actions.contains(tool)
    }

    /// Records that the server tool `tool` was allowed for this chat (or
    /// always, which the server keeps itself).
    pub fn remember_action(&mut self, tool: &str, decision: Decision) {
        if matches!(decision, Decision::AllowChat | Decision::AllowAlways) {
            let mut grants = self.grants();
            grants.chat_actions.insert(tool.to_string());
            grants.save_chat();
        }
    }

    /// Switches to the chat `conversation`: forgets the last chat's grants
    /// and loads this one's, kept by `store` (when given) as they change.
    pub fn open_chat(&mut self, store: Option<ChatGrantStore>, conversation: &str) {
        let loaded = store
            .as_ref()
            .map(|store| store.load(conversation))
            .transpose()
            .unwrap_or_else(|err| {
                crate::output::warn(&format!("{err:#}"));
                None
            })
            .unwrap_or_default();
        let mut grants = self.grants();
        grants.chat = loaded.rules;
        grants.chat_actions = loaded.actions;
        grants.scripts_wrote = loaded.scripts_wrote;
        grants.saved_chat = store.map(|store| (store, conversation.to_string()));
    }

    /// The store this chat's grants are kept in, for the next
    /// [`Self::open_chat`].
    /// Gives the fork `conversation` this chat's grants, so a copy of the
    /// chat carries on with what the user allowed in it.
    pub fn copy_chat_to(&self, conversation: &str) -> anyhow::Result<()> {
        let grants = self.grants();
        match &grants.saved_chat {
            Some((store, _)) => store.save(conversation, &grants.chat_grants()),
            None => Ok(()),
        }
    }

    pub fn chat_store(&self) -> Option<ChatGrantStore> {
        self.grants()
            .saved_chat
            .as_ref()
            .map(|(store, _)| store.clone())
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
        let grants = self.grants();
        session_read || grants.chat.allows(planned) || grants.always.allows(planned)
    }

    /// Records an approval: chat-scoped for [`Decision::AllowChat`], saved
    /// under `folder` (in memory; the caller writes the file) for
    /// [`Decision::AllowAlways`].
    pub fn remember(&self, planned: &Planned, decision: Decision, folder: &Path) {
        let mut grants = self.grants();
        match decision {
            Decision::AllowChat => {
                grants.chat.grant(planned);
                grants.save_chat();
            }
            Decision::AllowAlways => grants.always.grant(folder, planned),
            Decision::AllowOnce | Decision::Deny => {}
        }
    }

    /// Forgets everything allowed "for this chat", keeping it nowhere.
    #[cfg(test)]
    pub fn clear_chat(&mut self) {
        self.open_chat(None, "");
    }

    /// Whether the script `code` tried to write when it ran read-only in
    /// this chat.
    pub fn script_wrote(&self, code: &str) -> bool {
        self.grants().scripts_wrote.contains(&script_hash(code))
    }

    /// Remembers for this chat that the script `code` tried to write.
    pub fn remember_script_wrote(&self, code: &str) {
        let mut grants = self.grants();
        grants.scripts_wrote.insert(script_hash(code));
        grants.save_chat();
    }

    /// Replaces the saved rules (after a revoke elsewhere).
    pub fn set_saved(&mut self, always: SavedRules) {
        self.grants().always = always;
    }

    /// The prompt for `planned`: `scope_label` names the folder a read
    /// covers, `folder_label` the folder an "always" answer is saved for.
    pub fn request_for(
        planned: &Planned,
        scope_label: &str,
        folder_label: &str,
    ) -> PermissionRequest {
        let (chat_scope, always_scope) = match planned.access {
            Access::Read => (
                format!("reads in {scope_label}/"),
                format!("reads in {scope_label}/"),
            ),
            Access::Write => (
                "all edits in this chat".to_string(),
                format!("all edits in {folder_label}"),
            ),
            Access::Command => (
                "this exact command".to_string(),
                format!("this exact command in {folder_label}"),
            ),
            Access::Script => (
                "scripts in this chat".to_string(),
                format!("scripts in {folder_label}"),
            ),
            Access::Folder => (
                format!("work in {scope_label} for this session"),
                format!("always allow {scope_label}"),
            ),
            Access::ServerAction => (
                "this action in this chat".to_string(),
                "this action in every chat".to_string(),
            ),
            Access::Install => ("this install".to_string(), "this install".to_string()),
            Access::Trust => ("this version".to_string(), "this version".to_string()),
            Access::Fetch => {
                let host = planned.target.as_deref().unwrap_or("this site");
                (
                    format!("pages from {host} in this chat"),
                    format!("pages from {host} in {folder_label}"),
                )
            }
            Access::LocalTool => {
                let tool = planned.target.as_deref().unwrap_or("this tool");
                (
                    format!("{tool} in this chat"),
                    format!("{tool} in {folder_label}"),
                )
            }
        };
        PermissionRequest {
            access: planned.access,
            summary: planned.summary.clone(),
            preview: planned.preview.clone(),
            chat_scope,
            always_scope: Some(always_scope),
            action: None,
            asked_by: None,
            target: planned.target.clone(),
            note: None,
        }
    }

    /// The allow/ask/deny rule that decides `planned`, if any.
    pub fn rule_verdict(&self, planned: &Planned) -> Option<RuleVerdict> {
        self.grants().rules.verdict(planned)
    }

    /// Replaces the rules (at start, or with command-line rules added).
    pub fn set_rules(&mut self, rules: RuleSet) {
        self.grants().rules = rules;
    }

    /// The rules as they are now, for listing them.
    pub fn rules(&self) -> RuleSet {
        self.grants().rules.clone()
    }

    /// Reads the rules files again (after `/permissions` changed one).
    pub fn reload_rules(&mut self) {
        let mut grants = self.grants();
        grants.rules = grants.rules.reloaded();
    }

    /// Whether the edit `planned` is allowed without asking.
    pub fn edits_allowed(&self, planned: &Planned) -> bool {
        let grants = self.grants();
        grants.chat.edits || grants.always.allows(planned)
    }
}

/// [`PermissionRequest::action`] for an edit a script's helper makes, so
/// the prompt can say where it comes from.
pub const FROM_SCRIPT: &str = "run_script";

/// Prompts for edits a script's helpers make. "Yes, for this chat" and "Yes, always"
/// grant the same edits as for an edit made directly ([`Policy::remember`]),
/// so one answer covers both.
pub struct ScriptEdits;

impl ScriptEdits {
    /// The prompt for a helper edit; `folder_label` names the folder an
    /// "always" answer is saved for.
    pub fn request_for(planned: &Planned, folder_label: &str) -> PermissionRequest {
        PermissionRequest {
            action: Some(FROM_SCRIPT.to_string()),
            ..Policy::request_for(planned, folder_label, folder_label)
        }
    }
}

/// The folder an "always" answer for `planned` is saved under: the current
/// folder when the call is inside it, else the innermost open folder that
/// holds it.
pub fn grant_folder(sandbox: &Sandbox, planned: &Planned) -> PathBuf {
    let cwd = sandbox.cwd();
    if planned.scope.starts_with(&cwd) {
        return cwd;
    }
    sandbox
        .roots()
        .into_iter()
        .filter(|root| planned.scope.starts_with(root))
        .max_by_key(|root| root.components().count())
        .unwrap_or(cwd)
}

/// A script's identity for [`Policy::script_wrote`]: the hash of its code.
fn script_hash(code: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(code.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn command_of(planned: &Planned) -> &str {
    planned.preview.as_deref().unwrap_or_default()
}

/// Characters that make a command line more than one program with plain
/// arguments: chaining, pipes, redirection, substitution, expansion.
const SHELL_SYNTAX: &[char] = &[
    ';', '&', '|', '<', '>', '`', '$', '(', ')', '{', '}', '\n', '\r',
];
/// `cmd` (the shell commands run in on Windows) also escapes with `^` and
/// expands `%NAME%`.
const CMD_SYNTAX: &[char] = &['^', '%'];

/// Whether a command line has shell syntax beyond a program and its
/// arguments, so allowing how it starts would allow more than that program.
pub fn has_shell_syntax(command: &str) -> bool {
    command.contains(SHELL_SYNTAX) || (cfg!(windows) && command.contains(CMD_SYNTAX))
}

/// Whether `command` is `start` alone or `start` with more arguments. A
/// line with shell syntax never matches: `git status; rm -rf ~` starts
/// with `git status` and runs something else.
pub fn command_starts_with(command: &str, start: &str) -> bool {
    let command = command.trim();
    if has_shell_syntax(command) {
        return false;
    }
    match command.strip_prefix(start) {
        Some(rest) => rest.is_empty() || rest.starts_with(char::is_whitespace),
        None => false,
    }
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
        policy.remember(&read("src/a.rs"), Decision::AllowChat, &sandbox.cwd());
        assert!(policy.is_preapproved(&read("src/deep/b.rs")));
        assert!(!policy.is_preapproved(&read("docs/c.md")));
        policy.allow_reads_under(&sandbox.start());
        assert!(policy.is_preapproved(&read("docs/c.md")));
    }

    #[test]
    fn edits_ask_until_allowed_for_the_session() {
        let (dir, sandbox) = temp_workspace("policy-edit");
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        crate::assistant::sandbox::tests::seen_by_model(&sandbox, &dir.join("a.txt"));
        let write = plan(
            &sandbox,
            ToolName::WriteFile,
            &json!({"path": "a.txt", "content": "y"}),
        )
        .unwrap();
        let mut policy = Policy::default();
        policy.allow_reads_under(&sandbox.start());
        assert!(
            !policy.is_preapproved(&write),
            "reads must not cover writes"
        );
        policy.remember(&write, Decision::AllowChat, &sandbox.cwd());
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
        let policy = Policy::default();
        assert!(!policy.is_preapproved(&script));
        assert_eq!(
            Policy::request_for(&script, ".", ".").chat_scope,
            "scripts in this chat"
        );
        policy.remember(&script, Decision::AllowChat, &sandbox.cwd());
        assert!(policy.is_preapproved(&script));
    }

    #[test]
    fn chat_grants_clear_but_saved_rules_stay() {
        let (dir, sandbox) = temp_workspace("policy-scopes");
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        crate::assistant::sandbox::tests::seen_by_model(&sandbox, &dir.join("a.txt"));
        let write = plan(
            &sandbox,
            ToolName::WriteFile,
            &json!({"path": "a.txt", "content": "y"}),
        )
        .unwrap();
        let cmd = plan(&sandbox, ToolName::RunCommand, &json!({"command": "ls"})).unwrap();
        let mut policy = Policy::default();
        policy.remember(&write, Decision::AllowChat, &sandbox.cwd());
        policy.remember(&cmd, Decision::AllowAlways, &sandbox.cwd());
        policy.clear_chat();
        assert!(
            !policy.is_preapproved(&write),
            "chat grants end with the chat"
        );
        assert!(
            policy.is_preapproved(&cmd),
            "always grants survive a new chat"
        );
        policy.remember(&write, Decision::AllowOnce, &sandbox.cwd());
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
    fn one_chat_grant_covers_direct_and_script_edits() {
        let (_dir, sandbox) = temp_workspace("policy-script-edits");
        let write = plan(
            &sandbox,
            ToolName::WriteFile,
            &json!({"path": "a.txt", "content": "x"}),
        )
        .unwrap();
        let policy = Policy::default();
        assert!(!policy.edits_allowed(&write));
        policy.remember(&write, Decision::AllowChat, &sandbox.cwd());
        assert!(
            policy.edits_allowed(&write),
            "a direct grant covers scripts"
        );
        assert!(policy.is_preapproved(&write));
    }

    #[test]
    fn a_script_edit_prompt_reads_like_a_direct_one() {
        let (_dir, sandbox) = temp_workspace("policy-script-prompt");
        let write = plan(
            &sandbox,
            ToolName::WriteFile,
            &json!({"path": "a.txt", "content": "x"}),
        )
        .unwrap();
        let request = ScriptEdits::request_for(&write, "~/code/app");
        assert_eq!(request.summary, "write_file a.txt");
        assert_eq!(request.action.as_deref(), Some(FROM_SCRIPT));
        assert_eq!(request.chat_scope, "all edits in this chat");
        assert_eq!(
            request.always_scope.as_deref(),
            Some("all edits in ~/code/app")
        );
    }

    #[test]
    fn saved_rules_hold_only_in_the_folder_they_were_granted_in() {
        let (dir, sandbox) = temp_workspace("policy-folders");
        std::fs::create_dir_all(dir.join("app")).unwrap();
        std::fs::create_dir_all(dir.join("other")).unwrap();
        sandbox.set_cwd(&dir.join("app")).unwrap();
        let in_app = plan(&sandbox, ToolName::RunCommand, &json!({"command": "make"})).unwrap();
        let app_edit = plan(
            &sandbox,
            ToolName::WriteFile,
            &json!({"path": "a.txt", "content": "x"}),
        )
        .unwrap();
        let policy = Policy::default();
        policy.remember(&in_app, Decision::AllowAlways, &dir.join("app"));
        policy.remember(&app_edit, Decision::AllowAlways, &dir.join("app"));
        assert!(policy.is_preapproved(&in_app));
        sandbox.set_cwd(&dir.join("other")).unwrap();
        let in_other = plan(&sandbox, ToolName::RunCommand, &json!({"command": "make"})).unwrap();
        let other_edit = plan(
            &sandbox,
            ToolName::WriteFile,
            &json!({"path": "a.txt", "content": "x"}),
        )
        .unwrap();
        assert!(
            !policy.is_preapproved(&in_other),
            "same command, other folder"
        );
        assert!(!policy.is_preapproved(&other_edit));
        assert!(
            policy.is_preapproved(&app_edit),
            "still allowed back in app"
        );
    }

    #[test]
    fn chat_grants_come_back_with_their_conversation() {
        let (dir, sandbox) = temp_workspace("policy-chat-store");
        let store = ChatGrantStore::at(dir.join("store"));
        let write = plan(
            &sandbox,
            ToolName::WriteFile,
            &json!({"path": "a.txt", "content": "x"}),
        )
        .unwrap();
        let cmd = plan(&sandbox, ToolName::RunCommand, &json!({"command": "make"})).unwrap();
        let mut first = Policy::default();
        first.open_chat(Some(store.clone()), "conv-a");
        first.remember(&write, Decision::AllowChat, &sandbox.cwd());
        first.remember(&cmd, Decision::AllowOnce, &sandbox.cwd());
        first.remember_action("submit_job", Decision::AllowChat);
        first.remember_script_wrote("print(1)");

        let mut later = Policy::default();
        later.open_chat(Some(store.clone()), "conv-a");
        assert!(later.is_preapproved(&write), "edits allowed for the chat");
        assert!(!later.is_preapproved(&cmd), "once is never kept");
        assert!(later.allows_action("submit_job"));
        assert!(later.script_wrote("print(1)"));
        later.open_chat(Some(store), "conv-b");
        assert!(!later.is_preapproved(&write), "another chat starts empty");
        assert!(!later.allows_action("submit_job"));
    }

    #[test]
    fn a_fork_carries_the_chat_grants() {
        let (dir, sandbox) = temp_workspace("policy-chat-fork");
        let store = ChatGrantStore::at(dir.join("store"));
        let write = plan(
            &sandbox,
            ToolName::WriteFile,
            &json!({"path": "a.txt", "content": "x"}),
        )
        .unwrap();
        let mut policy = Policy::default();
        policy.open_chat(Some(store.clone()), "conv-a");
        policy.remember(&write, Decision::AllowChat, &sandbox.cwd());
        policy.remember_action("submit_job", Decision::AllowChat);
        policy.copy_chat_to("conv-fork").unwrap();

        policy.open_chat(Some(store), "conv-fork");
        assert!(
            policy.is_preapproved(&write),
            "the copy keeps the edit grant"
        );
        assert!(policy.allows_action("submit_job"));
    }

    #[test]
    fn a_shared_policy_sees_grants_from_either_side() {
        let (_dir, sandbox) = temp_workspace("policy-shared");
        let write = plan(
            &sandbox,
            ToolName::WriteFile,
            &json!({"path": "shared.txt", "content": "x"}),
        )
        .unwrap();
        let mut main = Policy::default();
        let subagent = main.shared();
        subagent.remember(&write, Decision::AllowChat, &sandbox.cwd());
        assert!(main.is_preapproved(&write), "the subagent's answer counts");
        main.remember_action("lakehouse_write", Decision::AllowChat);
        assert!(subagent.allows_action("lakehouse_write"));
        main.remember_action("drop_table", Decision::AllowOnce);
        assert!(!subagent.allows_action("drop_table"), "once is not shared");
        main.clear_chat();
        assert!(!subagent.is_preapproved(&write) && !subagent.allows_action("lakehouse_write"));
    }

    #[test]
    fn commands_are_remembered_exactly() {
        let (_dir, sandbox) = temp_workspace("policy-cmd");
        let cmd = |c: &str| plan(&sandbox, ToolName::RunCommand, &json!({"command": c})).unwrap();
        let policy = Policy::default();
        policy.remember(&cmd("cargo test"), Decision::AllowChat, &sandbox.cwd());
        assert!(policy.is_preapproved(&cmd("cargo test")));
        assert!(!policy.is_preapproved(&cmd("cargo test; rm -rf .")));
        let request = Policy::request_for(&cmd("ls"), "x", "x");
        assert_eq!(request.chat_scope, "this exact command");
    }
}
