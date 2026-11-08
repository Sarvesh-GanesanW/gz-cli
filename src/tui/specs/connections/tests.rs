//! The Connections screen end to end: list, detail, test action and links.

use crossterm::event::KeyCode;
use reqwest::Method;
use serde_json::{json, Value};

use crate::tui::app::test_support::*;
use crate::tui::app::{App, Overlay, Tone};
use crate::tui::fetch::{FetchTarget, Request};

/// Two hours before the test clock, and a day before.
const RECENT: &str = "2026-11-08T10:00:00.000Z";
const OLDER: &str = "2026-11-07T12:00:00.000Z";

fn rows() -> Value {
    json!([
        {"Id": 1000, "Name": "GZS3", "type": "AWSS3", "modified_by": "sarvesh", "modified_on": OLDER},
        {"Id": 1321, "Name": "LineageTest", "type": "GZLAKEHOUSE", "modified_by": "srijith", "modified_on": RECENT},
    ])
}

fn connections() -> App {
    let mut app = app();
    app.resolved.profile.site = Some("preview".into());
    app.open_and_focus(resource("Connections", "Connections"));
    answer(&mut app, rows());
    app
}

fn select(app: &mut App, name: &str) {
    select_row(app, &["Name"], name);
}

fn fetches_to(app: &App, path: &str) -> usize {
    app.outbox
        .iter()
        .filter(|r| matches!(r, Request::Fetch(f) if f.path == path))
        .count()
}

#[test]
fn list_shows_chosen_columns_with_readable_values_newest_first() {
    let app = connections();
    let grid = app.view().unwrap().grid();
    assert_eq!(
        grid.cols,
        vec!["Name", "Type", "Category", "Updated by", "Updated"]
    );
    assert_eq!(
        grid.rows[0],
        vec![
            "LineageTest",
            "Groundzero Lakehouse",
            "Lakehouse",
            "srijith",
            "2h ago"
        ]
    );
    assert_eq!(
        grid.rows[1],
        vec!["GZS3", "AWS S3", "Object storage", "sarvesh", "1d ago"]
    );
}

#[test]
fn renders_the_list() {
    let mut app = connections();
    let screen = draw(&mut app, 120, 20);
    assert!(screen.contains("Name"), "{screen}");
    assert!(screen.contains("Groundzero Lakehouse"), "{screen}");
    assert!(screen.contains("2h ago"), "{screen}");
    assert!(
        !screen.contains("1321"),
        "internal ids stay hidden\n{screen}"
    );
}

#[test]
fn empty_list_uses_the_screen_wording() {
    let mut app = app();
    app.open_and_focus(resource("Connections", "Connections"));
    answer(&mut app, json!([]));
    assert!(draw(&mut app, 100, 20).contains("No connections to show"));
}

/// Opens GZS3's detail view and answers its detail and related requests.
fn gzs3_detail() -> App {
    let mut app = connections();
    select(&mut app, "GZS3");
    press(&mut app, &[KeyCode::Enter]);
    assert_eq!(app.views.len(), 1);
    let detail = last_fetch(&app);
    assert_eq!(
        (detail.service.as_str(), detail.path.as_str()),
        ("connections", "/connection/1000")
    );
    assert!(matches!(detail.target, FetchTarget::View(_)));
    answer(
        &mut app,
        json!([{"name": "GZS3", "type": "AWSS3", "description": null,
                "config": {"bucketName": "gz-bucket", "region": "us-east-1", "secretAccessKey": "should-not-show"}}]),
    );
    let related = last_fetch(&app);
    assert_eq!(
        related.path,
        "/etl/lineage/assets?connectionId=1000&limit=10"
    );
    answer(
        &mut app,
        json!({"assets": [{"assetId": 7, "displayName": "sales.orders", "assetType": "TABLE",
                           "latestVersion": {"columnCount": 14, "observedOn": RECENT}}], "total": 1}),
    );
    app
}

#[test]
fn detail_shows_sections_masks_secrets_and_lists_related_rows() {
    let mut app = gzs3_detail();
    let screen = draw(&mut app, 110, 40);
    for expected in [
        "GZS3",
        "AWS S3 · Object storage",
        "Summary",
        "Updated by",
        "sarvesh",
        "2026-11-07 12:00 UTC",
        "Configured details",
        "Bucket name",
        "gz-bucket",
        "Secret access key",
        "Lineage assets",
        "sales.orders",
        "2h ago",
    ] {
        assert!(screen.contains(expected), "missing {expected:?}\n{screen}");
    }
    assert!(!screen.contains("should-not-show"), "{screen}");
    assert!(
        !screen.contains("Description"),
        "empty optional fields are left out"
    );
}

