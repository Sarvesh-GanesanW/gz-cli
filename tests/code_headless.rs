//! Scripted runs: `gz code -p`, `--output-format text|json|stream-json`,
//! `--input-format stream-json`, `--max-turns`, `--max-budget-usd`,
//! `--tools`, `--append-system-prompt`, `--add-dir` and `--bare`, against a
//! stub chat server. Nothing ever waits for a person.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod common;

use common::{read_request, respond, Recorded};

const EXIT_NEEDED_APPROVAL: i32 = 4;
const EXIT_MAX_TURNS: i32 = 5;
const EXIT_MAX_BUDGET: i32 = 6;

fn stream(events: &[Value]) -> String {
    let mut body = String::new();
    for (i, event) in events.iter().enumerate() {
        body.push_str(&format!("id: {}\ndata: {event}\n\n", i + 1));
    }
    body.push_str("data: [DONE]\n\n");
    body
}

fn metrics(cost: &str) -> Value {
    json!({"type": "response_metrics", "inputTokens": 100, "outputTokens": 20, "tokenUsageStatus": "reported", "costUsd": cost})
}

/// The run asks gz to read hello.txt (after costing `cost`), then pauses.
fn asks_to_read(cost: Option<&str>) -> String {
    let mut events = vec![json!({"type": "llm_chunk", "text": "Let me look."})];
    events.extend(cost.map(metrics));
    events.push(json!({"type": "client_tool_request", "interruptId": "ct_1", "toolUseId": "tu_1", "toolName": "read_file", "input": {"path": "hello.txt"}}));
    events.push(json!({"type": "run_status", "status": "interrupted", "reason": "client_tools"}));
    stream(&events)
}

fn answers() -> String {
    stream(&[
        json!({"type": "llm_chunk", "text": "All done."}),
        metrics("0.004200"),
        json!({"type": "run_status", "status": "completed"}),
    ])
}

const LIMITS: &str = r#""localContextLimits":{"instructionChars":20000,"instructionFiles":8,"memoryEntries":200,"memoryChars":20000,"memoryEntryChars":500,"skills":200,"skillDescriptionChars":1024}"#;

fn capabilities(local_context: bool) -> String {
    let mut caps: Value = serde_json::from_str(&format!("{{{LIMITS}}}")).unwrap();
    caps["groundzeroModes"] =
        json!([{"value": "sonnet55", "label": "Claude Sonnet 5.5", "reasoningEfforts": []}]);
    caps["defaultGroundzeroMode"] = json!("sonnet55");
    caps["features"] = json!({"localContext": local_context});
    caps.to_string()
}

struct Stub {
    base: String,
    log: Arc<Mutex<Vec<Recorded>>>,
}

impl Stub {
    fn serve(routes: Vec<(&'static str, String)>) -> Self {
        let mut routes: HashMap<&'static str, String> = routes.into_iter().collect();
        routes.insert("POST /chat/runs", r#"{"runId":"r1"}"#.into());
        routes.insert(
            "POST /chat/runs/r1/client-tools/resume",
            r#"{"runId":"r2"}"#.into(),
        );
        routes.insert("GET /connection", "[]".into());
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
                let path = request.target.split('?').next().unwrap();
                let key = format!("{} {path}", request.method);
                match routes.get(key.as_str()) {
                    Some(body) if path.ends_with("/events") => {
                        respond(&mut stream, "200 OK", "text/event-stream", body)
                    }
                    Some(body) => respond(&mut stream, "200 OK", "application/json", body),
                    None => {
                        stream
                            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                            .ok();
                    }
                }
            }
        });
        Self { base, log }
    }

    fn requests(&self, method: &str, path: &str) -> Vec<Recorded> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.method == method && r.target.split('?').next() == Some(path))
            .cloned()
            .collect()
    }

    fn run_body(&self) -> Value {
        self.requests("POST", "/chat/runs")
            .first()
            .expect("a run was started")
            .body
            .clone()
    }
}

/// A read-then-answer run, on a server with or without local context.
fn reading_stub(local_context: bool, first_cost: Option<&str>) -> Stub {
    Stub::serve(vec![
        ("GET /chat/capabilities", capabilities(local_context)),
        ("GET /chat/runs/r1/events", asks_to_read(first_cost)),
        ("GET /chat/runs/r2/events", answers()),
    ])
}

fn workspace(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gz-code-headless-{}-{name}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(dir.join("ws")).unwrap();
    std::fs::write(dir.join("ws/hello.txt"), "hello from file\n").unwrap();
    dir
}

