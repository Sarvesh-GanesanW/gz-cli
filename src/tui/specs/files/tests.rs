//! One browser per bucket: folders first, then files, at any depth, with
//! file actions through the file manager.

use crossterm::event::KeyCode;
use serde_json::{json, Value};

use crate::tui::app::test_support::*;
use crate::tui::app::{App, Overlay};
use crate::tui::fetch::Request;
use crate::tui::test_stub::{self, Reply};

const RECENT: &str = "2026-11-08T10:00:00.000Z";

fn listing_body(app: &App, path: &str) -> Value {
    app.outbox
        .iter()
        .rev()
        .find_map(|r| match r {
            Request::Fetch(f) if f.path == path => f.body.clone(),
            _ => None,
        })
        .unwrap()
}

/// Answers a folder's two listings.
fn answer_folder(app: &mut App, folders: Value, files: Value) {
    answer_path(
        app,
        "/storage/listFiles",
        json!({"success": true, "files": files}),
    );
    answer_path(
        app,
        "/storage/listFolders",
        json!({"success": true, "folders": folders}),
    );
}

fn in_gzs3() -> App {
    let mut app = app();
    app.open_and_focus(resource("Files", "Storage"));
    answer(
        &mut app,
        json!({"success": true, "data": [{"Id": 1000, "Name": "GZS3", "type": "AWSS3", "modified_on": RECENT}]}),
    );
    press(&mut app, &[KeyCode::Enter]);
    app
}

#[test]
fn a_bucket_lists_folders_then_files_from_both_listings() {
    let mut app = in_gzs3();
    for path in ["/storage/listFolders", "/storage/listFiles"] {
        assert_eq!(
            listing_body(&app, path),
            json!({"ConnectionName": "GZS3", "prefix": ""})
        );
    }
    answer_folder(
        &mut app,
        json!(["GzWarehouse", ""]),
        json!([{"fullPath": "a.csv", "name": "a.csv", "lastModified": RECENT, "size": 2048}]),
    );
    let grid = app.view().unwrap().grid();
    assert_eq!(
        grid.rows,
        vec![
            vec!["GzWarehouse/", "—", "—"],
            vec!["/", "—", "—"],
            vec!["a.csv", "2.0 KB", "2h ago"],
        ]
    );
}

#[test]
fn folders_open_at_any_depth_and_files_open_their_details() {
    let mut app = in_gzs3();
    answer_folder(&mut app, json!(["GzWarehouse", ""]), json!([]));
    press(&mut app, &[KeyCode::Enter]);
    assert_eq!(
        listing_body(&app, "/storage/listFiles"),
        json!({"ConnectionName": "GZS3", "prefix": "GzWarehouse/"})
    );
    answer_folder(
        &mut app,
        json!(["2026"]),
        json!([{"fullPath": "GzWarehouse/x.json", "name": "x.json", "lastModified": RECENT, "size": 10}]),
    );
    press(&mut app, &[KeyCode::Enter]);
    assert_eq!(
        listing_body(&app, "/storage/listFolders"),
        json!({"ConnectionName": "GZS3", "prefix": "GzWarehouse/2026/"})
    );
    answer_folder(&mut app, json!([]), json!([]));
    assert!(draw(&mut app, 100, 12).contains("This folder is empty"));
    press(&mut app, &[KeyCode::Backspace]);
    press(&mut app, &[KeyCode::Down, KeyCode::Enter]);
    assert_eq!(app.views.titles(), ["x.json"], "a file opens its details");
}

#[test]
fn the_root_folder_with_an_empty_name_lists_the_slash_prefix() {
    let mut app = in_gzs3();
    answer_folder(&mut app, json!([""]), json!([]));
    press(&mut app, &[KeyCode::Enter]);
    assert_eq!(listing_body(&app, "/storage/listFolders")["prefix"], "/");
}

#[test]
fn a_failed_listing_still_shows_the_other_and_says_so() {
    let mut app = in_gzs3();
    let files = app
        .outbox
        .iter()
        .rev()
        .find_map(|r| match r {
            Request::Fetch(f) if f.path == "/storage/listFiles" => Some(f.clone()),
            _ => None,
        })
        .unwrap();
    app.fetched(
        files.id,
        files.target,
        Err("POST x → HTTP 403 Forbidden".into()),
    );
    answer_path(&mut app, "/storage/listFolders", json!({"folders": ["a"]}));
    assert_eq!(app.view().unwrap().grid().rows.len(), 1);
    assert!(
        app.status.text.contains("this folder's files"),
        "{}",
        app.status.text
    );
}