#[test]
fn detail_toggles_raw_json_copies_and_closes() {
    let mut app = gzs3_detail();
    press(&mut app, &[KeyCode::Char('J')]);
    assert!(draw(&mut app, 100, 40).contains("\"bucketName\""));
    press(&mut app, &[KeyCode::Char('y')]);
    assert!(app
        .outbox
        .iter()
        .any(|r| matches!(r, Request::Copy(t) if t == "1000")));
    press(&mut app, &[KeyCode::Char('Y')]);
    assert!(app
        .outbox
        .iter()
        .any(|r| matches!(r, Request::Copy(t) if t == "GZS3")));
    press(&mut app, &[KeyCode::Esc]);
    assert!(app.views.is_empty());
}

#[test]
fn missing_connection_says_so() {
    let mut app = connections();
    press(&mut app, &[KeyCode::Enter]);
    answer(&mut app, json!([]));
    assert!(draw(&mut app, 100, 30).contains("Connection not found"));
}

#[test]
fn related_blocks_the_user_cannot_see_are_hidden() {
    let mut app = connections();
    select(&mut app, "GZS3");
    press(&mut app, &[KeyCode::Enter]);
    answer(
        &mut app,
        json!([{"name": "GZS3", "type": "AWSS3", "config": {}}]),
    );
    let related = last_fetch(&app);
    app.fetched(
        related.id,
        related.target,
        Err("GET u → HTTP 403 Forbidden: no".into()),
    );
    assert!(!draw(&mut app, 100, 30).contains("Lineage assets"));
}

#[test]
fn test_connection_is_offered_for_lakehouse_rows_only() {
    let mut app = connections();
    select(&mut app, "GZS3");
    let before = app.outbox.len();
    press(&mut app, &[KeyCode::Char('t')]);
    assert_eq!(
        app.outbox.len(),
        before,
        "S3 connections can't be tested from saved config"
    );

    select(&mut app, "LineageTest");
    press(&mut app, &[KeyCode::Char('t')]);
    let req = last_fetch(&app);
    assert_eq!(req.method, Method::POST);
    assert_eq!(
        (req.service.as_str(), req.path.as_str()),
        ("lakehouse", "/lakehouse/testconnection")
    );
    assert_eq!(
        req.body.unwrap(),
        json!({"connectionName": "LineageTest", "connectionConfig": {"config": {"siteName": "preview"}}})
    );
}

#[test]
fn test_connection_reports_success_and_failure_plainly() {
    let mut app = connections();
    select(&mut app, "LineageTest");
    press(&mut app, &[KeyCode::Char('t')]);
    answer(&mut app, json!({"message": "Test connection successful"}));
    assert_eq!(
        (app.status.tone, app.status.text.as_str()),
        (Tone::Ok, "Test connection successful")
    );

    press(&mut app, &[KeyCode::Char('t')]);
    let req = last_fetch(&app);
    app.fetched(
        req.id,
        req.target,
        Err("POST u → HTTP 404 Not Found: The database specified does not exist".into()),
    );
    assert_eq!(app.status.tone, Tone::Error);
    assert!(
        app.status.text.starts_with("Test connection failed:"),
        "{}",
        app.status.text
    );
    assert!(app
        .status
        .text
        .contains("The database specified does not exist"));
}

#[test]
fn detail_test_uses_the_site_in_the_saved_config() {
    let mut app = connections();
    select(&mut app, "LineageTest");
    press(&mut app, &[KeyCode::Enter]);
    answer(
        &mut app,
        json!([{"name": "LineageTest", "type": "GZLAKEHOUSE", "config": {"siteName": "demosite"}}]),
    );
    press(&mut app, &[KeyCode::Char('t')]);
    let req = last_fetch(&app);
    assert_eq!(
        req.body.unwrap()["connectionConfig"]["config"]["siteName"],
        "demosite"
    );
}

#[test]
fn action_menu_lists_what_the_row_offers() {
    let mut app = connections();
    select(&mut app, "LineageTest");
    press(&mut app, &[KeyCode::Char('a')]);
    let Some(Overlay::Menu(menu)) = &app.overlay else {
        panic!("the action menu should be open");
    };
    let labels: Vec<String> = menu.items.iter().map(|i| i.label()).collect();
    assert_eq!(
        labels,
        vec![
            "Browse tables",
            "Test connection",
            "Delete",
            "Go to lineage",
            "Go to warehouse"
        ]
    );
    press(&mut app, &[KeyCode::Down, KeyCode::Enter]);
    assert!(app.overlay.is_none());
    assert_eq!(last_fetch(&app).path, "/lakehouse/testconnection");
}

#[test]
fn lineage_link_opens_the_connection_there_and_back_returns() {
    let mut app = connections();
    select(&mut app, "GZS3");
    press(&mut app, &[KeyCode::Char('L')]);
    assert_eq!(app.active, resource("Catalog", "Data lineage"));
    answer(&mut app, rows());
    assert_eq!(
        fetches_to(&app, "/etl/lineage/assets?connectionId=1000"),
        1,
        "the link drills into the same connection"
    );
    press(&mut app, &[KeyCode::Backspace, KeyCode::Backspace]);
    assert_eq!(app.active, resource("Connections", "Connections"));
    assert_eq!(
        app.view().unwrap().grid().selected_raw().unwrap()["Name"],
        "GZS3"
    );
}

