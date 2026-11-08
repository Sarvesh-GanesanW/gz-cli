//! Your account's AI usage: the chat API's metrics (tokens and cost) and
//! the cost budget, as a compact panel. Shared by `gz usage` and `/usage`
//! inside `gz code`.

use anyhow::Result;
use reqwest::Method;
use serde_json::{json, Value};

use crate::cmd::Runtime;
use crate::code_ui::panel::{Panel, Row};
use crate::code_ui::usage::{compact, group, right_aligned};
use crate::http::ApiClient;
use crate::output::OutputFormat;

const CHAT: &str = "chat";

/// The raw replies of `GET /chat/metrics` and `GET /chat/budget-status`.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountUsage {
    pub metrics: Value,
    pub budget: Value,
}

pub async fn fetch(http: &ApiClient) -> Result<AccountUsage> {
    let (metrics, budget) = tokio::join!(
        http.request_json(CHAT, Method::GET, "/chat/metrics", &[], None),
        http.request_json(CHAT, Method::GET, "/chat/budget-status", &[], None),
    );
    Ok(AccountUsage {
        metrics: metrics?,
        budget: budget?,
    })
}

/// `gz usage`: the panel in a terminal, both replies with `-o json`.
pub async fn run(rt: &Runtime) -> Result<()> {
    let account = fetch(&rt.http).await?;
    if rt.format != OutputFormat::Table {
        return rt
            .show(json!({"metrics": account.metrics, "budget": account.budget}))
            .await;
    }
    for line in panel(&account).plain_lines() {
        println!("{line}");
    }
    Ok(())
}

/// Money strings like `"71.964530896800000000"` as `$71.96`.
fn dollars(value: &Value) -> Option<String> {
    let amount = match value {
        Value::String(text) => text.parse::<f64>().ok()?,
        Value::Number(n) => n.as_f64()?,
        _ => return None,
    };
    Some(format!("${amount:.2}"))
}

/// `2026-11-07T13:15:32Z` → `2026-11-07 13:15 UTC`; dates stay dates.
fn when(value: &Value) -> Option<String> {
    let text = value.as_str()?;
    if text.len() >= 16 && text.as_bytes()[10] == b'T' {
        return Some(format!("{} {} UTC", &text[..10], &text[11..16]));
    }
    Some(text.get(..10).unwrap_or(text).to_string())
}

/// The account panel.
pub fn panel(account: &AccountUsage) -> Panel {
    let m = &account.metrics;
    let mut aside = m["scope"].as_str().unwrap_or("you").to_string();
    if let (Some(from), Some(to)) = (when(&m["period"]["from"]), when(&m["period"]["to"])) {
        aside.push_str(&format!(" · {} → {}", &from[..10], &to[..10]));
    }
    let usage = &m["usage"];
    let count = |key: &str| usage[key].as_u64().unwrap_or(0);
    let tokens = if usage.is_object() {
        right_aligned(
            &[
                ("total", count("totalTokens")),
                ("input", count("inputTokens")),
                ("output", count("outputTokens")),
                ("cache read", count("cacheReadInputTokens")),
                ("cache write", count("cacheWriteInputTokens")),
            ],
            compact,
        )
    } else {
        Vec::new()
    };
    let mut totals = Vec::new();
    if usage.is_object() {
        totals.push(Row::field("requests", group(count("requestCount"))));
    }
    if let Some(total) = dollars(&m["costUsd"]["total"]) {
        totals.push(Row::field("cost", total));
    }
    let budget: Vec<Row> = budget_row(&account.budget).into_iter().collect();
    Panel::new("Account usage")
        .aside(aside)
        .section(Some("Tokens"), tokens)
        .section(None, totals)
        .section(Some("Budget"), budget)
}

fn budget_row(budget: &Value) -> Option<Row> {
    if !budget.is_object() {
        return None;
    }
    let window = budget["windowDays"]
        .as_u64()
        .map(|d| format!(" per {d} days"))
        .unwrap_or_default();
    let used = dollars(&budget["usedUsd"]).unwrap_or_else(|| "$0.00".into());
    let Some(limit_cents) = budget["limitCents"].as_u64() else {
        return Some(Row::field(
            "budget",
            format!("no limit set · used {used}{window}"),
        ));
    };
    let used_cents = budget["usedCents"].as_u64().unwrap_or(0);
    let fraction = if limit_cents == 0 {
        1.0
    } else {
        used_cents as f64 / limit_cents as f64
    };
    let state = if budget["exhausted"].as_bool() == Some(true) {
        " · exhausted"
    } else {
        ""
    };
    Some(Row::meter(
        "budget",
        fraction,
        format!(
            "{used} of ${:.2}{window}{state}",
            limit_cents as f64 / 100.0
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(budget: Value) -> AccountUsage {
        AccountUsage {
            metrics: json!({
                "scope": "tenant",
                "period": {"from": "2026-10-08T13:15:32+00:00", "to": "2026-11-07T13:15:32+00:00"},
                "usage": {"totalTokens": 317_493_917u64, "inputTokens": 1_950_309, "outputTokens": 9_847_650,
                          "cacheReadInputTokens": 17_753_231, "cacheWriteInputTokens": 287_942_727u64,
                          "requestCount": 86_683},
                "costUsd": {"total": "71.964530896800000000"},
            }),
            budget,
        }
    }

    #[test]
    fn renders_tokens_cost_and_budget() {
        let text = panel(&account(json!({
            "usedCents": 2500, "limitCents": 10000, "usedUsd": "25.000000", "windowDays": 30,
            "exhausted": false
        })))
        .plain_lines()
        .join("\n");
        for part in [
            "Account usage · tenant · 2026-10-08 → 2026-11-07",
            "total        317.5M",
            "input          2.0M",
            "output         9.8M",
            "requests     86,683",
            "cost         $71.96",
            "▕█████░░░░░░░░░░░░░░░▏  25% · $25.00 of $100.00 per 30 days",
        ] {
            assert!(text.contains(part), "missing {part:?} in\n{text}");
        }
    }

    #[test]
    fn says_when_no_budget_is_set() {
        let text = panel(&account(json!({
            "usedCents": 0, "limitCents": null, "usedUsd": "0.000000000000000000", "windowDays": 30
        })))
        .plain_lines()
        .join("\n");
        assert!(
            text.contains("no limit set · used $0.00 per 30 days"),
            "{text}"
        );
    }
}
