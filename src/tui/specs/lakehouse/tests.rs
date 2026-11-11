//! Warehouses → databases → tables (grouped from column rows) → preview,
//! and saved queries.

use crossterm::event::KeyCode;
use serde_json::json;

use crate::tui::app::test_support::*;
use crate::tui::app::App;

/// 2026-11-08T10:00Z: two hours before the test clock, in epoch ms.
const TWO_HOURS_AGO_MS: i64 = TEST_NOW_MS - 2 * 3_600_000;

fn at_tables() -> App {
    let mut app = app();
    app.open_and_focus(resource("Lakehouse", "Warehouses"));
    answer(
        &mut app,
        json!({"status": 200, "response": [
            {"id": 1117, "name": "Manufacturing-Lineage", "owner": "sarvesh", "bucket": "gz-lineage",
             "created_on": "2026-09-25T17:10:01.781", "description": null}]}),
    );
    press(&mut app, &[KeyCode::Enter]);
    assert_eq!(
        last_fetch(&app).path,
        "/lakehouse/warehouse/Manufacturing-Lineage/namespaces"
    );
    answer(
        &mut app,
        json!({"status": 200, "response": [{"namespace": "manufacturing", "records": 20989, "size": "4MB"}]}),
    );
    press(&mut app, &[KeyCode::Enter]);
    assert_eq!(
        last_fetch(&app).path,
        "/lakehouse/schema/Manufacturing-Lineage/manufacturing"
    );
    let column = |table: &str, name: &str, bytes: u64| {
        json!({"catalog": "Manufacturing-Lineage", "databaseName": "manufacturing", "tableName": table,
               "columnName": name, "dataType": "int", "totalBytes": bytes,
               "updatedAt": TWO_HOURS_AGO_MS, "createdBy": "svc"})
    };
    answer(
        &mut app,
        json!({"status": 200, "response": [
            column("machine_variant_lu", "machine_type", 2845),
            column("machine_variant_lu", "quality_tier", 2845),
            column("high_risk_readings", "udi", 15811),
        ]}),
    );
    app
}

#[test]
fn databases_show_rows_and_size_parsed_from_megabytes() {
    let mut app = app();
    app.open_and_focus(resource("Lakehouse", "Warehouses"));
    answer(
        &mut app,
        json!({"status": 200, "response": [{"id": 1, "name": "wh"}]}),
    );
    press(&mut app, &[KeyCode::Enter]);
    answer(
        &mut app,
        json!({"status": 200, "response": [{"namespace": "audit", "records": 450849429, "size": "10292MB"}]}),
    );
    let grid = app.view().unwrap().grid();
    assert_eq!(grid.rows[0], vec!["audit", "450,849,429", "10 GB"]);
}

#[test]
fn the_table_level_groups_column_rows_into_tables() {
    let app = at_tables();
    let grid = app.view().unwrap().grid();
    assert_eq!(
        grid.cols,
        vec!["Table", "Columns", "Size", "Updated", "Created by"]
    );
    assert_eq!(
        grid.rows,
        vec![
            vec!["high_risk_readings", "1", "15 KB", "2h ago", "svc"],
            vec!["machine_variant_lu", "2", "2.8 KB", "2h ago", "svc"],
        ]
    );
}

#[test]
fn enter_on_a_table_opens_its_preview_and_i_its_details() {
    let mut app = at_tables();
    press(&mut app, &[KeyCode::Down, KeyCode::Enter]);
    assert_eq!(app.views.len(), 1);
    assert_eq!(
        app.views.titles(),
        ["Manufacturing-Lineage › manufacturing › machine_variant_lu"]
    );
    assert!(last_fetch(&app).path.starts_with(
        "/lakehouse/samplerecords/Manufacturing-Lineage/manufacturing/machine_variant_lu"
    ));
    press(&mut app, &[KeyCode::Esc, KeyCode::Char('i')]);
    assert!(draw(&mut app, 100, 20).contains("Columns"));
}

#[test]
fn database_details_read_the_size_by_position_not_by_name() {
    let mut app = app();
    app.open_and_focus(resource("Lakehouse", "Warehouses"));
    answer(
        &mut app,
        json!({"status": 200, "response": [{"id": 1, "name": "wh"}]}),
    );
    press(&mut app, &[KeyCode::Enter]);
    answer(
        &mut app,
        json!([{"namespace": "sales", "records": 1, "size": "0MB"}]),
    );
    press(&mut app, &[KeyCode::Char('i')]);
    let req = last_fetch(&app);
    assert_eq!(req.path, "/lakehouse/dbsize");
    assert_eq!(
        req.body.unwrap(),
        json!({"warehouseName": "wh", "databaseName": "sales"})
    );
}

