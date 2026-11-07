//! Agents calling agents (ORCHESTRATION_CONTRACT.md Phase 2): editing an
//! agent's `subAgents` list and reading a run's child runs. The
//! `subagent_*` events those runs stream are decoded and shown by
//! [`crate::subagents`] through `stream::follow`.

use anyhow::{bail, Result};
use regex::Regex;
use serde_json::{json, Value};

/// Aliases become tool names (`call_<alias>`), so they follow the agent
/// builder's key rule.
const ALIAS_PATTERN: &str = r"^[a-z][a-z0-9_]{0,39}$";

/// The agent's sub-agent list with `entry` appended, or why it can't be.
pub fn add_sub_agent(current: &[Value], parent: &str, entry: Value) -> Result<Vec<Value>> {
    let alias = entry["alias"].as_str().unwrap_or_default();
    let child = entry["agentId"].as_str().unwrap_or_default();
    let pattern = Regex::new(ALIAS_PATTERN).expect("valid alias pattern");
    if !pattern.is_match(alias) {
        bail!("alias '{alias}' must be lowercase letters, digits, and _ (it becomes the tool call_{alias})");
    }
    if child == parent {
        bail!("an agent can't be its own sub-agent");
    }
    if current.iter().any(|s| s["alias"].as_str() == Some(alias)) {
        bail!("this agent already has a sub-agent called {alias}");
    }
    if current.iter().any(|s| s["agentId"].as_str() == Some(child)) {
        bail!("agent {child} is already a sub-agent of this agent");
    }
    let mut next = current.to_vec();
    next.push(entry);
    Ok(next)
}

/// The list without the sub-agent named by alias (or agent id).
pub fn remove_sub_agent(current: &[Value], key: &str) -> Result<Vec<Value>> {
    let matches =
        |s: &Value| s["alias"].as_str() == Some(key) || s["agentId"].as_str() == Some(key);
    if !current.iter().any(matches) {
        let known: Vec<&str> = current.iter().filter_map(|s| s["alias"].as_str()).collect();
        bail!(
            "no sub-agent called {key}{}",
            if known.is_empty() {
                String::new()
            } else {
                format!(" (have: {})", known.join(", "))
            }
        );
    }
    Ok(current.iter().filter(|s| !matches(s)).cloned().collect())
}

pub fn new_entry(child: &str, alias: &str, description: Option<&str>) -> Value {
    let mut entry = json!({"agentId": child, "alias": alias});
    if let Some(description) = description {
        entry["description"] = json!(description);
    }
    entry
}

/// What an agent is, for lists: `orchestrator · 3 members`,
/// `swarm · 4 members`, `agent · 2 sub-agents`, or `agent`.
pub fn kind_label(agent: &Value) -> String {
    let count = |key: &Value| key.as_array().map_or(0, Vec::len);
    let team = &agent["team"];
    if let Some(mode) = team["mode"].as_str() {
        return format!("{mode} · {} members", count(&team["members"]));
    }
    match count(&agent["subAgents"]) {
        0 => "agent".to_string(),
        n => format!("agent · {n} sub-agent(s)"),
    }
}

/// `agents` rows with a `kind` column added, for table output.
pub fn with_kind(list: &Value) -> Value {
    let mut list = list.clone();
    if let Some(items) = list["items"].as_array_mut() {
        for item in items {
            item["kind"] = json!(kind_label(item));
        }
    }
    list
}

/// Rows for a run's `children` (child runs started by sub-agents).
pub fn children_rows(run: &Value) -> Vec<Value> {
    run["children"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|child| {
            json!({
                "runId": child["runId"],
                "agentId": child["agentId"],
                "status": child["status"],
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_sub_agents_with_checks() {
        let current = vec![new_entry("child-a", "billing", Some("amounts"))];
        let next =
            add_sub_agent(&current, "parent", new_entry("child-b", "refunds", None)).unwrap();
        assert_eq!(next.len(), 2);
        assert_eq!(next[1], json!({"agentId": "child-b", "alias": "refunds"}));
        let err = |entry: Value| {
            add_sub_agent(&current, "parent", entry)
                .unwrap_err()
                .to_string()
        };
        assert!(err(new_entry("child-b", "Bad-Alias", None)).contains("lowercase"));
        assert!(err(new_entry("parent", "me", None)).contains("its own"));
        assert!(err(new_entry("child-c", "billing", None)).contains("already has"));
        assert!(err(new_entry("child-a", "other", None)).contains("already a sub-agent"));
        // No count limit: budgets, not list size, bound what agents do.
        let many: Vec<Value> = (0..20)
            .map(|i| new_entry(&format!("c{i}"), &format!("a{i}"), None))
            .collect();
        assert_eq!(
            add_sub_agent(&many, "p", new_entry("x", "x", None))
                .unwrap()
                .len(),
            21
        );
    }

    #[test]
    fn removes_by_alias_or_agent_id() {
        let current = vec![
            new_entry("child-a", "billing", None),
            new_entry("child-b", "refunds", None),
        ];
        assert_eq!(remove_sub_agent(&current, "billing").unwrap().len(), 1);
        assert_eq!(
            remove_sub_agent(&current, "child-b").unwrap()[0]["alias"],
            "billing"
        );
        let err = remove_sub_agent(&current, "nope").unwrap_err().to_string();
        assert!(err.contains("have: billing, refunds"), "{err}");
    }

    #[test]
    fn labels_teams_and_agents_with_sub_agents() {
        let team = json!({"team": {"mode": "swarm", "members": [{}, {}, {}]}});
        assert_eq!(kind_label(&team), "swarm · 3 members");
        let parent = json!({"subAgents": [{"alias": "a"}, {"alias": "b"}]});
        assert_eq!(kind_label(&parent), "agent · 2 sub-agent(s)");
        assert_eq!(kind_label(&json!({"name": "plain"})), "agent");
        let list = with_kind(
            &json!({"items": [{"name": "x", "team": {"mode": "orchestrator", "members": [{}]}}]}),
        );
        assert_eq!(list["items"][0]["kind"], "orchestrator · 1 members");
    }

    #[test]
    fn children_rows_keep_the_contract_fields() {
        let rows = children_rows(
            &json!({"children": [{"runId": "r2", "agentId": "u-1", "status": "completed", "extra": 1}]}),
        );
        assert_eq!(
            rows,
            vec![json!({"runId": "r2", "agentId": "u-1", "status": "completed"})]
        );
        assert!(children_rows(&json!({})).is_empty());
    }
}
