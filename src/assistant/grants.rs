//! Everything gz will do without asking: saved local rules
//! ([`saved_rules`]) plus the chat server's standing approvals
//! (`GET/DELETE /chat/approval-grants`, gz-chat-eks
//! api/routes/approval_grants.py). Listed with short ids — `L1` local,
//! `C1` this chat, `A1` always — so `gz approvals revoke` and `/permissions`
//! can name one.

use std::path::Path;

use anyhow::{bail, Context, Result};
use reqwest::Method;
use serde_json::{json, Value};

use crate::assistant::saved_rules::{SavedRule, Store};
use crate::http::ApiClient;

const CHAT: &str = "chat";
const GRANTS: &str = "/chat/approval-grants";

/// A server grant: one tool the user isn't asked about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerGrant {
    pub tool_name: String,
    /// For MCP tools, the connection and its own tool name.
    pub detail: Option<String>,
    pub granted_at: Option<String>,
}

impl ServerGrant {
    fn from_value(value: &Value) -> Option<Self> {
        let text = |key: &str| value[key].as_str().map(str::to_string);
        let detail = match (text("connectionName"), text("externalToolName")) {
            (Some(connection), Some(tool)) => Some(format!("{connection} · {tool}")),
            (Some(connection), None) => Some(connection),
            _ => None,
        };
        Some(Self {
            tool_name: text("toolName")?,
            detail,
            granted_at: text("grantedAt"),
        })
    }

    fn label(&self) -> String {
        match &self.detail {
            Some(detail) => format!("{} ({detail})", self.tool_name),
            None => self.tool_name.clone(),
        }
    }
}

/// Everything allowed without asking, as one numbered list.
#[derive(Debug, Default)]
pub struct Listing {
    pub local: Vec<SavedRule>,
    pub conversation: Vec<ServerGrant>,
    pub always: Vec<ServerGrant>,
    /// Why the server's grants couldn't be listed, if they couldn't.
    pub server_error: Option<String>,
    /// The conversation the `C` grants belong to.
    conversation_id: Option<String>,
}

/// One row of the listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub id: String,
    pub scope: &'static str,
    pub text: String,
}

impl Listing {
    pub fn entries(&self) -> Vec<Entry> {
        let mut out = Vec::new();
        for (i, saved) in self.local.iter().enumerate() {
            out.push(Entry {
                id: format!("L{}", i + 1),
                scope: "local · always",
                text: format!("{} (in {})", saved.rule, saved.workspace.display()),
            });
        }
        for (i, grant) in self.conversation.iter().enumerate() {
            out.push(Entry {
                id: format!("C{}", i + 1),
                scope: "chat · this chat",
                text: grant.label(),
            });
        }
        for (i, grant) in self.always.iter().enumerate() {
            out.push(Entry {
                id: format!("A{}", i + 1),
                scope: "chat · always",
                text: grant.label(),
            });
        }
        out
    }

    /// Lines for `/permissions` and `gz approvals list`.
    pub fn report(&self) -> Vec<String> {
        let entries = self.entries();
        let mut lines = vec!["Allowed without asking".to_string()];
        if entries.is_empty() {
            lines.push(
                "  nothing yet — answer a prompt with \"for this chat\" or \"always\"".into(),
            );
        }
        for entry in &entries {
            lines.push(format!(
                "  {:<4}{:<18}{}",
                entry.id, entry.scope, entry.text
            ));
        }
        if let Some(error) = &self.server_error {
            lines.push(format!("  (chat approvals unavailable: {error})"));
        }
        if !entries.is_empty() {
            lines.push(String::new());
            lines.push(
                "  revoke one with /permissions revoke <id>  (gz approvals revoke <id>)".into(),
            );
        }
        lines
    }
}

