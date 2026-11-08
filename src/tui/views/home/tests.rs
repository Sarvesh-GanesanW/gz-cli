use std::time::Duration;

use crossterm::event::KeyCode;
use serde_json::{json, Value};

use super::*;
use crate::tui::app::test_support::*;
use crate::tui::app::App;
use crate::tui::fetch::{FetchReq, FetchTarget, Request};

const HOUR_MS: i64 = 60 * 60 * 1000;

/// Requests a view sent whose path starts with `prefix`, oldest first
/// (the app's own background checks share some paths).
fn view_fetches(app: &App, prefix: &str) -> Vec<FetchReq> {
    app.outbox
        .iter()
        .filter_map(|r| match r {
            Request::Fetch(f)
                if f.path.starts_with(prefix) && matches!(f.target, FetchTarget::View(_)) =>
            {
                Some(f.clone())
            }
            _ => None,
        })
        .collect()
}

fn fetch_for(app: &App, prefix: &str) -> FetchReq {
    view_fetches(app, prefix)
        .pop()
        .unwrap_or_else(|| panic!("no request for {prefix}"))
}

fn fetches_for(app: &App, prefix: &str) -> usize {
    view_fetches(app, prefix).len()
}

fn reply(app: &mut App, prefix: &str, result: Result<Value, String>) {
    let req = fetch_for(app, prefix);
    app.fetched(req.id, req.target, result);
}

fn at(ms: i64) -> String {
    format::iso8601_utc(ms)
}

fn runs_answer() -> Value {
    json!({
        "items": [
            {"status": "failed", "jobName": "ChallanReceiptBulk", "projectName": "Invoice-Extractor",
             "executionId": "2cb3", "projectId": 1510, "jobId": 2125,
             "startedAt": at(TEST_NOW_MS - 2 * HOUR_MS), "completedAt": at(TEST_NOW_MS - 2 * HOUR_MS)},
            {"status": "running", "jobName": "inv-extraction", "projectName": "Invoice-processing",
             "executionId": "9f00", "startedAt": at(TEST_NOW_MS - 252_000)},
            {"status": "running", "jobName": "memTest", "projectName": "stressTest",
             "executionId": "df0d", "startedAt": "2026-10-01T17:16:37.706409+00:00"}
        ],
        "pagination": {"limit": 100, "offset": 0, "total": 3}
    })
}

fn approvals_answer() -> Value {
    json!({"items": [{
        "id": 271, "agentId": "98af", "agentVersion": 3, "requestedBy": "sarvesh",
        "requestedAt": at(TEST_NOW_MS - 3 * HOUR_MS),
        "definitionSnapshot": {"name": "rlo-pair-judge"}
    }], "limit": 100, "offset": 0})
}

fn spend_answer() -> Value {
    json!({
        "username": "sagatest",
        "costUsd": {"total": "24.684345973200000000"},
        "userMetrics": [{"username": "sagatest", "totalCostUsd": "6.864986640000000000"}],
        "agentMetrics": [{"agentType": "agent_builder", "totalCostUsd": "12.839020573200000000"}],
        "budget": {"limitCents": null, "usedCents": 0, "usedUsd": "0", "windowDays": 30}
    })
}

fn chats_answer() -> Value {
    json!({"conversations": [
        {"conversationId": "c1", "conversationName": "Revenue by region"},
        {"conversationId": "c2", "conversationName": ""}
    ]})
}

/// An app with the dashboard open and every tile answered.
fn loaded_home() -> App {
    let mut app = app();
    app.open_home();
    reply(&mut app, "/etl/consumption-metrics", Ok(runs_answer()));
    reply(&mut app, "/v1/approval-requests", Ok(approvals_answer()));
    reply(&mut app, "/chat/metrics", Ok(spend_answer()));
    reply(&mut app, "/chat/conversations", Ok(chats_answer()));
    app
}

#[test]
fn every_tile_target_exists() {
    for target in TARGETS {
        assert!(
            nav::find_resource(target.module, target.resource).is_some(),
            "home links to missing {} › {}",
            target.module,
            target.resource
        );
    }
}

