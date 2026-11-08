//! Your account's AI usage: the chat API's metrics (tokens and cost) and
//! the cost budget, as a compact panel. Shared by `gz usage` and `/usage`
//! inside `gz code`. `gz usage budget` adds gz code's own spend on this
//! machine and the user's caps.

use anyhow::Result;
use reqwest::Method;
use serde_json::{json, Value};

use crate::cmd::Runtime;
use crate::code_ui::panel::{Panel, Row};
use crate::code_ui::usage::{compact, group, right_aligned};
use crate::http::ApiClient;
use crate::output::OutputFormat;
use crate::spend::budget::BudgetStatus;
use crate::spend::caps::{CapKind, Caps};
use crate::spend::day;
use crate::spend::ledger::{Ledger, Summary};
use crate::spend::usd;

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
        fetch_budget(http),
    );
    Ok(AccountUsage {
        metrics: metrics?,
        budget: budget?,
    })
}

/// The caller's rolling budget (gz-chat-eks api/routes/budgets.py).
pub async fn fetch_budget(http: &ApiClient) -> Result<Value> {
    http.request_json(CHAT, Method::GET, "/chat/budget-status", &[], None)
        .await
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
    let budget = BudgetStatus::from_json(&account.budget)
        .map(|status| budget_rows(&status))
        .unwrap_or_default();
    Panel::new("Account usage")
        .aside(aside)
        .section(Some("Tokens"), tokens)
        .section(None, totals)
        .section(Some("Budget"), budget)
}

/// The server budget as panel rows: a meter of what is spent or held,
/// what's left, and who it is set for.
pub fn budget_rows(budget: &BudgetStatus) -> Vec<Row> {
    let Some(fraction) = budget.used_fraction() else {
        return vec![
            Row::field("budget", budget.left_text()),
            Row::field("used", usd(budget.used_usd)),
        ];
    };
    let mut detail = budget.left_text();
    if budget.exhausted {
        detail.push_str(" · used up");
    }
    let mut used = usd(budget.used_usd);
    if budget.reserved_usd > 0.0 {
        used.push_str(&format!(
            " · {} held by running turns",
            usd(budget.reserved_usd)
        ));
    }
    vec![
        Row::meter("budget", fraction, detail),
        Row::field("used", used),
        Row::field("set", budget.set_by()),
    ]
}

/// `gz usage budget`: the server budget, gz code's spend on this machine
/// today and this month (per model), and the caps.
pub async fn budget(rt: &Runtime) -> Result<()> {
    let budget = fetch_budget(&rt.http).await?;
    let resolved = rt.http.resolved();
    let today = day::today();
    let local = Ledger::open(&resolved.profile_name)?.summary(&today)?;
    let caps = resolved.profile.code_spend_caps.unwrap_or_default();
    if rt.format != OutputFormat::Table {
        return rt.show(budget_json(budget, &today, &local, &caps)).await;
    }
    for line in budget_panel(&budget, &local, &caps).plain_lines() {
        println!("{line}");
    }
    Ok(())
}

fn budget_json(budget: Value, today: &str, local: &Summary, caps: &Caps) -> Value {
    let models: Vec<Value> = local
        .month_models
        .iter()
        .map(|(model, usd)| json!({"model": model, "usd": usd}))
        .collect();
    json!({
        "budget": budget,
        "local": {
            "day": today,
            "todayUsd": local.today_usd,
            "monthUsd": local.month_usd,
            "monthModels": models,
        },
        "caps": {"sessionUsd": caps.session_usd, "dayUsd": caps.day_usd},
    })
}

fn budget_panel(budget: &Value, local: &Summary, caps: &Caps) -> Panel {
    let server = match BudgetStatus::from_json(budget) {
        Some(status) => budget_rows(&status),
        None => vec![Row::note("the server sent no budget status")],
    };
    let mut spent = vec![
        Row::field("today", usd(local.today_usd)),
        Row::field("this month", usd(local.month_usd)),
    ];
    spent.extend(
        local
            .month_models
            .iter()
            .map(|(model, amount)| Row::entry(model, usd(*amount))),
    );
    let caps = vec![
        Row::field("session cap", caps.shown(CapKind::Session)),
        Row::field("day cap", caps.shown(CapKind::Day)),
    ];
    Panel::new("AI budget")
        .section(Some("Groundzero budget"), server)
        .section(Some("gz code on this machine"), spent)
        .section(Some("Your gz code caps"), caps)
        .footer("set caps in gz code: /budget session <usd> · /budget day <usd>")
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
            "▕█████░░░░░░░░░░░░░░░▏  25% · $75.00 of $100 left (30d)",
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
        assert!(text.contains("no budget limit (30d)"), "{text}");
    }
}
