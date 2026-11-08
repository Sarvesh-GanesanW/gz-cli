//! The Agents screens end to end: agents, runs, approvals, tools, models.

use crossterm::event::KeyCode;
use reqwest::Method;
use serde_json::{json, Value};

use crate::tui::app::test_support::*;
use crate::tui::app::{App, Overlay};
use crate::tui::fetch::{FetchReq, Request};

const AGENT: &str = "f20a8945-6aa5-48b7-88e0-165a693f4a29";
const RUN: &str = "0b1c6c9e-1111-4a5b-9c1d-2e3f4a5b6c7d";
/// Two hours before the test clock.
const RECENT: &str = "2026-11-08T10:00:00.000000Z";

fn agents() -> Value {
    json!({"items": [
        {"id": AGENT, "name": "InvoiceExtractor", "approvalStatus": "approved", "approvedVersion": 10,
         "version": 11, "modelId": "in.openai.gpt-5.6-luna", "createdBy": "robin", "updatedAt": RECENT,
         "governanceState": "active", "tools": ["pdf_parser"], "instructions": "Extract invoices."},
        {"id": "a2", "name": "Drafty", "approvalStatus": "draft", "approvedVersion": null,
         "version": 1, "modelId": "global.anthropic.claude-sonnet-5-5", "createdBy": "sagatest",
         "updatedAt": "2026-11-07T12:00:00Z"},
    ], "limit": 100, "offset": 0})
}

fn agents_screen() -> App {
    let mut app = app();
    app.open_and_focus(resource("Agents", "Agents"));
    assert_eq!(last_fetch(&app).path, "/v1/agents?limit=100&offset=0");
    answer(&mut app, agents());
    app
}

fn fetches(app: &App) -> Vec<FetchReq> {
    app.outbox
        .iter()
        .filter_map(|r| match r {
            Request::Fetch(f) => Some(f.clone()),
            _ => None,
        })
        .collect()
}

fn typed(app: &mut App, text: &str) {
    for c in text.chars() {
        press(app, &[KeyCode::Char(c)]);
    }
}

#[test]
fn agent_list_names_models_and_states_like_the_web() {
    let app = agents_screen();
    let grid = app.view().unwrap().grid();
    assert_eq!(
        grid.cols,
        vec!["Name", "Status", "Live", "Model", "Owner", "Updated"]
    );
    assert_eq!(
        grid.rows[0],
        vec![
            "InvoiceExtractor",
            "Approved",
            "v10",
            "GPT-5.6 Luna",
            "robin",
            "2h ago"
        ]
    );
    assert_eq!(grid.rows[1][1], "Draft");
    assert_eq!(grid.rows[1][2], "—", "never approved means not live");
    assert_eq!(grid.rows[1][3], "Claude Sonnet 5.5");
}

/// Drills into InvoiceExtractor's runs and answers with one finished run.
fn runs_screen() -> App {
    let mut app = agents_screen();
    select_row(&mut app, &["name"], "InvoiceExtractor");
    press(&mut app, &[KeyCode::Enter]);
    assert_eq!(
        last_fetch(&app).path,
        format!("/v1/agents/{AGENT}/runs?limit=100&offset=0")
    );
    answer(
        &mut app,
        json!({"items": [{"id": RUN, "agentId": AGENT, "agentVersion": 10, "status": "completed",
            "executionMode": "published", "startedAt": RECENT, "durationMs": 5823,
            "totalTokens": 12400, "totalCostUsd": "0.012332733648", "createdBy": "robin",
            "modelId": "global.openai.gpt-5.6-sol", "inputText": "Read invoice 7",
            "outputText": "Invoice 7 totals $120."}], "limit": 100, "offset": 0}),
    );
    app
}

#[test]
fn runs_show_mode_version_duration_tokens_and_cost() {
    let app = runs_screen();
    let grid = app.view().unwrap().grid();
    assert_eq!(
        grid.rows[0],
        vec![
            "Completed",
            "2h ago",
            "invoke",
            "v10",
            "5.8 s",
            "12.4k",
            "$0.01",
            "robin"
        ]
    );
}

