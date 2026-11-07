//! `gz flows` and agent sub-agents against a stub agent builder: request
//! shapes, live run output, approvals, and exit codes.

mod common;

use common::{plain, read_request, respond, Recorded};
use serde_json::{json, Value};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

type Routes = fn(&str, &str) -> Option<(&'static str, &'static str, String)>;

/// Serves `routes(method, path)`; unknown routes get 404.
fn stub(routes: Routes) -> (String, Arc<Mutex<Vec<Recorded>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let log = Arc::new(Mutex::new(Vec::new()));
    let seen = log.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let Some(request) = read_request(&mut stream) else {
                continue;
            };
            seen.lock().unwrap().push(request.clone());
            let path = request
                .target
                .split('?')
                .next()
                .unwrap_or_default()
                .to_string();
            match routes(&request.method, &path) {
                Some((status, content_type, body)) => {
                    respond(&mut stream, status, content_type, &body)
                }
                None => respond(&mut stream, "404 Not Found", "application/json", "{}"),
            }
        }
    });
    (base, log)
}

fn sse(events: &[(&str, Value)]) -> String {
    let mut body = String::new();
    for (i, (kind, data)) in events.iter().enumerate() {
        let mut data = data.clone();
        data["type"] = json!(kind);
        body.push_str(&format!("id: {}\nevent: {kind}\ndata: {data}\n\n", i + 1));
    }
    body.push_str("event: done\ndata: [DONE]\n\n");
    body
}

fn ok(body: Value) -> Option<(&'static str, &'static str, String)> {
    Some(("200 OK", "application/json", body.to_string()))
}

fn events(body: String) -> Option<(&'static str, &'static str, String)> {
    Some(("200 OK", "text/event-stream", body))
}

fn flow_definition() -> Value {
    json!({"id": "f1", "name": "invoice-review", "nodes": [
        {"id": "extract", "type": "agent.invoke", "title": "Extract invoices"},
        {"id": "approve", "type": "people.approval", "title": "Finance sign-off"},
    ]})
}

fn workspace(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gz-flows-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn gz(dir: &PathBuf, base: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_gz"))
        .current_dir(dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("GZ_PROFILE", "flows-test")
        .env("GZ_HOST", base)
        .env("GZ_TOKEN", "stub-token")
        .env("GZ_SITE", "acme")
        .stdin(std::process::Stdio::null())
        .args(args)
        .output()
        .unwrap()
}

fn find<'a>(log: &'a [Recorded], method: &str, path: &str) -> &'a Recorded {
    log.iter()
        .find(|r| r.method == method && r.target.split('?').next() == Some(path))
        .unwrap_or_else(|| panic!("no {method} {path} in {log:#?}"))
}

#[test]
fn create_sends_the_yaml_definition_with_a_new_id_and_project() {
    let (base, log) = stub(|method, path| match (method, path) {
        ("POST", "/v1/flows") => ok(json!({"id": "f-new", "name": "invoice-review"})),
        _ => None,
    });
    let dir = workspace("create");
    std::fs::write(
        dir.join("flow.yaml"),
        "name: invoice-review\nnodes:\n  - {id: extract, type: agent.invoke, config: {agentId: a1}}\n",
    )
    .unwrap();
    let out = gz(
        &dir,
        &base,
        &["flows", "create", "-f", "flow.yaml", "--project", "7"],
    );
    assert!(out.status.success(), "{}", plain(&out.stderr));
    let log = log.lock().unwrap();
    let body = &find(&log, "POST", "/v1/flows").body;
    assert_eq!(body["projectId"], 7);
    assert_eq!(body["nodes"][0]["type"], "agent.invoke");
    assert_eq!(body["id"].as_str().unwrap().len(), 36);
}

#[test]
fn validate_lists_problems_by_step_and_fails() {
    let (base, _) = stub(|method, path| match (method, path) {
        ("POST", "/v1/flows/validate") => ok(json!({"valid": false, "errors": [
            {"code": "MemberUnknown", "message": "agent not found", "path": "nodes[0].config.agentId", "severity": "error"}
        ], "warnings": [{"code": "W", "message": "no trigger", "path": "triggers", "severity": "warning"}]})),
        _ => None,
    });
    let dir = workspace("validate");
    std::fs::write(
        dir.join("flow.json"),
        r#"{"name": "x", "nodes": [{"id": "extract", "type": "agent.invoke"}]}"#,
    )
    .unwrap();
    let out = gz(&dir, &base, &["flows", "validate", "flow.json"]);
    assert!(!out.status.success());
    let stderr = plain(&out.stderr);
    assert!(
        stderr.contains("step extract: config.agentId: agent not found"),
        "{stderr}"
    );
    assert!(stderr.contains("triggers: no trigger"), "{stderr}");
}

