//! Usage screens under Catalog: readable columns, drills and links.

use crossterm::event::KeyCode;
use serde_json::json;

use crate::tui::app::test_support::*;

#[test]
fn ai_usage_lists_models_by_spend_with_readable_names() {
    let mut app = app();
    app.open_and_focus(resource("Catalog", "AI usage"));
    assert_eq!(last_fetch(&app).path, "/chat/metrics");
    answer(
        &mut app,
        json!({"scope": "tenant", "modelMetrics": [
            {"modelId": "cohere.rerank-v3-5:0", "requestCount": 5, "totalTokens": 0, "avgOutputTokensPerSecond": null,
             "medianTimeToFirstTokenMs": null, "totalCostUsd": "0.001"},
            {"modelId": "global.openai.gpt-6-luna", "requestCount": 85929, "totalTokens": 300000000,
             "avgOutputTokensPerSecond": 219.2, "medianTimeToFirstTokenMs": 4715, "totalCostUsd": "48.49"}]}),
    );
    let grid = app.view().unwrap().grid();
    assert_eq!(
        grid.rows[0],
        vec!["GPT-6 Luna", "85,929", "300M", "219.2", "4.7 s", "$48.49"]
    );
    assert_eq!(grid.rows[1][0], "rerank-v3-5:0");
}

#[test]
fn agent_usage_shows_rates_durations_and_links_to_the_agent() {
    let mut app = app();
    app.open_and_focus(resource("Catalog", "Agent usage"));
    assert_eq!(
        last_fetch(&app).path,
        "/v1/agents/metrics?limit=100&offset=0"
    );
    answer(
        &mut app,
        json!({"items": [{"agentId": "f20a", "agentName": "InvoiceExtractor", "runCount": 653, "runningCount": 0,
            "successRate": 98.62, "averageDurationMs": 5823.54, "totalTokens": 4351577,
            "estimatedModelCostUsd": "12.8390205732"}]}),
    );
    assert_eq!(
        app.view().unwrap().grid().rows[0],
        vec![
            "InvoiceExtractor",
            "653",
            "0",
            "98.6%",
            "5.8 s",
            "4.4M",
            "$12.84"
        ]
    );
    press(&mut app, &[KeyCode::Char('A')]);
    assert_eq!(app.active, resource("Agents", "Agents"));
}

#[test]
fn job_usage_reads_seconds_and_float_costs() {
    let mut app = app();
    app.open_and_focus(resource("Catalog", "Job usage"));
    assert_eq!(
        last_fetch(&app).path,
        "/etl/consumption-metrics?workloadType=job"
    );
    answer(
        &mut app,
        json!({"items": [{"executionId": "faa4", "workloadType": "job", "jobName": "ChallanReceiptBulk",
            "projectId": 1510, "projectName": "Invoice-Extractor", "status": "succeeded",
            "startedAt": "2026-11-08T10:00:00+00:00", "durationSeconds": 449.97,
            "usage": {"cpu": {"coreHours": 0.18499}, "cost": {"totalUsd": 0.01769401}}}]}),
    );
    assert_eq!(
        app.view().unwrap().grid().rows[0],
        vec![
            "ChallanReceiptBulk",
            "Invoice-Extractor",
            "Succeeded",
            "2h ago",
            "7m 29s",
            "0.18",
            "$0.02"
        ]
    );
    press(&mut app, &[KeyCode::Enter]);
    assert_eq!(last_fetch(&app).path, "/etl/consumption-metrics/faa4");
}

#[test]
fn project_usage_drills_into_jobs_then_their_runs() {
    let mut app = app();
    app.open_and_focus(resource("Catalog", "Project usage"));
    answer(
        &mut app,
        json!({"items": [{"projectId": 1510, "projectName": "Invoice-Extractor", "jobCount": 3,
            "latestStartedAt": "2026-11-08T10:00:00+00:00",
            "summary": {"executionCount": 7, "successRate": 71.43, "averageDurationSeconds": 423.07,
                        "totalCostUsd": null, "observedCostUsd": 0.11956702}}]}),
    );
    assert_eq!(
        app.view().unwrap().grid().rows[0],
        vec![
            "Invoice-Extractor",
            "3",
            "7",
            "71.4%",
            "7m 03s",
            "$0.12",
            "2h ago"
        ]
    );
    press(&mut app, &[KeyCode::Enter]);
    assert_eq!(
        last_fetch(&app).path,
        "/etl/consumption-metrics?groupBy=job&projectId=1510"
    );
    answer(
        &mut app,
        json!({"items": [{"jobId": 2125, "jobName": "ChallanReceiptBulk", "summary": {"executionCount": 2}}]}),
    );
    press(&mut app, &[KeyCode::Enter]);
    assert_eq!(last_fetch(&app).path, "/etl/consumption-metrics?jobId=2125");
}