#[test]
fn run_detail_uses_the_row_and_durable_records_never_the_transient_store() {
    let mut app = runs_screen();
    press(&mut app, &[KeyCode::Enter]);
    assert_eq!(app.views.len(), 1);
    let paths: Vec<String> = fetches(&app).iter().map(|f| f.path.clone()).collect();
    let base = format!("/v1/agents/{AGENT}/runs/{RUN}");
    for suffix in [
        "observability",
        "governance",
        "evaluations",
        "feedback",
        "metrics",
    ] {
        assert!(paths.contains(&format!("{base}/{suffix}")), "{paths:?}");
    }
    assert!(
        !paths.contains(&base),
        "the 24 h run store answers 404 for old runs"
    );
    let screen = draw(&mut app, 110, 60);
    for expected in [
        "Completed",
        "Read invoice 7",
        "Invoice 7 totals $120.",
        "Prompt",
    ] {
        assert!(screen.contains(expected), "missing {expected:?}\n{screen}");
    }
}

#[test]
fn stop_is_offered_for_running_runs_only_and_asks_first() {
    let mut app = runs_screen();
    let before = fetches(&app).len();
    press(&mut app, &[KeyCode::Char('x')]);
    assert!(app.overlay.is_none(), "a completed run can't be stopped");
    assert_eq!(fetches(&app).len(), before);

    let mut app = agents_screen();
    select_row(&mut app, &["name"], "InvoiceExtractor");
    press(&mut app, &[KeyCode::Enter]);
    answer(
        &mut app,
        json!({"items": [{"id": RUN, "agentId": AGENT, "status": "running"}]}),
    );
    press(&mut app, &[KeyCode::Char('x')]);
    assert!(matches!(app.overlay, Some(Overlay::Confirm { .. })));
    press(&mut app, &[KeyCode::Enter]);
    let req = last_fetch(&app);
    assert_eq!(req.method, Method::POST);
    assert_eq!(req.path, format!("/v1/agents/{AGENT}/runs/{RUN}/cancel"));
}

#[test]
fn rating_a_run_sends_a_whole_number_score() {
    let mut app = runs_screen();
    press(&mut app, &[KeyCode::Char('f')]);
    typed(&mut app, "4");
    press(&mut app, &[KeyCode::Tab]);
    typed(&mut app, "good");
    submit_form(&mut app);
    let req = last_fetch(&app);
    assert_eq!(req.method, Method::PUT);
    assert_eq!(req.path, format!("/v1/agents/{AGENT}/runs/{RUN}/feedback"));
    assert_eq!(req.body.unwrap(), json!({"score": 4, "comment": "good"}));
}

#[test]
fn empty_runs_say_only_your_own_runs_may_be_listed() {
    let mut app = agents_screen();
    press(&mut app, &[KeyCode::Enter]);
    answer(&mut app, json!({"items": [], "limit": 100, "offset": 0}));
    let screen = draw(&mut app, 140, 20);
    assert!(screen.contains("only your own runs"), "{screen}");
}

#[test]
fn submit_for_approval_is_offered_to_drafts_and_sends_the_note() {
    let mut app = agents_screen();
    select_row(&mut app, &["name"], "InvoiceExtractor");
    press(&mut app, &[KeyCode::Char('s')]);
    assert!(
        app.overlay.is_none(),
        "approved versions aren't resubmitted"
    );

    select_row(&mut app, &["name"], "Drafty");
    press(&mut app, &[KeyCode::Char('s')]);
    assert!(matches!(app.overlay, Some(Overlay::Form { .. })));
    press(&mut app, &[KeyCode::Enter]);
    let req = last_fetch(&app);
    assert_eq!(
        (req.method, req.path.as_str()),
        (Method::POST, "/v1/agents/a2/approval-requests")
    );
    assert_eq!(req.body.unwrap(), json!({"comment": "Submitted from gz"}));
}

#[test]
fn deleting_an_agent_needs_its_name_typed() {
    let mut app = agents_screen();
    select_row(&mut app, &["name"], "Drafty");
    let before = fetches(&app).len();
    press(&mut app, &[KeyCode::Char('D'), KeyCode::Enter]);
    assert_eq!(fetches(&app).len(), before, "Enter alone doesn't delete");
    typed(&mut app, "Drafty");
    press(&mut app, &[KeyCode::Enter]);
    let req = last_fetch(&app);
    assert_eq!(
        (req.method, req.path.as_str()),
        (Method::DELETE, "/v1/agents/a2")
    );
}