#[test]
fn opening_home_asks_each_tile_endpoint_once() {
    let mut app = app();
    press(&mut app, &[KeyCode::Char('H')]);
    assert_eq!(app.breadcrumb().last().unwrap(), "Home");
    let runs = fetch_for(&app, "/etl/consumption-metrics");
    assert_eq!(runs.service, "etl");
    assert_eq!(
        runs.path,
        "/etl/consumption-metrics?workloadType=job&from=2026-11-07T12:00:00Z&limit=100"
    );
    assert_eq!(
        fetch_for(&app, "/v1/approval-requests").path,
        "/v1/approval-requests?status=waiting_for_approval&limit=100"
    );
    assert_eq!(
        fetch_for(&app, "/chat/metrics").path,
        "/chat/metrics?from=2026-11-01T00:00:00Z"
    );
    assert_eq!(
        fetches_for(&app, "/chat/conversations?assistantMode=chat"),
        1
    );
    assert!(
        !app.outbox
            .iter()
            .any(|r| matches!(r, Request::Fetch(f) if f.path.contains("current-run"))),
        "current-run settles runs as Failed; the dashboard must never call it"
    );
}

#[test]
fn tiles_show_failures_stuck_runs_spend_and_chats() {
    let mut app = loaded_home();
    for (width, height) in [(120, 32), (80, 40)] {
        let screen = draw(&mut app, width, height);
        assert!(screen.contains("1 failed job run · last 24h"), "{screen}");
        assert!(screen.contains("ChallanReceiptBulk"));
        assert!(screen.contains("1 agent version is waiting for your review"));
        assert!(screen.contains("rlo-pair-judge"));
        assert!(screen.contains("inv-extraction"));
        assert!(screen.contains("4m 12s"), "running time so far\n{screen}");
        assert!(screen.contains("may be stuck"));
        assert!(!screen.contains("memTest"), "stuck runs fold into one line");
        assert!(screen.contains("AI spend · November (UTC)"));
        assert!(screen.contains("$6.86"));
        assert!(screen.contains("incl. agents $12.84"));
        assert!(screen.contains("no budget limit (30d)"), "{screen}");
        assert!(screen.contains("Untitled chat"));
    }
}

#[test]
fn a_failing_tile_leaves_the_others_readable() {
    let mut app = app();
    app.open_home();
    reply(
        &mut app,
        "/etl/consumption-metrics",
        Err(
            "GET u → HTTP 500 Internal Server Error: Unable to list execution observability".into(),
        ),
    );
    reply(
        &mut app,
        "/v1/approval-requests",
        Err(
            r#"GET u → HTTP 401 Unauthorized: You do not have "Approve" permission on agent."#
                .into(),
        ),
    );
    reply(
        &mut app,
        "/chat/conversations",
        Ok(json!({"conversations": []})),
    );
    let screen = draw(&mut app, 120, 32);
    assert!(screen.contains("The etl service had an error"), "{screen}");
    assert!(screen.contains("You don't have access to agent approval requests"));
    assert!(
        !screen.contains("gz auth login"),
        "a permission 401 is not an expired sign-in"
    );
    assert!(screen.contains("No conversations yet · press n to start one"));
    assert!(
        screen.contains("Loading AI spend"),
        "spend is still loading\n{screen}"
    );
}

#[test]
fn enter_on_a_failed_run_selects_it_in_job_usage() {
    let mut app = loaded_home();
    press(&mut app, &[KeyCode::Enter]);
    assert!(app.views.is_empty());
    assert_eq!(app.active, resource("Catalog", "Job usage"));
    answer(
        &mut app,
        json!({"items": [{"executionId": "aaaa"}, {"executionId": "2cb3"}]}),
    );
    let row = app.view().unwrap().grid().selected_raw().unwrap().clone();
    assert_eq!(row["executionId"], "2cb3");
}

#[test]
fn p_opens_the_runs_project_jobs() {
    let mut app = loaded_home();
    press(&mut app, &[KeyCode::Char('p')]);
    assert_eq!(app.active, resource("DE/ML", "Projects"));
    answer(
        &mut app,
        json!([{"Id": 1000, "Name": "ETL"}, {"Id": 1510, "Name": "Invoice-Extractor"}]),
    );
    assert_eq!(last_fetch(&app).path, "/etl/etlJobs/1510/jobs");
}

