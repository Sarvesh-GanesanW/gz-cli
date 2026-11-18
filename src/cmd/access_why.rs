//! `gz access why`: why a user can or can't act on a lakehouse table.
//!
//! The answer is the lakehouse provider's own query authorizer, run on the
//! statement the user would run (`GET /lakehouse/governance/access/explain`,
//! groundzero-iceberg-provider-api app_routes/access_explain.py): its
//! decision, the check that settled it, and every check it made with the
//! permission rows and table rules it read. For a read it adds the row
//! filters and column masks the user gets, and what AI models get under the
//! site's model read policy (`GET chat /admin/model-read-policy`).

use anyhow::{bail, Result};
use clap::{Args, Subcommand, ValueEnum};
use reqwest::Method;
use serde_json::{json, Value};

use crate::cmd::lakehouse_table::{route_missing, LakehouseTable};
use crate::cmd::Runtime;
use crate::output::{self, OutputFormat};

const LAKEHOUSE: &str = "lakehouse";
const CHAT: &str = "chat";
const EXPLAIN_PATH: &str = "/lakehouse/governance/access/explain";
const MODEL_POLICY_PATH: &str = "/admin/model-read-policy";
/// Who `me` is: the server takes the signed-in caller when no user is named.
const ME: &str = "me";
/// The model read level of a table nobody classified.
const UNCLASSIFIED: &str = "unclassified";
const WAREHOUSE_RESOURCE: &str = "GZ Warehouse";

#[derive(Args, Debug)]
pub struct AccessArgs {
    #[command(subcommand)]
    pub action: AccessAction,
}

#[derive(Subcommand, Debug)]
pub enum AccessAction {
    #[command(
        about = "Why a user can or can't act on a table",
        long_about = "Asks the lakehouse provider's own authorizer whether USER may run ACTION \
on the table, and shows its decision, the check that settled it, and every check it made with \
the permissions and table rules it read. For select it also shows the row filters and column \
masks the user's reads get, and what AI models get under the site's model read policy.\n\n\
Anyone may ask about themselves (`me`); a site admin may ask about anyone. Groups always come \
from the platform: your sign-in, or for someone else the user directory."
    )]
    Why(WhyArgs),
}

#[derive(Args, Debug)]
pub struct WhyArgs {
    #[arg(help = "A user name, or `me`")]
    pub user: String,
    #[arg(help = "WAREHOUSE.DATABASE.TABLE")]
    pub table: String,
    #[arg(long, value_enum, default_value_t = TableAction::Select)]
    pub action: TableAction,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableAction {
    Select,
    Insert,
    Update,
    Delete,
    Truncate,
    Merge,
    Alter,
    Drop,
}

impl TableAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Select => "select",
            Self::Insert => "insert",
            Self::Update => "update",
            Self::Delete => "delete",
            Self::Truncate => "truncate",
            Self::Merge => "merge",
            Self::Alter => "alter",
            Self::Drop => "drop",
        }
    }
}

pub async fn run(rt: &Runtime, args: &AccessArgs) -> Result<()> {
    match &args.action {
        AccessAction::Why(why) => explain(rt, why).await,
    }
}

async fn explain(rt: &Runtime, args: &WhyArgs) -> Result<()> {
    let table = LakehouseTable::parse(&args.table)?;
    let reply = rt
        .http
        .request_json(
            LAKEHOUSE,
            Method::GET,
            EXPLAIN_PATH,
            &explain_query(&table, &args.user, args.action),
            None,
        )
        .await;
    let mut explanation = match reply {
        Ok(value) => value["response"].clone(),
        Err(err) if route_missing(&err) => bail!(
            "this site's lakehouse provider cannot explain access yet; it needs the \
             access explain route (GET {EXPLAIN_PATH}) deployed"
        ),
        Err(err) => return Err(err),
    };
    let model_actions = model_read_actions(rt, &explanation).await;
    if rt.format != OutputFormat::Table {
        if let Some(actions) = model_actions {
            explanation["modelReadActions"] = actions;
        }
        return output::print_value(&explanation, rt.format, rt.limit);
    }
    print!("{}", render(&explanation, model_actions.as_ref()));
    Ok(())
}

fn explain_query(table: &LakehouseTable, user: &str, action: TableAction) -> Vec<(String, String)> {
    let mut query = vec![
        ("warehouseName".to_string(), table.warehouse.clone()),
        ("databaseName".to_string(), table.name.database.clone()),
        ("tableName".to_string(), table.name.table.clone()),
        ("action".to_string(), action.as_str().to_string()),
    ];
    if !user.eq_ignore_ascii_case(ME) {
        query.push(("userName".to_string(), user.to_string()));
    }
    query
}

