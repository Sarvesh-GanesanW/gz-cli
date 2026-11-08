//! Schedules end to end: list, detail with the cron in words, actions.

use crossterm::event::KeyCode;
use reqwest::Method;
use serde_json::{json, Value};

use crate::tui::app::test_support::*;
use crate::tui::app::{App, Overlay};
use crate::tui::fetch::Request;

fn executions() -> Value {
    json!([
        {"scheduleId": 1102, "scheduleName": "nightly", "isActive": true, "status": "Success",
         "startTime": "2026-11-08T10:00:00.000Z", "stopTime": "2026-11-08T10:00:05.000Z",
         "executionId": "cd93", "sessionId": "275f", "jobId": 1000, "jobName": "testRunjob",
         "projectId": 1000, "projectName": "ETL", "computeId": 1000,
         "rteName": "gz-rte-duckdb-sklearn:1.0.0", "scheduleType": "BATCH"},
        {"scheduleId": 1033, "scheduleName": "testSchedule1", "isActive": false, "status": "Running",
         "startTime": "2026-08-04T03:27:18.972Z", "stopTime": null,
         "executionId": "54e1", "sessionId": "7fa4", "jobId": 1075, "jobName": "testRunjob",
         "projectId": 1021, "projectName": "Demo", "computeId": 1000,
         "rteName": "gz-rte-duckdb-sklearn:1.0.0", "scheduleType": "BATCH"},
    ])
}

fn schedules() -> App {
    let mut app = app();
    app.open_and_focus(resource("Schedules", "Schedules"));
    assert_eq!(last_fetch(&app).path, "/schedule/executions");
    answer(&mut app, executions());
    app
}

#[test]
fn list_shows_state_job_and_last_run_flagging_stale_runs() {
    let app = schedules();
    let grid = app.view().unwrap().grid();
    assert_eq!(
        grid.cols,
        vec![
            "Schedule",
            "State",
            "Job",
            "Last run",
            "Last run at",
            "Type"
        ]
    );
    assert_eq!(
        grid.rows[0][..4],
        [
            "testSchedule1",
            "○ Paused",
            "Demo › testRunjob",
            "May be stuck"
        ]
    );
    assert_eq!(
        grid.rows[1][..4],
        ["nightly", "● Active", "ETL › testRunjob", "Succeeded"]
    );
}

#[test]
fn detail_describes_the_cron_and_its_next_runs() {
    let mut app = schedules();
    select_row(&mut app, &["scheduleName"], "nightly");
    press(&mut app, &[KeyCode::Enter]);
    assert_eq!(last_fetch(&app).path, "/schedule/1102");
    answer(
        &mut app,
        json!({"cron": "cron(0 9 ? * MON-FRI *)", "enabled": true, "startAt": "2026-07-01T04:04:00.000Z",
               "stopAt": null, "scheduleType": "BATCH", "config": {}}),
    );
    let screen = draw(&mut app, 120, 40);
    assert!(screen.contains("every weekday at 09:00 UTC"), "{screen}");
    assert!(
        screen.contains("2026-11-09 09:00 UTC · 2026-11-10 09:00 UTC"),
        "{screen}"
    );
    assert!(app
        .outbox
        .iter()
        .any(|r| matches!(r, Request::Fetch(f) if f.path == "/schedule/1102/runs")));
}

#[test]
fn pause_and_resume_follow_the_schedules_state() {
    let mut app = schedules();
    select_row(&mut app, &["scheduleName"], "nightly");
    press(&mut app, &[KeyCode::Char('p')]);
    let Some(Overlay::Confirm { request, .. }) = &app.overlay else {
        panic!("pause asks first");
    };
    assert_eq!(
        (request.method.clone(), request.path.as_str()),
        (Method::PUT, "/schedule/stop/1102/nightly")
    );
    press(&mut app, &[KeyCode::Esc]);
    let before = app.outbox.len();
    press(&mut app, &[KeyCode::Char('s')]);
    assert_eq!(
        app.outbox.len(),
        before,
        "an active schedule can't be resumed"
    );

    select_row(&mut app, &["scheduleName"], "testSchedule1");
    press(&mut app, &[KeyCode::Char('s')]);
    let req = last_fetch(&app);
    assert_eq!(
        (req.method, req.path.as_str()),
        (Method::PUT, "/schedule/start/1033/testSchedule1")
    );
}

