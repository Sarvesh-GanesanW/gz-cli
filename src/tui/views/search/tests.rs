use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{json, Value};

use super::*;
use crate::tui::app::test_support::*;
use crate::tui::app::App;
use crate::tui::fetch::FetchTarget;
use crate::tui::palette::Action;

fn type_text(app: &mut App, text: &str) {
    for c in text.chars() {
        press(app, &[KeyCode::Char(c)]);
    }
}

fn search_fetches(app: &App) -> Vec<String> {
    app.outbox
        .iter()
        .filter_map(|r| match r {
            crate::tui::fetch::Request::Fetch(f) if f.path.starts_with("/search") => {
                Some(f.path.clone())
            }
            _ => None,
        })
        .collect()
}

/// Opens search, types `query` and lets the debounce pass.
fn searched(query: &str) -> App {
    let mut app = app();
    press(&mut app, &[KeyCode::Char('/')]);
    type_text(&mut app, query);
    app.on_tick(app.now.advanced(DEBOUNCE));
    app
}

fn hits() -> Value {
    json!({"kinds": [], "results": [
        {"kind": "agent", "id": 12, "name": "InvoiceExtractor", "context": "Finance",
         "owner": "sarvesh", "updatedOn": "2026-11-08T10:00:00+00:00",
         "routeKey": "7ec8197e-uuid", "parentId": 3},
        {"kind": "dataset", "id": 1138, "name": "Invoices 2026", "owner": "prathiksha"},
        {"kind": "agent", "id": 13, "name": "InvoiceRouter", "routeKey": "8fd9-uuid"},
        {"kind": "job", "id": 77, "name": "invoice_etl", "parentId": 5, "context": "Billing"},
    ]})
}

#[test]
fn slash_opens_search_from_the_sidebar_and_from_tables() {
    let mut app = app();
    press(&mut app, &[KeyCode::Char('/')]);
    assert_eq!(app.views.top().unwrap().1.title(), "Search");
    press(&mut app, &[KeyCode::Esc]);
    app.open_and_focus(resource("Designer", "Datasets"));
    press(&mut app, &[KeyCode::Char('/')]);
    assert_eq!(app.views.top().unwrap().1.title(), "Search");
    type_text(&mut app, "q?");
    assert!(!app.quit && !app.help, "letters are the query");
}

#[test]
fn typing_waits_for_a_pause_before_searching() {
    let mut app = app();
    let start = app.now;
    press(&mut app, &[KeyCode::Char('/')]);
    type_text(&mut app, "inv");
    app.on_tick(start.advanced(Duration::from_millis(100)));
    assert!(search_fetches(&app).is_empty(), "still typing");
    app.now = start.advanced(Duration::from_millis(100));
    type_text(&mut app, "oice & co");
    app.on_tick(start.advanced(Duration::from_millis(200)));
    assert!(
        search_fetches(&app).is_empty(),
        "the pause restarts per key"
    );
    app.on_tick(start.advanced(Duration::from_millis(250)));
    assert_eq!(search_fetches(&app), ["/search?q=invoice+%26+co&limit=50"]);
    let req = last_fetch(&app);
    assert_eq!(req.service, "chat");
    assert!(matches!(req.target, FetchTarget::View(_)));
    app.on_tick(start.advanced(Duration::from_secs(5)));
    assert_eq!(search_fetches(&app).len(), 1, "one search per pause");
}

#[test]
fn results_are_grouped_by_kind_after_matching_screens() {
    let mut app = searched("agent");
    answer(&mut app, hits());
    let screen = draw(&mut app, 100, 30);
    assert!(screen.contains("Screens ("), "{screen}");
    assert!(screen.contains("Agents › Agents"));
    let agents = screen.find("Agents (2)").unwrap();
    let datasets = screen.find("Datasets (1)").unwrap();
    let jobs = screen.find("Jobs (1)").unwrap();
    assert!(agents < datasets && datasets < jobs, "kinds by best hit");
    let line = screen
        .lines()
        .find(|l| l.contains("InvoiceExtractor"))
        .unwrap();
    assert!(line.contains("Finance · sarvesh · 2h ago"), "{line}");
    assert!(screen.contains("result(s)"));
}

