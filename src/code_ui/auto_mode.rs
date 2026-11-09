//! Auto mode: gz runs actions without asking when they are routine or when
//! a review (see `crate::assistant::action_review`) finds the user's request
//! backs them, and blocks the rest with a reason the model receives.
//!
//! Reads and edits inside the folder run at once; edits to files that
//! change how code is run or reviewed (`.git/`, CI config) and every
//! command, script and Groundzero action go to the review. Three blocks in
//! a row or twenty in a session pause auto mode until the user approves a
//! prompted action, as in Claude Code.

use std::collections::VecDeque;

pub use crate::assistant::action_review::BlockedAction;
use crate::assistant::action_review::{ReviewRequest, ReviewWorkspace, ReviewedAction};
use crate::assistant::auto_rules::{AutoRules, PastDecision};
use crate::assistant::tools::Access;
use crate::assistant::PermissionRequest;

pub const MAX_BLOCKS_IN_A_ROW: usize = 3;
pub const MAX_BLOCKS_IN_A_SESSION: usize = 20;
/// The review endpoint's limits.
const MAX_USER_MESSAGES: usize = 20;
const MAX_MESSAGE_CHARS: usize = 8_000;
const MAX_RECENT_ACTIONS: usize = 40;
const MAX_ACTION_CHARS: usize = 500;
const MAX_DETAIL_CHARS: usize = 40_000;
/// Blocks `/auto` lists, newest last.
const MAX_RECENT_BLOCKS: usize = 20;
/// Blocks the review sees, newest last (the endpoint's limit).
const MAX_REVIEWED_BLOCKS: usize = 5;
/// Paths whose edits change what runs or what gets reviewed: git hooks and
/// config, CI pipelines.
const PROTECTED_PATHS: &[&str] = &[".git/", ".github/", ".gitlab-ci.yml", ".husky/"];

/// What the user sees in the prompt when auto mode is paused.
pub const PAUSED_NOTE: &str =
    "Auto mode is paused after repeated blocks. Approve this to resume it, or deny it.";

/// Whether an action needs the review, or runs at once.
pub fn needs_review(request: &PermissionRequest) -> bool {
    match request.access {
        // A new folder is the user's call: the approver always asks.
        Access::Read | Access::Folder => false,
        Access::Write => request.target.as_deref().is_some_and(is_protected),
        // A fetch can carry conversation data out in its URL.
        Access::Command
        | Access::Script
        | Access::ServerAction
        | Access::Install
        | Access::Trust
        | Access::Fetch => true,
    }
}

fn is_protected(path: &str) -> bool {
    let path = path.trim_start_matches("./");
    PROTECTED_PATHS
        .iter()
        .any(|protected| path == protected.trim_end_matches('/') || path.starts_with(protected))
}

/// What auto mode remembers during a session: the user's messages and the
/// actions taken (what the review sees), and the blocks.
#[derive(Debug, Default)]
pub struct AutoContext {
    user_messages: VecDeque<String>,
    recent_actions: VecDeque<String>,
    blocks_in_a_row: usize,
    blocks_in_session: usize,
    paused: bool,
    recent_blocks: VecDeque<BlockedAction>,
    /// This conversation's newest blocks, for the review: "I authorize it"
    /// can only answer a block the user saw in this conversation.
    conversation_blocks: VecDeque<BlockedAction>,
    /// The server reads recent blocks (`features.actionReviewAuthorizations`).
    review_takes_blocks: bool,
}

impl AutoContext {
    /// Remembers a message the user sent; the review judges actions by them.
    pub fn user_said(&mut self, text: &str) {
        push_capped(
            &mut self.user_messages,
            clip(text, MAX_MESSAGE_CHARS),
            MAX_USER_MESSAGES,
        );
    }

    /// Remembers an action that ran.
    pub fn took(&mut self, summary: &str) {
        push_capped(
            &mut self.recent_actions,
            clip(summary, MAX_ACTION_CHARS),
            MAX_RECENT_ACTIONS,
        );
    }

    /// A new conversation: its messages, actions and reviewed blocks start
    /// empty. Blocks keep counting for the session.
    pub fn new_conversation(&mut self) {
        self.user_messages.clear();
        self.recent_actions.clear();
        self.conversation_blocks.clear();
    }

