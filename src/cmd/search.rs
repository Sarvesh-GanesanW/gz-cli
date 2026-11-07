//! Global search across every resource kind the caller can list — the web
//! app's search box (`Utils/globalSearchApi.js`, gz-chat `routes/search.py`).

use anyhow::Result;
use owo_colors::OwoColorize;
use reqwest::Method;
use serde_json::Value;

use crate::cli::SearchArgs;
use crate::cmd::Runtime;
use crate::output::{self, OutputFormat};

/// Results asked for when `--limit` isn't given (the web app's page size).
const DEFAULT_RESULTS: usize = 20;

pub async fn run(rt: &Runtime, args: &SearchArgs) -> Result<()> {
    let limit = rt.limit.unwrap_or(DEFAULT_RESULTS);
    let mut query = vec![
        ("q".to_string(), args.query.clone()),
        ("limit".to_string(), limit.to_string()),
    ];
    if !args.kinds.is_empty() {
        query.push(("kinds".to_string(), args.kinds.join(",")));
    }
    let value = rt
        .http
        .request_json("chat", Method::GET, "/search", &query, None)
        .await?;
    if rt.format != OutputFormat::Table {
        return rt.show(value).await;
    }
    let results = value["results"].as_array().cloned().unwrap_or_default();
    if results.is_empty() {
        output::warn(&format!("nothing matches \"{}\"", args.query));
        return Ok(());
    }
    for (kind, hits) in group_by_kind(&results) {
        println!("{}", kind.cyan().bold());
        for hit in hits {
            println!("  {}", describe(hit));
        }
    }
    Ok(())
}

/// Results grouped by kind, kinds in order of their best match.
fn group_by_kind(results: &[Value]) -> Vec<(String, Vec<&Value>)> {
    let mut groups: Vec<(String, Vec<&Value>)> = Vec::new();
    for hit in results {
        let kind = hit["kind"].as_str().unwrap_or("other").to_string();
        match groups.iter_mut().find(|(k, _)| *k == kind) {
            Some((_, hits)) => hits.push(hit),
            None => groups.push((kind, vec![hit])),
        }
    }
    groups
}

/// `name  · context · owner  id 42`
fn describe(hit: &Value) -> String {
    let name = hit["name"].as_str().unwrap_or("(unnamed)");
    let mut details: Vec<String> = ["context", "owner"]
        .iter()
        .filter_map(|k| hit[*k].as_str().filter(|s| !s.is_empty()))
        .map(str::to_string)
        .collect();
    if let Some(id) = hit["id"]
        .as_str()
        .map(str::to_string)
        .or(hit["id"].as_i64().map(|n| n.to_string()))
    {
        details.push(format!("id {id}"));
    }
    if details.is_empty() {
        return name.bold().to_string();
    }
    format!("{}  {}", name.bold(), details.join(" · ").dimmed())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn groups_results_by_kind_in_rank_order() {
        let results = vec![
            json!({"kind": "dashboard", "name": "a"}),
            json!({"kind": "dataset", "name": "b"}),
            json!({"kind": "dashboard", "name": "c"}),
        ];
        let groups = group_by_kind(&results);
        assert_eq!(groups[0].0, "dashboard");
        assert_eq!(groups[0].1.len(), 2);
        assert_eq!(groups[1].0, "dataset");
    }

    #[test]
    fn describes_a_hit_with_context_and_id() {
        let text = describe(&json!({"name": "Sales", "context": "Finance", "owner": "", "id": 7}));
        assert!(text.contains("Sales"));
        assert!(text.contains("Finance"));
        assert!(text.contains("id 7"));
        assert!(!text.contains(" ·  ·"));
    }
}