#[test]
fn agent_detail_lists_configuration_runs_and_approval_history() {
    let mut app = agents_screen();
    select_row(&mut app, &["name"], "InvoiceExtractor");
    press(&mut app, &[KeyCode::Char('i')]);
    assert_eq!(last_fetch(&app).path, format!("/v1/agents/{AGENT}"));
    answer(
        &mut app,
        json!({"id": AGENT, "name": "InvoiceExtractor", "approvalStatus": "approved",
               "tags": {"project-name": "Invoice-processing"}}),
    );
    let paths: Vec<String> = fetches(&app).iter().map(|f| f.path.clone()).collect();
    assert!(paths.contains(&format!("/v1/agents/{AGENT}/runs?limit=10&offset=0")));
    assert!(paths.contains(&format!("/v1/agents/{AGENT}/approvals?limit=10&offset=0")));
    let screen = draw(&mut app, 120, 70);
    for expected in [
        "InvoiceExtractor  ● Approved",
        "Invoice-processing",
        "Instructions",
        "Extract invoices.",
        "Recent runs",
    ] {
        assert!(screen.contains(expected), "missing {expected:?}\n{screen}");
    }
}

fn approvals_screen() -> App {
    let mut app = app();
    app.open_and_focus(resource("Agents", "Approval requests"));
    assert_eq!(last_fetch(&app).path, "/v1/approval-requests");
    answer(
        &mut app,
        json!({"items": [{"id": 201, "agentId": AGENT, "agentVersion": 11, "status": "waiting_for_approval",
            "requestedBy": "robin", "requestedAt": RECENT, "requestComment": "New tools",
            "definitionSnapshot": {"name": "InvoiceExtractor", "modelId": "in.openai.gpt-5.6-luna"}}],
            "limit": 50, "offset": 0}),
    );
    app
}

#[test]
fn approval_queue_shows_agent_version_and_note() {
    let app = approvals_screen();
    let grid = app.view().unwrap().grid();
    assert_eq!(
        grid.rows[0],
        vec![
            "InvoiceExtractor",
            "v11",
            "robin",
            "2h ago",
            "In review",
            "New tools"
        ]
    );
}

#[test]
fn requesting_changes_needs_a_comment() {
    let mut app = approvals_screen();
    let before = fetches(&app).len();
    press(&mut app, &[KeyCode::Char('c')]);
    submit_form(&mut app);
    assert_eq!(fetches(&app).len(), before, "no request without a comment");
    typed(&mut app, "Add tests");
    submit_form(&mut app);
    let req = last_fetch(&app);
    assert_eq!(req.path, "/v1/approval-requests/201/decision");
    assert_eq!(
        req.body.unwrap(),
        json!({"decision": "rejected", "comment": "Add tests"})
    );
}

/// Ctrl+S submits a form whose field is multiline.
fn submit_form(app: &mut App) {
    app.on_key(crossterm::event::KeyEvent::new(
        KeyCode::Char('s'),
        crossterm::event::KeyModifiers::CONTROL,
    ));
}

#[test]
fn approving_sends_the_decision_and_an_optional_comment() {
    let mut app = approvals_screen();
    press(&mut app, &[KeyCode::Char('v')]);
    submit_form(&mut app);
    let req = last_fetch(&app);
    assert_eq!(req.path, "/v1/approval-requests/201/decision");
    assert_eq!(
        req.body.unwrap(),
        json!({"decision": "approved", "comment": ""})
    );
}

#[test]
fn approval_links_back_to_its_agent() {
    let mut app = approvals_screen();
    press(&mut app, &[KeyCode::Char('A')]);
    assert_eq!(app.active, resource("Agents", "Agents"));
    answer(&mut app, agents());
    assert_eq!(
        app.view().unwrap().grid().selected_raw().unwrap()["name"],
        "InvoiceExtractor"
    );
}

