//! DE/ML end to end: projects, jobs, runs and their actions.

use crossterm::event::KeyCode;
use reqwest::Method;
use serde_json::json;

use crate::tui::app::test_support::*;
use crate::tui::app::{App, Overlay};
use crate::tui::fetch::Request;

const RECENT: &str = "2026-11-08T10:00:00.000Z";

fn projects() -> App {
    let mut app = app();
    app.open_and_focus(resource("DE/ML", "Projects"));
    answer(
        &mut app,
        json!([
            {"Id": 1000, "Name": "ETL", "owner": "srijith", "modified_on": "2026-05-04T04:15:08.298Z", "modified_by": null},
            {"Id": 1021, "Name": "Demo", "owner": "robin", "modified_on": RECENT, "modified_by": null},
        ]),
    );
    app
}

fn jobs() -> App {
    let mut app = projects();
    select_row(&mut app, &["Name"], "ETL");
    press(&mut app, &[KeyCode::Enter]);
    assert_eq!(last_fetch(&app).path, "/etl/etlJobs/1000/jobs");
    answer(
        &mut app,
        json!([{"Id": 1006, "Name": "testRunjobSpark", "modified_by": "srijith", "modified_on": RECENT,
                "config": {"connectionNames": ["GZS3"], "minimumWorkers": 2,
                           "selectedJobs": [{"Id": 1007, "Name": "a"}, {"Id": 1008, "Name": "b"}]}}]),
    );
    app
}

