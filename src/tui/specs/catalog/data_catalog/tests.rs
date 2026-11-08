//! The data catalog: datasets, every project's jobs and warehouses in one
//! list; each source fails on its own.

use crossterm::event::KeyCode;
use serde_json::json;

use crate::tui::app::test_support::*;
use crate::tui::app::App;

use super::tag_labels;

const RECENT: &str = "2026-11-08T10:00:00.000Z";

fn opened() -> App {
    let mut app = app();
    app.open_and_focus(resource("Catalog", "Data catalog"));
    app
}

fn answer_projects(app: &mut App) {
    answer_path(
        app,
        "/etl/etlProjects",
        json!([{"Id": 1000, "Name": "Sales"}, {"Id": 1001, "Name": "Ops"}]),
    );
}

#[test]
fn lists_datasets_jobs_and_warehouses_in_kind_order() {
    let mut app = opened();
    answer_path(
        &mut app,
        "/dataset",
        json!([{"Id": 7, "Name": "zeta", "modified_by": "me", "modified_on": RECENT, "tags": null}]),
    );
    answer_path(
        &mut app,
        "/lakehouse/warehouse",
        json!({"status": 200, "response": [{"id": 1117, "name": "Lineage", "owner": "sarvesh",
            "created_on": "2026-11-08T10:00:00", "tags": [{"warehouse": [{"domain": "mfg"}], "others": []}]}]}),
    );
    answer_projects(&mut app);
    answer_path(
        &mut app,
        "/etl/etlJobs/1000/jobs",
        json!([{"Id": 3, "Name": "load", "modified_by": "you", "modified_on": RECENT}]),
    );
    answer_path(&mut app, "/etl/etlJobs/1001/jobs", json!([]));
    let grid = app.view().unwrap().grid();
    assert_eq!(
        grid.rows,
        vec![
            vec![
                "Dataset",
                "zeta",
                "Designer › Datasets",
                "me",
                "0",
                "2h ago"
            ],
            vec!["ETL Job", "load", "DE/ML › Sales", "you", "0", "2h ago"],
            vec![
                "Warehouse",
                "Lineage",
                "Lakehouse › Warehouses",
                "sarvesh",
                "1",
                "2h ago"
            ],
        ]
    );
    press(&mut app, &[KeyCode::End, KeyCode::Char('w')]);
    assert_eq!(app.active, resource("Lakehouse", "Warehouses"));
}

#[test]
fn a_failed_source_keeps_the_rest_and_is_named() {
    let mut app = opened();
    let datasets = app
        .outbox
        .iter()
        .find_map(|r| match r {
            crate::tui::fetch::Request::Fetch(f) if f.path == "/dataset" => Some(f.clone()),
            _ => None,
        })
        .unwrap();
    app.fetched(
        datasets.id,
        datasets.target,
        Err("GET x → HTTP 403 Forbidden".into()),
    );
    answer_path(
        &mut app,
        "/lakehouse/warehouse",
        json!({"response": [{"id": 1, "name": "wh"}]}),
    );
    answer_path(&mut app, "/etl/etlProjects", json!([]));
    assert_eq!(app.view().unwrap().grid().rows.len(), 1);
    assert!(
        app.status.text.contains("Datasets failed to load"),
        "{}",
        app.status.text
    );
}

#[test]
fn tags_read_plain_lists_pairs_and_grouped_warehouse_tags() {
    assert_eq!(tag_labels(&json!(["pii", "pii", ""])), ["pii"]);
    assert_eq!(
        tag_labels(&json!([{"key": "tier", "value": "gold"}, {"key": "x"}])),
        ["tier=gold", "x"]
    );
    assert_eq!(
        tag_labels(&json!([{"warehouse": [{}], "others": [{"owner": "ops"}]}])),
        ["owner=ops"]
    );
    assert!(tag_labels(&json!(null)).is_empty());
}