#[test]
fn help_lists_this_screens_actions_and_links() {
    let mut app = connections();
    select(&mut app, "LineageTest");
    let sections = app.help_sections();
    let screen = sections.iter().find(|s| s.title == "Actions").unwrap();
    assert!(screen
        .entries
        .contains(&("t".into(), "Test connection".into())));
    assert!(screen
        .entries
        .contains(&("L".into(), "go to lineage".into())));
    assert!(
        !screen.entries.iter().any(|(k, _)| k == "f"),
        "files is for object storage only"
    );
}

/// The test action's request, sent for real to a local stub server.
#[tokio::test]
async fn test_connection_request_reaches_the_lakehouse_route() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut raw = Vec::new();
        let mut chunk = [0u8; 4096];
        // Read the head, then the body by Content-Length.
        loop {
            let n = socket.read(&mut chunk).await.unwrap();
            raw.extend_from_slice(&chunk[..n]);
            let text = String::from_utf8_lossy(&raw).to_string();
            if let Some((head, body)) = text.split_once("\r\n\r\n") {
                let length = head
                    .lines()
                    .find_map(|l| {
                        l.to_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().to_string())
                    })
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(0);
                if body.len() >= length {
                    break;
                }
            }
        }
        let reply = r#"{"message":"Test connection successful"}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
            reply.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        String::from_utf8_lossy(&raw).to_string()
    });

    let mut app = connections();
    select(&mut app, "LineageTest");
    press(&mut app, &[KeyCode::Char('t')]);
    let request = Request::Fetch(last_fetch(&app));

    let resolved = crate::config::resolve(
        &crate::config::Config::default(),
        Some("t"),
        Some(base),
        Some("tok".into()),
    );
    let client =
        crate::http::ApiClient::new(resolved, crate::output::OutputFormat::Json, true, 0, 5)
            .unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut executor = crate::tui::fetch::Executor::new(client, tx);
    executor.run(request);

    let seen = server.await.unwrap();
    assert!(
        seen.starts_with("POST /lakehouse/testconnection HTTP/1.1"),
        "{seen}"
    );
    assert!(seen.contains(r#""connectionName":"LineageTest""#), "{seen}");
    let Some(crate::tui::fetch::Inbound::Fetched { id, target, result }) = rx.recv().await else {
        panic!("the executor should report back");
    };
    app.fetched(id, target, result);
    assert_eq!(app.status.text, "Test connection successful");
}

/// Prints the screen, its detail, the action menu and help; run with
/// `--nocapture` to see them.
#[test]
fn snapshot_connections() {
    let mut app = connections();
    select(&mut app, "LineageTest");
    println!("{}\n", draw(&mut app, 110, 14));
    press(&mut app, &[KeyCode::Char('a')]);
    println!("{}\n", draw(&mut app, 110, 14));
    press(&mut app, &[KeyCode::Esc]);
    let mut detail = gzs3_detail();
    println!("{}\n", draw(&mut detail, 110, 34));
    press(&mut detail, &[KeyCode::Char('?')]);
    println!("{}", draw(&mut detail, 110, 34));
}

#[test]
fn browse_tables_reads_the_schema_with_the_lakehouse_site() {
    let mut app = connections();
    select(&mut app, "LineageTest");
    press(&mut app, &[KeyCode::Char('s')]);
    let req = last_fetch(&app);
    assert_eq!(req.method, Method::POST);
    assert_eq!(req.path, "/getSchema?siteName=preview");
    assert_eq!(req.body.unwrap(), json!({"ConnectionName": "LineageTest"}));
    answer(
        &mut app,
        json!({"success": true, "data": [
            {"databaseName": "nyc", "tableName": "trips", "columnName": "vendor_id", "dataType": "int", "totalBytes": 703577},
            {"databaseName": "nyc", "tableName": "trips", "columnName": "fare", "dataType": "double", "totalBytes": 703577},
        ]}),
    );
    press(&mut app, &[KeyCode::Enter]);
    let screen = draw(&mut app, 100, 20);
    assert!(screen.contains("nyc.trips  2 columns"), "{screen}");
    assert!(screen.contains("fare"), "{screen}");

    press(&mut app, &[KeyCode::Esc]);
    select(&mut app, "GZS3");
    press(&mut app, &[KeyCode::Char('s')]);
    assert_eq!(
        last_fetch(&app).path,
        "/getSchema",
        "no site for other types"
    );
}

#[test]
fn delete_says_nothing_checks_what_uses_the_connection() {
    let mut app = connections();
    select(&mut app, "GZS3");
    press(&mut app, &[KeyCode::Char('d')]);
    let Some(Overlay::Confirm { request, .. }) = &app.overlay else {
        panic!("delete asks first");
    };
    assert_eq!(request.path, "/connection/1000");
    let screen = draw(&mut app, 120, 30);
    assert!(
        screen.contains("Nothing checks whether datasets"),
        "{screen}"
    );
    assert!(screen.contains("Type GZS3"), "{screen}");
}