#[test]
fn warehouse_delete_types_the_name_and_uses_the_numeric_id() {
    let mut app = app();
    app.open_and_focus(resource("Lakehouse", "Warehouses"));
    answer(
        &mut app,
        json!({"status": 200, "response": [{"id": 1117, "name": "wh"}]}),
    );
    press(&mut app, &[KeyCode::Char('D')]);
    let screen = draw(&mut app, 120, 30);
    assert!(
        screen.contains("DELETE lakehouse /lakehouse/warehouse/1117"),
        "{screen}"
    );
    assert!(screen.contains("refuses while it still"), "{screen}");
}

#[test]
fn saved_query_details_show_the_sql_and_run_it_in_the_sql_view() {
    let mut app = app();
    app.open_and_focus(resource("Lakehouse", "Saved queries"));
    answer(
        &mut app,
        json!([{"Id": 42, "Name": "daily", "warehouseName": "wh", "modified_by": "me",
                "modified_on": "2026-11-08T10:00:00"}]),
    );
    press(&mut app, &[KeyCode::Enter]);
    assert_eq!(last_fetch(&app).path, "/lakehouse/query/42");
    answer(&mut app, json!({"query": "SELECT *\nFROM sales.orders"}));
    let screen = draw(&mut app, 100, 24);
    assert!(screen.contains("SQL"), "{screen}");
    assert!(screen.contains("FROM sales.orders"), "{screen}");
    press(&mut app, &[KeyCode::Char('s')]);
    assert_eq!(app.views.titles().last().unwrap(), "SQL on wh");
}

fn warehouses_with_one_long_description() -> App {
    let mut app = app();
    app.open_and_focus(resource("Lakehouse", "Warehouses"));
    let warehouse = |name: &str, description: Option<&str>| {
        json!({"name": name, "owner": "sarvesh.ganesan", "bucket": "gz-lakehouse-manufacturing-prod",
               "created_on": "2026-09-25T17:10:01.781", "description": description})
    };
    answer(
        &mut app,
        json!({"status": 200, "response": [
            warehouse("Manufacturing-Lineage", None),
            warehouse("GZWarehousePROD", Some("Curated production tables for the finance and operations teams")),
            warehouse("Scratch", None),
        ]}),
    );
    app
}

#[test]
fn a_long_description_narrows_instead_of_hiding_another_column() {
    let mut app = warehouses_with_one_long_description();
    let screen = draw(&mut app, 150, 42);
    let header = screen.lines().find(|l| l.contains("Warehouse ")).unwrap();
    for column in ["Description", "Created by", "Bucket", "Created"] {
        assert!(header.contains(column), "{column} is hidden: {header}");
    }
    assert!(!screen.contains("more ▶"), "no column is hidden\n{screen}");
    assert!(
        screen.contains("gz-lakehouse-manufacturing-prod"),
        "{screen}"
    );
    assert!(
        screen.contains("Curated production tables for th…"),
        "the long text is cut with an ellipsis\n{screen}"
    );
}

#[test]
fn every_column_shows_in_full_when_the_terminal_is_wide_enough() {
    let mut app = warehouses_with_one_long_description();
    let screen = draw(&mut app, 200, 20);
    assert!(
        screen.contains("Curated production tables for the finance and operations teams"),
        "{screen}"
    );
    assert!(
        !screen.contains('…') && !screen.contains("more ▶"),
        "{screen}"
    );
}

#[test]
fn a_full_table_still_says_columns_are_hidden() {
    let mut app = app();
    app.open_and_focus(resource("Lakehouse", "Warehouses"));
    let warehouses: Vec<_> = (0..40)
        .map(|i| {
            json!({"name": format!("warehouse-{i:02}"), "owner": "sarvesh.ganesan",
                   "bucket": "gz-lakehouse-manufacturing-prod",
                   "created_on": "2026-09-25T17:10:01.781",
                   "description": "Curated production tables for the finance and operations teams"})
        })
        .collect();
    answer(&mut app, json!({"status": 200, "response": warehouses}));
    let screen = draw(&mut app, 80, 20);
    let edge = screen
        .lines()
        .find(|line| line.contains("more ▶"))
        .unwrap_or_else(|| panic!("no hint with the rows filling the table\n{screen}"));
    assert!(
        edge.contains("╰") && edge.trim_end().ends_with("╯"),
        "on the frame's bottom edge: {edge}"
    );
    assert!(
        screen.contains("warehouse-14  Curated"),
        "the last rows stay visible under the hint\n{screen}"
    );
}

#[test]
fn columns_still_scroll_when_a_narrow_terminal_cant_hold_them() {
    let mut app = warehouses_with_one_long_description();
    let screen = draw(&mut app, 80, 20);
    assert!(screen.contains("more ▶"), "{screen}");
    assert!(screen.contains('…'), "{screen}");
}