fn run_routes(method: &str, path: &str) -> Option<(&'static str, &'static str, String)> {
    match (method, path) {
        ("POST", "/v1/flows/f1/runs") => Some((
            "202 Accepted",
            "application/json",
            json!({"runId": "r1"}).to_string(),
        )),
        ("GET", "/v1/flows/f1") => ok(flow_definition()),
        ("GET", "/v1/flows/f1/runs/r1/events") => events(sse(&[
            (
                "orchestration.node.started",
                json!({"nodeId": "extract", "attempt": 1}),
            ),
            (
                "orchestration.node.tool",
                json!({"nodeId": "extract", "toolName": "run_sql", "status": "started"}),
            ),
            (
                "orchestration.node.completed",
                json!({"nodeId": "extract", "durationMs": 1500, "totalCostUsd": "0.0021"}),
            ),
            (
                "orchestration.node.skipped",
                json!({"nodeId": "approve", "reason": "no approval needed"}),
            ),
            (
                "orchestration.totals",
                json!({"inputTokens": 1200, "outputTokens": 30, "totalCostUsd": "0.0021"}),
            ),
            (
                "completed",
                json!({"runId": "r1", "result": {"text": "Posted 3 invoices"}}),
            ),
        ])),
        _ => None,
    }
}

#[test]
fn run_streams_steps_and_prints_the_result() {
    let (base, log) = stub(run_routes);
    let dir = workspace("run");
    let out = gz(
        &dir,
        &base,
        &[
            "flows",
            "run",
            "f1",
            "--param",
            "region=APAC",
            "--param",
            "limit=5",
        ],
    );
    let stdout = plain(&out.stdout);
    let stderr = plain(&out.stderr);
    assert!(out.status.success(), "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("Posted 3 invoices"), "{stdout}");
    assert!(stderr.contains("▶ Extract invoices"), "{stderr}");
    assert!(stderr.contains("⎿  Extract invoices · run_sql"), "{stderr}");
    assert!(
        stderr.contains("✓ Extract invoices  1.5s · $0.0021"),
        "{stderr}"
    );
    assert!(stderr.contains("– Finance sign-off"), "{stderr}");
    let log = log.lock().unwrap();
    let start = &find(&log, "POST", "/v1/flows/f1/runs").body;
    assert_eq!(start["parameters"], json!({"region": "APAC", "limit": 5}));
    assert_eq!(start["version"], "approved");
    assert_eq!(start["background"], true);
}

#[test]
fn a_failed_run_exits_non_zero_with_the_reason() {
    let (base, _) = stub(|method, path| match (method, path) {
        ("POST", "/v1/flows/f1/runs") => ok(json!({"runId": "r1"})),
        ("GET", "/v1/flows/f1") => ok(flow_definition()),
        ("GET", "/v1/flows/f1/runs/r1/events") => events(sse(&[
            ("orchestration.node.started", json!({"nodeId": "extract"})),
            (
                "orchestration.node.failed",
                json!({"nodeId": "extract", "error": {"code": "AgentFailed", "message": "model refused"}, "willRetry": false}),
            ),
            (
                "failed",
                json!({"runId": "r1", "error": "AgentFailed", "message": "Extract invoices failed"}),
            ),
        ])),
        _ => None,
    });
    let dir = workspace("failed");
    let out = gz(&dir, &base, &["flows", "run", "f1", "--latest"]);
    assert!(!out.status.success());
    let stderr = plain(&out.stderr);
    assert!(
        stderr.contains("✗ Extract invoices · model refused"),
        "{stderr}"
    );
    assert!(
        stderr.contains("flow run r1 failed: Extract invoices failed"),
        "{stderr}"
    );
}

#[test]
fn a_pause_without_a_terminal_explains_how_to_resume() {
    let (base, _) = stub(|method, path| match (method, path) {
        ("POST", "/v1/flows/f1/runs") => ok(json!({"runId": "r1"})),
        ("GET", "/v1/flows/f1") => ok(flow_definition()),
        ("GET", "/v1/flows/f1/runs/r1/events") => events(sse(&[
            ("orchestration.node.started", json!({"nodeId": "approve"})),
            (
                "orchestration.interrupted",
                json!({"interrupts": [{"id": "orc:9:approval", "name": "orchestration_approval",
                "nodeId": "approve", "title": "Finance sign-off", "reason": "Approve posting"}]}),
            ),
            ("interrupted", json!({"runId": "r1"})),
        ])),
        _ => None,
    });
    let dir = workspace("paused");
    let out = gz(&dir, &base, &["flows", "run", "f1"]);
    assert!(!out.status.success(), "a paused run isn't done");
    let stderr = plain(&out.stderr);
    assert!(
        stderr.contains("Finance sign-off: Approve posting"),
        "{stderr}"
    );
    assert!(
        stderr.contains("gz flows resume f1 r1 --approve 'orc:9:approval'"),
        "{stderr}"
    );
}

#[test]
fn resume_answers_the_paused_segment_and_follows_the_new_one() {
    let (base, log) = stub(|method, path| match (method, path) {
        ("POST", "/v1/flows/f1/runs/r1/resume") => ok(json!({"runId": "r2", "status": "running"})),
        ("GET", "/v1/flows/f1") => ok(flow_definition()),
        ("GET", "/v1/flows/f1/runs/r2/events") => events(sse(&[
            (
                "orchestration.node.completed",
                json!({"nodeId": "approve", "durationMs": 10}),
            ),
            ("completed", json!({"runId": "r2", "result": "done"})),
        ])),
        _ => None,
    });
    let dir = workspace("resume");
    let out = gz(
        &dir,
        &base,
        &[
            "flows",
            "resume",
            "f1",
            "r1",
            "--reject",
            "orc:9:approval",
            "--comment",
            "wrong totals",
        ],
    );
    assert!(out.status.success(), "{}", plain(&out.stderr));
    assert!(plain(&out.stdout).contains("done"));
    let log = log.lock().unwrap();
    let body = &find(&log, "POST", "/v1/flows/f1/runs/r1/resume").body;
    assert_eq!(
        body["interruptResponses"],
        json!([{"interruptId": "orc:9:approval", "decision": "rejected", "comment": "wrong totals"}])
    );
    assert_eq!(body["background"], true);
}

#[test]
fn add_sub_agent_patches_the_whole_list() {
    let (base, log) = stub(|method, path| match (method, path) {
        ("GET", "/v1/agents/p1") => {
            ok(json!({"id": "p1", "subAgents": [{"agentId": "c1", "alias": "billing"}]}))
        }
        ("PATCH", "/v1/agents/p1") => ok(json!({"id": "p1"})),
        _ => None,
    });
    let dir = workspace("subagents");
    let out = gz(
        &dir,
        &base,
        &[
            "agents",
            "add-sub-agent",
            "p1",
            "c2",
            "--alias",
            "refunds",
            "--description",
            "Issues refunds",
        ],
    );
    assert!(out.status.success(), "{}", plain(&out.stderr));
    let log = log.lock().unwrap();
    assert_eq!(
        find(&log, "PATCH", "/v1/agents/p1").body,
        json!({"subAgents": [
            {"agentId": "c1", "alias": "billing"},
            {"agentId": "c2", "alias": "refunds", "description": "Issues refunds"},
        ]})
    );
}

#[test]
fn invoke_nests_sub_agent_progress_under_the_answer() {
    let (base, _) = stub(|method, path| match (method, path) {
        ("POST", "/v1/agents/p1/invoke") => ok(json!({"runId": "r1", "sessionId": "s1"})),
        ("GET", "/v1/agents/p1/runs/r1/events") => events(sse(&[
            (
                "subagent_started",
                json!({"subagentId": "sa_1", "name": "billing", "agentId": "c1"}),
            ),
            (
                "subagent_tool",
                json!({"subagentId": "sa_1", "toolName": "run_sql", "status": "started"}),
            ),
            (
                "subagent_finished",
                json!({"subagentId": "sa_1", "status": "completed", "summary": "Invoice 42 is paid", "durationMs": 800}),
            ),
            ("llm_chunk", json!({"text": "Invoice 42 was paid on time."})),
            ("run_status", json!({"status": "completed"})),
        ])),
        _ => None,
    });
    let dir = workspace("invoke");
    let out = gz(
        &dir,
        &base,
        &["agents", "invoke", "p1", "Was invoice 42 paid?"],
    );
    let stderr = plain(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(plain(&out.stdout).contains("Invoice 42 was paid on time."));
    assert!(stderr.contains("  ● billing"), "{stderr}");
    assert!(stderr.contains("    ⎿  billing · run_sql"), "{stderr}");
    assert!(
        stderr.contains("  ✓ billing · 800ms · Invoice 42 is paid"),
        "{stderr}"
    );
}
