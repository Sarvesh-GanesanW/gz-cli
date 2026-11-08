//! Workspaces, their contents and actions.

use crossterm::event::KeyCode;
use reqwest::Method;
use serde_json::json;

use crate::tui::app::test_support::*;
use crate::tui::app::{App, Overlay};

fn contents() -> App {
    let mut app = app();
    app.open_and_focus(resource("Workspaces", "Workspaces"));
    answer(
        &mut app,
        json!([{"Id": 1009, "Name": "ORDERS", "owner": "swathi", "modifiedBy": null, "modifiedOn": "2026-05-15T06:17:20.669Z"}]),
    );
    press(&mut app, &[KeyCode::Enter]);
    assert_eq!(last_fetch(&app).path, "/workspace/1009/contents");
    answer(
        &mut app,
        json!([
            {"id": 1135, "Name": "Inventory", "isDashboard": true, "owner": "prajjwal", "modifiedBy": "prasath", "modifiedOn": "2026-11-08T10:00:00Z"},
            {"id": 1140, "Name": "Archive", "isDashboard": false, "owner": "prajjwal", "modifiedBy": "prasath", "modifiedOn": "2026-11-07T10:00:00Z"},
        ]),
    );
    app
}

#[test]
fn contents_tell_dashboards_from_folders() {
    let app = contents();
    let grid = app.view().unwrap().grid();
    assert_eq!(grid.rows[0][..2], ["▦ dashboard", "Inventory"]);
    assert_eq!(grid.rows[1][..2], ["▸ folder", "Archive"]);
}

#[test]
fn a_dashboard_copy_opens_its_snapshot_in_the_workspace() {
    let mut app = contents();
    press(&mut app, &[KeyCode::Enter]);
    assert_eq!(last_fetch(&app).path, "/workspace/dashboard/1009/1135");
    answer(
        &mut app,
        json!({"Id": 1114, "Name": "Inventory", "tabs": [{"tabId": "1"}]}),
    );
    let screen = draw(&mut app, 100, 30);
    assert!(screen.contains("Copied from dashboard  1114"), "{screen}");
}

#[test]
fn deleting_a_child_names_its_parent_in_the_path() {
    let mut app = contents();
    press(&mut app, &[KeyCode::Char('d')]);
    let Some(Overlay::Confirm { request, .. }) = &app.overlay else {
        panic!("delete asks first");
    };
    assert_eq!(
        (request.method.clone(), request.path.as_str()),
        (Method::DELETE, "/workspace/1009/1135")
    );
}

#[test]
fn renaming_a_child_patches_its_own_row() {
    let mut app = contents();
    press(&mut app, &[KeyCode::Char('e')]);
    press(&mut app, &[KeyCode::Char('2')]);
    press(&mut app, &[KeyCode::Enter]);
    let req = last_fetch(&app);
    assert_eq!(
        (req.method, req.path.as_str()),
        (Method::PATCH, "/workspace/1135")
    );
    assert_eq!(req.body.unwrap(), json!({"Name": "Inventory2"}));
}
