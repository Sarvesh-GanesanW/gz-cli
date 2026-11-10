use super::*;
use crossterm::event::KeyCode;
use serde_json::json;

use crate::tui::app::test_support::*;
use crate::tui::app::App;
use crate::tui::test_stub::{self, Reply as Canned};

fn opened() -> App {
    let mut app = app();
    app.push_view(Box::new(SqlView::new("Manufacturing-Lineage", "SELECT 1")));
    app
}

fn answer_task(app: &mut App, value: Value) {
    let task = last_task(app);
    app.fetched(task.id, task.target, Ok(value));
}

fn rows() -> Value {
    json!({"columns": ["n"], "rows": [{"n": 1}], "pageNumber": 1, "hasMore": false, "totalRecords": 1, "seconds": 0.42})
}

#[test]
fn starting_a_session_is_an_explicit_step_with_progress() {
    let mut app = opened();
    assert_eq!(last_task(&app).label, "prepare a lakehouse session");
    answer_task(
        &mut app,
        json!({"warehouseId": 1117, "session": null, "computes": [
            {"id": 1000, "name": "mini", "description": "Mini"},
            {"id": 1001, "name": "standard", "description": "Standard"}]}),
    );
    let screen = draw(&mut app, 110, 20);
    assert!(
        screen.contains("No running session on Manufacturing-Lineage"),
        "{screen}"
    );
    assert!(screen.contains("a minute or more"), "{screen}");
    assert!(screen.contains("standard"), "{screen}");
    let tasks = app.outbox.len();
    press(&mut app, &[KeyCode::Down]);
    assert_eq!(app.outbox.len(), tasks, "picking a compute starts nothing");
    press(&mut app, &[KeyCode::Enter]);
    assert_eq!(last_task(&app).label, "start a lakehouse session");
    assert!(draw(&mut app, 110, 20).contains("Starting a lakehouse session…"));
    answer_task(&mut app, json!({"session": "s-1"}));
    assert_eq!(last_task(&app).label, "run a lakehouse query");
    assert!(draw(&mut app, 110, 20).contains("Running…"));
    answer_task(&mut app, rows());
    let screen = draw(&mut app, 110, 20);
    assert!(screen.contains("1 row · 0.42s"), "{screen}");
    assert!(screen.contains("Session s-1 is running"), "{screen}");
}

#[test]
fn a_running_session_is_reused_and_the_query_runs_at_once() {
    let mut app = opened();
    answer_task(
        &mut app,
        json!({"warehouseId": 1117, "session": "s-9", "computes": []}),
    );
    assert_eq!(last_task(&app).label, "run a lakehouse query");
}

#[test]
fn the_query_can_be_edited_and_rerun() {
    let mut app = opened();
    answer_task(
        &mut app,
        json!({"warehouseId": 1117, "session": "s-9", "computes": []}),
    );
    answer_task(&mut app, rows());
    press(&mut app, &[KeyCode::Char('e'), KeyCode::Backspace]);
    press(&mut app, &[KeyCode::Char('2'), KeyCode::Char('q')]);
    assert_eq!(app.views.len(), 1, "q is text while editing");
    assert!(draw(&mut app, 100, 10).contains("SELECT 2q"));
    let tasks = app.outbox.len();
    press(&mut app, &[KeyCode::Enter]);
    assert!(app.outbox.len() > tasks);
    assert_eq!(last_task(&app).label, "run a lakehouse query");
}

#[test]
fn failures_are_shown_with_what_to_do_next() {
    let mut app = opened();
    let task = last_task(&app);
    app.fetched(
        task.id,
        task.target,
        Err("You can't see warehouse X (or it was deleted).".into()),
    );
    let screen = draw(&mut app, 110, 12);
    assert!(screen.contains("You can't see warehouse X"), "{screen}");
    assert!(screen.contains("n start a new session"), "{screen}");
}

#[test]
fn outcomes_survive_the_trip_through_json() {
    let page = Page {
        columns: vec!["a".into()],
        column_types: Vec::new(),
        rows: vec![json!({"a": 1})],
        page_number: 1,
        has_more: true,
        total_records: Some(500),
    };
    assert_eq!(
        output_of(&outcome_json(&Reply::Rows(page.clone()), 1.5)),
        Output::Rows(page, 1.5)
    );
    assert_eq!(
        output_of(&outcome_json(&Reply::Done("Table created".into()), 0.1)),
        Output::Message("Table created".into())
    );
}

/// Preparing reads the warehouses and the compute sizes with the same
/// client calls `gz sql` makes.
#[tokio::test]
async fn prepare_finds_the_warehouse_id_and_lists_computes() {
    let (base, server) = test_stub::serve(vec![
        Canned::json(
            r#"{"status": 200, "response": [{"id": 1117, "name": "Manufacturing-Lineage"}]}"#,
        ),
        Canned::json(r#"[{"Id": 1000, "Name": "mini", "Description": "Mini"}]"#),
    ])
    .await;
    let answer = prepare(test_stub::client(&base), "manufacturing-lineage")
        .await
        .unwrap();
    assert_eq!(answer["warehouseId"], 1117);
    assert_eq!(answer["session"], Value::Null);
    assert_eq!(answer["computes"][0]["name"], "mini");
    let seen = server.await.unwrap();
    assert!(seen[0].starts_with("GET /lakehouse/warehouse HTTP/1.1"));
    assert!(seen[1].starts_with("GET /rte/compute HTTP/1.1"));
}