/// Lists local rules for `root` (or every folder) and the server's grants
/// (this conversation's too, when given). A server failure is reported in
/// the listing rather than failing it.
pub async fn load(
    http: &ApiClient,
    root: Option<&Path>,
    conversation: Option<&str>,
) -> Result<Listing> {
    let local = Store::open()?.list(root)?;
    let query: Vec<(String, String)> = conversation
        .map(|id| vec![("conversationId".to_string(), id.to_string())])
        .unwrap_or_default();
    let mut listing = Listing {
        local,
        conversation_id: conversation.map(str::to_string),
        ..Listing::default()
    };
    match http
        .request_json(CHAT, Method::GET, GRANTS, &query, None)
        .await
    {
        Ok(reply) => {
            let grants = |key: &str| -> Vec<ServerGrant> {
                reply[key]
                    .as_array()
                    .map(|rows| rows.iter().filter_map(ServerGrant::from_value).collect())
                    .unwrap_or_default()
            };
            listing.conversation = grants("conversation");
            listing.always = grants("always");
        }
        Err(err) => listing.server_error = Some(crate::http::error_detail(&format!("{err:#}"))),
    }
    Ok(listing)
}

/// Revokes the entry with `id` from `listing`; returns what was revoked.
pub async fn revoke(http: &ApiClient, listing: &Listing, id: &str) -> Result<String> {
    let id = id.trim().to_ascii_uppercase();
    let (kind, number) = id.split_at(1.min(id.len()));
    let index = number
        .parse::<usize>()
        .ok()
        .and_then(|n| n.checked_sub(1))
        .with_context(|| format!("'{id}' isn't an id from the list (e.g. L1, C1, A1)"))?;
    let missing = || anyhow::anyhow!("there's no {id} in the list; list again to see current ids");
    match kind {
        "L" => {
            let saved = listing.local.get(index).ok_or_else(missing)?;
            if !Store::open()?.revoke(saved)? {
                bail!("{id} was already gone");
            }
            Ok(format!("{} (in {})", saved.rule, saved.workspace.display()))
        }
        "C" | "A" => {
            let (grants, scope) = if kind == "C" {
                (&listing.conversation, "conversation")
            } else {
                (&listing.always, "always")
            };
            let grant = grants.get(index).ok_or_else(missing)?;
            let mut body = json!({"scope": scope, "toolName": grant.tool_name});
            if scope == "conversation" {
                let conversation = listing
                    .conversation_id
                    .as_deref()
                    .context("this-chat grants need the conversation id")?;
                body["conversationId"] = json!(conversation);
            }
            let reply = http
                .request_json(CHAT, Method::DELETE, GRANTS, &[], Some(body))
                .await?;
            if reply["revoked"] == Value::Bool(false) {
                bail!("{id} was already gone");
            }
            Ok(grant.label())
        }
        _ => bail!("'{id}' isn't an id from the list (e.g. L1, C1, A1)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant::saved_rules::Rule;
    use std::path::PathBuf;

    fn listing() -> Listing {
        Listing {
            local: vec![SavedRule {
                workspace: PathBuf::from("/work/app"),
                rule: Rule::Command("cargo test".into()),
            }],
            conversation: vec![ServerGrant {
                tool_name: "run_query".into(),
                detail: None,
                granted_at: None,
            }],
            always: vec![ServerGrant::from_value(&json!({
                "toolName": "mcp_call", "connectionName": "jira", "externalToolName": "create_issue",
                "grantedAt": "2026-11-07T10:00:00Z"
            }))
            .unwrap()],
            server_error: None,
            conversation_id: Some("c1".into()),
        }
    }

    #[test]
    fn numbers_local_chat_and_always_entries() {
        let ids: Vec<String> = listing().entries().into_iter().map(|e| e.id).collect();
        assert_eq!(ids, vec!["L1", "C1", "A1"]);
        let report = listing().report().join("\n");
        assert!(
            report.contains("command `cargo test` (in /work/app)"),
            "{report}"
        );
        assert!(
            report.contains("mcp_call (jira · create_issue)"),
            "{report}"
        );
    }

    #[test]
    fn empty_listing_explains_how_to_add() {
        let report = Listing::default().report().join("\n");
        assert!(report.contains("nothing yet"));
    }
}
