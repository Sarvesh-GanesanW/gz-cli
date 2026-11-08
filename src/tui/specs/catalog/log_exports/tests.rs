//! Log export runs (keyed by exportRunId, paged) and schedules (keyed by
//! scheduleName, ended past their end date).

use crossterm::event::KeyCode;
use serde_json::{json, Value};

use crate::tui::app::test_support::*;

use super::describe_cron;

fn run(id: &str, trigger: &str, name: Option<&str>) -> Value {
    json!({"exportRunId": id, "exportName": name, "triggerType": trigger,
           "scheduleName": if trigger == "scheduled" { Some("test12") } else { None },
           "status": "success", "resourceTypes": ["admin"],
           "logWindowStartAt": "2026-11-01T08:25:00Z", "logWindowEndAt": "2026-11-02T08:25:00Z",
           "createdAt": "2026-11-08T10:00:00Z", "startedAt": "2026-11-08T10:00:00Z",
           "completedAt": "2026-11-08T10:00:20Z", "runStartedAt": null, "connectionId": "1000",
           "outputObjects": [{"bucket": "b", "key": "p/x.jsonl", "eventCount": 1793, "outputBytes": 2641812}]})
}

#[test]
fn runs_page_through_every_run_and_name_unnamed_ones() {
    let mut app = app();
    app.open_and_focus(resource("Catalog", "Log exports"));
    assert_eq!(last_fetch(&app).path, "/log-export/runs?limit=100&offset=0");
    let mut runs: Vec<Value> = (0..99)
        .map(|i| run(&format!("r{i}"), "scheduled", None))
        .collect();
    runs.insert(0, run("export-1", "manual", None));
    answer(
        &mut app,
        json!({"runs": runs, "total": 1441, "limit": 100, "offset": 0}),
    );
    let grid = app.view().unwrap().grid();
    assert_eq!(
        grid.rows[0],
        vec![
            "Run Once · 2026-11-01T08:25:00Z",
            "Success",
            "Run Once",
            "Admin Logs",
            "2h ago",
            "2.5 MB"
        ]
    );
    assert_eq!(grid.rows[1][0], "test12");
    assert!(draw(&mut app, 140, 10).contains("100 of 1,441 rows"));
    press(&mut app, &[KeyCode::End]);
    assert_eq!(
        last_fetch(&app).path,
        "/log-export/runs?limit=100&offset=100"
    );
}

#[test]
fn a_run_opens_its_details_by_export_run_id_and_only_run_once_deletes() {
    let mut app = app();
    app.open_and_focus(resource("Catalog", "Log exports"));
    answer(
        &mut app,
        json!({"runs": [run("export-1", "manual", Some("etllogs6")), run("t-2", "scheduled", None)], "total": 2}),
    );
    press(&mut app, &[KeyCode::Enter]);
    let screen = draw(&mut app, 110, 40);
    assert!(screen.contains("export-1"), "{screen}");
    assert!(screen.contains("s3://b/p/x.jsonl"), "{screen}");
    assert!(
        screen.contains("20.0 s") || screen.contains("20 s"),
        "{screen}"
    );
    press(&mut app, &[KeyCode::Char('d')]);
    assert!(app.overlay.is_some(), "a Run Once export can be deleted");
    press(
        &mut app,
        &[
            KeyCode::Esc,
            KeyCode::Esc,
            KeyCode::Down,
            KeyCode::Char('d'),
        ],
    );
    assert!(app.overlay.is_none(), "a scheduled run can't");
}

#[test]
fn schedules_past_their_end_date_show_as_ended() {
    let mut app = app();
    app.open_and_focus(resource("Catalog", "Export schedules"));
    answer(
        &mut app,
        json!({"schedules": [
            {"scheduleName": "test12", "status": "active", "cronExpression": "0/1 * * * *",
             "timezone": "UTC", "exportWindowMinutes": 60, "resourceTypes": ["etl"],
             "scheduleEndDate": "2026-11-06T04:35:00Z"},
            {"scheduleName": "nightly", "status": "active", "cronExpression": "30 4 * * *",
             "timezone": "UTC", "exportWindowMinutes": 1440, "resourceTypes": ["chat"],
             "scheduleEndDate": null},
            {"scheduleName": "old", "status": "stopped", "cronExpression": "0 * * * *",
             "exportWindowMinutes": 30, "resourceTypes": ["all"]},
        ]}),
    );
    let grid = app.view().unwrap().grid();
    assert_eq!(
        grid.rows,
        vec![
            vec![
                "nightly",
                "Active",
                "Daily 04:30 UTC",
                "last 1 d",
                "Chat",
                "—"
            ],
            vec![
                "old",
                "Stopped",
                "Hourly at :00 UTC",
                "last 30 min",
                "All",
                "—"
            ],
            vec![
                "test12",
                "Ended",
                "Every 1 minute(s) UTC",
                "last 1 h",
                "ETL",
                "2026-11-06"
            ],
        ]
    );
    // Stop is offered only for active schedules.
    press(&mut app, &[KeyCode::Char('x')]);
    assert!(app.overlay.is_some());
    press(&mut app, &[KeyCode::Esc, KeyCode::End, KeyCode::Char('x')]);
    assert!(app.overlay.is_none());
    press(&mut app, &[KeyCode::Enter]);
    assert_eq!(
        last_fetch(&app).path,
        "/log-export/runs?scheduleName=test12&limit=10&offset=0"
    );
}

#[test]
fn cron_reads_as_words_for_common_shapes() {
    assert_eq!(describe_cron("*/5 * * * *"), "Every 5 minute(s)");
    assert_eq!(describe_cron("15 */2 * * *"), "Every 2 hour(s) at :15");
    assert_eq!(describe_cron("0 9 * * 1"), "Weekly on Mon 09:00");
    assert_eq!(describe_cron("0 9 1 * *"), "Monthly on day 1 at 09:00");
    assert_eq!(describe_cron("0 9 * 1 *"), "0 9 * 1 *");
    assert_eq!(describe_cron("cron(0 9 * * ? *)"), "cron(0 9 * * ? *)");
}
