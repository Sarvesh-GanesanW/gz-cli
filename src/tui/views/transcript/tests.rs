//! The transcript view: rendering, totals, folded tools, continue command.

use crossterm::event::KeyCode;
use serde_json::json;

use super::*;
use crate::tui::app::test_support::*;
use crate::tui::app::App;
use crate::tui::fetch::Request;

const ID: &str = "7c4c1f90-e080-44d1-8957-ae750d0b9401";

fn opened() -> App {
    let mut app = app();
    app.push_view(Box::new(Transcript::new(
        &json!({"conversationId": ID, "conversationName": "Listing jobs"}),
    )));
    let paths: Vec<String> = app
        .outbox
        .iter()
        .filter_map(|r| match r {
            Request::Fetch(f) => Some(f.path.clone()),
            _ => None,
        })
        .collect();
    assert!(
        paths.contains(&format!("/chat/conversations/{ID}")),
        "{paths:?}"
    );
    assert!(
        paths.contains(&"/chat/capabilities".to_string()),
        "{paths:?}"
    );
    let capabilities = last_fetch(&app);
    app.fetched(
        capabilities.id,
        capabilities.target,
        Ok(json!({"groundzeroModes": [{"value": "grok47", "label": "Grok 4.7"}]})),
    );
    let transcript = app
        .outbox
        .iter()
        .find_map(|r| match r {
            Request::Fetch(f) if f.path.starts_with("/chat/conversations/") => Some(f.clone()),
            _ => None,
        })
        .unwrap();
    app.fetched(
        transcript.id,
        transcript.target,
        Ok(json!({"conversationName": "Listing jobs", "groundzeroMode": "grok47", "reasoningEffort": "xhigh",
            "useRag": true, "messages": [
            {"speaker": "Human", "message": "list my **jobs**", "tokenCount": 0},
            {"speaker": "AI", "groundzeroMode": "grok47", "message": [
                {"type": "text", "text": "You have *two* jobs."},
                {"type": "tool_use", "name": "list_jobs"},
                {"type": "tool_result", "is_error": false}],
             "responseMetrics": {"costUsd": "0.018081600000000000", "totalTokens": 11678,
                                 "timeToFirstTokenMs": 4003, "latencyMs": 4457}}]})),
    );
    app
}

#[test]
fn turns_render_as_markdown_with_a_footer_per_answer() {
    let mut app = opened();
    let screen = draw(&mut app, 100, 24);
    for expected in [
        "Grok 4.7 · xhigh effort · RAG on · 2 turns · 11.7k tokens · $0.02",
        "list my jobs",
        "You have two jobs.",
        "Grok 4.7 · 11.7k tokens · $0.02 · first token 4.0 s · 4.5 s",
        "▸ 2 tool step(s)",
    ] {
        assert!(screen.contains(expected), "missing {expected:?}\n{screen}");
    }
    assert!(
        !screen.contains("**"),
        "markdown is rendered, not shown raw"
    );
    assert!(!screen.contains("list_jobs"), "tool steps start folded");
    press(&mut app, &[KeyCode::Char('t')]);
    assert!(draw(&mut app, 100, 24).contains("⚙ list_jobs"));
}

#[test]
fn c_copies_the_command_that_continues_it_in_gz_code() {
    let mut app = opened();
    press(&mut app, &[KeyCode::Char('c')]);
    let expected = format!("gz code -c {ID}");
    assert!(app
        .outbox
        .iter()
        .any(|r| matches!(r, Request::Copy(t) if *t == expected)));
}

#[test]
fn a_missing_conversation_is_explained() {
    let mut app = app();
    app.push_view(Box::new(Transcript::new(
        &json!({"conversationId": "gone"}),
    )));
    let req = app
        .outbox
        .iter()
        .find_map(|r| match r {
            Request::Fetch(f) if f.path == "/chat/conversations/gone" => Some(f.clone()),
            _ => None,
        })
        .unwrap();
    app.fetched(
        req.id,
        req.target,
        Err("GET u → HTTP 404 Not Found: Conversation not found".into()),
    );
    let screen = draw(&mut app, 120, 10);
    assert!(
        screen.contains("This conversation was deleted or never existed. (Conversation not found)"),
        "{screen}"
    );
}
