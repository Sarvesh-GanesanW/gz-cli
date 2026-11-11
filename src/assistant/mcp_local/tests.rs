//! The MCP client against a real stdio server: a small Python script
//! written into a temp folder. Skipped, with a note, without `python3`.

use super::*;
use std::path::PathBuf;

/// A stdio MCP server with tools that echo, sleep, crash, read an env var
/// and return an image. It logs to stderr, prints one stray non-JSON line
/// to stdout, splits its tool list over two pages, and counts its starts
/// in `starts.txt` next to itself.
const FAKE_SERVER: &str = r#"
import json, os, sys, time

here = os.path.dirname(os.path.abspath(__file__))
with open(os.path.join(here, "starts.txt"), "a") as starts:
    starts.write("start\n")
print("fake server starting", file=sys.stderr, flush=True)

def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()

def result(mid, value):
    send({"jsonrpc": "2.0", "id": mid, "result": value})

def text(mid, value):
    result(mid, {"content": [{"type": "text", "text": value}]})

OBJECT = {"type": "object"}
PAGE_ONE = [{"name": "echo", "description": "Echo the text back",
             "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}},
                             "required": ["text"]}}]
PAGE_TWO = [{"name": "sleep", "description": "Wait", "inputSchema": OBJECT},
            {"name": "crash", "description": "Exit at once", "inputSchema": OBJECT},
            {"name": "env", "description": "Read an env var", "inputSchema": OBJECT},
            {"name": "picture", "description": "A chart", "inputSchema": OBJECT}]

for line in sys.stdin:
    if not line.strip():
        continue
    message = json.loads(line)
    method, mid = message.get("method"), message.get("id")
    params = message.get("params") or {}
    if method == "initialize":
        print("a stray log line on stdout", flush=True)
        result(mid, {"protocolVersion": params["protocolVersion"],
                     "capabilities": {"tools": {}},
                     "serverInfo": {"name": "fake", "version": "1"}})
    elif method == "tools/list":
        if params.get("cursor") == "page-2":
            result(mid, {"tools": PAGE_TWO})
        else:
            result(mid, {"tools": PAGE_ONE, "nextCursor": "page-2"})
    elif method == "tools/call":
        name, args = params["name"], params.get("arguments") or {}
        if name == "echo":
            text(mid, args.get("text", ""))
        elif name == "sleep":
            time.sleep(float(args.get("seconds", 3)))
            text(mid, "slept")
        elif name == "crash":
            print("fake server crashing on purpose", file=sys.stderr, flush=True)
            sys.exit(3)
        elif name == "env":
            text(mid, os.environ.get(args["name"], "<unset>") + " " + " ".join(sys.argv[1:]))
        elif name == "picture":
            result(mid, {"content": [{"type": "text", "text": "Sales by month"},
                                     {"type": "image", "mimeType": "image/png",
                                      "data": "QUJD" * 4096}]})
        else:
            send({"jsonrpc": "2.0", "id": mid,
                  "error": {"code": -32602, "message": "Unknown tool: " + name}})
    elif mid is not None:
        send({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "no"}})
"#;

/// `python3` from PATH, or `None` (the caller skips its test).
pub(crate) fn python() -> Option<PathBuf> {
    let found = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join("python3"))
            .find(|candidate| candidate.is_file())
    });
    if found.is_none() {
        eprintln!("skipped: python3 is not on PATH, so the fake MCP server can't run");
    }
    found
}

/// A fresh folder with the fake server and an `mcp.toml` naming it as
/// `fake`, plus `extra` config lines for that server. Returns the folder
/// and the config file.
pub(crate) fn fake_config(name: &str, python: &Path, extra: &str) -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!("gz-mcp-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("fake_server.py");
    std::fs::write(&script, FAKE_SERVER).unwrap();
    let config = dir.join("mcp.toml");
    std::fs::write(
        &config,
        format!(
            "[servers.fake]\ncommand = {:?}\nargs = [{:?}]\n{extra}\n",
            python.display().to_string(),
            script.display().to_string()
        ),
    )
    .unwrap();
    (dir, config)
}

fn starts(dir: &Path) -> usize {
    std::fs::read_to_string(dir.join("starts.txt"))
        .map(|text| text.lines().count())
        .unwrap_or(0)
}