#[test]
fn stale_answers_are_dropped() {
    let mut app = searched("inv");
    let first = last_fetch(&app);
    type_text(&mut app, "o");
    app.on_tick(app.now.advanced(DEBOUNCE));
    app.fetched(first.id, first.target, Ok(hits()));
    assert!(!draw(&mut app, 100, 30).contains("InvoiceExtractor"));
    answer(&mut app, json!({"results": []}));
    assert!(draw(&mut app, 100, 30).contains("No matching resources found."));
}

#[test]
fn tab_narrows_the_search_to_one_kind() {
    let mut app = searched("inv");
    press(&mut app, &[KeyCode::Tab]);
    app.on_tick(app.now.advanced(DEBOUNCE));
    assert_eq!(
        search_fetches(&app).last().unwrap(),
        "/search?q=inv&limit=50&kinds=dataset"
    );
    app.on_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
    app.on_tick(app.now.advanced(DEBOUNCE));
    assert_eq!(
        search_fetches(&app).last().unwrap(),
        "/search?q=inv&limit=50"
    );
}

#[test]
fn enter_opens_an_agent_by_its_route_key_and_back_returns() {
    let mut app = searched("invoice");
    answer(&mut app, hits());
    press(&mut app, &[KeyCode::Enter]);
    assert!(app.views.is_empty());
    assert_eq!(app.active, resource("Agents", "Agents"));
    assert_eq!(last_fetch(&app).path, "/v1/agents");
    answer(
        &mut app,
        json!([{"id": "1-uuid", "name": "Other"}, {"id": "7ec8197e-uuid", "name": "InvoiceExtractor"}]),
    );
    let grid = app.view().unwrap().grid();
    assert_eq!(grid.selected_raw().unwrap()["name"], "InvoiceExtractor");
    assert_eq!(app.recent[0].label, "Agents › Agents › InvoiceExtractor");
    press(&mut app, &[KeyCode::Esc]);
    assert_eq!(app.active, resource("Chat", "Conversations"));
}

#[test]
fn a_job_opens_inside_its_project() {
    let mut app = searched("invoice");
    answer(&mut app, hits());
    press(
        &mut app,
        &[KeyCode::Down, KeyCode::Down, KeyCode::Down, KeyCode::Enter],
    );
    assert_eq!(app.active, resource("DE/ML", "Projects"));
    answer(
        &mut app,
        json!([{"Id": 4, "Name": "Ops"}, {"Id": 5, "Name": "Billing"}]),
    );
    assert_eq!(last_fetch(&app).path, "/etl/etlJobs/5/jobs");
    answer(
        &mut app,
        json!([{"Id": 76}, {"Id": 77, "Name": "invoice_etl"}]),
    );
    let grid = app.view().unwrap().grid();
    assert_eq!(grid.selected_raw().unwrap()["Id"], 77);
}

#[test]
fn a_screen_result_opens_that_screen() {
    let mut app = searched("warehouses");
    press(&mut app, &[KeyCode::Enter]);
    assert_eq!(app.active, resource("Lakehouse", "Warehouses"));
}

#[test]
fn failures_are_explained_in_place() {
    let mut app = searched("inv");
    let req = last_fetch(&app);
    app.fetched(
        req.id,
        req.target,
        Err("GET /search → HTTP 503 Service Unavailable: Search is temporarily unavailable".into()),
    );
    let screen = draw(&mut app, 100, 30);
    assert!(screen.contains("chat service"), "{screen}");
}

#[test]
fn the_palette_hands_its_query_to_search() {
    let mut app = app();
    app.run_action(Action::Search("invoice".into()));
    assert_eq!(search_fetches(&app), ["/search?q=invoice&limit=50"]);
}

#[test]
fn every_kind_opens_a_screen_that_exists() {
    for kind in KINDS {
        let Target { module, resource } = kind.target;
        assert!(
            nav::find_resource(module, resource).is_some(),
            "{} results open missing {module} › {resource}",
            kind.id
        );
    }
    assert_eq!(KINDS.len(), 12, "the server searches 12 kinds");
}

#[test]
fn hits_without_their_key_field_say_so() {
    let err = place_for(&json!({"kind": "agent", "id": 1, "name": "x"})).unwrap_err();
    assert_eq!(err, "This result has no routeKey");
    let err = place_for(&json!({"kind": "notebook", "id": 1})).unwrap_err();
    assert!(err.contains("can't open notebook"));
}
