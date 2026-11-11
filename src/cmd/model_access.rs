//! `gz govern model-access`: what AI models may read, per data level.
//!
//! The chat server filters every tool result by the site's model read
//! policy before a model sees it: per classification level (and for
//! unclassified and ungoverned sources), Groundzero models and
//! customer-brought models each `allow`, `mask` sensitive columns, or `deny`.
//! Reading needs AI chat access; changing needs the site's Admin group.

use anyhow::{Context, Result};
use clap::{Args, Subcommand, ValueEnum};
use reqwest::Method;
use serde_json::{json, Value};

use crate::cli::Confirm;
use crate::cmd::Runtime;
use crate::output::{self, OutputFormat};

const CHAT: &str = "chat";
const POLICY_PATH: &str = "/admin/model-read-policy";
/// Display order: the lakehouse classifications, then the extra levels.
const LEVELS: [Level; 6] = [
    Level::Public,
    Level::Internal,
    Level::Confidential,
    Level::Restricted,
    Level::Unclassified,
    Level::Ungoverned,
];

#[derive(Args, Debug)]
#[command(
    about = "What AI models may read per data classification: allow, mask, or deny",
    long_about = "Shows the site's model read policy. The chat server applies it to every \
tool result before a model reads it: per level, Groundzero models and customer-brought \
models (your own LLM connections, and AI tools connected through `gz mcp`) may read the \
data (allow), get its sensitive columns masked (mask), or not get it at all (deny).\n\n\
Levels are the lakehouse classifications, plus `unclassified` (a lakehouse table with no \
classification) and `ungoverned` (external connections, uploaded CSVs, MCP connections)."
)]
pub struct ModelAccessArgs {
    #[command(subcommand)]
    pub action: Option<ModelAccessAction>,
}

#[derive(Subcommand, Debug)]
pub enum ModelAccessAction {
    #[command(about = "Change one cell of the policy (site admins)")]
    Set {
        #[arg(value_enum, help = "Which models: groundzero or customer")]
        model: ModelKind,
        #[arg(value_enum, help = "The data level")]
        level: Level,
        #[arg(
            value_enum,
            help = "allow, mask (sensitive columns), or deny (no rows)"
        )]
        action: ReadAction,
        #[command(flatten)]
        confirm: Confirm,
    },
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelKind {
    Groundzero,
    Customer,
}

impl ModelKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Groundzero => "groundzero",
            Self::Customer => "customer",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Groundzero => "Groundzero models",
            Self::Customer => "customer-brought models",
        }
    }
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Public,
    Internal,
    Confidential,
    Restricted,
    Unclassified,
    Ungoverned,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Internal => "internal",
            Self::Confidential => "confidential",
            Self::Restricted => "restricted",
            Self::Unclassified => "unclassified",
            Self::Ungoverned => "ungoverned",
        }
    }
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadAction {
    Allow,
    Mask,
    Deny,
}

impl ReadAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Mask => "mask",
            Self::Deny => "deny",
        }
    }
}

pub async fn run(rt: &Runtime, args: &ModelAccessArgs) -> Result<()> {
    let current = fetch(rt).await?;
    match &args.action {
        None => show(rt, &current),
        Some(ModelAccessAction::Set {
            model,
            level,
            action,
            confirm,
        }) => {
            let policy = with_cell(&current, *model, *level, *action)?;
            crate::cmd::confirm(
                confirm.yes,
                &format!(
                    "Set {} data to {} for {} on this site?",
                    level.as_str(),
                    action.as_str(),
                    model.label()
                ),
            )?;
            let saved = rt
                .http
                .request_json(CHAT, Method::PUT, POLICY_PATH, &[], Some(policy))
                .await?;
            show(rt, &saved)
        }
    }
}

async fn fetch(rt: &Runtime) -> Result<Value> {
    rt.http
        .request_json(CHAT, Method::GET, POLICY_PATH, &[], None)
        .await
}

/// The saved policy with one cell changed; the server takes it whole.
fn with_cell(current: &Value, model: ModelKind, level: Level, action: ReadAction) -> Result<Value> {
    let mut policy = current
        .get("policy")
        .cloned()
        .context("the chat server's answer has no policy; is it up to date?")?;
    let actions = policy
        .get_mut(model.as_str())
        .and_then(Value::as_object_mut)
        .with_context(|| format!("the policy has no {} entry", model.as_str()))?;
    actions.insert(level.as_str().to_string(), json!(action.as_str()));
    Ok(policy)
}

/// One row per level, a column per kind of model.
fn rows(reply: &Value) -> Value {
    let policy = &reply["policy"];
    let row = |level: Level| {
        let cell = |model: ModelKind| policy[model.as_str()][level.as_str()].clone();
        json!({
            "level": level.as_str(),
            "groundzero": cell(ModelKind::Groundzero),
            "customer": cell(ModelKind::Customer),
        })
    };
    Value::Array(LEVELS.iter().map(|level| row(*level)).collect())
}

