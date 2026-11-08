use crossterm::event::KeyCode;
use serde_json::json;

use super::*;
use crate::tui::app::test_support::*;
use crate::tui::app::App;
use crate::tui::fetch::{FetchReq, Request};

fn request_to(app: &App, path: &str) -> Option<FetchReq> {
    app.outbox.iter().rev().find_map(|r| match r {
        Request::Fetch(f) if f.path == path => Some(f.clone()),
        _ => None,
    })
}

fn reply(app: &mut App, path: &str, value: Value) {
    let req = request_to(app, path).unwrap_or_else(|| panic!("no request to {path}"));
    app.fetched(req.id, req.target, Ok(value));
}

fn record(enabled: bool) -> Value {
    json!({"computeId": 1000, "cron": "cron(04/4 * ? * * *)", "description": "nightly",
           "enabled": enabled, "jobId": 1075, "projectId": 1021, "name": "testSchedule1",
           "rteName": "gz-rte-duckdb-sklearn:1.0.0", "scheduleType": "BATCH",
           "startAt": "2026-07-01T04:04:00.000Z", "stopAt": null,
           "config": {"selectedProject": {"Id": 1021, "Name": "Demo"},
                      "selectedJob": {"Id": 1075, "Name": "testRunjob"}, "whenToRepeat": ""}})
}

fn editing(enabled: bool) -> App {
    let mut app = app();
    let listed = json!({"scheduleId": 1033, "scheduleName": "testSchedule1", "isActive": enabled});
    app.push_view(Box::new(ScheduleForm::edit(&listed)));
    reply(&mut app, "/schedule/1033", record(enabled));
    app
}

fn type_text(app: &mut App, text: &str) {
    for c in text.chars() {
        press(app, &[KeyCode::Char(c)]);
    }
}

fn clear_field(app: &mut App) {
    app.on_key(crossterm::event::KeyEvent::new(
        KeyCode::Char('u'),
        crossterm::event::KeyModifiers::CONTROL,
    ));
}

#[test]
fn edit_previews_the_cron_in_plain_english_as_you_type() {
    let mut app = editing(true);
    let screen = draw(&mut app, 100, 30);
    assert!(
        screen.contains("every 4 minutes from :04 to :59 UTC"),
        "{screen}"
    );
    clear_field(&mut app);
    type_text(&mut app, "cron(0 9 ? * MON-FRI *)");
    let screen = draw(&mut app, 100, 30);
    assert!(screen.contains("every weekday at 09:00 UTC"), "{screen}");
    assert!(
        screen.contains("2026-11-09 09:00 UTC"),
        "next run shown\n{screen}"
    );
    clear_field(&mut app);
    type_text(&mut app, "cron(0 9 * * MON *)");
    assert!(draw(&mut app, 100, 30).contains("✗ Use ? in either"));
}

#[test]
fn edit_warns_that_saving_resumes_a_paused_schedule() {
    let mut app = editing(false);
    assert!(draw(&mut app, 100, 30).contains("paused. Saving resumes it"));
}

#[test]
fn edit_resends_the_whole_record_with_the_new_timing() {
    let mut app = editing(false);
    clear_field(&mut app);
    type_text(&mut app, "cron(30 9 ? * 2 *)");
    press(&mut app, &[KeyCode::Enter]);
    let put = request_to(&app, "/schedule/1033/testSchedule1").expect("a PUT");
    assert_eq!(put.method, Method::PUT);
    let body = put.body.unwrap();
    assert_eq!(body["cron"], "cron(30 9 ? * 2 *)");
    assert_eq!(body["description"], "nightly");
    assert_eq!(body["scheduleType"], "BATCH");
    assert_eq!(
        body["jobDetails"],
        json!({"projectId": 1021, "jobId": 1075, "computeId": 1000,
               "rteName": "gz-rte-duckdb-sklearn:1.0.0", "projectName": "Demo", "jobName": "testRunjob"})
    );
    // The saved start is in the past, so it moves just ahead of now.
    assert_eq!(body["startTime"], "2026-11-08T12:02:00.000Z");
    assert!(body.get("stopTime").is_none(), "no end means never");
    assert_eq!(body["config"]["endsOn"], "Never");
    assert_eq!(body["config"]["selectedJob"]["Name"], "testRunjob");
    reply(&mut app, "/schedule/1033/testSchedule1", Value::Null);
    assert!(app.views.is_empty());
    assert!(app.status.text.contains("successfully rescheduled"));
}

#[test]
fn server_validation_errors_show_under_the_form() {
    let mut app = editing(true);
    press(&mut app, &[KeyCode::Enter]);
    let put = request_to(&app, "/schedule/1033/testSchedule1").unwrap();
    app.fetched(
        put.id,
        put.target,
        Err("PUT u → HTTP 400 Bad Request: cron, must have at least 17 characters!".into()),
    );
    assert!(
        draw(&mut app, 100, 30).contains("Save failed: cron, must have at least 17 characters!")
    );
}

fn job_row() -> RowContext {
    RowContext {
        row: json!({"Id": 1000, "Name": "testRunjob"}),
        title: Some("testRunjob".into()),
        noun: "job",
        spec: None,
        parents: vec![json!({"Id": 1000, "Name": "ETL"})],
        ids: vec!["1000".into(), "1000".into()],
        resource: None,
        view: None,
    }
}

#[test]
fn create_posts_the_webs_body_for_the_job() {
    let mut app = app();
    app.push_view(Box::new(ScheduleForm::create(&job_row())));
    reply(
        &mut app,
        "/rte/image",
        json!([{"Name": "gz-rte-duckdb-sklearn:1.0.0"}, {"Name": "demo-rte-custom:duckdb-x"}]),
    );
    reply(
        &mut app,
        "/rte/compute",
        json!([{"Id": 1000, "Name": "mini", "Description": "Mini"}]),
    );
    type_text(&mut app, "nightly_etl");
    press(&mut app, &[KeyCode::Enter]);
    let post = request_to(&app, "/schedule").expect("a POST");
    assert_eq!(post.method, Method::POST);
    let body = post.body.unwrap();
    assert_eq!(body["name"], "nightly_etl");
    assert_eq!(body["cron"], "cron(0 9 ? * MON-FRI *)");
    assert_eq!(body["scheduleType"], "BATCH");
    assert_eq!(body["startTime"], "2026-11-08T12:02:00.000Z");
    assert_eq!(
        body["jobDetails"],
        json!({"projectName": "ETL", "projectId": 1000, "jobName": "testRunjob", "jobId": 1000,
               "computeId": 1000, "rteName": "gz-rte-duckdb-sklearn:1.0.0"})
    );
    assert_eq!(
        body["config"]["selectedProject"],
        json!({"Id": 1000, "Name": "ETL"})
    );
}

#[test]
fn streaming_schedules_run_once_at_their_start() {
    assert_eq!(
        one_shot_cron(1_794_139_200_000 + 10 * 60_000),
        "cron(10 12 8 11 ? 2026)"
    );
}

#[test]
fn custom_runtimes_cannot_be_scheduled() {
    let mut app = app();
    app.push_view(Box::new(ScheduleForm::create(&job_row())));
    reply(
        &mut app,
        "/rte/image",
        json!([{"Name": "demo-rte-custom:duckdb-x"}]),
    );
    reply(
        &mut app,
        "/rte/compute",
        json!([{"Id": 1000, "Name": "mini"}]),
    );
    type_text(&mut app, "n1");
    press(&mut app, &[KeyCode::Enter]);
    assert!(request_to(&app, "/schedule").is_none());
    assert!(draw(&mut app, 100, 34).contains("can't be scheduled"));
}
