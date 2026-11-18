//! `gz audit agents` against a stub agent builder: the paged audit route,
//! findings in the table, JSON and CSV, the filters, `--fail-on`, and the
//! refusals for non-administrators and older agent builders.

mod common;

use common::{plain, read_request, respond, Recorded};
use serde_json::{json, Value};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

const AUDIT: &str = "/v1/admin/agents/audit";

#[derive(Clone, Copy, PartialEq)]
enum Builder {
    /// Answers the audit with the three agents below.
    Admin,
    /// Answers with 101 tidy agents, so gz has to read two pages.
    Many,
    /// Refuses: the caller isn't in the Admin group.
    NotAdmin,
    /// Predates the audit route.
    Older,
}

fn tool(name: &str, category: &str, mutating: bool, elevated: bool, approval: bool) -> Value {
    json!({
        "name": name, "kind": "builtIn", "category": category, "isMutating": mutating,
        "isElevated": elevated, "isDestructive": false, "requiresApproval": approval,
        "skipsApprovalInEtlJobs": false,
    })
}

fn agent(id: &str, name: &str, owner: &str, owner_status: &str, extra: Value) -> Value {
    let mut record = json!({
        "id": id, "name": name, "description": null, "version": 1,
        "createdBy": owner, "ownerStatus": owner_status,
        "createdAt": "2026-01-01T00:00:00+00:00", "modifiedBy": owner,
        "updatedAt": "2026-02-01T00:00:00+00:00", "isPublished": true, "isExposed": false,
        "governanceState": "active", "riskTier": "unclassified", "dataClassification": null,
        "model": {"modelId": "global.openai.gpt-5.6-luna", "provider": "GZ", "connectionName": null},
        "tools": [tool("get_datasets", "datasets", false, false, false)],
        "mcpConnections": [], "dataConnections": [{"use": "history", "name": "GZS3"}],
        "subAgents": [], "team": null,
        "approvals": {"requiredTools": [], "effectiveTools": [], "etlAutoApprovedTools": []},
        "limits": {"maxTurns": 300, "maxTokens": 4096, "maxRunOutputTokens": null,
                   "maxRunTreeSpendUsd": null, "quotaLimitCents": null, "quotaSpendUsd": null},
        "runs": {"count": 2, "childRunCount": 0, "completedCount": 2, "failedCount": 0,
                 "cancelledCount": 0, "interruptedCount": 0, "runningCount": 0,
                 "costUsd": "0.040000", "unpricedRunCount": 0,
                 "lastRunAt": "2099-01-01T00:00:00+00:00", "lastRunStatus": "completed"},
    });
    for (key, value) in extra.as_object().unwrap() {
        record[key] = value.clone();
    }
    record
}

fn three_agents() -> Vec<Value> {
    vec![
        agent(
            "a1",
            "writer",
            "alice",
            "active",
            json!({"tools": [tool("submit_etl_job", "etl", true, true, false)]}),
        ),
        agent("a2", "clean", "bob", "active", json!({})),
        agent(
            "a3",
            "orphan",
            "carol",
            "missing",
            json!({"tools": [], "runs": {
                "count": 0, "childRunCount": 0, "completedCount": 0, "failedCount": 0,
                "cancelledCount": 0, "interruptedCount": 0, "runningCount": 0,
                "costUsd": "0", "unpricedRunCount": 0, "lastRunAt": null, "lastRunStatus": null,
            }}),
        ),
    ]
}

fn offset_of(target: &str) -> usize {
    target
        .split(['?', '&'])
        .find_map(|pair| pair.strip_prefix("offset="))
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

fn reply(builder: Builder, request: &Recorded) -> (&'static str, Value) {
    let path = request.target.split('?').next().unwrap_or_default();
    if builder == Builder::Older || path != AUDIT {
        return ("404 Not Found", json!({"detail": "Not Found"}));
    }
    let items = match builder {
        Builder::NotAdmin => {
            return (
                "403 Forbidden",
                json!({"error": "AdminRequired", "message": "This operation requires the Admin Cognito group"}),
            );
        }
        Builder::Admin => three_agents(),
        _ => {
            let all: Vec<Value> = (0..101)
                .map(|i| {
                    agent(
                        &format!("m{i}"),
                        &format!("tidy{i}"),
                        "bob",
                        "active",
                        json!({}),
                    )
                })
                .collect();
            all.into_iter()
                .skip(offset_of(&request.target))
                .take(100)
                .collect()
        }
    };
    (
        "200 OK",
        json!({"items": items, "runsFrom": "2026-10-19T00:00:00+00:00", "limit": 100, "offset": 0}),
    )
}

fn stub(builder: Builder) -> (String, Arc<Mutex<Vec<Recorded>>>) {
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
            let (status, body) = reply(builder, &request);
            respond(&mut stream, status, "application/json", &body.to_string());
        }
    });
    (base, log)
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gz-audit-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn gz(name: &str, builder: Builder, args: &[&str]) -> (Output, Vec<String>) {
    let dir = temp_dir(name);
    let (base, log) = stub(builder);
    let out = run(&dir, &base, args);
    let requests = log
        .lock()
        .unwrap()
        .iter()
        .map(|r| {
            // GET handlers read the query string only.
            assert_eq!(r.body, Value::Null, "{} sent a body", r.target);
            format!("{} {}", r.method, r.target)
        })
        .collect();
    (out, requests)
}