/// What each kind of model gets for the table's level, when the answer
/// covers a read. A policy that cannot be read is reported, not fatal: the
/// access decision stands on its own.
async fn model_read_actions(rt: &Runtime, explanation: &Value) -> Option<Value> {
    let model_read = explanation
        .get("modelRead")
        .filter(|value| !value.is_null())?;
    let level = model_read["classification"]
        .as_str()
        .unwrap_or(UNCLASSIFIED)
        .to_string();
    let reply = rt
        .http
        .request_json(CHAT, Method::GET, MODEL_POLICY_PATH, &[], None)
        .await;
    Some(match reply {
        Ok(policy) => json!({
            "level": level,
            "groundzero": policy["policy"]["groundzero"][&level],
            "customer": policy["policy"]["customer"][&level],
        }),
        Err(err) => json!({"level": level, "unavailable": format!("{err:#}")}),
    })
}

/// The explanation as text: the decision, what settled it, every check
/// with its rules, then the data policy and model reads for a read.
fn render(explanation: &Value, model_actions: Option<&Value>) -> String {
    let mut out = String::new();
    let subject = &explanation["subject"];
    let user = text(&subject["userName"]);
    let action = text(&explanation["action"]);
    let table = format!(
        "{}.{}.{}",
        text(&explanation["warehouseName"]),
        text(&explanation["databaseName"]),
        text(&explanation["tableName"])
    );
    if explanation["decision"] == "allow" {
        out.push_str(&format!("{user} may {action} {table}.\n"));
    } else {
        out.push_str(&format!(
            "{user} may not {action} {table}: {}\n",
            text(&explanation["reason"])
        ));
    }
    let decided_by = &explanation["decidedBy"];
    if decided_by.is_object() {
        out.push_str(&format!("Decided by: {}\n", describe_step(decided_by)));
    }
    out.push_str(&groups_line(subject));
    out.push_str("\nChecks, in the order the authorizer made them:\n");
    let checks = explanation["checks"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if checks.is_empty() {
        out.push_str("  (none: the statement was settled before any permission check)\n");
    }
    for (number, check) in checks.iter().enumerate() {
        out.push_str(&format!("  {}. {}\n", number + 1, describe_step(check)));
        for line in rule_lines(check) {
            out.push_str(&format!("       {line}\n"));
        }
    }
    if explanation["dataPolicy"].is_object() {
        out.push_str(&data_policy_text(&explanation["dataPolicy"]));
    }
    if explanation["modelRead"].is_object() {
        out.push_str(&model_read_text(&explanation["modelRead"], model_actions));
    }
    out
}

fn groups_line(subject: &Value) -> String {
    let groups: Vec<String> = subject["groups"]
        .as_array()
        .map(|items| items.iter().map(text).collect())
        .unwrap_or_default();
    let source = match subject["groupsFrom"].as_str() {
        Some("directory") => "from the user directory",
        _ => "from the sign-in",
    };
    if groups.is_empty() {
        return format!("Groups counted: none ({source})\n");
    }
    format!("Groups counted: {} ({source})\n", groups.join(", "))
}

fn describe_step(step: &Value) -> String {
    let effect = match step["effect"].as_str() {
        Some("allow") => "allows",
        Some("deny") => "refuses",
        _ => "no rule either way",
    };
    let action = text(&step["action"]);
    let described = match step["check"].as_str().unwrap_or_default() {
        "warehouse" => format!("warehouse permission {action}: {effect}"),
        "database" => format!(
            "database {} permission {action}: {effect}",
            text(&step["database"])
        ),
        "table" if step.get("table").is_some() => {
            let rules = if step["hasRules"] == false {
                " (the table has no rules of its own)"
            } else {
                ""
            };
            format!(
                "table {}.{} rules for {action}: {effect}{rules}",
                text(&step["database"]),
                text(&step["table"])
            )
        }
        "table" => format!("table rules for {action}: {effect}"),
        "namespace" => format!(
            "database missing from the warehouse: {}",
            list(&step["missing"])
        ),
        "audit_logs" => format!("audit.logs rule ({}): {effect}", text(&step["rule"])),
        other => format!("{other}: {effect}"),
    };
    match step["reason"].as_str() {
        Some(reason) => format!("{described}. {reason}"),
        None => described,
    }
}

/// One line per rule a check read: who it names, what it grants or
/// refuses, and (for table rules) whether it applies to this user.
fn rule_lines(check: &Value) -> Vec<String> {
    let Some(rules) = check["rules"].as_array() else {
        return Vec::new();
    };
    if rules.is_empty() {
        return vec!["no rule names this user or their groups".to_string()];
    }
    let resource = if check["resourceType"] == WAREHOUSE_RESOURCE {
        "warehouse"
    } else {
        "database"
    };
    rules
        .iter()
        .map(|rule| {
            if rule.get("member").is_some() {
                let scope = if rule["scope"] == "every" {
                    "every"
                } else {
                    "this"
                };
                format!(
                    "{} {}: {} {} (on {scope} {resource})",
                    text(&rule["memberType"]),
                    text(&rule["member"]),
                    text(&rule["permission"]),
                    text(&rule["effect"])
                )
            } else {
                let applies = if rule["applies"] == true {
                    "applies"
                } else {
                    "does not apply"
                };
                format!(
                    "{}: {} {} ({applies})",
                    table_rule_principal(rule),
                    text(&rule["action"]),
                    text(&rule["effect"])
                )
            }
        })
        .collect()
}

fn table_rule_principal(rule: &Value) -> String {
    let user = rule["userName"].as_str().map(|name| format!("user {name}"));
    let group = rule["groupName"]
        .as_str()
        .map(|name| format!("group {name}"));
    match (user, group) {
        (Some(user), Some(group)) => format!("{user} {} {group}", text(&rule["operator"])),
        (Some(one), None) | (None, Some(one)) => one,
        (None, None) => "anyone".to_string(),
    }
}

fn data_policy_text(policy: &Value) -> String {
    let mut out = String::from("\nWhat this user's reads get (data policy");
    match policy["policyVersion"].as_str().filter(|v| !v.is_empty()) {
        Some(version) => out.push_str(&format!(" {version}):\n")),
        None => out.push_str("):\n"),
    }
    let filters: Vec<String> = policy["rowFilters"]
        .as_array()
        .map(|items| items.iter().map(text).collect())
        .unwrap_or_default();
    out.push_str(&format!(
        "  Row filters: {}\n",
        or_none(&filters.join(" AND "))
    ));
    let masks: Vec<String> = policy["columnMasks"]
        .as_object()
        .map(|masks| {
            masks
                .iter()
                .map(|(column, mask)| {
                    let kind = mask["type"].as_str().unwrap_or("full");
                    format!("{column} ({kind})")
                })
                .collect()
        })
        .unwrap_or_default();
    out.push_str(&format!(
        "  Masked columns: {}\n",
        or_none(&masks.join(", "))
    ));
    let unmasked = list(&policy["unmaskedColumns"]);
    if !unmasked.is_empty() {
        out.push_str(&format!("  Unmasked for this user: {unmasked}\n"));
    }
    out
}

fn model_read_text(model_read: &Value, actions: Option<&Value>) -> String {
    let level = model_read["classification"]
        .as_str()
        .unwrap_or(UNCLASSIFIED);
    let mut out = format!("\nAI model reads (classification: {level}):\n");
    match actions {
        Some(actions) if actions.get("unavailable").is_some() => out.push_str(&format!(
            "  Model read policy unavailable: {}\n",
            text(&actions["unavailable"])
        )),
        Some(actions) => out.push_str(&format!(
            "  Groundzero models: {}; customer-brought models: {}\n",
            text(&actions["groundzero"]),
            text(&actions["customer"])
        )),
        None => {}
    }
    out.push_str(&format!(
        "  Masked for models by data policy: {}\n",
        or_none(&list(&model_read["maskedColumns"]))
    ));
    let tagged: Vec<String> = model_read["columnTags"]
        .as_object()
        .map(|columns| {
            columns
                .iter()
                .map(|(column, tags)| format!("{column} ({})", tag_text(tags)))
                .collect()
        })
        .unwrap_or_default();
    out.push_str(&format!("  Column tags: {}\n", or_none(&tagged.join(", "))));
    let table_tags = tag_text(&model_read["tableTags"]);
    if !table_tags.is_empty() {
        out.push_str(&format!("  Table tags: {table_tags}\n"));
    }
    out
}

fn tag_text(tags: &Value) -> String {
    tags.as_object()
        .map(|tags| {
            tags.iter()
                .map(|(key, value)| format!("{key}={}", text(value)))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

fn list(value: &Value) -> String {
    value
        .as_array()
        .map(|items| items.iter().map(text).collect::<Vec<_>>().join(", "))
        .unwrap_or_default()
}

fn or_none(text: &str) -> &str {
    if text.is_empty() {
        "none"
    } else {
        text
    }
}

fn text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "—".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn denied_update() -> Value {
        json!({
            "warehouseName": "wh", "databaseName": "sales", "tableName": "orders",
            "action": "update", "decision": "deny",
            "reason": "You do not have All or Update permission",
            "subject": {"userName": "bob", "groups": ["finance"], "groupsFrom": "directory"},
            "decidedBy": {"check": "table", "effect": "deny", "database": "sales",
                "table": "orders", "action": "Update", "hasRules": true},
            "checks": [
                {"check": "database", "effect": "allow", "database": "sales", "action": "Update",
                 "resourceType": "GZLakehouse Database",
                 "rules": [{"member": "finance", "memberType": "group", "permission": "All",
                            "effect": "allow", "scope": "this"}]},
                {"check": "table", "effect": "deny", "database": "sales", "table": "orders",
                 "action": "Update", "hasRules": true,
                 "rules": [
                    {"userName": "bob", "groupName": null, "operator": "or", "action": "Update",
                     "effect": "deny", "applies": true},
                    {"userName": null, "groupName": "ops", "operator": "or", "action": "All",
                     "effect": "allow", "applies": false}]},
            ],
            "dataPolicy": null, "modelRead": null,
        })
    }

    #[test]
    fn me_asks_about_the_caller_without_naming_anyone() {
        let table = LakehouseTable::parse("wh.sales.orders").unwrap();
        let query = explain_query(&table, "ME", TableAction::Delete);
        assert!(query.iter().all(|(key, _)| key != "userName"));
        assert!(query.contains(&("action".to_string(), "delete".to_string())));
        let query = explain_query(&table, "bob", TableAction::Select);
        assert!(query.contains(&("userName".to_string(), "bob".to_string())));
    }

    #[test]
    fn a_refusal_names_the_deciding_rule_and_every_rule_considered() {
        let text = render(&denied_update(), None);
        assert!(text.starts_with(
            "bob may not update wh.sales.orders: You do not have All or Update permission\n"
        ));
        assert!(text.contains("Decided by: table sales.orders rules for Update: refuses\n"));
        assert!(text.contains("Groups counted: finance (from the user directory)\n"));
        assert!(text.contains("  1. database sales permission Update: allows\n"));
        assert!(text.contains("       group finance: All allow (on this database)\n"));
        assert!(text.contains("       user bob: Update deny (applies)\n"));
        assert!(text.contains("       group ops: All allow (does not apply)\n"));
        assert!(!text.contains("AI model reads"));
    }

    #[test]
    fn a_read_shows_the_data_policy_and_model_reads() {
        let mut explanation = denied_update();
        explanation["decision"] = json!("allow");
        explanation["action"] = json!("select");
        explanation["dataPolicy"] = json!({
            "rowFilters": ["region = 'north'"], "columnMasks": {"email": {"type": "partial"}},
            "unmaskedColumns": [], "policyVersion": "v3",
        });
        explanation["modelRead"] = json!({
            "classification": "confidential", "maskedColumns": ["email"],
            "columnTags": {"dob": {"pii": "true"}}, "tableTags": {},
        });
        let actions = json!({"level": "confidential", "groundzero": "allow", "customer": "mask"});
        let text = render(&explanation, Some(&actions));
        assert!(text.starts_with("bob may select wh.sales.orders.\n"));
        assert!(text.contains("  Row filters: region = 'north'\n"));
        assert!(text.contains("  Masked columns: email (partial)\n"));
        assert!(text.contains("AI model reads (classification: confidential):\n"));
        assert!(text.contains("  Groundzero models: allow; customer-brought models: mask\n"));
        assert!(text.contains("  Column tags: dob (pii=true)\n"));
    }

    #[test]
    fn checks_without_rules_say_so() {
        let check = json!({"check": "database", "effect": "none", "database": "sales",
            "action": "Select", "resourceType": "GZLakehouse Database", "rules": []});
        assert_eq!(
            describe_step(&check),
            "database sales permission Select: no rule either way"
        );
        assert_eq!(
            rule_lines(&check),
            ["no rule names this user or their groups"]
        );
    }
}