    /// Whether the review gets the recent blocks, so the user can
    /// authorize a blocked action; only servers that advertise it take them.
    pub fn set_review_takes_blocks(&mut self, takes_blocks: bool) {
        self.review_takes_blocks = takes_blocks;
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// Records a block; true when it pauses auto mode.
    pub fn blocked(&mut self, summary: &str, reason: &str) -> bool {
        self.blocks_in_a_row += 1;
        self.blocks_in_session += 1;
        let block = BlockedAction {
            summary: clip(summary, MAX_ACTION_CHARS),
            reason: clip(reason, MAX_ACTION_CHARS),
        };
        push_capped(
            &mut self.conversation_blocks,
            block.clone(),
            MAX_REVIEWED_BLOCKS,
        );
        push_capped(&mut self.recent_blocks, block, MAX_RECENT_BLOCKS);
        let pause = self.blocks_in_a_row >= MAX_BLOCKS_IN_A_ROW
            || self.blocks_in_session >= MAX_BLOCKS_IN_A_SESSION;
        if pause {
            self.paused = true;
        }
        pause
    }

    /// An action the review allowed: the run of blocks is over.
    pub fn allowed(&mut self) {
        self.blocks_in_a_row = 0;
    }

    /// The user approved a prompt while paused: auto mode carries on, and
    /// the session count starts over once the limit that paused it is met.
    pub fn resume(&mut self) {
        self.paused = false;
        self.blocks_in_a_row = 0;
        if self.blocks_in_session >= MAX_BLOCKS_IN_A_SESSION {
            self.blocks_in_session = 0;
        }
    }

    pub fn recent_blocks(&self) -> impl Iterator<Item = &BlockedAction> {
        self.recent_blocks.iter()
    }

    /// The review of `request`; `None` before the user said anything, as
    /// nothing could back an action yet.
    pub fn review_request(
        &self,
        request: &PermissionRequest,
        workspace: ReviewWorkspace,
        user_rules: AutoRules,
        past_decisions: Vec<PastDecision>,
    ) -> Option<ReviewRequest> {
        if self.user_messages.is_empty() {
            return None;
        }
        Some(ReviewRequest {
            user_messages: self.user_messages.iter().cloned().collect(),
            recent_actions: self.recent_actions.iter().cloned().collect(),
            action: ReviewedAction {
                kind: kind_of(request),
                summary: clip(&request.summary, MAX_ACTION_CHARS),
                detail: request
                    .preview
                    .as_deref()
                    .map(|d| clip(d, MAX_DETAIL_CHARS)),
                requested_by: request
                    .asked_by
                    .as_deref()
                    .map(|a| clip(a, MAX_ACTION_CHARS)),
            },
            workspace,
            user_rules,
            past_decisions,
            recent_blocks: self.reviewed_blocks(),
        })
    }

    /// This conversation's newest blocks, for a server that reads them;
    /// none otherwise.
    fn reviewed_blocks(&self) -> Vec<BlockedAction> {
        if !self.review_takes_blocks {
            return Vec::new();
        }
        self.conversation_blocks.iter().cloned().collect()
    }
}

fn kind_of(request: &PermissionRequest) -> &'static str {
    match request.access {
        Access::Write if request.summary.starts_with("write_file") => "write",
        Access::Read | Access::Write => "edit",
        Access::Command => "command",
        Access::Script => "script",
        Access::ServerAction => "server_action",
        Access::Folder => "folder",
        Access::Install => "install",
        Access::Trust => "trust",
        Access::Fetch => "fetch",
    }
}