#[test]
fn approvals_lead_to_the_request_to_decide() {
    let mut app = loaded_home();
    press(&mut app, &[KeyCode::Down, KeyCode::Enter]);
    assert_eq!(app.active, resource("Agents", "Approval requests"));
    answer(&mut app, approvals_answer());
    let row = app.view().unwrap().grid().selected_raw().unwrap().clone();
    assert_eq!(row["id"], 271);
}

#[test]
fn tab_reaches_recent_chats_and_enter_opens_one() {
    let mut app = loaded_home();
    press(
        &mut app,
        &[KeyCode::Tab, KeyCode::Tab, KeyCode::Tab, KeyCode::Enter],
    );
    assert!(app.views.is_empty());
    assert!(app.is_chat());
    assert_eq!(app.chat.open.as_ref().unwrap().id, "c1");
    assert_eq!(last_fetch(&app).path, "/chat/conversations/c1");
}

#[test]
fn activity_refreshes_every_minute_and_spend_every_five() {
    let mut app = loaded_home();
    let start = app.now;
    app.on_tick(start);
    app.on_tick(start.advanced(Duration::from_secs(60)));
    assert_eq!(fetches_for(&app, "/etl/consumption-metrics"), 2);
    assert_eq!(fetches_for(&app, "/v1/approval-requests"), 2);
    assert_eq!(fetches_for(&app, "/chat/metrics"), 1);
    assert_eq!(
        fetches_for(&app, "/chat/conversations"),
        1,
        "chats load on open and r only"
    );
    app.on_tick(start.advanced(Duration::from_secs(300)));
    assert_eq!(fetches_for(&app, "/chat/metrics"), 2);
    press(&mut app, &[KeyCode::Esc]);
    app.on_tick(start.advanced(Duration::from_secs(900)));
    assert_eq!(
        fetches_for(&app, "/chat/metrics"),
        2,
        "a closed dashboard stops polling"
    );
}

#[test]
fn a_refresh_keeps_showing_the_last_answer() {
    let mut app = loaded_home();
    press(&mut app, &[KeyCode::Char('r')]);
    let screen = draw(&mut app, 120, 32);
    assert!(screen.contains("ChallanReceiptBulk"), "{screen}");
    assert!(!screen.contains("Loading failed runs"));
}

/// The foreground color where `needle` starts on a `width`×`height` screen.
fn color_at(app: &mut App, width: u16, height: u16, needle: &str) -> ratatui::style::Color {
    use ratatui::{backend::TestBackend, Terminal};
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|f| crate::tui::ui::render(app, f)).unwrap();
    let buffer = terminal.backend().buffer().clone();
    for y in 0..height {
        let row: String = (0..width)
            .map(|x| buffer[(x, y)].symbol().to_string())
            .collect();
        if let Some(at) = row.find(needle) {
            return buffer[(row[..at].chars().count() as u16, y)].fg;
        }
    }
    panic!("{needle:?} is not on screen");
}

#[test]
fn the_spend_tile_meters_the_budget_and_shows_gz_code_spend() {
    let ledger = crate::spend::ledger::temp_ledger("home-tile", "t");
    let today = crate::spend::day::on(TEST_NOW_MS / 1000);
    ledger.record(&today, Some("Claude Opus 5"), 1.20).unwrap();
    ledger
        .record(&today, Some("Claude Haiku 5.5"), 0.04)
        .unwrap();
    let mut app = app();
    app.spend_ledger = Some(ledger);
    app.open_home();
    let mut answer = spend_answer();
    answer["budget"] = json!({"policySource": "group", "policySubjectName": "data-team",
        "limitCents": 5000, "usedCents": 4100, "remainingCents": 900, "usedUsd": "41.00",
        "activeReservedUsd": "0", "availableUsd": "9.00", "exhausted": false, "windowDays": 30});
    reply(&mut app, "/chat/metrics", Ok(answer));
    for (width, height) in [(120, 32), (80, 40)] {
        let screen = draw(&mut app, width, height);
        assert!(
            screen.contains("████████··  82% · $9.00 of $50 left (30d)"),
            "{screen}"
        );
        assert!(
            screen.contains("gz code      $1.24 today · $1.24 this month"),
            "{screen}"
        );
    }
    assert_eq!(
        color_at(&mut app, 120, 32, "████████··"),
        crate::tui::theme::WARN
    );
}
