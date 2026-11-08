//! Spend against a stub chat server, with an isolated config: one-shot
//! `gz code` adds each run's reported cost to the local ledger and refuses
//! (exit code 3, nothing sent) once the day cap is spent; `gz usage budget`
//! shows the server budget next to that local spend.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

mod common;

use common::{plain, read_request, respond, Recorded};

const PROFILE: &str = "spend-test";
const RUN: &str = concat!(
    "id: 1\ndata: {\"type\":\"llm_chunk\",\"text\":\"Done.\"}\n\n",
    "id: 2\ndata: {\"type\":\"response_metrics\",\"inputTokens\":900,\"outputTokens\":100,",
    "\"tokenUsageStatus\":\"reported\",\"costUsd\":\"0.400000\"}\n\n",
    "id: 3\ndata: {\"type\":\"run_status\",\"status\":\"completed\"}\n\n",
    "data: [DONE]\n\n",
);
const BUDGET: &str = r#"{"site":"acme","policySource":"group","policySubjectName":"data-team",
  "limitCents":5000,"usedCents":1190,"remainingCents":3810,"usedUsd":"11.900000000000000000",
  "activeReservedUsd":"0.000000000000000000","availableUsd":"38.100000000000000000",
  "exhausted":false,"currency":"USD","windowDays":30}"#;

fn stub_chat_server() -> (String, Arc<Mutex<Vec<Recorded>>>) {
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
            match request.target.split('?').next().unwrap() {
                "/chat/runs" => respond(
                    &mut stream,
                    "202 Accepted",
                    "application/json",
                    r#"{"runId":"r1"}"#,
                ),
                "/chat/runs/r1/events" => respond(&mut stream, "200 OK", "text/event-stream", RUN),
                "/chat/budget-status" => respond(&mut stream, "200 OK", "application/json", BUDGET),
                _ => respond(&mut stream, "404 Not Found", "application/json", "{}"),
            }
        }
    });
    (base, log)
}

/// A workspace and an isolated config folder (HOME too) with `caps`.
fn workspace(name: &str, caps: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gz-spend-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("ws")).unwrap();
    std::fs::create_dir_all(dir.join("config/gz")).unwrap();
    let config = format!(
        "current_profile = \"{PROFILE}\"\n\n[profiles.{PROFILE}.code_spend_caps]\n{caps}\n"
    );
    std::fs::write(dir.join("config/gz/config.toml"), config).unwrap();
    dir
}

fn gz(dir: &Path, base: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_gz"))
        .current_dir(dir.join("ws"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("HOME", dir)
        .env("GZ_PROFILE", PROFILE)
        .env("GZ_HOST", base)
        .env("GZ_TOKEN", "stub-token")
        .env("GZ_SITE", "acme")
        .stdin(std::process::Stdio::null())
        .args(args)
        .output()
        .unwrap()
}

/// What each started run asked, in order.
fn prompts_sent(log: &Arc<Mutex<Vec<Recorded>>>) -> Vec<String> {
    log.lock()
        .unwrap()
        .iter()
        .filter(|r| r.method == "POST" && r.target == "/chat/runs")
        .filter_map(|r| r.body["userInput"].as_str().map(str::to_string))
        .collect()
}

fn ledger(dir: &Path) -> serde_json::Value {
    let text = std::fs::read_to_string(dir.join("config/gz/code_spend.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

fn today_usd(dir: &Path) -> f64 {
    let days = &ledger(dir)["profiles"][PROFILE];
    let (_, day) = days.as_object().unwrap().iter().next_back().unwrap();
    day["totalUsd"].as_f64().unwrap()
}

#[test]
fn one_shot_runs_count_toward_the_day_cap_and_stop_at_it() {
    let dir = workspace("day-cap", "day_usd = 0.75");
    let (base, log) = stub_chat_server();

    let first = gz(&dir, &base, &["code", "hello"]);
    assert!(first.status.success(), "{}", plain(&first.stderr));
    assert!((today_usd(&dir) - 0.4).abs() < 1e-9);

    let second = gz(&dir, &base, &["code", "hello again"]);
    assert!(second.status.success(), "{}", plain(&second.stderr));
    assert!((today_usd(&dir) - 0.8).abs() < 1e-9);
    assert!(
        plain(&second.stderr).contains("Your $0.75 day cap is reached ($0.80 spent)"),
        "{}",
        plain(&second.stderr)
    );

    let third = gz(&dir, &base, &["code", "one more"]);
    assert_eq!(third.status.code(), Some(3), "{}", plain(&third.stderr));
    assert!(
        plain(&third.stderr).contains("not sent: $0.80 of your $0.75 day cap is spent"),
        "{}",
        plain(&third.stderr)
    );
    assert_eq!(
        prompts_sent(&log),
        ["hello", "hello again"],
        "nothing is sent past the cap"
    );
}

#[test]
fn usage_budget_shows_the_server_budget_and_local_spend() {
    let dir = workspace("usage", "session_usd = 5.0");
    let (base, _log) = stub_chat_server();
    assert!(gz(&dir, &base, &["code", "hello"]).status.success());

    let table = gz(&dir, &base, &["usage", "budget"]);
    let text = plain(&table.stdout);
    assert!(table.status.success(), "{}", plain(&table.stderr));
    for part in [
        "24% · $38.10 of $50 left (30d)",
        "set for group data-team",
        "today        $0.40",
        "session cap  $5",
        "day cap      off",
    ] {
        assert!(text.contains(part), "missing {part:?} in\n{text}");
    }

    let json = gz(&dir, &base, &["usage", "budget", "-o", "json"]);
    let value: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(value["budget"]["policySubjectName"], "data-team");
    assert_eq!(value["local"]["todayUsd"], 0.4);
    assert_eq!(value["local"]["monthModels"][0]["usd"], 0.4);
    assert_eq!(value["caps"]["sessionUsd"], 5.0);
    assert_eq!(value["caps"]["dayUsd"], serde_json::Value::Null);
}
