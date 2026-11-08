//! The playground end to end: start, stream, finish, stop, approvals.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use reqwest::Method;
use serde_json::{json, Value};

use super::*;
use crate::sse::Event;
use crate::tui::app::test_support::*;
use crate::tui::app::App;
use crate::tui::fetch::{Request, StreamReq};

const AGENT: &str = "f20a8945-6aa5-48b7-88e0-165a693f4a29";

fn row(invokable: bool) -> Value {
    json!({"id": AGENT, "name": "InvoiceExtractor", "version": 11, "isInvokable": invokable})
}

fn typed(app: &mut App, text: &str) {
    for c in text.chars() {
        press(app, &[KeyCode::Char(c)]);
    }
}

fn last_stream(app: &App) -> StreamReq {
    app.outbox
        .iter()
        .rev()
        .find_map(|r| match r {
            Request::Stream(s) => Some(s.clone()),
            _ => None,
        })
        .expect("a stream request")
}

fn event(app: &mut App, data: Value) {
    let stream = last_stream(app);
    let item = StreamItem::Event(Event {
        id: Some("1".into()),
        event: None,
        data: data.to_string(),
    });
    app.view_streamed(stream.view, stream.id, item);
}

/// Opens the playground, sends "hi" and answers the start request.
fn running(invokable: bool) -> App {
    let mut app = app();
    app.push_view(Box::new(Playground::new(&row(invokable))));
    typed(&mut app, "hi");
    press(&mut app, &[KeyCode::Enter]);
    let start = last_fetch(&app);
    assert_eq!(start.method, Method::POST);
    assert_eq!(start.path, format!("/v1/agents/{AGENT}/preview"));
    assert_eq!(
        start.body.unwrap(),
        json!({"message": "hi", "background": true})
    );
    answer(&mut app, json!({"runId": "run-1", "sessionId": "s-1"}));
    app
}

#[test]
fn a_prompt_starts_a_draft_run_and_follows_its_events() {
    let app = running(false);
    let stream = last_stream(&app);
    assert_eq!(stream.service, "agents");
    assert_eq!(stream.path, format!("/v1/agents/{AGENT}/runs/run-1/events"));
}

#[test]
fn streamed_text_and_the_final_cost_show_in_the_transcript() {
    let mut app = running(false);
    event(&mut app, json!({"type": "llm_chunk", "text": "Invoice 7 "}));
    event(
        &mut app,
        json!({"type": "llm_chunk", "text": "totals **$120**."}),
    );
    assert!(draw(&mut app, 90, 20).contains("running · esc stops"));
    event(
        &mut app,
        json!({"type": "completed", "runId": "run-1", "result": {
            "response": "Invoice 7 totals $120.", "durationMs": 4500, "cycleCount": 3,
            "usage": {"totalTokens": 12400}, "estimatedModelCost": {"totalUsd": "0.0123"}}}),
    );
    let screen = draw(&mut app, 90, 20);
    assert!(screen.contains("Invoice 7 totals $120."), "{screen}");
    assert!(
        screen.contains("v11 draft · 4.5 s · 3 steps · 12.4k tokens · $0.01"),
        "{screen}"
    );
    // The tools the run called come from its observability record.
    let tools = last_fetch(&app);
    assert_eq!(
        tools.path,
        format!("/v1/agents/{AGENT}/runs/run-1/observability")
    );
    answer(
        &mut app,
        json!({"tools": [{"name": "pdf_parser", "callCount": 2, "errorCount": 1, "totalDurationMs": 900}]}),
    );
    let screen = draw(&mut app, 90, 20);
    assert!(screen.contains("▸ 2 tool call(s)"), "{screen}");
    assert!(!screen.contains("pdf_parser"), "tool calls start folded");
    app.on_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
    let screen = draw(&mut app, 90, 20);
    assert!(
        screen.contains("pdf_parser ×2, 1 failed · 900 ms"),
        "{screen}"
    );
}

#[test]
fn escape_stops_a_running_run_and_then_goes_back() {
    let mut app = running(false);
    let stream = last_stream(&app);
    press(&mut app, &[KeyCode::Esc]);
    assert_eq!(app.views.len(), 1, "the first esc stops the run");
    assert!(app
        .outbox
        .iter()
        .any(|r| matches!(r, Request::StopStream { id } if *id == stream.id)));
    let cancel = last_fetch(&app);
    assert_eq!(cancel.path, format!("/v1/agents/{AGENT}/runs/run-1/cancel"));
    assert!(draw(&mut app, 90, 20).contains("Agent run stopped."));
    press(&mut app, &[KeyCode::Esc]);
    assert!(app.views.is_empty());
}

