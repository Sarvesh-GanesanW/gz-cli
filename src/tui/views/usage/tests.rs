//! The usage view: ranges, side-by-side totals, per-day and per-model bars.

use crossterm::event::KeyCode;
use serde_json::json;

use super::*;
use crate::tui::app::test_support::*;
use crate::tui::app::App;
use crate::tui::fetch::{FetchReq, Request};

fn fetches(app: &App) -> Vec<FetchReq> {
    app.outbox
        .iter()
        .filter_map(|r| match r {
            Request::Fetch(f) => Some(f.clone()),
            _ => None,
        })
        .collect()
}

fn reply(app: &mut App, path_start: &str, value: serde_json::Value) {
    for req in fetches(app)
        .into_iter()
        .filter(|f| f.path.starts_with(path_start))
    {
        app.fetched(req.id, req.target, Ok(value.clone()));
    }
}

#[test]
fn seven_days_ending_today_go_to_every_source() {
    let mut app = app();
    app.push_view(Box::new(Usage::new()));
    let paths: Vec<String> = fetches(&app).iter().map(|f| f.path.clone()).collect();
    // The test clock is 2026-11-08 12:00 UTC.
    let period = "from=2026-11-02T00:00:00Z&to=2026-11-09T00:00:00Z";
    assert!(
        paths.contains(&format!("/chat/metrics?{period}")),
        "{paths:?}"
    );
    assert!(paths.contains(&format!(
        "/v1/agents/metrics/timeseries?{period}&bucket=day"
    )));
    assert!(paths.contains(&format!("/v1/agents/metrics/models?{period}")));
    let daily = paths
        .iter()
        .filter(|p| p.starts_with("/chat/metrics?") && !p.contains(period))
        .count();
    assert_eq!(daily, CHAT_PARALLEL, "per-day chat calls are throttled");
    assert!(paths
        .contains(&"/chat/metrics?from=2026-11-02T00:00:00Z&to=2026-11-03T00:00:00Z".to_string()));
    // Each answered day lets the next one go.
    let first = fetches(&app)
        .into_iter()
        .find(|f| f.path.contains("from=2026-11-02T00:00:00Z&to=2026-11-03"))
        .unwrap();
    app.fetched(
        first.id,
        first.target,
        Ok(json!({"costUsd": {"total": "1"}, "usage": {"requestCount": 3}})),
    );
    assert!(fetches(&app)
        .iter()
        .any(|f| f.path.contains("from=2026-11-06T00:00:00Z&to=2026-11-07")));
}

#[test]
fn chat_and_agent_spend_show_side_by_side_never_summed() {
    let mut app = app();
    app.push_view(Box::new(Usage::new()));
    reply(
        &mut app,
        "/chat/metrics?from=2026-11-02T00:00:00Z&to=2026-11-09",
        json!({"costUsd": {"total": "76.501646"}, "usage": {"requestCount": 86744, "totalTokens": 319705041},
               "modelMetrics": [
                 {"modelId": "global.openai.gpt-6-luna", "requestCount": 85929, "totalTokens": 1, "totalCostUsd": "48.49"},
                 {"modelId": "in.openai.gpt-5.6-luna", "requestCount": 10, "totalTokens": 1, "totalCostUsd": "0.5"}]}),
    );
    reply(
        &mut app,
        "/v1/agents/metrics/timeseries",
        json!({"items": [
            {"bucketStart": "2026-11-07T00:00:00Z", "runCount": 40, "estimatedModelCostUsd": "2.5"},
            {"bucketStart": "2026-11-08T00:00:00Z", "runCount": 10, "estimatedModelCostUsd": "0.5"}]}),
    );
    reply(
        &mut app,
        "/v1/agents/metrics/models",
        json!({"items": [
            {"agentName": "A", "modelMetrics": [{"modelId": "global.openai.gpt-5.6-sol", "runCount": 30, "totalTokens": 5, "estimatedModelCostUsd": "2"}]},
            {"agentName": "B", "modelMetrics": [{"modelId": "global.openai.gpt-5.6-sol", "runCount": 20, "totalTokens": 5, "estimatedModelCostUsd": "1"}]}]}),
    );
    let screen = draw(&mut app, 140, 40);
    for expected in [
        "AI usage (chat)",
        "$76.50 · 86.7k requests · 320M tokens",
        "Agent usage",
        "$3.00 · 50 runs",
        "GPT-6 Luna",
        "GPT-5.6 Luna",
        "$3.00 · 50 runs",
        "never added",
    ] {
        assert!(screen.contains(expected), "missing {expected:?}\n{screen}");
    }
    assert!(!screen.contains("$79.50"), "the two totals are never added");
    assert!(screen.contains("11-08 "), "per-day rows\n{screen}");
}

#[test]
fn ninety_days_skips_per_day_chat_calls_and_says_so() {
    let mut app = app();
    app.push_view(Box::new(Usage::new()));
    let before = fetches(&app).len();
    press(&mut app, &[KeyCode::Char('9')]);
    let new: Vec<FetchReq> = fetches(&app).into_iter().skip(before).collect();
    assert_eq!(new.len(), 3, "totals, agent days and agent models only");
    assert!(
        new[0].path.contains("from=2026-08-11T00:00:00Z"),
        "{}",
        new[0].path
    );
    assert!(draw(&mut app, 140, 40).contains("Daily figures cover up to 30 days"));
}

#[test]
fn failures_are_shown_on_their_side_only() {
    let mut app = app();
    app.push_view(Box::new(Usage::new()));
    for req in fetches(&app).into_iter().filter(|f| f.service == "agents") {
        app.fetched(
            req.id,
            req.target,
            Err("GET u → HTTP 403 Forbidden: no".into()),
        );
    }
    let screen = draw(&mut app, 160, 40);
    assert!(
        screen.contains("You don't have access to agents usage."),
        "{screen}"
    );
}

#[test]
fn sparklines_scale_to_the_busiest_day() {
    assert_eq!(sparkline(&[Some(0.0), Some(1.0), Some(2.0), None]), "▁▅█ ");
    assert_eq!(bar(0.0, 0.0), "·".repeat(BAR_WIDTH));
    assert!(
        bar(0.01, 100.0).starts_with('█'),
        "any spend shows a sliver"
    );
}