/// Runs `gz code` with `input` piped in (stdin then closed); a run that
/// waited for a person would hang until the timeout.
fn gz_code(dir: &Path, stub: &Stub, args: &[&str], input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_gz"))
        .current_dir(dir.join("ws"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("GZ_PROFILE", "headless-test")
        .env("GZ_HOST", &stub.base)
        .env("GZ_TOKEN", "stub-token")
        .env("GZ_SITE", "acme")
        .arg("code")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(input.as_bytes()).unwrap();
    drop(stdin);
    let started = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if started.elapsed() > Duration::from_secs(30) {
            child.kill().ok();
            panic!("gz code waited instead of finishing");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().unwrap()
}

fn json_out(out: &Output) -> Value {
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(&stdout).unwrap_or_else(|err| {
        panic!(
            "stdout is not one JSON object ({err}):\n{stdout}\nstderr: {}",
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

fn lines_out(out: &Output) -> Vec<Value> {
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|err| panic!("not JSON ({err}): {line}"))
        })
        .collect()
}

#[test]
fn print_mode_prints_only_the_final_answer() {
    let dir = workspace("print");
    let stub = reading_stub(false, None);
    let out = gz_code(
        &dir,
        &stub,
        &["-p", "Summarize hello.txt", "--yes-reads"],
        "",
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "All done.\n");
    let stderr = common::plain(&out.stderr);
    assert!(
        !stderr.contains("read_file hello.txt"),
        "progress only with --verbose: {stderr}"
    );
    assert_eq!(stub.run_body()["userInput"], "Summarize hello.txt");
}

#[test]
fn verbose_print_mode_shows_progress_on_stderr() {
    let dir = workspace("verbose");
    let stub = reading_stub(false, None);
    let out = gz_code(
        &dir,
        &stub,
        &["-p", "Summarize", "--yes-reads", "--verbose"],
        "",
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "All done.\n");
    assert!(common::plain(&out.stderr).contains("read_file hello.txt"));
}

#[test]
fn print_mode_reads_a_piped_prompt() {
    let dir = workspace("print-piped");
    let stub = reading_stub(false, None);
    let out = gz_code(&dir, &stub, &["--print", "--yes-reads"], "fix\nthe test\n");
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(stub.run_body()["userInput"], "fix\nthe test");
}

#[test]
fn json_output_is_one_result_object() {
    let dir = workspace("json");
    let stub = reading_stub(false, None);
    let out = gz_code(
        &dir,
        &stub,
        &["-p", "Summarize", "--yes-reads", "--output-format", "json"],
        "",
    );
    let result = json_out(&out);
    assert_eq!(out.status.code(), Some(0), "{result}");
    assert_eq!(result["type"], "result");
    assert_eq!(result["subtype"], "success");
    assert_eq!(result["is_error"], false);
    assert_eq!(result["result"], "All done.");
    assert_eq!(result["conversation_id"], stub.run_body()["conversationId"]);
    assert_eq!(result["model"], "sonnet55");
    assert_eq!(result["num_turns"], 2);
    assert_eq!(result["cost_usd"], 0.0042);
    assert_eq!(result["usage"]["input_tokens"], 100);
    assert_eq!(result["permission_denials"], json!([]));
    assert_eq!(result["tool_calls"][0]["tool"], "read_file");
    assert_eq!(result["exit_code"], 0);
    assert!(result["duration_ms"].is_u64());
}

#[test]
fn refused_actions_are_listed_as_permission_denials() {
    let dir = workspace("denied");
    // Reads in the folder never ask, so the refused action is a write.
    let asks_to_write = stream(&[
        json!({"type": "client_tool_request", "interruptId": "ct_1", "toolUseId": "tu_1", "toolName": "write_file", "input": {"path": "new.txt", "content": "x"}}),
        json!({"type": "run_status", "status": "interrupted", "reason": "client_tools"}),
    ]);
    let stub = Stub::serve(vec![
        ("GET /chat/capabilities", capabilities(false)),
        ("GET /chat/runs/r1/events", asks_to_write),
        ("GET /chat/runs/r2/events", answers()),
    ]);
    let out = gz_code(
        &dir,
        &stub,
        &["-p", "Summarize", "--output-format", "json"],
        "",
    );
    let result = json_out(&out);
    assert_eq!(out.status.code(), Some(EXIT_NEEDED_APPROVAL), "{result}");
    let denial = &result["permission_denials"][0];
    assert_eq!(denial["tool"], "write_file");
    assert!(!dir.join("ws/new.txt").exists(), "nothing was written");
    assert!(denial["reason"]
        .as_str()
        .unwrap()
        .contains("needs the user's approval"));
    assert_eq!(result["subtype"], "success", "the run itself finished");
}

#[test]
fn stream_json_writes_one_event_per_line() {
    let dir = workspace("stream");
    let stub = reading_stub(false, None);
    let out = gz_code(
        &dir,
        &stub,
        &[
            "-p",
            "Summarize",
            "--yes-reads",
            "--output-format",
            "stream-json",
        ],
        "",
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let lines = lines_out(&out);
    let types: Vec<&str> = lines.iter().map(|l| l["type"].as_str().unwrap()).collect();
    assert_eq!(
        types,
        [
            "init",
            "assistant",
            "tool_use",
            "tool_result",
            "assistant",
            "usage",
            "result"
        ],
        "{lines:#?}"
    );
    assert_eq!(lines[0]["model"], "sonnet55");
    assert!(lines[0]["tools"]
        .as_array()
        .unwrap()
        .contains(&json!("read_file")));
    assert_eq!(lines[1]["text"], "Let me look.");
    assert_eq!(lines[2]["input"], json!({"path": "hello.txt"}));
    assert!(lines[3]["output"]
        .as_str()
        .unwrap()
        .contains("hello from file"));
    assert_eq!(lines[4]["text"], "All done.");
    assert_eq!(lines[6]["result"], "All done.");
}

#[test]
fn max_turns_stops_before_the_next_turn_without_running_tools() {
    let dir = workspace("max-turns");
    let stub = reading_stub(false, None);
    let out = gz_code(
        &dir,
        &stub,
        &[
            "-p",
            "Summarize",
            "--yes-reads",
            "--max-turns",
            "1",
            "--output-format",
            "json",
        ],
        "",
    );
    let result = json_out(&out);
    assert_eq!(out.status.code(), Some(EXIT_MAX_TURNS), "{result}");
    assert_eq!(result["subtype"], "error_max_turns");
    assert_eq!(result["is_error"], true);
    assert_eq!(result["num_turns"], 1);
    assert!(result["error"].as_str().unwrap().contains("--max-turns 1"));
    assert_eq!(result["tool_calls"], json!([]), "the read never ran");
    assert!(stub
        .requests("POST", "/chat/runs/r1/client-tools/resume")
        .is_empty());
}

#[test]
fn max_budget_stops_once_the_reported_cost_reaches_it() {
    let dir = workspace("budget");
    let stub = reading_stub(false, Some("0.020000"));
    let out = gz_code(
        &dir,
        &stub,
        &["-p", "Summarize", "--yes-reads", "--max-budget-usd", "0.01"],
        "",
    );
    assert_eq!(out.status.code(), Some(EXIT_MAX_BUDGET), "{out:?}");
    assert!(common::plain(&out.stderr).contains("--max-budget-usd 0.01"));
    assert!(stub
        .requests("POST", "/chat/runs/r1/client-tools/resume")
        .is_empty());
}

#[test]
fn tools_limits_what_the_model_is_offered() {
    let dir = workspace("tools");
    let stub = reading_stub(true, None);
    let out = gz_code(
        &dir,
        &stub,
        &[
            "-p",
            "Summarize",
            "--yes-reads",
            "--tools",
            "read_file,search_files",
        ],
        "",
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(
        stub.run_body()["clientTools"],
        json!(["read_file", "search_files"])
    );

    let dir = workspace("no-tools");
    let stub = reading_stub(true, None);
    let out = gz_code(&dir, &stub, &["-p", "Summarize", "--tools", "none"], "");
    let body = stub.run_body();
    assert_eq!(body["clientTools"], json!([]));
    assert!(body.get("clientContext").is_none(), "{body}");
    // The model asked anyway: gz refuses a tool it did not offer.
    let resume = &stub.requests("POST", "/chat/runs/r1/client-tools/resume")[0].body;
    assert_eq!(resume["toolResults"][0]["result"]["isError"], true);
    assert_eq!(out.status.code(), Some(0), "{out:?}");

    let out = gz_code(&dir, &stub, &["-p", "x", "--tools", "rm_rf"], "");
    assert_eq!(out.status.code(), Some(1));
    assert!(common::plain(&out.stderr).contains("unknown tool 'rm_rf'"));
}

#[test]
fn appended_instructions_go_out_as_the_users_own() {
    let dir = workspace("append");
    std::fs::write(dir.join("extra.md"), "Answer in French.").unwrap();
    let stub = reading_stub(true, None);
    let extra = dir.join("extra.md");
    let out = gz_code(
        &dir,
        &stub,
        &[
            "-p",
            "Summarize",
            "--yes-reads",
            "--append-system-prompt",
            "Be brief.",
            "--append-system-prompt-file",
            extra.to_str().unwrap(),
        ],
        "",
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let instructions = &stub.run_body()["clientContext"]["instructions"];
    assert_eq!(
        instructions[0],
        json!({"source": "user", "path": "--append-system-prompt", "text": "Be brief."})
    );
    assert_eq!(instructions[1]["source"], "user");
    assert_eq!(instructions[1]["text"], "Answer in French.");

    let dir = workspace("append-old-server");
    let stub = reading_stub(false, None);
    let out = gz_code(
        &dir,
        &stub,
        &["-p", "x", "--append-system-prompt", "Be brief."],
        "",
    );
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(common::plain(&out.stderr).contains("does not take local context"));
    assert!(stub.requests("POST", "/chat/runs").is_empty());
}

#[test]
fn add_dir_opens_another_folder() {
    let dir = workspace("add-dir");
    std::fs::create_dir_all(dir.join("other")).unwrap();
    let stub = reading_stub(true, None);
    let other = dir.join("other");
    let out = gz_code(
        &dir,
        &stub,
        &[
            "-p",
            "Summarize",
            "--yes-reads",
            "--add-dir",
            other.to_str().unwrap(),
        ],
        "",
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let roots = stub.run_body()["clientContext"]["roots"].to_string();
    assert!(roots.contains("other"), "{roots}");

    let out = gz_code(
        &dir,
        &stub,
        &["-p", "x", "--add-dir", "/no/such/folder"],
        "",
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(common::plain(&out.stderr).contains("--add-dir /no/such/folder"));
}

#[test]
fn bare_runs_send_no_instruction_files_or_personal_tools() {
    let dir = workspace("bare");
    std::fs::write(dir.join("ws/GZ.md"), "Project rules.").unwrap();
    let stub = reading_stub(true, None);
    let out = gz_code(&dir, &stub, &["-p", "Summarize", "--yes-reads"], "");
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(stub.run_body()["clientContext"]["instructions"]
        .to_string()
        .contains("Project rules."));

    let dir = workspace("bare-on");
    std::fs::write(dir.join("ws/GZ.md"), "Project rules.").unwrap();
    let stub = reading_stub(true, None);
    let out = gz_code(
        &dir,
        &stub,
        &["-p", "Summarize", "--yes-reads", "--bare"],
        "",
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let body = stub.run_body();
    assert!(
        body["clientContext"].get("instructions").is_none(),
        "{body}"
    );
    let tools = body["clientTools"].as_array().unwrap();
    for left_out in ["memory", "load_skill", "local_mcp_tools", "local_mcp_call"] {
        assert!(!tools.contains(&json!(left_out)), "{left_out}: {tools:?}");
    }
}

#[test]
fn stream_json_input_runs_one_turn_per_line_in_one_conversation() {
    let dir = workspace("input-stream");
    let stub = Stub::serve(vec![
        ("GET /chat/capabilities", capabilities(false)),
        ("GET /chat/runs/r1/events", answers()),
    ]);
    let input = concat!(
        r#"{"type":"user","message":{"role":"user","content":"first"}}"#,
        "\n\n",
        r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"second"}]}}"#,
        "\n"
    );
    let out = gz_code(
        &dir,
        &stub,
        &[
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
        ],
        input,
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let runs = stub.requests("POST", "/chat/runs");
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].body["userInput"], "first");
    assert_eq!(runs[1].body["userInput"], "second");
    assert_eq!(
        runs[0].body["conversationId"],
        runs[1].body["conversationId"]
    );
    let results: Vec<Value> = lines_out(&out)
        .into_iter()
        .filter(|l| l["type"] == "result")
        .collect();
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|r| r["subtype"] == "success"));
}

#[test]
fn a_bad_input_line_ends_with_an_error_result() {
    let dir = workspace("input-bad");
    let stub = Stub::serve(vec![("GET /chat/capabilities", capabilities(false))]);
    let out = gz_code(
        &dir,
        &stub,
        &[
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
        ],
        "not json\n",
    );
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let last = lines_out(&out).pop().unwrap();
    assert_eq!(last["type"], "result");
    assert_eq!(last["subtype"], "error");
    assert!(last["error"]
        .as_str()
        .unwrap()
        .contains("standard input line 1"));
    assert!(stub.requests("POST", "/chat/runs").is_empty());
}

#[test]
fn conflicting_or_misplaced_flags_fail_before_anything_is_sent() {
    let dir = workspace("conflicts");
    let stub = reading_stub(false, None);
    for (args, says) in [
        (
            vec!["x", "-o", "json", "--output-format", "json"],
            "not both",
        ),
        (vec!["--max-turns", "2"], "one-shot"),
        (
            vec!["--input-format", "stream-json"],
            "--output-format stream-json",
        ),
        (vec!["-p", "x", "--max-turns", "0"], "--max-turns"),
    ] {
        let out = gz_code(&dir, &stub, &args, "");
        assert_ne!(out.status.code(), Some(0), "{args:?}");
        // With -o json the error itself is printed as JSON on stdout.
        let printed = format!(
            "{}{}",
            common::plain(&out.stderr),
            String::from_utf8_lossy(&out.stdout)
        );
        assert!(printed.contains(says), "{args:?}: {printed}");
    }
    assert!(stub.requests("POST", "/chat/runs").is_empty());
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.email=t@example.com", "-c", "user.name=t"])
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

#[test]
fn worktree_runs_the_session_in_its_own_checkout() {
    let dir = workspace("worktree");
    let ws = dir.join("ws");
    git(&ws, &["init", "-q", "-b", "main"]);
    git(&ws, &["add", "-A"]);
    git(&ws, &["commit", "-q", "-m", "first"]);
    let stub = reading_stub(false, None);
    let out = gz_code(
        &dir,
        &stub,
        &[
            "-p",
            "Summarize",
            "--yes-reads",
            "--worktree",
            "feat",
            "--output-format",
            "json",
        ],
        "",
    );
    let result = json_out(&out);
    assert_eq!(out.status.code(), Some(0), "{result}");
    assert_eq!(stub.run_body()["clientWorkspace"]["name"], "feat");
    assert!(ws.join(".gz/worktrees/feat/hello.txt").is_file());
    let stderr = common::plain(&out.stderr);
    assert!(stderr.contains("worktree remove /"), "{stderr}");
    assert!(stderr.contains(".gz/worktrees/feat && git -C"), "{stderr}");

    let out = gz_code(
        &dir,
        &stub,
        &["-q", "-p", "Summarize", "--yes-reads", "--worktree", "feat"],
        "",
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(
        !common::plain(&out.stderr).contains("Worktree"),
        "-q leaves out the worktree notes"
    );

    let outside = workspace("worktree-outside");
    let out = gz_code(&outside, &stub, &["-p", "x", "--worktree"], "");
    assert_eq!(out.status.code(), Some(1));
    assert!(common::plain(&out.stderr).contains("needs a git repository"));
}

#[cfg(unix)]
#[test]
fn ctrl_c_while_waiting_for_the_next_message_ends_the_stream() {
    use std::io::BufRead;
    let dir = workspace("input-interrupt");
    let stub = Stub::serve(vec![
        ("GET /chat/capabilities", capabilities(false)),
        ("GET /chat/runs/r1/events", answers()),
    ]);
    let mut child = Command::new(env!("CARGO_BIN_EXE_gz"))
        .current_dir(dir.join("ws"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("GZ_PROFILE", "headless-test")
        .env("GZ_HOST", &stub.base)
        .env("GZ_TOKEN", "stub-token")
        .env("GZ_SITE", "acme")
        .args([
            "code",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // stdin stays open: gz is left waiting for a second message.
    let mut stdin = child.stdin.take().unwrap();
    writeln!(
        stdin,
        r#"{{"type":"user","message":{{"role":"user","content":"first"}}}}"#
    )
    .unwrap();
    let mut lines = std::io::BufReader::new(child.stdout.take().unwrap()).lines();
    let first: Value = loop {
        let line = lines.next().expect("a result line").unwrap();
        let event: Value = serde_json::from_str(&line).unwrap();
        if event["type"] == "result" {
            break event;
        }
    };
    assert_eq!(first["subtype"], "success");
    // Let gz go back to waiting on standard input.
    std::thread::sleep(Duration::from_millis(300));
    let killed = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() > Duration::from_secs(15) {
            child.kill().ok();
            panic!("gz code kept waiting after Ctrl+C");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    drop(stdin);
    assert_eq!(status.code(), Some(130));
    let last: Value =
        serde_json::from_str(&lines.last().expect("a closing result").unwrap()).unwrap();
    assert_eq!(last["type"], "result");
    assert_eq!(last["subtype"], "interrupted");
    assert_eq!(last["exit_code"], 130);
}

#[test]
fn more_appended_instructions_than_the_chat_takes_are_refused() {
    let dir = workspace("append-too-many");
    let stub = reading_stub(true, None);
    let mut args = vec!["-p".to_string(), "x".to_string()];
    for i in 1..=9 {
        let file = dir.join(format!("r{i}.md"));
        std::fs::write(&file, format!("rule {i}")).unwrap();
        args.push("--append-system-prompt-file".into());
        args.push(file.display().to_string());
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = gz_code(&dir, &stub, &args, "");
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let stderr = common::plain(&out.stderr);
    assert!(
        stderr.contains("9 appended") && stderr.contains("at most 8"),
        "{stderr}"
    );
    assert!(stub.requests("POST", "/chat/runs").is_empty());

    // The server's own, lower limit counts too.
    let low = capabilities(true).replace(r#""instructionFiles":8"#, r#""instructionFiles":1"#);
    let stub = Stub::serve(vec![("GET /chat/capabilities", low)]);
    let out = gz_code(
        &dir,
        &stub,
        &[
            "-p",
            "x",
            "--append-system-prompt",
            "a",
            "--append-system-prompt-file",
            dir.join("r1.md").to_str().unwrap(),
            "--output-format",
            "json",
        ],
        "",
    );
    let result = json_out(&out);
    assert_eq!(out.status.code(), Some(1), "{result}");
    assert!(
        result["error"].as_str().unwrap().contains("at most 1"),
        "{result}"
    );
    assert_eq!(
        result["model"], "sonnet55",
        "the model it would have run on"
    );
    assert!(stub.requests("POST", "/chat/runs").is_empty());
}

#[test]
fn early_errors_come_out_in_the_result_schema() {
    let dir = workspace("early-errors");
    let stub = reading_stub(true, None);
    let out = gz_code(
        &dir,
        &stub,
        &[
            "-p",
            "x",
            "--add-dir",
            "/no/such/folder",
            "--output-format",
            "json",
        ],
        "",
    );
    let result = json_out(&out);
    assert_eq!(out.status.code(), Some(1), "{result}");
    assert_eq!(result["type"], "result");
    assert_eq!(result["subtype"], "error");
    assert!(result["error"]
        .as_str()
        .unwrap()
        .contains("--add-dir /no/such/folder"));

    let out = gz_code(&dir, &stub, &["-p", "--output-format", "stream-json"], "\n");
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let last = lines_out(&out).pop().expect("a result line");
    assert_eq!(last["type"], "result");
    assert!(last["error"]
        .as_str()
        .unwrap()
        .contains("standard input was empty"));

    let out = gz_code(
        &dir,
        &stub,
        &["-p", "@missing.txt", "--output-format", "json"],
        "",
    );
    let result = json_out(&out);
    assert_eq!(out.status.code(), Some(1), "{result}");
    assert_eq!(result["model"], "sonnet55");
    assert!(stub.requests("POST", "/chat/runs").is_empty());
}

#[test]
fn worktree_add_dir_is_read_from_where_gz_started() {
    let dir = workspace("worktree-add-dir");
    let ws = dir.join("ws");
    git(&ws, &["init", "-q", "-b", "main"]);
    git(&ws, &["add", "-A"]);
    git(&ws, &["commit", "-q", "-m", "first"]);
    std::fs::create_dir_all(dir.join("other")).unwrap();
    let stub = reading_stub(true, None);
    let out = gz_code(
        &dir,
        &stub,
        &[
            "-p",
            "Summarize",
            "--yes-reads",
            "--worktree",
            "w1",
            "--add-dir",
            "../other",
            "--output-format",
            "json",
        ],
        "",
    );
    let result = json_out(&out);
    assert_eq!(out.status.code(), Some(0), "{result}");
    let roots = stub.run_body()["clientContext"]["roots"].to_string();
    assert!(roots.contains("other"), "{roots}");

    let out = gz_code(
        &dir,
        &stub,
        &["-p", "x", "--worktree", "w2", "--add-dir", "../missing"],
        "",
    );
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(!ws.join(".gz/worktrees/w2").exists(), "no worktree made");
}

#[test]
fn quiet_runs_print_no_worktree_notes() {
    let dir = workspace("worktree-quiet");
    let ws = dir.join("ws");
    git(&ws, &["init", "-q", "-b", "main"]);
    git(&ws, &["add", "-A"]);
    git(&ws, &["commit", "-q", "-m", "first"]);
    let stub = reading_stub(false, None);
    let out = gz_code(
        &dir,
        &stub,
        &["-q", "-p", "Summarize", "--yes-reads", "--worktree", "q"],
        "",
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let stderr = common::plain(&out.stderr);
    assert!(!stderr.contains("worktree"), "{stderr}");
}

#[test]
fn early_errors_carry_exit_code_and_no_conversation() {
    let dir = workspace("early-errors-local");
    let stub = reading_stub(false, None);
    let missing = dir.join("no-such-folder");
    let out = gz_code(
        &dir,
        &stub,
        &[
            "-p",
            "hi",
            "--add-dir",
            missing.to_str().unwrap(),
            "--output-format",
            "json",
        ],
        "",
    );
    let result = json_out(&out);
    assert_eq!(out.status.code(), Some(1), "{result}");
    assert_eq!(result["type"], "result");
    assert_eq!(result["subtype"], "error");
    assert_eq!(result["is_error"], true);
    assert_eq!(result["exit_code"], 1);
    assert_eq!(result["conversation_id"], Value::Null, "no run started");
    assert!(
        result["error"].as_str().unwrap().contains("--add-dir"),
        "{result}"
    );

    let out = gz_code(
        &dir,
        &stub,
        &["-p", "--output-format", "stream-json"],
        "  \n",
    );
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let last = lines_out(&out).pop().expect("a result line");
    assert_eq!(last["type"], "result");
    assert_eq!(last["subtype"], "error");
    assert!(
        last["error"]
            .as_str()
            .unwrap()
            .contains("standard input was empty"),
        "{last}"
    );

    let out = gz_code(
        &dir,
        &stub,
        &["-p", "@missing-prompt.txt", "--output-format", "json"],
        "",
    );
    let result = json_out(&out);
    assert_eq!(out.status.code(), Some(1), "{result}");
    assert_eq!(result["model"], "sonnet55", "the model list was loaded");
    assert!(result["conversation_id"].is_string());

    let out = gz_code(
        &dir,
        &stub,
        &[
            "-p",
            "hi",
            "--append-system-prompt",
            "Be brief.",
            "--output-format",
            "json",
        ],
        "",
    );
    let result = json_out(&out);
    assert_eq!(out.status.code(), Some(1), "{result}");
    assert!(result["error"]
        .as_str()
        .unwrap()
        .contains("does not take local context"));
    assert!(stub.requests("POST", "/chat/runs").is_empty());
}

#[test]
fn appended_instructions_beyond_the_servers_limits_are_refused() {
    let dir = workspace("append-limits");
    let mut args = vec!["-p".to_string(), "hi".to_string()];
    for i in 0..9 {
        let file = dir.join(format!("r{i}.md"));
        std::fs::write(&file, format!("rule {i}")).unwrap();
        args.push("--append-system-prompt-file".into());
        args.push(file.display().to_string());
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let stub = reading_stub(true, None);
    let out = gz_code(&dir, &stub, &args, "");
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let stderr = common::plain(&out.stderr);
    assert!(
        stderr.contains("9 appended instruction entries") && stderr.contains("at most 8"),
        "{stderr}"
    );
    assert!(stub.requests("POST", "/chat/runs").is_empty());

    // A server that takes fewer entries is held to its own limit.
    let fewer = capabilities(true).replace(r#""instructionFiles":8"#, r#""instructionFiles":2"#);
    let stub = Stub::serve(vec![("GET /chat/capabilities", fewer)]);
    let out = gz_code(&dir, &stub, &args[..8], "");
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let stderr = common::plain(&out.stderr);
    assert!(
        stderr.contains("3 appended instruction entries") && stderr.contains("at most 2"),
        "{stderr}"
    );
    assert!(stub.requests("POST", "/chat/runs").is_empty());
}

#[test]
fn a_relative_add_dir_is_read_from_where_gz_started() {
    let dir = workspace("worktree-add-dir");
    let ws = dir.join("ws");
    std::fs::create_dir_all(dir.join("other")).unwrap();
    git(&ws, &["init", "-q", "-b", "main"]);
    git(&ws, &["add", "-A"]);
    git(&ws, &["commit", "-q", "-m", "first"]);
    let stub = reading_stub(true, None);
    let out = gz_code(
        &dir,
        &stub,
        &[
            "-p",
            "Summarize",
            "--yes-reads",
            "--worktree",
            "w1",
            "--add-dir",
            "../other",
            "--output-format",
            "json",
        ],
        "",
    );
    let result = json_out(&out);
    assert_eq!(out.status.code(), Some(0), "{result}");
    let other = std::fs::canonicalize(dir.join("other")).unwrap();
    let roots = stub.run_body()["clientContext"]["roots"].clone();
    assert!(
        roots
            .as_array()
            .unwrap()
            .iter()
            .any(|root| root.as_str() == Some(other.to_str().unwrap())),
        "{roots}"
    );

    let out = gz_code(
        &dir,
        &stub,
        &["-p", "x", "--worktree", "w2", "--add-dir", "../missing"],
        "",
    );
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(common::plain(&out.stderr).contains("--add-dir ../missing"));
    assert!(
        !ws.join(".gz/worktrees/w2").exists(),
        "no worktree is made for a run that can't start"
    );
}

/// Tokens one run leg used (`llm_metrics`, sent as each leg's model work
/// ends; the cost only comes with the turn's last leg).
fn leg_tokens() -> Value {
    json!({"type": "llm_metrics", "source": "llm_direct", "metrics": {"input_tokens": 100, "output_tokens": 20, "usage_available": true}})
}

/// Two legs: the first pauses for a read, the second answers; only the
/// second reports a cost, and that cost covers itself alone.
fn two_leg_stub() -> Stub {
    let first = stream(&[
        json!({"type": "llm_chunk", "text": "Let me look."}),
        leg_tokens(),
        json!({"type": "client_tool_request", "interruptId": "ct_1", "toolUseId": "tu_1", "toolName": "read_file", "input": {"path": "hello.txt"}}),
        json!({"type": "run_status", "status": "interrupted", "reason": "client_tools"}),
    ]);
    let last = stream(&[
        json!({"type": "llm_chunk", "text": "All done."}),
        leg_tokens(),
        metrics("0.004200"),
        json!({"type": "run_status", "status": "completed"}),
    ]);
    Stub::serve(vec![
        ("GET /chat/capabilities", capabilities(false)),
        ("GET /chat/runs/r1/events", first),
        ("GET /chat/runs/r2/events", last),
    ])
}

#[test]
fn json_reports_the_cost_and_tokens_of_every_leg_of_the_turn() {
    let dir = workspace("multi-leg-json");
    let stub = two_leg_stub();
    let out = gz_code(
        &dir,
        &stub,
        &["-p", "Summarize", "--yes-reads", "--output-format", "json"],
        "",
    );
    let result = json_out(&out);
    assert_eq!(out.status.code(), Some(0), "{result}");
    // The first leg is priced at the rate the last one reported.
    assert_eq!(result["cost_usd"], 0.0084, "{result}");
    // The first leg's share is gz's estimate; the server priced the last.
    assert_eq!(result["estimated_cost_usd"], 0.0042, "{result}");
    assert_eq!(result["unpriced_legs"], 0, "{result}");
    assert_eq!(result["usage"]["input_tokens"], 200);
    assert_eq!(result["usage"]["output_tokens"], 40);
}

#[test]
fn stream_json_usage_events_carry_the_whole_turns_cost() {
    let dir = workspace("multi-leg-stream");
    let stub = two_leg_stub();
    let out = gz_code(
        &dir,
        &stub,
        &[
            "-p",
            "Summarize",
            "--yes-reads",
            "--output-format",
            "stream-json",
        ],
        "",
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let lines = lines_out(&out);
    let usage = lines.iter().find(|l| l["type"] == "usage").unwrap();
    assert_eq!(usage["cost_usd"], 0.0084, "{usage}");
    assert_eq!(usage["estimated_cost_usd"], 0.0042, "{usage}");
    assert_eq!(usage["unpriced_legs"], 0, "{usage}");
    assert_eq!(usage["input_tokens"], 200);
    let result = lines.last().unwrap();
    assert_eq!(result["cost_usd"], 0.0084, "{result}");
}

#[test]
fn max_budget_counts_every_leg_of_a_turn() {
    let dir = workspace("multi-leg-budget");
    let stub = two_leg_stub();
    let input = concat!(
        r#"{"type":"user","message":{"role":"user","content":"first"}}"#,
        "\n",
        r#"{"type":"user","message":{"role":"user","content":"second"}}"#,
        "\n"
    );
    // The last leg alone (0.0042) is under the budget; the turn (0.0084)
    // is not, so the second message never starts a run.
    let out = gz_code(
        &dir,
        &stub,
        &[
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--yes-reads",
            "--max-budget-usd",
            "0.006",
        ],
        input,
    );
    assert_eq!(out.status.code(), Some(EXIT_MAX_BUDGET), "{out:?}");
    assert_eq!(stub.requests("POST", "/chat/runs").len(), 1);
}

#[test]
fn the_result_names_the_model_a_command_file_ran_the_message_on() {
    let dir = workspace("command-model");
    let commands = dir.join("ws/.gz/commands");
    std::fs::create_dir_all(&commands).unwrap();
    std::fs::write(
        commands.join("deep.md"),
        "---\nmodel: opus5\n---\nThink hard about $ARGUMENTS\n",
    )
    .unwrap();
    let mut caps: Value = serde_json::from_str(&capabilities(false)).unwrap();
    caps["groundzeroModes"] = json!([
        {"value": "sonnet55", "label": "Claude Sonnet 5.5", "reasoningEfforts": []},
        {"value": "opus5", "label": "Claude Opus 5", "reasoningEfforts": []},
    ]);
    let stub = Stub::serve(vec![
        ("GET /chat/capabilities", caps.to_string()),
        ("GET /chat/runs/r1/events", answers()),
    ]);
    let out = gz_code(
        &dir,
        &stub,
        &[
            "-p",
            "/deep the parser",
            "--model",
            "sonnet55",
            "--output-format",
            "json",
        ],
        "",
    );
    let result = json_out(&out);
    assert_eq!(out.status.code(), Some(0), "{result}");
    assert_eq!(
        stub.run_body()["groundzeroMode"],
        "opus5",
        "{}",
        stub.run_body()
    );
    assert_eq!(result["model"], "opus5");
}

#[test]
fn stream_json_server_tool_use_carries_its_input_summary() {
    let dir = workspace("server-tool");
    let events = stream(&[
        json!({"type": "tool_use", "source": "agent", "toolName": "Query lakehouse", "toolUseId": "t1", "input": {"sql": "select 1"}}),
        json!({"type": "tool_result", "source": "agent", "toolUseId": "t1", "content": [{"text": "1 row"}], "status": "success"}),
        json!({"type": "llm_chunk", "text": "One row."}),
        json!({"type": "run_status", "status": "completed"}),
    ]);
    let stub = Stub::serve(vec![
        ("GET /chat/capabilities", capabilities(false)),
        ("GET /chat/runs/r1/events", events),
    ]);
    let out = gz_code(
        &dir,
        &stub,
        &["-p", "Count", "--output-format", "stream-json"],
        "",
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let lines = lines_out(&out);
    let used = lines.iter().find(|l| l["type"] == "tool_use").unwrap();
    assert_eq!(used["tool"], "Query lakehouse");
    assert_eq!(used["local"], false);
    assert_eq!(used["summary"], "Query lakehouse select 1", "{used}");
    let result = lines.iter().find(|l| l["type"] == "tool_result").unwrap();
    assert_eq!(result["summary"], used["summary"]);
}

#[test]
fn include_partial_messages_streams_text_as_it_arrives() {
    let dir = workspace("partial");
    let events = stream(&[
        json!({"type": "llm_chunk", "text": "All "}),
        json!({"type": "llm_chunk", "text": "done."}),
        metrics("0.004200"),
        json!({"type": "run_status", "status": "completed"}),
    ]);
    let stub = Stub::serve(vec![
        ("GET /chat/capabilities", capabilities(false)),
        ("GET /chat/runs/r1/events", events),
    ]);
    let out = gz_code(
        &dir,
        &stub,
        &[
            "-p",
            "Go",
            "--output-format",
            "stream-json",
            "--include-partial-messages",
        ],
        "",
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let lines = lines_out(&out);
    let partial: Vec<&Value> = lines
        .iter()
        .filter(|l| l["type"] == "partial_assistant")
        .map(|l| &l["text"])
        .collect();
    assert_eq!(partial, [&json!("All "), &json!("done.")]);
    let whole = lines.iter().find(|l| l["type"] == "assistant").unwrap();
    assert_eq!(whole["text"], "All done.");
}
