use super::*;
use crossterm::event::KeyCode;
use serde_json::json;

use crate::tui::app::test_support::*;

/// A centre table fed by one table and feeding two, one of them through
/// another (so the tree has depth 2), with column lineage on one edge.
fn graph(runs: &str) -> Value {
    json!({"graph": {
        "centerAssetId": 2,
        "assets": [
            {"assetId": 1, "displayName": "raw.readings", "assetType": "TABLE", "isDeleted": false},
            {"assetId": 2, "displayName": "mfg.telemetry", "assetType": "TABLE", "isDeleted": false,
             "qualifiedName": "mfg.telemetry",
             "bindings": [{"connectionId": 1357, "connectionType": "GZLAKEHOUSE"}]},
            {"assetId": 3, "displayName": "mfg.high_risk", "assetType": "TABLE", "isDeleted": true},
            {"assetId": 4, "displayName": "summary-ds", "assetType": "DATASET", "isDeleted": false},
        ],
        "relations": [
            {"sourceAssetId": 1, "targetAssetId": 2, "relationType": "DATA_FLOW", "jobVersionId": 10, "runIds": [5]},
            {"sourceAssetId": 2, "targetAssetId": 3, "relationType": "DATA_FLOW", "jobVersionId": 11, "runIds": []},
            {"sourceAssetId": 2, "targetAssetId": 3, "relationType": "COLUMN_LINEAGE", "jobVersionId": 11,
             "sourceColumn": "udi", "targetColumn": "udi", "runIds": []},
            {"sourceAssetId": 3, "targetAssetId": 4, "relationType": "DATA_FLOW", "jobVersionId": 12, "runIds": []},
            {"sourceAssetId": 2, "targetAssetId": 4, "relationType": "DATA_FLOW", "jobVersionId": 12, "runIds": []},
        ],
        "jobVersions": [
            {"jobVersionId": 10, "jobName": "ingest", "workloadType": "job"},
            {"jobVersionId": 11, "jobName": "risk-ctas", "workloadType": "dataset"},
            {"jobVersionId": 12, "jobName": "summarise", "workloadType": "dataset"},
        ],
        "runs": serde_json::from_str::<Value>(runs).unwrap(),
        "maskedSummary": {"assets": 1, "jobVersions": 0, "dashboards": 0},
    }})
}

#[test]
fn relations_collapse_into_flows_labelled_by_workload() {
    let parsed = parse(&graph("[]"));
    assert_eq!(parsed.center, 2);
    assert_eq!(parsed.flows.len(), 4, "the two 2→3 relations are one flow");
    let risk = parsed
        .flows
        .iter()
        .find(|f| f.source == 2 && f.target == 3)
        .unwrap();
    assert_eq!(risk.workload, "risk-ctas (dataset)");
    assert_eq!(
        risk.kinds.iter().cloned().collect::<Vec<_>>(),
        ["COLUMN_LINEAGE", "DATA_FLOW"]
    );
    assert_eq!(risk.columns, [("udi".to_string(), "udi".to_string())]);
    assert!(!risk.runtime);
    assert!(parsed.flows.iter().any(|f| f.source == 1 && f.runtime));
    assert_eq!(parsed.hidden, 1);
}

#[test]
fn the_tree_walks_both_ways_and_marks_repeated_nodes() {
    let rows = layout(&parse(&graph("[]")), false);
    let nodes: Vec<(i64, bool)> = rows
        .iter()
        .filter_map(|r| match r {
            Row::Node { id, repeated, .. } => Some((*id, *repeated)),
            _ => None,
        })
        .collect();
    // upstream 1, centre 2, downstream 3 → 4, then 4 again straight from 2.
    assert_eq!(
        nodes,
        [(1, false), (2, false), (3, false), (4, false), (4, true)]
    );
}

fn open(app: &mut crate::tui::app::App) {
    app.push_view(Box::new(LineageGraph::new(2, "mfg.telemetry")));
}

