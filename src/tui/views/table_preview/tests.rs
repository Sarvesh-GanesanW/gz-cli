use super::*;
use crossterm::event::KeyCode;
use serde_json::json;

use crate::tui::app::test_support::*;
use crate::tui::app::App;
use crate::tui::test_stub::{self, Reply};

fn table() -> TableRef {
    TableRef {
        warehouse: "Manufacturing-Lineage".into(),
        database: "manufacturing".into(),
        table: "machine variant".into(),
    }
}

fn opened() -> App {
    let mut app = app();
    app.push_view(Box::new(TablePreview::new(table())));
    app
}

fn properties() -> Value {
    json!({"status": 200, "response": {
        "Row Count": 3, "Total Size Bytes": 2845, "File Count": 3,
        "Last Updated": "2026-11-08 10:00:00", "Description": "No description set",
        "Partitioned By": "Not partitioned", "Format Version": 2, "Compression Codec": "zstd",
        "Snapshot History": [{"snapshot_id": 9007199254740993u64, "operation": "append",
            "added_records": 3, "deleted_records": 0, "timestamp": "2026-11-07 12:00:00"}],
    }})
}

fn schema() -> Value {
    json!({"status": 200, "response": [
        {"columnId": 2, "columnName": "quality_tier", "dataType": "string"},
        {"columnId": 1, "columnName": "machine_type", "dataType": "string"},
        {"columnId": 3, "columnName": "tool_wear_rate_min", "dataType": "int"},
    ]})
}

#[test]
fn loads_schema_properties_and_sample_rows_for_the_table() {
    let app = opened();
    let paths: Vec<String> = app
        .outbox
        .iter()
        .filter_map(|r| match r {
            crate::tui::fetch::Request::Fetch(f) => Some(f.path.clone()),
            _ => None,
        })
        .collect();
    for route in ["schema", "tableproperties", "samplerecords"] {
        let path =
            format!("/lakehouse/{route}/Manufacturing-Lineage/manufacturing/machine%20variant");
        assert!(paths.contains(&path), "{paths:?}");
    }
}

#[test]
fn shows_summary_columns_in_order_samples_and_snapshots() {
    let mut app = opened();
    answer_path(
        &mut app,
        "/lakehouse/schema/Manufacturing-Lineage/manufacturing/machine%20variant",
        schema(),
    );
    answer_path(
        &mut app,
        "/lakehouse/tableproperties/Manufacturing-Lineage/manufacturing/machine%20variant",
        properties(),
    );
    // Sample rows arrive as a JSON string.
    answer_path(
        &mut app,
        "/lakehouse/samplerecords/Manufacturing-Lineage/manufacturing/machine%20variant",
        json!({"status": 200, "response": "[{\"quality_tier\":\"gold\",\"machine_type\":\"L\",\"tool_wear_rate_min\":5}]"}),
    );
    let screen = draw(&mut app, 120, 30);
    assert!(
        screen.contains("3 rows · 2.8 KB · 3 files · updated 2h ago · v2 · not partitioned · zstd"),
        "{screen}"
    );
    assert!(!screen.contains("No description set"));
    let first = screen.find("machine_type").unwrap();
    assert!(
        first < screen.find("quality_tier").unwrap(),
        "columnId order"
    );
    assert!(screen.contains("tool_wear_rate_min  int"), "{screen}");
    assert!(screen.contains("L             gold          5"), "{screen}");
    assert!(screen.contains("append"), "{screen}");
}

#[test]
fn an_unknown_table_answers_200_with_an_error_inside() {
    let mut app = opened();
    answer_path(
        &mut app,
        "/lakehouse/tableproperties/Manufacturing-Lineage/manufacturing/machine%20variant",
        json!({"response": {"error": "No matching records for provided keys: Warehouse, DB and/or Table"}}),
    );
    assert!(draw(&mut app, 120, 20).contains("The lakehouse can't find this table"));
}

