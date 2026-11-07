//! The local assistant: Groundzero chat that can use this machine's files
//! and shell through the chat server's client-tool catalog, with the user's
//! permission. See `session` for the API front ends use.

pub mod attachments;
pub mod audit;
pub mod grants;
pub mod models;
pub mod permissions;
pub mod sandbox;
pub mod saved_rules;
pub mod script;
pub mod session;
pub mod tools;

pub use permissions::{Approver, Decision, DecisionFuture, PermissionRequest};
pub use session::{AssistantEvent, Canceller, Session, TurnEnd};