fn paths(app: &App) -> Vec<String> {
    app.outbox
        .iter()
        .filter_map(|r| match r {
            Request::Fetch(f) => Some(f.path.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn projects_list_newest_first_without_ids() {
    let app = projects();
    let grid = app.view().unwrap().grid();
    assert_eq!(grid.cols, vec!["Name", "Owner", "Updated by", "Updated"]);
    assert_eq!(grid.rows[0], vec!["Demo", "robin", "—", "2h ago"]);
}

#[test]
fn project_detail_shows_job_aggregates_and_recent_runs_without_the_broken_route() {
    let mut app = projects();
    select_row(&mut app, &["Name"], "ETL");
    press(&mut app, &[KeyCode::Char('i')]);
    let requested = paths(&app);
    assert!(requested
        .contains(&"/etl/consumption-metrics?groupBy=job&projectId=1000&limit=100".to_string()));
    assert!(requested.contains(
        &"/etl/consumption-metrics?projectId=1000&workloadType=job&limit=10".to_string()
    ));
    assert!(
        !requested.iter().any(|p| p == "/etl/etlProjects/1000"),
        "that route always answers 500"
    );
}

#[test]
fn jobs_show_their_definition_and_sub_job_count() {
    let app = jobs();
    let grid = app.view().unwrap().grid();
    assert_eq!(
        grid.cols,
        vec![
            "Name",
            "Connections",
            "Sub-jobs",
            "Workers",
            "Updated by",
            "Updated"
        ]
    );
    assert_eq!(grid.rows[0][..4], ["testRunjobSpark", "GZS3", "2", "2"]);
}

#[test]
fn job_detail_reads_the_saved_job_and_runs_from_consumption_metrics() {
    let mut app = jobs();
    press(&mut app, &[KeyCode::Char('i')]);
    assert_eq!(last_fetch(&app).path, "/etl/etlJobs/jobs/1000/1006");
    answer(
        &mut app,
        json!({"config": {"connectionNames": ["GZS3"], "minimumWorkers": 2,
                                       "selectedJobs": [{"Id": 1007, "Name": "a"}, {"Id": 1008, "Name": "b"}]},
                            "modified_by": "srijith", "modified_on": RECENT}),
    );
    let requested = paths(&app);
    assert!(requested
        .contains(&"/etl/consumption-metrics?jobId=1006&workloadType=job&limit=10".to_string()));
    assert!(requested.contains(
        &"/etl/consumption-metrics?groupBy=job&projectId=1000&jobId=1006&limit=1".to_string()
    ));
    assert!(!requested
        .iter()
        .any(|p| p.contains("/runs") || p.contains("current-run")));
    let screen = draw(&mut app, 110, 40);
    assert!(screen.contains("Runs these jobs"), "{screen}");
    assert!(screen.contains("a → b"), "{screen}");
}

#[test]
fn run_now_opens_the_runtime_picker_for_the_job_in_its_project() {
    let mut app = jobs();
    press(&mut app, &[KeyCode::Char('R')]);
    assert_eq!(app.views.titles(), vec!["Run testRunjobSpark"]);
    let requested = paths(&app);
    assert!(requested.contains(&"/rte/image".to_string()));
    assert!(requested.contains(&"/etl/etlJobs/jobs/1000/1006".to_string()));
}

#[test]
fn view_logs_finds_the_latest_run_from_consumption_metrics() {
    let mut app = jobs();
    press(&mut app, &[KeyCode::Char('L')]);
    assert_eq!(
        last_fetch(&app).path,
        "/etl/consumption-metrics?jobId=1006&workloadType=job&limit=1"
    );
}

#[test]
fn new_job_posts_to_the_selected_project() {
    let mut app = projects();
    select_row(&mut app, &["Name"], "ETL");
    press(&mut app, &[KeyCode::Char('N')]);
    for c in "nightly-load".chars() {
        press(&mut app, &[KeyCode::Char(c)]);
    }
    press(&mut app, &[KeyCode::Enter]);
    let req = last_fetch(&app);
    assert_eq!(
        (req.method.clone(), req.path.as_str()),
        (Method::POST, "/etl/etlJobs/1000")
    );
    assert_eq!(
        req.body.unwrap(),
        json!({"Name": "nightly-load", "projectId": 1000})
    );
}

#[test]
fn names_breaking_the_backend_rule_are_refused_before_sending() {
    let mut app = projects();
    press(&mut app, &[KeyCode::Char('n')]);
    for c in "bad--name".chars() {
        press(&mut app, &[KeyCode::Char(c)]);
    }
    let before = app.outbox.len();
    press(&mut app, &[KeyCode::Enter]);
    assert_eq!(app.outbox.len(), before);
    assert!(matches!(app.overlay, Some(Overlay::Form { .. })));
}

#[test]
fn change_owner_is_for_admins_only() {
    let app = projects();
    let help = app.help_sections();
    assert!(
        !help
            .iter()
            .flat_map(|s| &s.entries)
            .any(|(_, what)| what == "Change owner"),
        "non-admins don't see it"
    );
}

fn runs() -> App {
    let mut app = app();
    app.open_and_focus(resource("DE/ML", "Job runs"));
    assert_eq!(
        last_fetch(&app).path,
        "/etl/consumption-metrics?workloadType=job&limit=100"
    );
    answer(
        &mut app,
        json!({"items": [
            {"executionId": "e1", "sessionId": "s1", "status": "running", "jobId": 1000, "jobName": "testRunjob",
             "projectId": 1000, "projectName": "ETL", "startedAt": "2026-06-01T00:00:00+00:00",
             "durationSeconds": 2.0e7},
            {"executionId": "e2", "sessionId": "s2", "status": "succeeded", "jobId": 1000, "jobName": "testRunjob",
             "projectId": 1000, "projectName": "ETL", "startedAt": "2026-11-08T11:00:00+00:00",
             "durationSeconds": 9.1, "usage": {"cost": {"totalUsd": 0.0177}}},
        ]}),
    );
    app
}

#[test]
fn months_old_running_rows_are_flagged_as_maybe_stuck() {
    let app = runs();
    let grid = app.view().unwrap().grid();
    assert_eq!(grid.rows[0][0], "May be stuck");
    assert_eq!(grid.rows[1][0], "Succeeded");
    assert_eq!(grid.rows[1][5], "$0.02");
}

#[test]
fn run_rows_open_their_log_and_stop_asks_first() {
    let mut app = runs();
    select_row(&mut app, &["executionId"], "e1");
    press(&mut app, &[KeyCode::Char('x')]);
    let Some(Overlay::Confirm { confirm, request }) = &app.overlay else {
        panic!("stop asks first");
    };
    assert!(confirm.message.contains("whole session"));
    assert_eq!(
        request.body,
        Some(json!({"action": "Abort", "sessionId": "s1", "jobIdentifier": "e1"}))
    );
    press(&mut app, &[KeyCode::Esc]);
    select_row(&mut app, &["executionId"], "e2");
    let before = app.outbox.len();
    press(&mut app, &[KeyCode::Char('x')]);
    assert!(
        app.overlay.is_none() && app.outbox.len() == before,
        "finished runs can't be stopped"
    );
    press(&mut app, &[KeyCode::Char('L')]);
    let logs = app
        .outbox
        .iter()
        .find_map(|r| match r {
            Request::Fetch(f) if f.path == "/etl/logs" => Some(f.clone()),
            _ => None,
        })
        .expect("the log viewer asks for this run's logs");
    assert_eq!(
        logs.body.unwrap(),
        json!({"jobIdentifier": "e2", "sessionId": "s2"})
    );
}

#[test]
fn notebooks_are_created_in_the_selected_project() {
    let mut app = app();
    app.open_and_focus(resource("DE/ML", "Notebooks"));
    answer(
        &mut app,
        json!([{"Id": 1000, "Name": "ETL", "owner": "x", "modified_on": RECENT}]),
    );
    press(&mut app, &[KeyCode::Char('n')]);
    for c in "explore".chars() {
        press(&mut app, &[KeyCode::Char(c)]);
    }
    press(&mut app, &[KeyCode::Enter]);
    let req = last_fetch(&app);
    assert_eq!(req.path, "/jupyter/notebook");
    assert_eq!(
        req.body.unwrap(),
        json!({"notebookName": "explore", "projectId": 1000})
    );
}

#[test]
fn model_delete_names_the_model_in_the_query() {
    let mut app = app();
    app.open_and_focus(resource("DE/ML", "Models"));
    answer(
        &mut app,
        json!({"success": true, "models": [
        {"id": 1037, "name": "Predictive Maintenance", "parentId": 1111, "parentName": "Manufacturing"}]}),
    );
    press(&mut app, &[KeyCode::Char('d')]);
    let Some(Overlay::Confirm { request, .. }) = &app.overlay else {
        panic!("delete asks first");
    };
    assert_eq!(
        request.path,
        "/mlflow/projects/1111/models?name=Predictive%20Maintenance"
    );
}