#[test]
fn sample_rows_for_non_admins_offer_sql_which_opens_the_sql_view() {
    let mut app = opened();
    let req = app
        .outbox
        .iter()
        .find_map(|r| match r {
            crate::tui::fetch::Request::Fetch(f) if f.path.contains("samplerecords") => {
                Some(f.clone())
            }
            _ => None,
        })
        .unwrap();
    app.fetched(
        req.id,
        req.target,
        Err("GET x → HTTP 403 Forbidden: Unauthorized access".into()),
    );
    let screen = draw(&mut app, 120, 30);
    assert!(
        screen.contains("only shown to lakehouse Admins"),
        "{screen}"
    );
    assert!(screen.contains("Press s to preview with SQL"), "{screen}");
    press(&mut app, &[KeyCode::Char('s')]);
    assert_eq!(app.views.len(), 2);
    assert_eq!(last_task(&app).label, "prepare a lakehouse session");
}

#[test]
fn a_lineage_asset_finds_its_warehouse_in_the_connection_config() {
    let mut app = app();
    app.push_view(Box::new(TablePreview::from_connection(
        "1357".into(),
        "manufacturing.machine_telemetry".into(),
    )));
    assert_eq!(last_fetch(&app).path, "/connection/1357");
    answer(
        &mut app,
        json!([{"name": "ML", "type": "GZLAKEHOUSE", "config": {"warehouseName": "Manufacturing-Lineage", "databaseName": "manufacturing"}}]),
    );
    assert!(app.outbox.iter().any(|r| matches!(r,
        crate::tui::fetch::Request::Fetch(f)
            if f.path == "/lakehouse/schema/Manufacturing-Lineage/manufacturing/machine_telemetry")));
}

#[test]
fn l_opens_the_graph_of_the_asset_the_lookup_finds() {
    let mut app = opened();
    press(&mut app, &[KeyCode::Char('L')]);
    let task = last_task(&app);
    app.fetched(
        task.id,
        task.target,
        Ok(json!({"assetId": 1184, "displayName": "manufacturing.machine_variant"})),
    );
    assert_eq!(app.views.len(), 2);
    assert!(last_fetch(&app)
        .path
        .starts_with("/etl/lineage/assets/1184?"));
}

/// The lookup reads the connections, the lakehouse one's config, then
/// searches that connection's assets for the table.
#[tokio::test]
async fn lineage_lookup_matches_the_warehouse_then_the_qualified_name() {
    let (base, server) = test_stub::serve(vec![
        Reply::json(r#"[{"Id": 1000, "Name": "GZS3", "type": "AWSS3"}, {"Id": 1357, "Name": "ML", "type": "GZLAKEHOUSE"}]"#),
        Reply::json(r#"[{"name": "ML", "type": "GZLAKEHOUSE", "config": {"warehouseName": "manufacturing-lineage"}}]"#),
        Reply::json(r#"{"assets": [{"assetId": 7, "qualifiedName": "other.machine variant"}, {"assetId": 8, "qualifiedName": "manufacturing.machine variant"}], "hasMore": false}"#),
    ])
    .await;
    let found = find_lineage_asset(test_stub::client(&base), &table())
        .await
        .unwrap();
    assert_eq!(found["assetId"], 8);
    let seen = server.await.unwrap();
    assert!(
        seen[0].starts_with("GET /connection HTTP/1.1"),
        "{}",
        seen[0]
    );
    assert!(
        seen[1].starts_with("GET /connection/1357 HTTP/1.1"),
        "{}",
        seen[1]
    );
    assert!(
        seen[2].starts_with(
            "GET /etl/lineage/assets?connectionId=1357&query=machine+variant&limit=100 HTTP/1.1"
        ) || seen[2].starts_with(
            "GET /etl/lineage/assets?connectionId=1357&query=machine%20variant&limit=100 HTTP/1.1"
        ),
        "{}",
        seen[2]
    );
}