fn origin(reply: &Value) -> String {
    let site = reply["site"].as_str().unwrap_or("this site");
    if reply["isDefault"].as_bool().unwrap_or(false) {
        return format!("Model read policy for {site}: the defaults (no admin has changed it)");
    }
    let by = reply["updatedBy"].as_str().unwrap_or("an admin");
    let at = reply["updatedAt"].as_str().unwrap_or("an unknown time");
    format!("Model read policy for {site}: set by {by} at {at}")
}

fn show(rt: &Runtime, reply: &Value) -> Result<()> {
    if rt.format != OutputFormat::Table {
        return output::print_value(reply, rt.format, rt.limit);
    }
    output::info(&origin(reply));
    rt.show_columns(&rows(reply), &["level", "groundzero", "customer"])?;
    output::info(
        "mask: sensitive columns (tagged or under a mask policy) are masked; \
deny: the model is told the data exists but gets no rows",
    );
    Ok(())
}

/// The `gz code` notice for a `system_warning` the chat server sends when
/// the model read policy masked or withheld a tool result; None for any
/// other warning.
pub fn notice(event: &Value) -> Option<String> {
    if event["source"].as_str() != Some("model_read_policy") {
        return None;
    }
    let tool = event["toolName"].as_str().unwrap_or("a tool");
    let list = |key: &str| {
        event[key]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default()
    };
    let sources = list("sources");
    let from = if sources.is_empty() {
        String::new()
    } else {
        format!(" ({sources})")
    };
    Some(match event["action"].as_str() {
        Some("masked") => format!(
            "data policy: masked {} in {tool}'s result before the model read it{from}",
            list("columns")
        ),
        Some("withheld") => {
            format!("data policy: withheld {tool}'s result from the model{from}")
        }
        _ => format!("data policy: {}", crate::cmd::stream::message(event)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply() -> Value {
        json!({
            "site": "acme",
            "isDefault": true,
            "policy": {
                "groundzero": {"public": "allow", "internal": "allow", "confidential": "allow",
                    "restricted": "mask", "unclassified": "allow", "ungoverned": "allow"},
                "customer": {"public": "allow", "internal": "allow", "confidential": "mask",
                    "restricted": "deny", "unclassified": "mask", "ungoverned": "mask"}
            }
        })
    }

    #[test]
    fn set_changes_only_the_named_cell() {
        let policy = with_cell(
            &reply(),
            ModelKind::Customer,
            Level::Confidential,
            ReadAction::Deny,
        )
        .unwrap();
        assert_eq!(policy["customer"]["confidential"], "deny");
        assert_eq!(policy["customer"]["restricted"], "deny");
        assert_eq!(policy["groundzero"], reply()["policy"]["groundzero"]);
        assert!(policy.get("site").is_none(), "only the policy is sent");
    }

    #[test]
    fn rows_list_every_level_for_both_kinds_of_model() {
        let rows = rows(&reply());
        assert_eq!(rows.as_array().map(Vec::len), Some(6));
        assert_eq!(
            rows[3],
            json!({"level": "restricted", "groundzero": "mask", "customer": "deny"})
        );
    }

    #[test]
    fn says_whether_the_defaults_are_in_force() {
        assert!(origin(&reply()).contains("the defaults"));
        let mut saved = reply();
        saved["isDefault"] = json!(false);
        saved["updatedBy"] = json!("root");
        saved["updatedAt"] = json!("2026-11-11T10:00:00Z");
        assert_eq!(
            origin(&saved),
            "Model read policy for acme: set by root at 2026-11-11T10:00:00Z"
        );
    }

    #[test]
    fn a_masked_result_becomes_a_notice_naming_columns_and_sources() {
        let event = json!({
            "type": "system_warning",
            "source": "model_read_policy",
            "action": "masked",
            "toolName": "run_command",
            "columns": ["email", "ssn"],
            "sources": ["lakehouse table Main.sales.customers"],
            "message": "Masked email, ssn for customer-brought models"
        });
        assert_eq!(
            notice(&event).as_deref(),
            Some(
                "data policy: masked email, ssn in run_command's result before the model \
read it (lakehouse table Main.sales.customers)"
            )
        );
    }

    #[test]
    fn a_withheld_result_says_so_and_other_warnings_are_not_notices_of_it() {
        let withheld = json!({"source": "model_read_policy", "action": "withheld",
            "toolName": "query_data_provider", "columns": [], "sources": []});
        assert_eq!(
            notice(&withheld).as_deref(),
            Some("data policy: withheld query_data_provider's result from the model")
        );
        assert_eq!(
            notice(&json!({"source": "ontology_context", "message": "x"})),
            None
        );
    }

    #[test]
    fn the_run_stream_turns_the_server_event_into_the_notice() {
        use crate::cmd::stream::RunEvent;
        let event = crate::sse::Event {
            id: Some("7".into()),
            event: None,
            data: json!({"type": "system_warning", "source": "model_read_policy",
                "action": "withheld", "toolName": "run_command",
                "sources": ["lakehouse table sales.payroll"]})
            .to_string(),
        };
        assert_eq!(
            RunEvent::parse(&event),
            RunEvent::Warning(
                "data policy: withheld run_command's result from the model \
(lakehouse table sales.payroll)"
                    .into()
            )
        );
    }
}
