//! Chat screens: conversation history actions and per-user metrics.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use reqwest::Method;
use serde_json::json;

use crate::tui::app::test_support::*;
use crate::tui::app::App;

fn history() -> App {
    let mut app = app();
    app.open_and_focus(resource("Chat", "History"));
    assert_eq!(
        last_fetch(&app).path,
        "/chat/conversations?assistantMode=chat"
    );
    answer(
        &mut app,
        json!({"conversations": [
            {"conversationId": "c1", "conversationName": "Listing jobs", "groundzeroMode": "grok47"},
            {"conversationId": "c2", "conversationName": null, "groundzeroMode": "luna6"}],
            "search": null}),
    );
    app
}

#[test]
fn history_lists_titles_in_server_order() {
    let app = history();
    let grid = app.view().unwrap().grid();
    assert_eq!(grid.rows[0], vec!["Listing jobs", "grok47"]);
    assert_eq!(grid.visible_len(), 2);
}

#[test]
fn t_opens_the_transcript() {
    let mut app = history();
    press(&mut app, &[KeyCode::Char('t')]);
    assert_eq!(app.views.len(), 1);
    assert!(app.breadcrumb().last().unwrap().contains("Listing jobs"));
}

#[test]
fn rename_posts_the_new_name() {
    let mut app = history();
    press(&mut app, &[KeyCode::Char('n')]);
    for _ in 0.."Listing jobs".len() {
        press(&mut app, &[KeyCode::Backspace]);
    }
    for c in "Jobs".chars() {
        press(&mut app, &[KeyCode::Char(c)]);
    }
    press(&mut app, &[KeyCode::Enter]);
    let req = last_fetch(&app);
    assert_eq!(
        (req.method, req.path.as_str()),
        (Method::POST, "/chat/conversations")
    );
    assert_eq!(
        req.body.unwrap(),
        json!({"conversationId": "c1", "conversationName": "Jobs"})
    );
}

#[test]
fn delete_asks_for_the_title_first() {
    let mut app = history();
    press(&mut app, &[KeyCode::Char('D')]);
    for c in "Listing jobs".chars() {
        app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    press(&mut app, &[KeyCode::Enter]);
    let req = last_fetch(&app);
    assert_eq!(
        (req.method, req.path.as_str()),
        (Method::DELETE, "/chat/conversations/c1")
    );
}

#[test]
fn metrics_list_users_by_cost() {
    let mut app = app();
    app.open_and_focus(resource("Chat", "Metrics"));
    answer(
        &mut app,
        json!({"scope": "tenant", "userMetrics": [
            {"username": "robin", "requestCount": 10, "invocationCount": 12, "totalTokens": 4400000,
             "medianTimeToFirstTokenMs": 4715, "totalCostUsd": "1.5"},
            {"username": "lakehouse-svc-accnt", "requestCount": 86552, "invocationCount": 86600,
             "totalTokens": 300000000, "medianTimeToFirstTokenMs": null, "totalCostUsd": "61.28"}]}),
    );
    let grid = app.view().unwrap().grid();
    assert_eq!(
        grid.rows[0],
        vec![
            "lakehouse-svc-accnt",
            "86,552",
            "86,600",
            "300M",
            "—",
            "$61.28"
        ]
    );
    assert_eq!(grid.rows[1][4], "4.7 s");
}