async fn phase(mcp: &LocalMcp) -> Phase {
    mcp.statuses().await.remove(0).phase
}

#[tokio::test]
async fn lists_every_page_of_tools_and_starts_only_when_used() {
    let Some(python) = python() else { return };
    let (dir, config) = fake_config("list", &python, "");
    let mcp = LocalMcp::from_file(&config);
    assert!(mcp.has_servers() && mcp.config_error().is_none());
    assert_eq!(phase(&mcp).await, Phase::NotStarted);
    assert_eq!(starts(&dir), 0, "configured servers don't start until used");

    let out = mcp.list_tools(None).await;
    assert!(!out.is_error, "{}", out.output);
    let listing: Value = serde_json::from_str(&out.output).unwrap();
    let server = &listing["servers"][0];
    assert_eq!(server["server"], "fake");
    let names: Vec<&str> = server["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["echo", "sleep", "crash", "env", "picture"]);
    assert_eq!(server["tools"][0]["description"], "Echo the text back");
    assert_eq!(
        server["tools"][0]["inputSchema"]["required"],
        json!(["text"])
    );
    assert_eq!(phase(&mcp).await, Phase::Running { tools: 5 });

    // The server's stderr went to its log, not the screen.
    let log = dir.join("mcp-logs/fake.log");
    let mut logged = String::new();
    for _ in 0..50 {
        logged = std::fs::read_to_string(&log).unwrap_or_default();
        if logged.contains("fake server starting") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    assert!(logged.contains("fake server starting"), "{logged:?}");
}

#[tokio::test]
async fn calls_a_tool_and_returns_its_text() {
    let Some(python) = python() else { return };
    let (_dir, config) = fake_config("call", &python, "");
    let mcp = LocalMcp::from_file(&config);
    let out = mcp.call("fake", "echo", json!({"text": "hello"})).await;
    assert_eq!((out.output.as_str(), out.is_error), ("hello", false));

    let unknown = mcp.call("fake", "nope", json!({})).await;
    assert!(unknown.is_error);
    assert!(
        unknown.output.contains("Unknown tool: nope"),
        "{}",
        unknown.output
    );
    let missing = mcp.call("other", "echo", json!({})).await;
    assert!(missing
        .output
        .contains("no local MCP server 'other'; configured: fake"));
}

#[tokio::test]
async fn non_text_content_is_described_in_one_line_not_dumped() {
    let Some(python) = python() else { return };
    let (_dir, config) = fake_config("picture", &python, "");
    let mcp = LocalMcp::from_file(&config);
    let out = mcp.call("fake", "picture", json!({})).await;
    assert_eq!(
        out.output,
        "Sales by month\n[image: image/png, 12 KB, not shown]"
    );
}

#[tokio::test]
async fn a_slow_call_times_out_and_the_server_keeps_working() {
    let Some(python) = python() else { return };
    let (_dir, config) = fake_config("timeout", &python, "timeout_seconds = 2");
    let mcp = LocalMcp::from_file(&config);
    let started = std::time::Instant::now();
    let out = mcp.call("fake", "sleep", json!({"seconds": 2.6})).await;
    assert!(out.is_error);
    assert!(out.output.contains("no answer within 2s"), "{}", out.output);
    assert!(started.elapsed() < Duration::from_millis(2500));
    // The late answer to the timed-out call is skipped, not taken for this one.
    let next = mcp
        .call("fake", "echo", json!({"text": "still here"}))
        .await;
    assert_eq!(next.output, "still here");
    assert_eq!(phase(&mcp).await, Phase::Running { tools: 5 });
}

#[tokio::test]
async fn a_crash_restarts_once_then_the_server_is_failed_until_restarted() {
    let Some(python) = python() else { return };
    let (dir, config) = fake_config("crash", &python, "");
    let mcp = LocalMcp::from_file(&config);

    let crashed = mcp.call("fake", "crash", json!({})).await;
    assert!(crashed.is_error);
    assert!(
        crashed.output.contains("exited") && crashed.output.contains("crashing on purpose"),
        "the reason carries the exit and the last stderr line: {}",
        crashed.output
    );
    assert!(matches!(phase(&mcp).await, Phase::Crashed(_)));

    let again = mcp.call("fake", "echo", json!({"text": "back"})).await;
    assert_eq!(again.output, "back", "restarted on the next use");
    assert_eq!(starts(&dir), 2);

    mcp.call("fake", "crash", json!({})).await;
    let failed = mcp.call("fake", "echo", json!({"text": "x"})).await;
    assert!(failed.is_error);
    assert!(
        failed.output.contains("crashed again after a restart")
            && failed.output.contains("/mcp restart fake"),
        "{}",
        failed.output
    );
    assert_eq!(starts(&dir), 2, "no second automatic restart");
    assert!(matches!(phase(&mcp).await, Phase::Failed(_)));

    assert_eq!(mcp.restart("fake").await.unwrap(), 5);
    assert_eq!(starts(&dir), 3);
    let restored = mcp.call("fake", "echo", json!({"text": "ok"})).await;
    assert_eq!(restored.output, "ok");
}

#[tokio::test]
async fn env_and_args_take_vars_from_gzs_environment() {
    let Some(python) = python() else { return };
    let var = format!("GZ_MCP_TEST_SECRET_{}", std::process::id());
    std::env::set_var(&var, "s3cret-value");
    let extra = format!("env = {{ FAKE_SECRET = \"${{{var}}}\" }}");
    let (dir, config) = fake_config("vars", &python, &extra);
    // The script also gets an argument filled from the environment.
    let text = std::fs::read_to_string(&config).unwrap().replace(
        "fake_server.py\"]",
        &format!("fake_server.py\", \"--key=${{{var}}}\"]"),
    );
    std::fs::write(&config, text).unwrap();
    let mcp = LocalMcp::from_file(&config);

    let out = mcp
        .call("fake", "env", json!({"name": "FAKE_SECRET"}))
        .await;
    assert_eq!(out.output, "s3cret-value --key=s3cret-value");
    let status = &mcp.statuses().await[0];
    assert!(
        status.command.contains("FAKE_SECRET=****") && !status.command.contains("s3cret"),
        "listing masks env values and never expands: {}",
        status.command
    );

    let missing = fake_config(
        "vars-missing",
        &python,
        "env = { K = \"${GZ_MCP_TEST_UNSET_VAR}\" }",
    );
    let mcp = LocalMcp::from_file(&missing.1);
    let out = mcp.call("fake", "echo", json!({"text": "x"})).await;
    assert!(
        out.is_error && out.output.contains("${GZ_MCP_TEST_UNSET_VAR} is not set"),
        "{}",
        out.output
    );
    assert_eq!(starts(&missing.0), 0);
    let _ = dir;
}

#[test]
fn a_bad_config_names_the_file_and_leaves_no_servers() {
    let dir = std::env::temp_dir().join(format!("gz-mcp-bad-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("mcp.toml");
    std::fs::write(&config, "[servers.files]\nargs = []\n").unwrap();
    let mcp = LocalMcp::from_file(&config);
    let why = mcp.config_error().unwrap();
    assert!(
        why.contains(&config.display().to_string()) && why.contains("missing field `command`"),
        "{why}"
    );
    assert!(!mcp.has_servers());
    let none = LocalMcp::from_file(&dir.join("absent.toml"));
    assert!(none.config_error().is_none() && !none.has_servers());
}

#[test]
fn the_tools_are_offered_only_with_server_support_and_a_configured_server() {
    let dir = std::env::temp_dir().join(format!("gz-mcp-offer-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("mcp.toml");
    std::fs::write(&config, "[servers.files]\ncommand = \"srv\"\n").unwrap();
    let configured = LocalMcp::from_file(&config);
    let empty = LocalMcp::from_file(&dir.join("absent.toml"));
    let supporting: Vec<String> = ["read_file", "local_mcp_tools", "local_mcp_call"]
        .map(str::to_string)
        .to_vec();
    let older = vec!["read_file".to_string()];

    assert_eq!(
        offered(Some(&supporting), Some(&configured)),
        ["local_mcp_tools", "local_mcp_call"]
    );
    assert!(offered(Some(&older), Some(&configured)).is_empty());
    assert!(offered(None, Some(&configured)).is_empty());
    assert!(offered(Some(&supporting), Some(&empty)).is_empty());
    assert!(offered(Some(&supporting), None).is_empty());
}
