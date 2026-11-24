//! Everything gz will do without asking: allow/ask/deny rules
//! ([`crate::assistant::rules`]), saved local answers ([`saved_rules`])
//! plus the chat server's standing approvals
//! (`GET/DELETE /chat/approval-grants`, gz-chat-eks
//! api/routes/approval_grants.py). Listed with short ids — `R1` rule,
//! `L1` local, `C1` this chat, `A1` always — so `gz approvals revoke` and `/permissions`
//! can name one.

use std::path::Path;

use anyhow::{bail, Context, Result};
use reqwest::Method;
use serde_json::{json, Value};

use crate::assistant::rules::{remove_rule, RuleSet};
use crate::assistant::saved_rules::{SavedRule, Store};
use crate::code_ui::panel::{Panel, Row};
use crate::http::ApiClient;

const CHAT: &str = "chat";
const RULES: &str = "rules";
const LOCAL_ALWAYS: &str = "local · always";
const CHAT_THIS: &str = "chat · this chat";
const CHAT_ALWAYS: &str = "chat · always";
/// Each entry scope with the heading its `/permissions` section gets.
const SCOPE_HEADINGS: [(&str, &str); 4] = [
    (RULES, "Rules (deny beats ask beats allow)"),
    (LOCAL_ALWAYS, "Saved on this machine"),
    (CHAT_THIS, "This chat"),
    (CHAT_ALWAYS, "Every chat"),
];
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
    /// The allow/ask/deny rules, when the caller has them.
    pub rules: Option<RuleSet>,
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
    /// This listing with `rules` shown first.
    pub fn with_rules(mut self, rules: RuleSet) -> Self {
        self.rules = Some(rules);
        self
    }

    pub fn entries(&self) -> Vec<Entry> {
        let mut out = Vec::new();
        let rules = self.rules.as_ref().map(RuleSet::rules).unwrap_or_default();
        for (i, rule) in rules.iter().enumerate() {
            let waiting = if rule.active {
                ""
            } else {
                " · waits for /permissions trust"
            };
            out.push(Entry {
                id: format!("R{}", i + 1),
                scope: RULES,
                text: format!("{rule} · {}{waiting}", rule.source.label()),
            });
        }
        for (i, saved) in self.local.iter().enumerate() {
            out.push(Entry {
                id: format!("L{}", i + 1),
                scope: LOCAL_ALWAYS,
                text: format!("{} (in {})", saved.rule, saved.workspace.display()),
            });
        }
        for (i, grant) in self.conversation.iter().enumerate() {
            out.push(Entry {
                id: format!("C{}", i + 1),
                scope: CHAT_THIS,
                text: grant.label(),
            });
        }
        for (i, grant) in self.always.iter().enumerate() {
            out.push(Entry {
                id: format!("A{}", i + 1),
                scope: CHAT_ALWAYS,
                text: grant.label(),
            });
        }
        out
    }

    /// The `/permissions` and `gz approvals list` panel, one section per
    /// scope.
    pub fn report(&self) -> Panel {
        let entries = self.entries();
        let mut panel = Panel::new("Permissions").aside("allowed without asking");
        if entries.is_empty() {
            panel = panel.section(
                None,
                vec![Row::note(
                    "Nothing yet. Answer a prompt with \"for this chat\" or \"always\", \
                     or grant something up front with /permissions allow <what>.",
                )],
            );
        }
        for (scope, heading) in SCOPE_HEADINGS {
            let rows = entries
                .iter()
                .filter(|e| e.scope == scope)
                .map(|e| Row::entry(&e.id, e.text.clone()))
                .collect();
            panel = panel.section(Some(heading), rows);
        }
        let problems = self.rules.iter().flat_map(|r| r.problems.iter());
        let problem_rows: Vec<Row> = problems.map(|p| Row::note(p.clone())).collect();
        if !problem_rows.is_empty() {
            panel = panel.section(Some("Rules gz couldn't read"), problem_rows);
        }
        if let Some(error) = &self.server_error {
            panel = panel.section(
                None,
                vec![Row::note(format!("Chat approvals unavailable: {error}"))],
            );
        }
        if !entries.is_empty() {
            panel = panel.footer(
                "/permissions allow|ask|deny [user|project|local] <rule> adds a rule · \
                 /permissions revoke <id> takes one back · /permissions trust trusts the \
                 project's rules",
            );
        }
        panel
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
        .with_context(|| format!("'{id}' isn't an id from the list (e.g. R1, L1, C1, A1)"))?;
    let missing = || anyhow::anyhow!("there's no {id} in the list; list again to see current ids");
    match kind {
        "R" => {
            let rules = listing.rules.as_ref().ok_or_else(missing)?;
            let rule = rules.rules().get(index).ok_or_else(missing)?;
            let places = rules
                .places()
                .context("these rules have no files to change")?;
            if !remove_rule(places, rule.source, rule.kind, &rule.pattern.to_string())? {
                bail!("{id} was already gone");
            }
            Ok(format!("{rule} ({})", rule.source.label()))
        }
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
        _ => bail!("'{id}' isn't an id from the list (e.g. R1, L1, C1, A1)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant::saved_rules::Rule;
    use std::path::PathBuf;

    fn listing() -> Listing {
        Listing {
            rules: None,
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
        let report = listing().report().plain_lines().join("\n");
        assert!(
            report.contains("command `cargo test` (in /work/app)"),
            "{report}"
        );
        assert!(
            report.contains("mcp_call (jira · create_issue)"),
            "{report}"
        );
    }

    #[tokio::test]
    async fn rules_are_numbered_with_their_source_and_revoked_by_id() {
        use crate::assistant::rules::{add_rule, FlagRules, Kind, Places, Scope};
        let dir = crate::assistant::sandbox::tests::temp_dir("grants-rules");
        let places = Places {
            folder: dir.clone(),
            home: None,
            user_file: Some(dir.join("user.toml")),
            trust: Some(crate::assistant::hooks::trust::TrustStore::at(
                dir.join("trust.json"),
            )),
        };
        std::fs::create_dir_all(dir.join(".gz")).unwrap();
        std::fs::write(dir.join(".gz/permissions.toml"), "allow = [\"run_script\"]").unwrap();
        add_rule(&places, Scope::User, Kind::Deny, "run_command(rm:*)").unwrap();
        let flags = FlagRules::new(&["read".into()], &[]).unwrap();
        let load = || Listing::default().with_rules(RuleSet::load(places.clone(), flags.clone()));
        let listing = load();
        let rows: Vec<String> = listing
            .entries()
            .iter()
            .map(|e| format!("{} {}", e.id, e.text))
            .collect();
        assert_eq!(
            rows,
            [
                "R1 allow read · command line",
                "R2 allow run_script · project · waits for /permissions trust",
                "R3 deny run_command(rm:*) · user",
            ]
        );
        let report = listing.report().plain_lines().join("\n");
        assert!(
            report.contains("Rules (deny beats ask beats allow)"),
            "{report}"
        );

        let resolved = crate::config::resolve(
            &crate::config::Config::default(),
            Some("grants-test"),
            Some("http://127.0.0.1:9".into()),
            Some("stub-token".to_string()),
        );
        let http = ApiClient::new(resolved, crate::output::OutputFormat::Json, true, 0, 5).unwrap();
        let revoked = revoke(&http, &listing, "r3").await.unwrap();
        assert_eq!(revoked, "deny run_command(rm:*) (user)");
        assert_eq!(load().entries().len(), 2);
        let err = revoke(&http, &load(), "r1").await.unwrap_err();
        assert!(format!("{err:#}").contains("this session only"), "{err:#}");
    }

    #[test]
    fn empty_listing_explains_how_to_add() {
        let report = Listing::default().report().plain_lines().join("\n");
        assert!(report.contains("Nothing yet"));
    }
}