#[test]
fn tools_and_models_read_plainly() {
    let mut app = app();
    app.open_and_focus(resource("Agents", "Tools"));
    answer(
        &mut app,
        json!({"items": [{"name": "delete_ontology_model", "category": "ontology", "isMutating": true,
            "isDestructive": true, "requiresApproval": true,
            "inputSchema": {"properties": {"modelId": {}, "force": {}}}, "description": "Delete a model."}]}),
    );
    assert_eq!(
        app.view().unwrap().grid().rows[0],
        vec![
            "delete_ontology_model",
            "Ontology",
            "yes",
            "yes",
            "yes",
            "2",
            "Delete a model."
        ]
    );

    app.open_and_focus(resource("Agents", "Models"));
    answer(
        &mut app,
        json!({"defaultModelId": "x", "items": [
            {"id": "b", "name": "Grok 4.7", "providerName": "xAI", "capabilityOrder": 2, "isDefault": false,
             "maxOutputTokens": 524288, "pricing": {"inputUsdPerMillion": "2", "outputUsdPerMillion": "10"}},
            {"id": "a", "name": "GPT-5.6 Luna", "providerName": "OpenAI", "capabilityOrder": 1, "isDefault": true,
             "maxOutputTokens": 128000, "pricing": {"inputUsdPerMillion": "0.24", "outputUsdPerMillion": "1.44"}}]}),
    );
    let grid = app.view().unwrap().grid();
    assert_eq!(
        grid.rows[0],
        vec!["GPT-5.6 Luna", "OpenAI", "yes", "128k", "$0.24", "$1.44"],
        "catalog order, not server order"
    );
}

/// The approve decision, sent for real to a local stub server.
#[tokio::test]
async fn approve_decision_reaches_the_agent_builder_route() {
    let (base, server) = stub_server(r#"{"id":201,"status":"approved"}"#).await;
    let mut app = approvals_screen();
    press(&mut app, &[KeyCode::Char('v')]);
    typed(&mut app, "Looks good");
    submit_form(&mut app);
    let seen = execute(last_fetch(&app), base, server).await;
    assert!(
        seen.starts_with("POST /v1/approval-requests/201/decision HTTP/1.1"),
        "{seen}"
    );
    assert!(
        seen.contains(r#"{"comment":"Looks good","decision":"approved"}"#),
        "{seen}"
    );
}

/// Sends `req` through the real executor to the stub at `base` and returns
/// the raw HTTP request the stub saw.
pub(crate) async fn execute(
    req: FetchReq,
    base: String,
    server: tokio::task::JoinHandle<String>,
) -> String {
    let resolved = crate::config::resolve(
        &crate::config::Config::default(),
        Some("t"),
        Some(base),
        Some("tok".into()),
    );
    let client =
        crate::http::ApiClient::new(resolved, crate::output::OutputFormat::Json, true, 0, 5)
            .unwrap();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut executor = crate::tui::fetch::Executor::new(client, tx);
    executor.run(Request::Fetch(req));
    server.await.unwrap()
}

/// A one-shot HTTP server that answers `reply` (JSON) and returns the raw
/// request it received.
pub(crate) async fn stub_server(reply: &'static str) -> (String, tokio::task::JoinHandle<String>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut raw = Vec::new();
        let mut chunk = [0u8; 4096];
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
                if body.len() >= length || n == 0 {
                    break;
                }
            }
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
            reply.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        String::from_utf8_lossy(&raw).to_string()
    });
    (base, server)
}

#[test]
fn blocks_the_agent_builder_denies_with_401_are_hidden_not_sign_in_errors() {
    let mut app = agents_screen();
    select_row(&mut app, &["name"], "InvoiceExtractor");
    press(&mut app, &[KeyCode::Char('i')]);
    answer(&mut app, json!({"id": AGENT, "name": "InvoiceExtractor"}));
    let approvals = fetches(&app)
        .into_iter()
        .find(|f| f.path.contains("/approvals?"))
        .unwrap();
    app.fetched(
        approvals.id,
        approvals.target,
        Err(r#"GET u → HTTP 401 Unauthorized: You do not have "All" or "Approve" permission on agent."#.into()),
    );
    let screen = draw(&mut app, 120, 70);
    assert!(!screen.contains("Approval history"), "{screen}");
    assert!(!screen.contains("gz auth login"), "{screen}");
}
