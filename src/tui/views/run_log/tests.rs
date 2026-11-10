use std::time::Duration;

use crossterm::event::KeyCode;
use serde_json::json;

use super::*;
use crate::tui::app::test_support::*;
use crate::tui::app::{App, Overlay};
use crate::tui::fetch::Request;

const EXEC: &str = "8a16d7d532ff45de80da096a9919f07d";

fn run_row() -> Value {
    json!({"executionId": EXEC, "sessionId": "792d1045", "jobId": 1000, "jobName": "testRunjob",
           "projectId": 1000, "status": "running", "startedAt": "2026-11-08T11:58:00+00:00"})
}

fn fetches(app: &App, path: &str) -> Vec<crate::tui::fetch::FetchReq> {
    app.outbox
        .iter()
        .filter_map(|r| match r {
            Request::Fetch(f) if f.path == path => Some(f.clone()),
            _ => None,
        })
        .collect()
}

fn answer_path(app: &mut App, path: &str, value: Value) {
    let req = fetches(app, path)
        .pop()
        .unwrap_or_else(|| panic!("no request to {path}"));
    app.fetched(req.id, req.target, Ok(value));
}

fn fail_path(app: &mut App, path: &str, err: &str) {
    let req = fetches(app, path).pop().unwrap();
    app.fetched(req.id, req.target, Err(err.into()));
}

fn page(ids: &[&str], status: &str, more: bool) -> Value {
    let events: Vec<Value> = ids
        .iter()
        .map(|id| json!({"eventId": id, "message": format!("INFO step {id}"), "timestampMs": TEST_NOW_MS}))
        .collect();
    json!({"events": events, "hasMoreLogs": more, "cursor": "c1",
           "jobStatus": [{"jobstatus": status, "sessionId": "792d1045",
                          "startedat": "Sun, 08 Nov 2026 11:58:00 GMT",
                          "stoppedat": if status == "Running" { Value::Null } else { json!("Sun, 08 Nov 2026 12:00:00 GMT") }}]})
}

const USUAL: &str = "/etl/consumption-metrics?groupBy=job&projectId=1000&jobId=1000&limit=1";

fn open() -> App {
    let mut app = app();
    app.push_view(Box::new(RunLog::new(RunTarget::from_row(&run_row()))));
    app
}

#[test]
fn asks_only_for_this_runs_logs_and_the_jobs_usual_cost() {
    let app = open();
    let logs = fetches(&app, "/etl/logs");
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].method, Method::POST);
    assert_eq!(
        logs[0].body,
        Some(json!({"jobIdentifier": EXEC, "sessionId": "792d1045"}))
    );
    assert_eq!(fetches(&app, USUAL).len(), 1);
    assert!(
        !app.outbox
            .iter()
            .any(|r| matches!(r, Request::Fetch(f) if f.path.contains("current-run"))),
        "current-run can mark runs Failed; the viewer never calls it"
    );
}

#[test]
fn shows_status_elapsed_last_line_and_an_estimated_cost() {
    let mut app = open();
    answer_path(
        &mut app,
        USUAL,
        json!({"items": [{"statistics": {"costUsd": {"averageRatePerHour": 0.6},
                                          "durationSeconds": {"average": 100.0}}}]}),
    );
    answer_path(&mut app, "/etl/logs", page(&["1", "2"], "Running", true));
    let screen = draw(&mut app, 110, 20);
    for expected in [
        "Run 8a16d7d5 · testRunjob",
        "● Running",
        "2m 00s",
        "INFO step 2",
        "≈ $0.02 so far (est. at $0.60/h)",
    ] {
        assert!(screen.contains(expected), "missing {expected:?}\n{screen}");
    }
}