#[test]
fn deleting_a_folder_warns_it_is_recursive_and_names_the_prefix() {
    let mut app = in_gzs3();
    answer_folder(&mut app, json!(["logs"]), json!([]));
    press(&mut app, &[KeyCode::Char('x')]);
    let screen = draw(&mut app, 140, 30);
    assert!(
        screen.contains("EVERYTHING inside it, recursively"),
        "{screen}"
    );
    assert!(matches!(app.overlay, Some(Overlay::Confirm { .. })));
}

#[test]
fn delete_file_and_new_folder_send_the_data_provider_bodies() {
    let mut app = in_gzs3();
    answer_folder(
        &mut app,
        json!([]),
        json!([{"fullPath": "a.csv", "name": "a.csv", "lastModified": RECENT, "size": 1}]),
    );
    let Some(Overlay::Confirm { request, .. }) = ({
        press(&mut app, &[KeyCode::Char('d')]);
        app.overlay.clone()
    }) else {
        panic!("delete asks first");
    };
    assert_eq!(request.path, "/storage/deleteFiles");
    assert_eq!(
        request.body.unwrap(),
        json!({"ConnectionName": "GZS3", "fileNamesWithPrefix": ["a.csv"]})
    );
    press(&mut app, &[KeyCode::Esc, KeyCode::Char('n')]);
    for c in "reports".chars() {
        press(&mut app, &[KeyCode::Char(c)]);
    }
    press(&mut app, &[KeyCode::Enter]);
    let req = last_fetch(&app);
    assert_eq!(req.path, "/storage/createFolder");
    assert_eq!(
        req.body.unwrap(),
        json!({"ConnectionName": "GZS3", "folderNameWithPrefix": "reports"})
    );
}

#[test]
fn download_asks_where_and_runs_a_task() {
    let mut app = in_gzs3();
    answer_folder(
        &mut app,
        json!([]),
        json!([{"fullPath": "dir/a.csv", "name": "a.csv", "lastModified": RECENT, "size": 1}]),
    );
    press(&mut app, &[KeyCode::Char('w')]);
    let Some(Overlay::Form { form, .. }) = &app.overlay else {
        panic!("download asks for a destination");
    };
    assert_eq!(form.values_json()["dest"], "./a.csv");
    press(&mut app, &[KeyCode::Enter]);
    assert_eq!(last_task(&app).label, "download a file");
    let task = last_task(&app);
    app.fetched(
        task.id,
        task.target,
        Ok(json!({"message": "Saved ./a.csv (1 B)"})),
    );
    assert_eq!(app.status.text, "Saved ./a.csv (1 B)");
}

/// The download goes to the file manager with the key, writes the bytes,
/// and never overwrites a file.
#[tokio::test]
async fn downloads_write_the_file_managers_bytes_without_overwriting() {
    let dir = std::env::temp_dir().join(format!("gz-tui-dl-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let dest = dir.join("a.csv");
    let _ = std::fs::remove_file(&dest);
    let (base, server) = test_stub::serve(vec![Reply {
        status: 200,
        content_type: "application/octet-stream",
        body: b"x,y\n1,2\n".to_vec(),
    }])
    .await;
    let body = json!({"ConnectionName": "GZS3", "fileNameWithPrefix": "dir/a.csv"});
    let done = super::download_to(test_stub::client(&base), body.clone(), &dest)
        .await
        .unwrap();
    assert!(done["message"].as_str().unwrap().starts_with("Saved"));
    assert_eq!(std::fs::read(&dest).unwrap(), b"x,y\n1,2\n");
    let seen = server.await.unwrap();
    assert!(seen[0].starts_with("POST /api/storage/getFile HTTP/1.1"));
    assert!(seen[0].contains(r#""fileNameWithPrefix":"dir/a.csv""#));
    let again = super::download_to(test_stub::client(&base), body, &dest).await;
    assert!(again.unwrap_err().contains("already exists"));
    let _ = std::fs::remove_dir_all(&dir);
}