fn run(dir: &Path, base: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_gz"))
        .current_dir(dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("GZ_PROFILE", "audit-test")
        .env("GZ_HOST", base)
        .env("GZ_TOKEN", "stub-token")
        .env("GZ_SITE", "acme")
        .args(args)
        .output()
        .unwrap()
}

fn json_out(out: &Output) -> Value {
    assert!(out.status.success(), "{}", plain(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn the_table_shows_every_agent_and_its_findings() {
    let (out, requests) = gz("table", Builder::Admin, &["audit", "agents"]);

    assert!(out.status.success(), "{}", plain(&out.stderr));
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].starts_with(&format!("GET {AUDIT}?from=")),
        "{requests:?}"
    );
    assert!(requests[0].ends_with("&limit=100&offset=0"), "{requests:?}");
    let stdout = plain(&out.stdout);
    for expected in [
        "writer",
        "clean",
        "carol (gone)",
        "write-without-approval",
        "changes data without asking: submit_etl_job",
        "owner-gone",
        "owner carol no longer exists in the user pool",
        "never run; created",
    ] {
        assert!(
            stdout.contains(expected),
            "missing {expected:?} in\n{stdout}"
        );
    }
    assert!(
        plain(&out.stderr).contains("3 agents · 2 high, 0 medium, 1 low"),
        "{}",
        plain(&out.stderr)
    );
}

#[test]
fn json_attaches_findings_to_each_record_and_sums_them() {
    let (out, _) = gz("json", Builder::Admin, &["-o", "json", "audit", "agents"]);

    let shown = json_out(&out);
    assert_eq!(
        shown["summary"],
        json!({"agents": 3, "high": 2, "medium": 0, "low": 1})
    );
    assert_eq!(shown["runsFrom"], "2026-10-19T00:00:00+00:00");
    let writer = &shown["agents"][0];
    assert_eq!(writer["name"], "writer");
    assert_eq!(writer["tools"][0]["name"], "submit_etl_job");
    assert_eq!(
        writer["findings"],
        json!([{
            "agentId": "a1", "agent": "writer", "finding": "write-without-approval",
            "severity": "high", "reason": "changes data without asking: submit_etl_job",
        }])
    );
    assert_eq!(shown["agents"][1]["findings"], json!([]));
}

#[test]
fn every_page_is_read() {
    let (out, requests) = gz("pages", Builder::Many, &["-o", "json", "audit", "agents"]);

    let shown = json_out(&out);
    assert_eq!(shown["agents"].as_array().unwrap().len(), 101);
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1].ends_with("&limit=100&offset=100"),
        "{requests:?}"
    );
}

#[test]
fn filters_narrow_agents_and_findings() {
    let (by_owner, _) = gz(
        "owner",
        Builder::Admin,
        &["-o", "json", "audit", "agents", "--owner", "bob"],
    );
    let (by_finding, _) = gz(
        "finding",
        Builder::Admin,
        &["-o", "json", "audit", "agents", "--finding", "owner-gone"],
    );
    let (by_severity, _) = gz(
        "severity",
        Builder::Admin,
        &["-o", "json", "audit", "agents", "--severity", "high"],
    );

    let names = |shown: &Value| -> Vec<String> {
        shown["agents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["name"].as_str().unwrap().to_string())
            .collect()
    };
    let by_owner = json_out(&by_owner);
    assert_eq!(names(&by_owner), ["clean"]);
    assert_eq!(by_owner["findings"], json!([]));
    let by_finding = json_out(&by_finding);
    assert_eq!(names(&by_finding), ["orphan"]);
    assert_eq!(by_finding["findings"].as_array().unwrap().len(), 1);
    let by_severity = json_out(&by_severity);
    assert_eq!(names(&by_severity), ["writer", "orphan"]);
    assert_eq!(by_severity["summary"]["low"], 0);
}

#[test]
fn fail_on_exits_two_only_when_a_shown_finding_reaches_the_severity() {
    let (failing, _) = gz(
        "fail",
        Builder::Admin,
        &["audit", "agents", "--fail-on", "high"],
    );
    let (clean, _) = gz(
        "clean",
        Builder::Admin,
        &["audit", "agents", "--owner", "bob", "--fail-on", "low"],
    );

    assert_eq!(failing.status.code(), Some(2), "{}", plain(&failing.stderr));
    assert!(plain(&failing.stderr).contains("2 finding(s) at high severity or above"));
    // The report still prints before the exit.
    assert!(plain(&failing.stdout).contains("write-without-approval"));
    assert!(clean.status.success(), "{}", plain(&clean.stderr));
}

#[test]
fn csv_prints_one_row_per_finding() {
    let (out, _) = gz("csv", Builder::Admin, &["-o", "csv", "audit", "agents"]);

    assert!(out.status.success(), "{}", plain(&out.stderr));
    let stdout = plain(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 4, "{stdout}");
    assert_eq!(lines[0], "agent,finding,reason,severity");
    assert!(lines[1].ends_with(",high"), "{stdout}");
}

#[test]
fn non_administrators_are_told_why_they_are_refused() {
    let (out, _) = gz("refused", Builder::NotAdmin, &["audit", "agents"]);

    assert_eq!(out.status.code(), Some(1));
    let stderr = plain(&out.stderr);
    assert!(
        stderr.contains("gz audit agents is for site administrators"),
        "{stderr}"
    );
    assert!(
        stderr.contains("requires the Admin Cognito group"),
        "{stderr}"
    );
    assert!(stderr.contains("Admin group"), "{stderr}");
}

#[test]
fn an_older_agent_builder_says_what_to_deploy() {
    let (out, _) = gz("older", Builder::Older, &["audit", "agents"]);

    assert_eq!(out.status.code(), Some(1));
    let stderr = plain(&out.stderr);
    assert!(
        stderr.contains("needs the gz-agent-builder-provider-api update"),
        "{stderr}"
    );
}