#[test]
fn polls_on_the_web_cadence_and_continues_from_the_cursor() {
    let mut app = open();
    let start = app.now;
    answer_path(&mut app, "/etl/logs", page(&["1"], "Running", true));
    app.on_tick(start.advanced(Duration::from_millis(900)));
    assert_eq!(fetches(&app, "/etl/logs").len(), 1);
    app.on_tick(start.advanced(Duration::from_millis(1_000)));
    let logs = fetches(&app, "/etl/logs");
    assert_eq!(logs.len(), 2, "1 s after new lines");
    assert_eq!(logs[1].body.as_ref().unwrap()["cursor"], "c1");
    app.now = start.advanced(Duration::from_millis(1_000));
    answer_path(&mut app, "/etl/logs", page(&["1"], "Running", true));
    app.on_tick(start.advanced(Duration::from_millis(3_900)));
    assert_eq!(fetches(&app, "/etl/logs").len(), 2);
    app.on_tick(start.advanced(Duration::from_millis(4_000)));
    assert_eq!(fetches(&app, "/etl/logs").len(), 3, "3 s when idle");
}

#[test]
fn finishing_stops_polling_and_says_how_it_ended() {
    let mut app = open();
    let start = app.now;
    answer_path(&mut app, "/etl/logs", page(&["1"], "Failed", false));
    assert_eq!(app.status.text, "Job failed. Check the logs for the cause.");
    app.on_tick(start.advanced(Duration::from_secs(60)));
    assert_eq!(fetches(&app, "/etl/logs").len(), 1);
    assert!(draw(&mut app, 100, 20).contains("● Failed"));
}

#[test]
fn shows_the_collected_cost_once_it_arrives() {
    let mut app = open();
    let start = app.now;
    answer_path(&mut app, "/etl/logs", page(&["1"], "Success", false));
    app.on_tick(start);
    let path = format!("/etl/consumption-metrics/{EXEC}");
    answer_path(
        &mut app,
        &path,
        json!({"usage": {"cost": {"totalUsd": 0.0177}}}),
    );
    assert!(draw(&mut app, 100, 20).contains("cost $0.02"));
}

#[test]
fn throttling_waits_as_asked_and_errors_back_off() {
    let mut app = open();
    let start = app.now;
    fail_path(
        &mut app,
        "/etl/logs",
        "POST u → HTTP 429 Too Many Requests (retry after 5s): CloudWatch is throttling",
    );
    app.on_tick(start.advanced(Duration::from_millis(4_900)));
    assert_eq!(fetches(&app, "/etl/logs").len(), 1);
    app.on_tick(start.advanced(Duration::from_secs(5)));
    assert_eq!(fetches(&app, "/etl/logs").len(), 2);
    app.now = start.advanced(Duration::from_secs(5));
    fail_path(&mut app, "/etl/logs", "POST u → HTTP 502 Bad Gateway: ");
    assert!(draw(&mut app, 100, 20).contains("retrying"));
}

#[test]
fn tail_follows_new_lines_until_the_user_scrolls_up() {
    let mut app = open();
    let ids: Vec<String> = (1..=40).map(|i| i.to_string()).collect();
    let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    answer_path(&mut app, "/etl/logs", page(&refs, "Running", true));
    let screen = draw(&mut app, 100, 16);
    assert!(screen.contains("12:00 INFO step 40"), "{screen}");
    press(&mut app, &[KeyCode::PageUp]);
    let screen = draw(&mut app, 100, 16);
    assert!(!screen.contains("12:00 INFO step 40"), "{screen}");
    assert!(screen.contains("tail off"), "{screen}");
    press(&mut app, &[KeyCode::End]);
    assert!(draw(&mut app, 100, 16).contains("12:00 INFO step 40"));
}

#[test]
fn stop_asks_first_and_names_the_whole_session() {
    let mut app = open();
    answer_path(&mut app, "/etl/logs", page(&["1"], "Running", true));
    press(&mut app, &[KeyCode::Char('x')]);
    let Some(Overlay::Confirm { confirm, request }) = &app.overlay else {
        panic!("stopping asks first");
    };
    assert!(confirm.message.contains("whole session"));
    assert_eq!(request.path, "/etl/job");
    assert_eq!(
        request.body,
        Some(json!({"action": "Abort", "sessionId": "792d1045", "jobIdentifier": EXEC}))
    );
}