fn clip(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

fn push_capped<T>(items: &mut VecDeque<T>, item: T, max: usize) {
    items.push_back(item);
    while items.len() > max {
        items.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(access: Access, summary: &str, target: Option<&str>) -> PermissionRequest {
        PermissionRequest {
            access,
            summary: summary.into(),
            preview: Some("detail".into()),
            chat_scope: String::new(),
            always_scope: None,
            action: None,
            asked_by: None,
            target: target.map(str::to_string),
            note: None,
        }
    }

    fn workspace() -> ReviewWorkspace {
        ReviewWorkspace {
            name: "gz-cli".into(),
            platform: "macos",
        }
    }

    #[test]
    fn reads_and_ordinary_edits_run_at_once_the_rest_is_reviewed() {
        assert!(!needs_review(&request(Access::Read, "read_file a", None)));
        assert!(!needs_review(&request(
            Access::Write,
            "edit_file src/a.rs",
            Some("src/a.rs")
        )));
        for protected in [
            ".git/hooks/pre-commit",
            ".github/workflows/ci.yml",
            ".gitlab-ci.yml",
        ] {
            assert!(
                needs_review(&request(Access::Write, "edit_file x", Some(protected))),
                "{protected}"
            );
        }
        assert!(!needs_review(&request(
            Access::Write,
            "edit_file x",
            Some(".github-notes.md")
        )));
        for access in [Access::Command, Access::Script, Access::ServerAction] {
            assert!(needs_review(&request(access, "x", None)));
        }
    }

    #[test]
    fn three_blocks_in_a_row_pause_and_an_approval_resumes() {
        let mut auto = AutoContext::default();
        assert!(!auto.blocked("a", "no"));
        auto.allowed();
        assert!(!auto.blocked("b", "no"));
        assert!(!auto.blocked("c", "no"));
        assert!(auto.blocked("d", "no"), "third in a row pauses");
        assert!(auto.is_paused());
        auto.resume();
        assert!(!auto.is_paused());
        assert_eq!(auto.recent_blocks().count(), 4);
    }

    #[test]
    fn twenty_blocks_in_a_session_pause_even_with_allows_between() {
        let mut auto = AutoContext::default();
        for i in 0..MAX_BLOCKS_IN_A_SESSION - 1 {
            assert!(!auto.blocked(&format!("{i}"), "no"));
            auto.allowed();
        }
        assert!(auto.blocked("last", "no"));
        auto.resume();
        assert!(
            !auto.blocked("after", "no"),
            "the session count started over"
        );
    }

    #[test]
    fn the_review_sees_messages_actions_and_the_action_but_nothing_before_a_message() {
        let mut auto = AutoContext::default();
        let push = request(Access::Command, "run_command git push", None);
        assert!(auto
            .review_request(&push, workspace(), AutoRules::default(), vec![])
            .is_none());
        auto.user_said("push my branch");
        auto.took("edit_file src/a.rs");
        let review = auto
            .review_request(&push, workspace(), AutoRules::default(), vec![])
            .unwrap();
        assert_eq!(review.user_messages, vec!["push my branch"]);
        assert_eq!(review.recent_actions, vec!["edit_file src/a.rs"]);
        assert_eq!(review.action.kind, "command");
        assert_eq!(review.action.detail.as_deref(), Some("detail"));
        auto.new_conversation();
        assert!(auto
            .review_request(&push, workspace(), AutoRules::default(), vec![])
            .is_none());
    }

    #[test]
    fn this_conversations_newest_five_blocks_are_reviewed_only_when_the_server_reads_them() {
        let mut auto = AutoContext::default();
        auto.user_said("rebase onto main");
        for i in 0..7 {
            auto.blocked(&format!("run_command git push -f {i}"), &"no ".repeat(300));
        }
        let push = request(Access::Command, "run_command git push -f 6", None);
        let review = |auto: &AutoContext| {
            auto.review_request(&push, workspace(), AutoRules::default(), vec![])
                .unwrap()
        };
        assert!(review(&auto).recent_blocks.is_empty(), "not advertised");

        auto.set_review_takes_blocks(true);
        let blocks = review(&auto).recent_blocks;
        let summaries: Vec<&str> = blocks.iter().map(|b| b.summary.as_str()).collect();
        assert_eq!(
            summaries,
            (2..7)
                .map(|i| format!("run_command git push -f {i}"))
                .collect::<Vec<_>>(),
            "newest five, newest last"
        );
        assert!(blocks
            .iter()
            .all(|b| b.reason.chars().count() <= MAX_ACTION_CHARS));

        auto.new_conversation();
        auto.user_said("I authorize it");
        assert!(
            review(&auto).recent_blocks.is_empty(),
            "an earlier conversation's blocks are not authorized"
        );
        assert_eq!(auto.recent_blocks().count(), 7, "/auto still lists them");
    }

    #[test]
    fn long_inputs_are_clipped_to_the_endpoint_limits() {
        let mut auto = AutoContext::default();
        for i in 0..30 {
            auto.user_said(&format!("{i} {}", "x".repeat(9_000)));
        }
        let review = auto
            .review_request(
                &request(Access::Script, "run_script python", None),
                workspace(),
                AutoRules::default(),
                vec![],
            )
            .unwrap();
        assert_eq!(review.user_messages.len(), MAX_USER_MESSAGES);
        assert!(review
            .user_messages
            .iter()
            .all(|m| m.chars().count() <= MAX_MESSAGE_CHARS));
        assert!(
            review.user_messages[0].starts_with("10 "),
            "the oldest are dropped"
        );
    }
}