#[test]
fn opens_with_one_hop_each_way_and_draws_labelled_edges() {
    let mut app = app();
    open(&mut app);
    assert_eq!(
        last_fetch(&app).path,
        "/etl/lineage/assets/2?depth=1&upstreamDepth=1&downstreamDepth=1&historyLimit=20&runLimit=20&eventLimit=100"
    );
    answer(&mut app, graph("[]"));
    let screen = draw(&mut app, 140, 24);
    assert!(screen.contains("Upstream · feeds this asset"), "{screen}");
    assert!(screen.contains("◀ raw.readings"), "{screen}");
    assert!(
        screen.contains("via ingest (job) · DATA_FLOW · runtime"),
        "{screen}"
    );
    assert!(
        screen.contains("via risk-ctas (dataset) · COLUMN_LINEAGE + DATA_FLOW 1 cols · code"),
        "{screen}"
    );
    assert!(screen.contains("deleted"), "{screen}");
    assert!(screen.contains("1 hidden (no access)"), "{screen}");
    assert!(screen.contains("↺ shown above"), "{screen}");
}

#[test]
fn hops_change_and_enter_recentres_then_backspace_returns() {
    let mut app = app();
    open(&mut app);
    answer(&mut app, graph("[]"));
    press(&mut app, &[KeyCode::Char('+')]);
    assert!(last_fetch(&app)
        .path
        .contains("depth=2&upstreamDepth=2&downstreamDepth=2"));
    answer(&mut app, graph("[]"));
    press(&mut app, &[KeyCode::Char('d')]);
    assert!(last_fetch(&app)
        .path
        .contains("upstreamDepth=0&downstreamDepth=2"));
    answer(&mut app, graph("[]"));
    // The selection starts on the centre; one down is mfg.high_risk.
    press(&mut app, &[KeyCode::Down, KeyCode::Enter]);
    assert!(last_fetch(&app).path.starts_with("/etl/lineage/assets/3?"));
    answer(&mut app, graph("[]"));
    press(&mut app, &[KeyCode::Backspace]);
    assert!(last_fetch(&app).path.starts_with("/etl/lineage/assets/2?"));
    assert_eq!(app.views.len(), 1, "Backspace walked back, not out");
}

fn graph_fetches(app: &crate::tui::app::App) -> usize {
    app.outbox
        .iter()
        .filter(|r| matches!(r, crate::tui::fetch::Request::Fetch(f) if f.path.starts_with("/etl/lineage/assets/")))
        .count()
}

#[test]
fn refreshes_only_while_a_run_is_active() {
    let mut app = app();
    open(&mut app);
    answer(&mut app, graph(r#"[{"runId": 1, "status": "COMPLETE"}]"#));
    let fetches = graph_fetches(&app);
    app.on_tick(app.now);
    app.on_tick(app.now.advanced(Duration::from_secs(11)));
    assert_eq!(graph_fetches(&app), fetches, "nothing running, no polling");

    press(&mut app, &[KeyCode::Char('r')]);
    answer(&mut app, graph(r#"[{"runId": 1, "status": "RUNNING"}]"#));
    assert!(draw(&mut app, 120, 10).contains("1 run(s) active"));
    let before = graph_fetches(&app);
    let later = app.now.advanced(Duration::from_secs(11));
    app.on_tick(later);
    app.on_tick(later.advanced(Duration::from_secs(11)));
    assert!(
        graph_fetches(&app) > before,
        "a running run refreshes the graph"
    );
}

#[test]
fn too_large_scopes_suggest_fewer_hops() {
    let mut app = app();
    open(&mut app);
    let req = last_fetch(&app);
    app.fetched(
        req.id,
        req.target,
        Err("GET x → HTTP 413 Payload Too Large: LINEAGE_PAGE_TOO_LARGE".into()),
    );
    assert!(draw(&mut app, 100, 10).contains("Press - for fewer hops"));
}

#[test]
fn t_opens_a_lakehouse_nodes_table_through_its_connection() {
    let mut app = app();
    open(&mut app);
    answer(&mut app, graph("[]"));
    press(&mut app, &[KeyCode::Char('t')]);
    assert_eq!(app.views.len(), 2);
    assert_eq!(last_fetch(&app).path, "/connection/1357");
}