#[test]
fn missing_permission_explains_who_may_test_drafts() {
    let mut app = app();
    app.push_view(Box::new(Playground::new(&row(false))));
    typed(&mut app, "hi");
    press(&mut app, &[KeyCode::Enter]);
    let start = last_fetch(&app);
    app.fetched(
        start.id,
        start.target,
        Err(r#"POST u → HTTP 401 Unauthorized: You do not have "All" or "Modify" permission on agent."#.into()),
    );
    let screen = draw(&mut app, 200, 20);
    assert!(
        screen.contains("You don't have access to this agent."),
        "{screen}"
    );
    assert!(screen.contains("creator or an admin"), "{screen}");
    assert!(!screen.contains("gz auth login"), "{screen}");
}

#[test]
fn the_approved_version_keeps_its_session_between_prompts() {
    let mut app = app();
    app.push_view(Box::new(Playground::new(&row(true))));
    press(&mut app, &[KeyCode::Tab]);
    typed(&mut app, "one");
    press(&mut app, &[KeyCode::Enter]);
    assert_eq!(last_fetch(&app).path, format!("/v1/agents/{AGENT}/invoke"));
    answer(&mut app, json!({"runId": "run-1", "sessionId": "s-1"}));
    event(
        &mut app,
        json!({"type": "completed", "result": {"response": "1"}}),
    );
    answer(&mut app, json!({"tools": []}));
    typed(&mut app, "two");
    press(&mut app, &[KeyCode::Enter]);
    let second = last_fetch(&app);
    assert_eq!(
        second.body.unwrap(),
        json!({"message": "two", "background": true, "sessionId": "s-1"})
    );
}

#[test]
fn drafts_without_an_approved_version_cannot_switch_mode() {
    let mut app = app();
    app.push_view(Box::new(Playground::new(&row(false))));
    press(&mut app, &[KeyCode::Tab]);
    typed(&mut app, "x");
    press(&mut app, &[KeyCode::Enter]);
    assert!(last_fetch(&app).path.ends_with("/preview"));
}

#[test]
fn paused_runs_ask_and_resume_with_the_decision() {
    let mut app = running(false);
    event(
        &mut app,
        json!({"type": "interrupted", "result": {"interrupts": [
            {"id": "int-1", "name": "tool_approval", "toolName": "create_ontology_model", "reason": "writes data"}]}}),
    );
    let screen = draw(&mut app, 90, 20);
    assert!(
        screen.contains("Paused: a tool call needs your decision."),
        "{screen}"
    );
    assert!(
        screen.contains("create_ontology_model · writes data"),
        "{screen}"
    );
    press(&mut app, &[KeyCode::Char('y')]);
    let resume = last_fetch(&app);
    assert_eq!(resume.path, format!("/v1/agents/{AGENT}/runs/run-1/resume"));
    assert_eq!(
        resume.body.unwrap(),
        json!({"interruptResponses": [{"interruptId": "int-1", "decision": "approved", "remember": "once"}]})
    );
}

#[test]
fn events_from_an_older_stream_are_ignored() {
    let mut app = running(false);
    let stream = last_stream(&app);
    let stale = StreamItem::Event(Event {
        id: None,
        event: None,
        data: json!({"type": "llm_chunk", "text": "ghost"}).to_string(),
    });
    app.view_streamed(stream.view, stream.id + 100, stale);
    assert!(!draw(&mut app, 90, 20).contains("ghost"));
}

/// The stream request, followed for real against a local SSE stub: every
/// event reaches the view, then the end.
#[tokio::test]
async fn stream_requests_deliver_server_events_to_the_view() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut head = vec![0u8; 4096];
        let n = socket.read(&mut head).await.unwrap();
        let body = "id: 1\nevent: llm_chunk\ndata: {\"type\":\"llm_chunk\",\"text\":\"hi\"}\n\n\
                    id: 2\nevent: completed\ndata: {\"type\":\"completed\",\"result\":{}}\n\n\
                    event: done\ndata: [DONE]\n\n";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        String::from_utf8_lossy(&head[..n]).to_string()
    });

    let app = running(false);
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
    executor.run(Request::Stream(last_stream(&app)));

    let seen = server.await.unwrap();
    assert!(
        seen.starts_with(&format!(
            "GET /v1/agents/{AGENT}/runs/run-1/events HTTP/1.1"
        )),
        "{seen}"
    );
    let mut kinds = Vec::new();
    while let Some(crate::tui::fetch::Inbound::Streamed { item, .. }) = rx.recv().await {
        let end = !matches!(item, StreamItem::Event(_));
        kinds.push(match item {
            StreamItem::Event(e) => e.event.unwrap_or_default(),
            StreamItem::Ended => "ended".into(),
            StreamItem::Failed(err) => format!("failed: {err}"),
        });
        if end {
            break;
        }
    }
    assert_eq!(kinds, vec!["llm_chunk", "completed", "ended"]);
}
