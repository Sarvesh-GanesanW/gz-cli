//! Designer lists, details and the create/rename/delete forms.

use crossterm::event::KeyCode;
use reqwest::Method;
use serde_json::json;

use crate::tui::app::test_support::*;
use crate::tui::app::{App, Overlay};

fn open(label: &str, rows: serde_json::Value) -> App {
    let mut app = app();
    app.open_and_focus(resource("Designer", label));
    answer(&mut app, rows);
    app
}

fn type_text(app: &mut App, text: &str) {
    for c in text.chars() {
        press(app, &[KeyCode::Char(c)]);
    }
}

#[test]
fn visualizations_show_chart_labels_newest_first() {
    let app = open(
        "Visualizations",
        json!([
            {"Id": 1006, "Name": "bar2", "chartType": null, "created_by": "a", "modified_by": "a", "modified_on": "2026-01-01T00:00:00Z"},
            {"Id": 1621, "Name": "qty-pur-trend", "chartType": "ComboChart", "created_by": "munvar", "modified_by": "munvar", "modified_on": "2026-11-08T10:00:00Z"},
        ]),
    );
    let grid = app.view().unwrap().grid();
    assert_eq!(
        grid.rows[0][..4],
        ["qty-pur-trend", "Combo", "munvar", "2h ago"]
    );
    assert_eq!(grid.rows[1][1], "—");
}

#[test]
fn filters_read_the_capitalised_list_type() {
    let app = open(
        "Filters",
        json!([{"Id": 1111, "Name": "Productcategory", "displayName": null, "FilterType": "multiSelect",
                "modified_by": "p", "modified_on": "2026-08-11T08:12:20.389Z"}]),
    );
    assert_eq!(app.view().unwrap().grid().rows[0][2], "Multi select list");
}

#[test]
fn new_dataset_posts_only_its_name() {
    let mut app = open(
        "Datasets",
        json!([{"Id": 1921, "Name": "telemetry", "modified_by": "s", "modified_on": "2026-09-25T18:45:24.676Z"}]),
    );
    press(&mut app, &[KeyCode::Char('n')]);
    type_text(&mut app, "sales-2026");
    press(&mut app, &[KeyCode::Enter]);
    let req = last_fetch(&app);
    assert_eq!((req.method, req.path.as_str()), (Method::POST, "/dataset"));
    assert_eq!(req.body.unwrap(), json!({"Name": "sales-2026"}));
}

#[test]
fn rename_validates_and_patches() {
    let mut app = open(
        "Dashboards",
        json!([{"Id": 1114, "Name": "Exec", "modified_by": "p", "modified_on": "2026-10-21T04:54:33.173Z"}]),
    );
    press(&mut app, &[KeyCode::Char('e')]);
    type_text(&mut app, "_bad");
    let before = app.outbox.len();
    press(&mut app, &[KeyCode::Enter]);
    assert_eq!(
        app.outbox.len(),
        before,
        "underscores break the backend rule"
    );
    press(
        &mut app,
        &[
            KeyCode::Backspace,
            KeyCode::Backspace,
            KeyCode::Backspace,
            KeyCode::Backspace,
        ],
    );
    type_text(&mut app, "-2");
    press(&mut app, &[KeyCode::Enter]);
    let req = last_fetch(&app);
    assert_eq!(
        (req.method, req.path.as_str()),
        (Method::PATCH, "/dashboard/1114")
    );
    assert_eq!(req.body.unwrap(), json!({"Name": "Exec-2"}));
}

#[test]
fn delete_warns_that_nothing_checks_usage() {
    let mut app = open(
        "Datasets",
        json!([{"Id": 1921, "Name": "telemetry", "modified_by": "s", "modified_on": "2026-09-25T18:45:24.676Z"}]),
    );
    press(&mut app, &[KeyCode::Char('d')]);
    let Some(Overlay::Confirm { confirm, request }) = &app.overlay else {
        panic!("delete asks first");
    };
    assert!(confirm.message.contains("will break"));
    assert_eq!(request.path, "/dataset/1921");
}

#[test]
fn visualization_detail_links_to_its_dataset() {
    let mut app = open(
        "Visualizations",
        json!([{"Id": 1621, "Name": "qty", "chartType": "LineChart", "modified_on": "2026-11-08T10:00:00Z"}]),
    );
    press(&mut app, &[KeyCode::Enter]);
    assert_eq!(last_fetch(&app).path, "/visualization/1621");
    answer(
        &mut app,
        json!({"Id": 1621, "Name": "qty", "config": {"chartType": "LineChart", "chartConfig": {"datasetName": 1801}}}),
    );
    assert!(draw(&mut app, 100, 30).contains("1801"));
    press(&mut app, &[KeyCode::Char('D')]);
    assert_eq!(app.active, resource("Designer", "Datasets"));
}
