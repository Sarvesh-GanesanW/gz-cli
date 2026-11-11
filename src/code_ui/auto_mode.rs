//! Auto mode: gz runs actions without asking when they are routine or when
//! a review (see `crate::assistant::action_review`) finds the user's request
//! backs them, and blocks the rest with a reason the model receives.
//!
//! Reads and edits inside the folder run at once; edits to files that
//! change how code is run or reviewed (`.git/`, CI config) and every
//! command, script and Groundzero action go to the review. Three blocks in
//! a row or twenty in a session pause auto mode until the user approves a
//! prompted action.
//!
//! A block is not the last word: the user can let that same action run
//! (`/allow`, see [`AutoContext::allow_blocked`]), and it then runs once
//! without another review.

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
        | Access::Fetch
        | Access::LocalTool => true,
    }
}

fn is_protected(path: &str) -> bool {
    let path = path.trim_start_matches("./");
    PROTECTED_PATHS
        .iter()
        .any(|protected| path == protected.trim_end_matches('/') || path.starts_with(protected))
}

/// What makes two requests the same action: its kind, its summary line and
/// its whole detail (the command line, script, diff or server action
/// input), unclipped.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ActionKey {
    kind: &'static str,
    summary: String,
    detail: Option<String>,
}

impl ActionKey {
    /// `None` for a Groundzero action that came without its input: its
    /// summary names the tool, so it can't be told from another call of it.
    fn of(request: &PermissionRequest) -> Option<Self> {
        if request.access == Access::ServerAction && request.preview.is_none() {
            return None;
        }
        Some(Self {
            kind: kind_of(request),
            summary: request.summary.clone(),
            detail: request.preview.clone(),
        })
    }
}

/// A block, with the action itself so the user can allow exactly that one.
#[derive(Debug, Clone)]
struct Block {
    /// What `/auto` lists and the review reads.
    shown: BlockedAction,
    /// `None` when the action can't be recognised again (see
    /// [`ActionKey::of`]); allowing it then rests on the review.
    action: Option<ActionKey>,
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
    recent_blocks: VecDeque<Block>,
    /// Blocked actions the user allowed by hand; each runs once.
    user_allowed: Vec<ActionKey>,
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
    /// empty, and what the user allowed in the last one is over. Blocks
    /// keep counting for the session.
    pub fn new_conversation(&mut self) {
        self.user_messages.clear();
        self.recent_actions.clear();
        self.conversation_blocks.clear();
        self.user_allowed.clear();
    }

