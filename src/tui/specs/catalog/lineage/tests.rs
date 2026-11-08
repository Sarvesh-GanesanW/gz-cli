//! Lineage: connections that have lineage (named from /connection), their
//! assets a page at a time, keyed by assetId, opening the graph.

use crossterm::event::KeyCode;
use serde_json::{json, Value};

use crate::tui::app::test_support::*;
use crate::tui::app::App;

fn lineage_connections() -> Value {
    json!({"connections": [
        {"connectionId": 1357, "assetCount": 51, "assetTypes": [{"assetType": "TABLE", "count": 51}]},
        {"connectionId": 1000, "assetCount": 9, "assetTypes": [{"assetType": "OBJECT", "count": 9}]},
        {"connectionId": 4242, "assetCount": 1, "assetTypes": []},
    ]})
}

fn names() -> Value {
    json!([
        {"Id": 1000, "Name": "GZS3", "type": "AWSS3"},
        {"Id": 1357, "Name": "ManufacturingLineage", "type": "GZLAKEHOUSE"},
        {"Id": 1171, "Name": "Agents", "type": "GZAGENT"},
    ])
}

fn asset(id: u64, name: &str) -> Value {
    json!({"assetId": id, "displayName": name, "qualifiedName": name, "assetType": "TABLE",
           "platform": "gzlakehouse",
           "latestVersion": {"columnCount": 14, "observedOn": "2026-11-08T10:00:00+00:00"},
           "bindings": [{"connectionId": 1357, "connectionName": "ManufacturingLineage",
                         "connectionType": "GZLAKEHOUSE", "externalKey": "X"}]})
}

fn lineage() -> App {
    let mut app = app();
    app.open_and_focus(resource("Catalog", "Data lineage"));
    answer_path(&mut app, "/etl/lineage/connections", lineage_connections());
    answer_path(&mut app, "/connection", names());
    app
}

#[test]
fn lists_only_connections_with_lineage_named_from_the_connection_list() {
    let app = lineage();
    let grid = app.view().unwrap().grid();
    assert_eq!(
        grid.rows,
        vec![
            vec!["Connection 4242", "—", "1", "—"],
            vec!["GZS3", "AWS S3", "9", "OBJECT 9"],
            vec![
                "ManufacturingLineage",
                "Groundzero Lakehouse",
                "51",
                "TABLE 51"
            ],
        ]
    );
}

#[test]
fn a_failed_name_lookup_still_lists_the_connections() {
    let mut app = app();
    app.open_and_focus(resource("Catalog", "Data lineage"));
    answer_path(&mut app, "/etl/lineage/connections", lineage_connections());
    let names = last_fetch(&app);
    app.fetched(
        names.id,
        names.target,
        Err("GET x → HTTP 401 Unauthorized".into()),
    );
    assert_eq!(app.view().unwrap().grid().rows.len(), 3);
    assert!(app.status.text.contains("Connection names didn't load"));
}

#[test]
fn assets_page_with_limit_and_offset_and_load_more_at_the_end() {
    let mut app = lineage();
    select_row(&mut app, &["Name"], "ManufacturingLineage");
    press(&mut app, &[KeyCode::Enter]);
    assert_eq!(
        last_fetch(&app).path,
        "/etl/lineage/assets?connectionId=1357&limit=100&offset=0"
    );
    let page: Vec<Value> = (0..100)
        .map(|i| asset(2000 + i, &format!("t{i:03}")))
        .collect();
    answer(
        &mut app,
        json!({"assets": page, "total": 101, "hasMore": true, "nextOffset": 100}),
    );
    assert!(draw(&mut app, 120, 12).contains("100 of 101 rows · ↓ more"));
    press(&mut app, &[KeyCode::End]);
    assert_eq!(
        last_fetch(&app).path,
        "/etl/lineage/assets?connectionId=1357&limit=100&offset=100"
    );
    answer(
        &mut app,
        json!({"assets": [asset(9, "t100")], "total": 101, "hasMore": false, "nextOffset": null}),
    );
    let view = app.view().unwrap();
    assert_eq!(view.grid().rows.len(), 101);
    assert_eq!(view.more_hint(), None);
    assert_eq!(view.grid().selected, 99, "the place is kept");
}

#[test]
fn enter_opens_the_graph_by_asset_id_and_i_the_details() {
    let mut app = lineage();
    select_row(&mut app, &["Name"], "ManufacturingLineage");
    press(&mut app, &[KeyCode::Enter]);
    answer(
        &mut app,
        json!({"assets": [asset(1184, "manufacturing.machine_telemetry")], "hasMore": false}),
    );
    press(&mut app, &[KeyCode::Enter]);
    assert_eq!(
        app.views.titles(),
        ["Lineage manufacturing.machine_telemetry"]
    );
    assert!(last_fetch(&app)
        .path
        .starts_with("/etl/lineage/assets/1184?depth=1"));
    press(&mut app, &[KeyCode::Esc, KeyCode::Char('i')]);
    assert_eq!(
        last_fetch(&app).path,
        "/etl/lineage/assets/1184/impact?depth=3&assetLimit=100"
    );
    let screen = draw(&mut app, 110, 30);
    assert!(screen.contains("External key"), "{screen}");
}

#[test]
fn a_lakehouse_asset_opens_its_table() {
    let mut app = lineage();
    select_row(&mut app, &["Name"], "ManufacturingLineage");
    press(&mut app, &[KeyCode::Enter]);
    answer(
        &mut app,
        json!({"assets": [asset(1184, "manufacturing.machine_telemetry")], "hasMore": false}),
    );
    press(&mut app, &[KeyCode::Char('t')]);
    assert_eq!(last_fetch(&app).path, "/connection/1357");
}