#[test]
fn flags_runs_that_may_be_stuck() {
    let mut app = app();
    let mut row = run_row();
    row["startedAt"] = json!("2026-11-08T01:00:00+00:00");
    app.push_view(Box::new(RunLog::new(RunTarget::from_row(&row))));
    answer_path(
        &mut app,
        USUAL,
        json!({"items": [{"statistics": {"durationSeconds": {"average": 9.0}}}]}),
    );
    let mut stale = page(&["1"], "Running", true);
    stale["jobStatus"][0]["startedat"] = json!("Sun, 08 Nov 2026 01:00:00 GMT");
    answer_path(&mut app, "/etl/logs", stale);
    let screen = draw(&mut app, 120, 20);
    assert!(
        screen.contains("May be stuck: running 11h 00m, usually 9.0 s"),
        "{screen}"
    );
}

#[test]
fn opened_from_a_job_it_finds_the_latest_run_without_current_run() {
    let mut app = app();
    app.push_view(Box::new(RunLog::latest_of(
        Some(1000),
        1000,
        Some("testRunjob".into()),
    )));
    let path = "/etl/consumption-metrics?jobId=1000&workloadType=job&limit=1";
    answer_path(&mut app, path, json!({"items": [run_row()]}));
    assert_eq!(
        fetches(&app, "/etl/logs")[0].body.as_ref().unwrap()["jobIdentifier"],
        EXEC
    );
}

#[test]
fn a_job_without_runs_says_so() {
    let mut app = app();
    app.push_view(Box::new(RunLog::latest_of(Some(1000), 7, None)));
    answer_path(
        &mut app,
        "/etl/consumption-metrics?jobId=7&workloadType=job&limit=1",
        json!({"items": []}),
    );
    let screen = draw(&mut app, 100, 12);
    assert!(screen.contains("No runs yet"), "{screen}");
}

#[test]
fn fix_waits_until_the_stream_reports_the_run_failed() {
    use crate::tui::views::HandOff;
    let mut app = open();
    let start = app.now;
    answer_path(&mut app, "/etl/logs", page(&["1"], "Running", true));
    press(&mut app, &[KeyCode::Char('F')]);
    assert_eq!(app.hand_off, None, "a running run isn't handed over");
    assert_eq!(
        app.status.text,
        "Only a failed run can be fixed with gz code"
    );
    app.on_tick(start.advanced(Duration::from_secs(1)));
    answer_path(&mut app, "/etl/logs", page(&["2"], "Failed", false));
    press(&mut app, &[KeyCode::Char('F')]);
    assert_eq!(
        app.hand_off,
        Some(HandOff::FixRun {
            project: Some(1000),
            job: 1000,
            run: Some(EXEC.into()),
        })
    );
}

fn offers_stop(app: &mut App) -> (bool, bool) {
    let footer = draw(app, 150, 30).lines().last().unwrap().to_string();
    let (_, view) = app.views.top().unwrap();
    let in_help = view
        .help()
        .iter()
        .flat_map(|section| &section.entries)
        .any(|(keys, _)| keys == "x");
    (footer.contains("x stop"), in_help)
}

#[test]
fn stop_is_offered_only_while_the_run_is_running() {
    let mut app = open();
    answer_path(&mut app, "/etl/logs", page(&["1"], "Running", true));
    assert_eq!(offers_stop(&mut app), (true, true));
    answer_path(&mut app, USUAL, json!({"items": []}));
    let mut app = open();
    answer_path(&mut app, "/etl/logs", page(&["1"], "Success", false));
    assert_eq!(offers_stop(&mut app), (false, false));
}

#[test]
fn stopping_a_run_that_ended_says_so_and_sends_nothing() {
    let mut app = open();
    answer_path(&mut app, "/etl/logs", page(&["1"], "Success", false));
    let before = app.outbox.len();
    press(&mut app, &[KeyCode::Char('x')]);
    assert!(app.overlay.is_none(), "nothing to confirm");
    assert_eq!(app.outbox.len(), before);
    assert!(
        app.status.text.contains("already ended"),
        "{}",
        app.status.text
    );
}