#[test]
fn stop_is_offered_only_while_the_latest_run_is_unfinished() {
    let mut app = schedules();
    select_row(&mut app, &["scheduleName"], "nightly");
    press(&mut app, &[KeyCode::Char('x')]);
    assert!(app.overlay.is_none());
    select_row(&mut app, &["scheduleName"], "testSchedule1");
    press(&mut app, &[KeyCode::Char('x')]);
    let Some(Overlay::Confirm { request, .. }) = &app.overlay else {
        panic!("stop asks first");
    };
    assert_eq!(
        request.body,
        Some(json!({"action": "Abort", "sessionId": "7fa4", "jobIdentifier": "54e1"}))
    );
}

#[test]
fn edit_and_run_now_open_their_views() {
    let mut app = schedules();
    select_row(&mut app, &["scheduleName"], "nightly");
    press(&mut app, &[KeyCode::Char('e')]);
    assert_eq!(app.views.titles(), vec!["Change nightly"]);
    assert_eq!(last_fetch(&app).path, "/schedule/1102");
    press(&mut app, &[KeyCode::Esc]);
    app.views.clear();
    press(&mut app, &[KeyCode::Char('R')]);
    assert_eq!(app.views.titles(), vec!["Run nightly now"]);
    assert_eq!(last_fetch(&app).path, "/schedule/1102/runs");
}

#[test]
fn executions_drill_into_a_schedules_runs_with_links_back() {
    let mut app = app();
    app.open_and_focus(resource("Schedules", "Executions"));
    answer(&mut app, executions());
    select_row(&mut app, &["scheduleName"], "nightly");
    press(&mut app, &[KeyCode::Enter]);
    assert_eq!(last_fetch(&app).path, "/schedule/1102/runs");
    answer(
        &mut app,
        json!([{"scheduleId": 1102, "scheduleName": "nightly", "executionId": "cd93", "sessionId": "275f",
                "status": "Failed", "startTime": "2026-08-04T03:27:18.972Z", "stopTime": "2026-08-04T03:27:18.972Z",
                "jobId": 1000, "jobName": "testRunjob", "projectId": 1000, "projectName": "ETL",
                "rteName": "gz-rte-duckdb-sklearn:1.0.0"}]),
    );
    assert_eq!(app.view().unwrap().grid().rows[0][0], "Failed");
    press(&mut app, &[KeyCode::Char('s')]);
    assert_eq!(app.active, resource("Schedules", "Schedules"));
}

/// The pause request, sent for real to a local stub server.
#[tokio::test]
async fn pause_reaches_the_schedule_lambda_route() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut chunk = [0u8; 4096];
        let n = socket.read(&mut chunk).await.unwrap();
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        String::from_utf8_lossy(&chunk[..n]).to_string()
    });

    let mut app = schedules();
    select_row(&mut app, &["scheduleName"], "nightly");
    press(&mut app, &[KeyCode::Char('p'), KeyCode::Enter]);
    let request = Request::Fetch(last_fetch(&app));
    let resolved = crate::config::resolve(
        &crate::config::Config::default(),
        Some("t"),
        Some(base),
        Some("tok".into()),
    );
    let client =
        crate::http::ApiClient::new(resolved, crate::output::OutputFormat::Json, true, 0, 5)
            .unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    crate::tui::fetch::Executor::new(client, tx).run(request);

    let seen = server.await.unwrap();
    assert!(
        seen.starts_with("PUT /schedule/stop/1102/nightly HTTP/1.1"),
        "{seen}"
    );
    let Some(crate::tui::fetch::Inbound::Fetched { id, target, result }) = rx.recv().await else {
        panic!("the executor should report back");
    };
    app.fetched(id, target, result);
    assert_eq!(app.status.text, "Schedule paused");
}
