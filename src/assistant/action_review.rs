//! Auto mode's review of one action: gz-chat-eks `POST /chat/action-review`
//! (api/routes/chat/action_review.py) decides from the user's messages, the
//! actions already taken, the user's rules and past answers whether the
//! action may run without a prompt. It never sees tool output.

use std::future::Future;
use std::pin::Pin;

use anyhow::{bail, Context, Result};
use reqwest::Method;
use serde::Serialize;
use serde_json::Value;

use crate::assistant::auto_rules::{AutoRules, PastDecision};
use crate::http::ApiClient;

const CHAT: &str = "chat";
const REVIEW_PATH: &str = "/chat/action-review";

/// Everything the review sees. Field names are the endpoint's.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewRequest {
    pub user_messages: Vec<String>,
    pub recent_actions: Vec<String>,
    pub action: ReviewedAction,
    pub workspace: ReviewWorkspace,
    pub user_rules: AutoRules,
    pub past_decisions: Vec<PastDecision>,
}

/// The action the agent wants to take.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewedAction {
    /// `edit`, `write`, `command`, `script` or `server_action`.
    pub kind: &'static str,
    pub summary: String,
    /// The diff, command line, script or server action input.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_by: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReviewWorkspace {
    pub name: String,
    pub platform: &'static str,
}

/// The review's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    /// Blocked, with one sentence on why.
    Block(String),
}

pub type ReviewFuture<'a> = Pin<Box<dyn Future<Output = Result<Verdict>> + Send + 'a>>;

/// Reviews actions for auto mode; the HTTP one in use, a scripted one in tests.
pub trait ActionReviewer: Send + Sync {
    fn review<'a>(&'a self, request: &'a ReviewRequest) -> ReviewFuture<'a>;
}

/// Reviews through the chat backend.
pub struct HttpReviewer {
    http: ApiClient,
}

impl HttpReviewer {
    pub fn new(http: ApiClient) -> Self {
        Self { http }
    }
}

impl ActionReviewer for HttpReviewer {
    fn review<'a>(&'a self, request: &'a ReviewRequest) -> ReviewFuture<'a> {
        Box::pin(async move {
            let body = serde_json::to_value(request).context("cannot encode the review")?;
            let reply = self
                .http
                .request_json(CHAT, Method::POST, REVIEW_PATH, &[], Some(body))
                .await?;
            verdict_from(&reply)
        })
    }
}

/// Reads `{"decision": "allow" | "block", "reason": "..."}`.
pub fn verdict_from(reply: &Value) -> Result<Verdict> {
    let reason = reply["reason"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_string();
    match reply["decision"].as_str() {
        Some("allow") => Ok(Verdict::Allow),
        Some("block") if !reason.is_empty() => Ok(Verdict::Block(reason)),
        _ => bail!("the review gave no usable answer"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant::auto_rules::Answer;
    use serde_json::json;

    #[test]
    fn reads_allow_and_block_and_refuses_anything_else() {
        assert_eq!(
            verdict_from(&json!({"decision": "allow", "reason": "Asked for."})).unwrap(),
            Verdict::Allow
        );
        assert_eq!(
            verdict_from(&json!({"decision": "block", "reason": " Force push. "})).unwrap(),
            Verdict::Block("Force push.".into())
        );
        assert!(verdict_from(&json!({"decision": "block"})).is_err());
        assert!(verdict_from(&json!({"decision": "maybe", "reason": "x"})).is_err());
    }

    #[test]
    fn the_request_matches_the_endpoint() {
        let request = ReviewRequest {
            user_messages: vec!["fix the tests".into()],
            recent_actions: vec![],
            action: ReviewedAction {
                kind: "command",
                summary: "run_command cargo test".into(),
                detail: Some("cargo test".into()),
                requested_by: None,
            },
            workspace: ReviewWorkspace {
                name: "gz-cli".into(),
                platform: "macos",
            },
            user_rules: AutoRules {
                allow: vec!["make docs".into()],
                ..AutoRules::default()
            },
            past_decisions: vec![PastDecision {
                action: "run_command git push".into(),
                decision: Answer::Denied,
            }],
        };
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            json!({
                "userMessages": ["fix the tests"],
                "recentActions": [],
                "action": {"kind": "command", "summary": "run_command cargo test", "detail": "cargo test"},
                "workspace": {"name": "gz-cli", "platform": "macos"},
                "userRules": {"allow": ["make docs"], "deny": [], "environment": []},
                "pastDecisions": [{"action": "run_command git push", "decision": "denied"}],
            })
        );
    }
}
