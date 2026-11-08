use crossterm::event::KeyCode;
use reqwest::Method;
use serde_json::{json, Value};

use super::*;
use crate::tui::app::test_support::*;
use crate::tui::nav::{ResourceId, Source, Verb};
use crate::tui::overlay::confirm::Mode;
use crate::tui::screen::{Choices, FormField, FormSpec};
use crate::tui::views::Ctx;

const DELETE: ActionSpec = ActionSpec::new(
    'd',
    "Delete",
    ActionKind::Request(RequestSpec {
        source: Source::new("connections", Verb::Delete, "/connection/{Id}", None),
        success: "Connection deleted",
        after: After::RefreshAndClose,
    }),
)
.destructive("Delete connection {Name}? Datasets and jobs using it will break.");

static RENAME_FORM: FormSpec = FormSpec {
    title: "Rename",
    submit: "Rename",
    fields: &[
        FormField::new("name", "New name", FieldKind::Text)
            .required()
            .default("{Name}"),
        FormField::new(
            "owner",
            "Owner",
            FieldKind::Select(Choices::Endpoint {
                source: Source::get("admin", "/users?site={$site}"),
                value: &["id"],
                label: &["name"],
            }),
        ),
    ],
};

const RENAME: ActionSpec = ActionSpec::new(
    'n',
    "Rename",
    ActionKind::Form(
        &RENAME_FORM,
        RequestSpec {
            source: Source::new(
                "connections",
                Verb::Patch,
                "/connection/{Id}",
                Some(r#"{"Name":"{name}","owner":"{owner}"}"#),
            ),
            success: "Renamed",
            after: After::Refresh,
        },
    ),
);

fn shout(row: &Value, ctx: &mut Ctx) {
    ctx.toast(Tone::Ok, format!("custom ran for {}", row["Name"]));
}

const CUSTOM: ActionSpec = ActionSpec::new('x', "Custom", ActionKind::Custom(shout));

const NEEDS_ASSET: ActionSpec = ActionSpec::new(
    'g',
    "Graph",
    ActionKind::Request(RequestSpec {
        source: Source::get("etl", "/etl/lineage/assets/{assetId}"),
        success: "",
        after: After::Stay,
    }),
);

fn connections_id() -> ResourceId {
    resource("Connections", "Connections")
}

fn row() -> RowContext {
    RowContext {
        row: json!({"Id": 1000, "Name": "GZS3"}),
        title: Some("GZS3".into()),
        noun: "connection",
        spec: None,
        parents: Vec::new(),
        ids: vec!["1000".into()],
        resource: Some(connections_id()),
        view: None,
    }
}

fn type_text(app: &mut App, text: &str) {
    for c in text.chars() {
        press(app, &[KeyCode::Char(c)]);
    }
}

fn action_requests(app: &App) -> Vec<FetchReq> {
    app.outbox
        .iter()
        .filter_map(|r| match r {
            Request::Fetch(f) if matches!(f.target, FetchTarget::Action(_)) => Some(f.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn destructive_actions_spell_out_the_request_and_need_the_name() {
    let mut app = app();
    app.start_action(&DELETE, row());
    let Some(Overlay::Confirm { confirm, .. }) = &app.overlay else {
        panic!("a destructive action must ask first");
    };
    assert_eq!(
        confirm.message,
        "Delete connection GZS3? Datasets and jobs using it will break."
    );
    assert_eq!(confirm.request, "DELETE connections /connection/1000");
    assert_eq!(confirm.mode, Mode::TypeName("GZS3".into()));
    press(&mut app, &[KeyCode::Char('y'), KeyCode::Enter]);
    assert!(action_requests(&app).is_empty(), "y is not the name");
    press(&mut app, &[KeyCode::Backspace]);
    type_text(&mut app, "GZS3");
    press(&mut app, &[KeyCode::Enter]);
    let sent = action_requests(&app);
    assert_eq!(sent.len(), 1);
    assert_eq!(
        (sent[0].method.clone(), sent[0].path.as_str()),
        (Method::DELETE, "/connection/1000")
    );
}

#[test]
fn successful_actions_toast_and_refresh_the_screen() {
    let mut app = app();
    app.open_and_focus(connections_id());
    answer(&mut app, json!([{"Id": 1000, "Name": "GZS3"}]));
    app.start_action(&DELETE, row());
    type_text(&mut app, "GZS3");
    press(&mut app, &[KeyCode::Enter]);
    answer(&mut app, json!({}));
    assert_eq!(
        (app.status.tone, app.status.text.as_str()),
        (Tone::Ok, "Connection deleted")
    );
    let reload = last_fetch(&app);
    assert_eq!(reload.target, FetchTarget::Grid(connections_id()));
}

#[test]
fn cancelled_confirmations_send_nothing() {
    let mut app = app();
    app.start_action(&DELETE, row());
    press(&mut app, &[KeyCode::Esc]);
    assert!(app.overlay.is_none());
    assert!(action_requests(&app).is_empty());
    assert_eq!(app.status.text, "Cancelled");
}

#[test]
fn form_values_fill_the_request_template() {
    let mut app = app();
    app.resolved.profile.site = Some("preview".into());
    app.start_action(&RENAME, row());
    let choices = last_fetch(&app);
    assert_eq!(choices.target, FetchTarget::FormChoices);
    assert_eq!(choices.path, "/users?site=preview");
    answer(&mut app, json!([{"id": "u1", "name": "Ana"}]));
    let Some(Overlay::Form { form, .. }) = &app.overlay else {
        panic!("the form should be open");
    };
    assert_eq!(form.display(0), "GZS3", "defaults come from the row");
    press(&mut app, &[KeyCode::Backspace]);
    type_text(&mut app, "4");
    press(&mut app, &[KeyCode::Enter]);
    let sent = action_requests(&app);
    assert_eq!(sent.len(), 1, "no confirm configured, so it runs on submit");
    assert_eq!(sent[0].method, Method::PATCH);
    assert_eq!(
        sent[0].body.clone().unwrap(),
        json!({"Name": "GZS4", "owner": "u1"})
    );
}

#[test]
fn invalid_forms_stay_open() {
    let mut app = app();
    app.start_action(&RENAME, row());
    for _ in 0.."GZS3".len() {
        press(&mut app, &[KeyCode::Backspace]);
    }
    press(&mut app, &[KeyCode::Enter]);
    assert!(matches!(app.overlay, Some(Overlay::Form { .. })));
    assert!(action_requests(&app).is_empty());
}

#[test]
fn custom_actions_run_their_code() {
    let mut app = app();
    app.start_action(&CUSTOM, row());
    assert_eq!(app.status.text, "custom ran for \"GZS3\"");
}

#[test]
fn unfillable_templates_explain_what_is_missing() {
    let mut app = app();
    app.start_action(&NEEDS_ASSET, row());
    assert_eq!(app.status.tone, Tone::Error);
    assert_eq!(app.status.text, "Can't graph: this row has no `assetId`");
    assert!(action_requests(&app).is_empty());
}

#[test]
fn actions_from_a_detail_view_report_back_to_it() {
    let mut app = app();
    app.resolved.profile.site = Some("preview".into());
    app.open_and_focus(connections_id());
    answer(
        &mut app,
        json!([{"Id": 1321, "Name": "LineageTest", "type": "GZLAKEHOUSE"}]),
    );
    press(&mut app, &[KeyCode::Enter]);
    let (view, _) = app.views.top().unwrap();
    answer(
        &mut app,
        json!([{"name": "LineageTest", "type": "GZLAKEHOUSE", "config": {}}]),
    );
    press(&mut app, &[KeyCode::Char('t')]);
    let FetchTarget::Action(done) = last_fetch(&app).target else {
        panic!("t should send the test request");
    };
    assert_eq!(
        done.view,
        Some(view),
        "so Refresh/RefreshAndClose reach the view"
    );
    assert_eq!(done.resource, Some(connections_id()));
}

#[test]
fn failed_actions_say_why_in_plain_words() {
    let mut app = app();
    app.start_action(&DELETE, row());
    type_text(&mut app, "GZS3");
    press(&mut app, &[KeyCode::Enter]);
    let req = last_fetch(&app);
    app.fetched(
        req.id,
        req.target,
        Err("DELETE u → HTTP 403 Forbidden: no Delete permission".into()),
    );
    assert_eq!(
        app.status.text,
        "Delete failed: You don't have access to this connection. (no Delete permission)"
    );
}
