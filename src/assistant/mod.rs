//! The local assistant: Groundzero chat that can use this machine's files
//! and shell through the chat server's client-tool catalog, with the user's
//! permission. See `session` for the API front ends use.

pub mod action_review;
pub mod attachments;
pub mod audit;
pub mod auto_rules;
pub mod background;
pub mod chat_grants;
pub mod checkpoints;
pub mod compaction;
pub mod diagnostics;
pub mod edit_text;
pub mod environment;
pub mod git_guard;
pub mod grants;
pub mod history;
pub mod hooks;
pub mod instruction_imports;
pub mod instruction_rules;
pub mod instructions;
pub mod interrupt;
pub mod jobs;
pub mod local_context;
pub mod mcp_local;
pub mod memory;
pub mod models;
pub mod notebook;
pub mod os_sandbox;
pub mod output_spill;
pub mod permissions;
pub mod private_file;
pub mod questions;
pub mod read_ledger;
pub mod read_only;
pub mod rules;
pub mod run_controls;
pub mod sandbox;
pub mod saved_rules;
pub mod script;
pub mod search;
pub mod secret_text;
pub mod session;
pub mod session_tools;
pub mod steering;
pub mod subagent_settings;
pub mod todos;
pub mod tools;
pub mod web_fetch;

pub use permissions::{Approver, Decision, DecisionFuture, PermissionRequest};
pub use session::{AssistantEvent, Canceller, Session, TurnEnd};