    /// Whether the review gets the recent blocks, so the user can
    /// authorize a blocked action; only servers that advertise it take them.
    pub fn set_review_takes_blocks(&mut self, takes_blocks: bool) {
        self.review_takes_blocks = takes_blocks;
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// Records that the review blocked `request`; true when it pauses auto
    /// mode.
    pub fn blocked(&mut self, request: &PermissionRequest, reason: &str) -> bool {
        self.blocks_in_a_row += 1;
        self.blocks_in_session += 1;
        let shown = BlockedAction {
            summary: clip(&request.summary, MAX_ACTION_CHARS),
            reason: clip(reason, MAX_ACTION_CHARS),
        };
        push_capped(
            &mut self.conversation_blocks,
            shown.clone(),
            MAX_REVIEWED_BLOCKS,
        );
        let block = Block {
            shown,
            action: ActionKey::of(request),
        };
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
        self.recent_blocks.iter().map(|block| &block.shown)
    }

    /// The user lets a blocked action run (`/allow`): the newest, or the
    /// one `/auto` numbers `number`. `None` when there is no such block.
    pub fn allow_blocked(&mut self, number: Option<usize>) -> Option<BlockedAction> {
        let block = match number {
            Some(number) => self.recent_blocks.get(number.checked_sub(1)?)?,
            None => self.recent_blocks.back()?,
        };
        if let Some(action) = &block.action {
            if !self.user_allowed.contains(action) {
                self.user_allowed.push(action.clone());
            }
        }
        Some(block.shown.clone())
    }

    /// Whether the user allowed exactly this action after a block. It
    /// counts once: running it again is a new action.
    pub fn take_allowance(&mut self, request: &PermissionRequest) -> bool {
        let Some(action) = ActionKey::of(request) else {
            return false;
        };
        let allowed = self.user_allowed.iter().position(|a| *a == action);
        if let Some(at) = allowed {
            self.user_allowed.remove(at);
        }
        allowed.is_some()
    }

    /// What the review reads as the user's words, oldest first.
    #[cfg(test)]
    pub fn user_messages(&self) -> Vec<String> {
        self.user_messages.iter().cloned().collect()
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
        Access::LocalTool => "local_tool",
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

    fn command(summary: &str) -> PermissionRequest {
        request(Access::Command, summary, None)
    }

    /// A command request with its own command line.
    fn command_line(line: &str) -> PermissionRequest {
        PermissionRequest {
            preview: Some(line.into()),
            ..command(&format!("run_command {line}"))
        }
    }

    #[test]
    fn the_user_can_allow_a_blocked_action_and_it_runs_once() {
        let mut auto = AutoContext::default();
        assert!(auto.allow_blocked(None).is_none(), "nothing was blocked");

        let scan = command_line("find / -type f > /tmp/all.txt");
        let push = command_line("git push -f");
        auto.blocked(&scan, "Leaves the folder.");
        auto.blocked(&push, "Not asked for.");
        assert!(!auto.take_allowance(&push), "a block alone allows nothing");

        assert_eq!(
            auto.allow_blocked(None).unwrap().summary,
            "run_command git push -f",
            "the newest block by default"
        );
        assert!(
            !auto.take_allowance(&scan),
            "the other blocked action stays blocked"
        );
        assert!(
            !auto.take_allowance(&command_line("git push -f origin main")),
            "a different command line is a different action"
        );
        assert!(auto.take_allowance(&push));
        assert!(!auto.take_allowance(&push), "allowed once, not from now on");

        assert_eq!(
            auto.allow_blocked(Some(1)).unwrap().summary,
            "run_command find / -type f > /tmp/all.txt",
            "numbered as /auto lists them"
        );
        assert!(auto.allow_blocked(Some(0)).is_none());
        assert!(auto.allow_blocked(Some(3)).is_none());
        auto.new_conversation();
        assert!(
            !auto.take_allowance(&scan),
            "what was allowed ends with its conversation"
        );
    }

    #[test]
    fn a_groundzero_action_without_its_input_is_never_allowed_unreviewed() {
        let mut auto = AutoContext::default();
        let drop_table = PermissionRequest {
            preview: None,
            ..request(
                Access::ServerAction,
                "Groundzero wants to run drop_table",
                None,
            )
        };
        auto.blocked(&drop_table, "Not asked for.");
        assert!(
            auto.allow_blocked(None).is_some(),
            "the user's word still goes to the review"
        );
        assert!(
            !auto.take_allowance(&drop_table),
            "another call of the tool would look the same"
        );

        let with_input = PermissionRequest {
            preview: Some(r#"{"table": "sales.orders_tmp"}"#.into()),
            ..drop_table.clone()
        };
        auto.blocked(&with_input, "Not asked for.");
        auto.allow_blocked(None).unwrap();
        let other_table = PermissionRequest {
            preview: Some(r#"{"table": "sales.orders"}"#.into()),
            ..drop_table
        };
        assert!(!auto.take_allowance(&other_table));
        assert!(auto.take_allowance(&with_input));
    }

    #[test]
    fn a_long_command_is_allowed_by_its_whole_line_not_its_clipped_summary() {
        let mut auto = AutoContext::default();
        let long = "x".repeat(MAX_ACTION_CHARS * 2);
        let blocked = command_line(&format!("echo {long} && rm -rf build"));
        auto.blocked(&blocked, "no");
        auto.allow_blocked(None).unwrap();
        // Same first 500 characters, a different ending.
        let other = command_line(&format!("echo {long} && rm -rf ~"));
        assert!(!auto.take_allowance(&other));
        assert!(auto.take_allowance(&blocked));
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
        assert!(!auto.blocked(&command("a"), "no"));
        auto.allowed();
        assert!(!auto.blocked(&command("b"), "no"));
        assert!(!auto.blocked(&command("c"), "no"));
        assert!(auto.blocked(&command("d"), "no"), "third in a row pauses");
        assert!(auto.is_paused());
        auto.resume();
        assert!(!auto.is_paused());
        assert_eq!(auto.recent_blocks().count(), 4);
    }

    #[test]
    fn twenty_blocks_in_a_session_pause_even_with_allows_between() {
        let mut auto = AutoContext::default();
        for i in 0..MAX_BLOCKS_IN_A_SESSION - 1 {
            assert!(!auto.blocked(&command(&format!("{i}")), "no"));
            auto.allowed();
        }
        assert!(auto.blocked(&command("last"), "no"));
        auto.resume();
        assert!(
            !auto.blocked(&command("after"), "no"),
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
            auto.blocked(
                &command(&format!("run_command git push -f {i}")),
                &"no ".repeat(300),
            );
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
