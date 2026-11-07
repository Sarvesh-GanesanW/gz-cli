//! Your account's AI usage: the chat API's metrics (tokens, cost, the
//! rolling rate limit) and budget status, as a compact panel. Shared by
//! `gz usage` and `/usage` inside `gz code`.

use anyhow::Result;
use reqwest::Method;
use serde_json::{json, Value};

use crate::cmd::Runtime;
use crate::code_ui::usage::{compact, group};
use crate::http::ApiClient;
use crate::output::OutputFormat;

const CHAT: &str = "chat";
/// Cells in the usage bars.
const BAR_CELLS: usize = 20;

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
    for line in panel(&account) {
        println!("{line}");
    }
    Ok(())
}

/// `▕██████░░░░░░░░░░░░░░▏` for `fraction` of the way full.
pub fn bar(fraction: f64) -> String {
    let filled = (fraction.clamp(0.0, 1.0) * BAR_CELLS as f64).round() as usize;
    format!("▕{}{}▏", "█".repeat(filled), "░".repeat(BAR_CELLS - filled))
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

/// The account panel, one string per line.
pub fn panel(account: &AccountUsage) -> Vec<String> {
    let m = &account.metrics;
    let mut lines = vec![format!(
        "Account usage · {}{}",
        m["scope"].as_str().unwrap_or("you"),
        match (when(&m["period"]["from"]), when(&m["period"]["to"])) {
            (Some(from), Some(to)) => format!(" · {} → {}", &from[..10], &to[..10]),
            _ => String::new(),
        }
    )];
    let usage = &m["usage"];
    let count = |key: &str| usage[key].as_u64().unwrap_or(0);
    if usage.is_object() {
        lines.push(format!(
            "  tokens       {} total · in {} · out {} · cache read {} · cache write {}",
            compact(count("totalTokens")),
            compact(count("inputTokens")),
            compact(count("outputTokens")),
            compact(count("cacheReadInputTokens")),
            compact(count("cacheWriteInputTokens")),
        ));
        lines.push(format!("  requests     {}", group(count("requestCount"))));
    }
    if let Some(total) = dollars(&m["costUsd"]["total"]) {
        lines.push(format!("  cost         {total}"));
    }
    lines.extend(rate_limit_line(&m["rateLimit"]));
    lines.extend(budget_line(&account.budget));
    lines
}

fn rate_limit_line(limit: &Value) -> Option<String> {
    let total = limit["tokensLimit"].as_u64()?;
    let used = limit["tokensUsed"].as_u64().unwrap_or(0);
    let fraction = if total == 0 {
        0.0
    } else {
        used as f64 / total as f64
    };
    let window = limit["window"].as_str().unwrap_or("window");
    let reset = when(&limit["resetAt"])
        .map(|at| format!(" · resets {}", at.get(11..).unwrap_or(&at)))
        .unwrap_or_default();
    Some(format!(
        "  rate limit   {} {:>3.0}% · {} / {} tokens per {window}{reset}",
        bar(fraction),
        fraction * 100.0,
        compact(used),
        compact(total),
    ))
}

fn budget_line(budget: &Value) -> Option<String> {
    if !budget.is_object() {
        return None;
    }
    let window = budget["windowDays"]
        .as_u64()
        .map(|d| format!(" per {d} days"))
        .unwrap_or_default();
    let used = dollars(&budget["usedUsd"]).unwrap_or_else(|| "$0.00".into());
    let Some(limit_cents) = budget["limitCents"].as_u64() else {
        return Some(format!("  budget       no limit set · used {used}{window}"));
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
    Some(format!(
        "  budget       {} {:>3.0}% · {used} of ${:.2}{window}{state}",
        bar(fraction),
        fraction * 100.0,
        limit_cents as f64 / 100.0,
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
                "rateLimit": {"tokensLimit": 30_000_000, "tokensUsed": 7_500_000, "window": "5 hours",
                              "resetAt": "2026-11-07T13:15:32.551051+00:00"},
            }),
            budget,
        }
    }

    #[test]
    fn renders_tokens_cost_rate_limit_and_budget() {
        let text = panel(&account(json!({
            "usedCents": 2500, "limitCents": 10000, "usedUsd": "25.000000", "windowDays": 30,
            "exhausted": false
        })))
        .join("\n");
        for part in [
            "Account usage · tenant · 2026-10-08 → 2026-11-07",
            "317.5M total · in 2.0M · out 9.8M",
            "requests     86,683",
            "cost         $71.96",
            "▕█████░░░░░░░░░░░░░░░▏  25% · 7.5M / 30.0M tokens per 5 hours · resets 13:15 UTC",
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
        .join("\n");
        assert!(
            text.contains("no limit set · used $0.00 per 30 days"),
            "{text}"
        );
    }

    #[test]
    fn bars_fill_proportionally() {
        assert_eq!(bar(0.0), format!("▕{}▏", "░".repeat(20)));
        assert_eq!(bar(1.5), format!("▕{}▏", "█".repeat(20)));
    }
}
