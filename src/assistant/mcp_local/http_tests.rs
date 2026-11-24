//! Remote MCP servers against a stub (`stub_http_server.py`): Streamable
//! HTTP, the older HTTP+SSE transport, headers from config with `${VAR}`
//! defaults, session expiry, `tools/list_changed`, output limits, and
//! OAuth discovery, registration, PKCE token exchange and refresh.
//! Skipped, with a note, without `python3`.

use super::config::Remote;
use super::oauth::{AuthStore, Browser, AUTH_FILE};
use super::tests::python;
use super::*;
use std::process::{Child, Command, Stdio};

const STUB: &str = include_str!("stub_http_server.py");
/// How many tools the stub lists before any change.
const STUB_TOOLS: usize = 6;

/// The stub server process, stopped when dropped.
struct Stub {
    child: Child,
    base: String,
    dir: PathBuf,
}

impl Drop for Stub {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Stub {
    fn start(name: &str) -> Option<Stub> {
        let python = python()?;
        let dir = std::env::temp_dir().join(format!("gz-mcp-http-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("stub.py");
        std::fs::write(&script, STUB).unwrap();
        let mut child = Command::new(python)
            .arg(&script)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut line = String::new();
        use std::io::BufRead;
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let base = format!("http://127.0.0.1:{}", line.trim());
        Some(Stub { child, base, dir })
    }

    /// An `mcp.toml` with these server tables; `{base}` is the stub's URL.
    fn config(&self, tables: &str) -> PathBuf {
        let path = self.dir.join("mcp.toml");
        std::fs::write(&path, tables.replace("{base}", &self.base)).unwrap();
        path
    }

    /// Changes the stub's settings (`basic`, `expires_in`).
    async fn control(&self, settings: Value) {
        reqwest::Client::new()
            .post(format!("{}/control", self.base))
            .json(&settings)
            .send()
            .await
            .unwrap();
    }

    /// Polls the stub's state until `done` holds, for up to ten seconds.
    async fn wait_for(&self, what: &str, done: impl Fn(&Value) -> bool) -> Value {
        for _ in 0..100 {
            let state = self.state().await;
            if done(&state) {
                return state;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("{what}: {}", self.state().await)
    }

    async fn state(&self) -> Value {
        reqwest::get(format!("{}/state", self.base))
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }
}

const REMOTE: &str = r#"
[servers.remote]
url = "{base}/mcp"
headers = { "X-Api-Key" = "${GZ_TEST_UNSET_KEY:-k-123}" }
timeout_seconds = 10
"#;

fn tool_names(tools: &[ToolInfo]) -> Vec<&str> {
    tools.iter().map(|t| t.name.as_str()).collect()
}

#[tokio::test]
async fn streamable_http_lists_and_calls_tools_with_the_configured_headers() {
    let Some(stub) = Stub::start("streamable") else {
        return;
    };
    let mcp = LocalMcp::from_file(&stub.config(REMOTE));
    let tools = mcp.tools("remote").await.unwrap();
    assert_eq!(
        tool_names(&tools),
        [
            "echo",
            "change",
            "expire",
            "rotate",
            "big",
            "expire_and_change"
        ]
    );
    let out = mcp
        .call("remote", "echo", json!({"text": "over http"}))
        .await;
    assert!(!out.is_error, "{}", out.output);
    assert_eq!(out.output, "over http");
    let seen = stub.state().await["seen_headers"].clone();
    let seen = seen.as_array().unwrap();
    assert!(seen.iter().all(|h| h["X-Api-Key"] == "k-123"), "{seen:?}");
    assert_eq!(
        seen.last().unwrap()["version"],
        PROTOCOL_VERSION,
        "requests after initialize carry the negotiated version"
    );
    let status = &mcp.statuses()[0];
    assert_eq!(status.transport, "http");
    assert!(matches!(status.phase, Phase::Running { .. }));
}

#[tokio::test]
async fn a_refused_header_is_reported_with_the_servers_status() {
    let Some(stub) = Stub::start("refused") else {
        return;
    };
    let mcp = LocalMcp::from_file(&stub.config(
        "[servers.remote]\nurl = \"{base}/mcp\"\nheaders = { \"X-Api-Key\" = \"wrong\" }\n",
    ));
    let err = mcp.tools("remote").await.unwrap_err();
    assert!(format!("{err:#}").contains("HTTP 403"), "{err:#}");
    assert!(matches!(mcp.statuses()[0].phase, Phase::Failed(_)));
}

#[tokio::test]
async fn a_changed_tool_list_is_fetched_again_and_server_pings_are_answered() {
    let Some(stub) = Stub::start("changed") else {
        return;
    };
    let mcp = LocalMcp::from_file(&stub.config(REMOTE));
    assert!(!tool_names(&mcp.tools("remote").await.unwrap()).contains(&"added"));
    let out = mcp.call("remote", "change", json!({})).await;
    assert_eq!(out.output, "changed", "a ping before the answer is handled");
    let Phase::Running { tools } = &mcp.statuses()[0].phase else {
        panic!("{:?}", mcp.statuses())
    };
    assert!(tools.contains(&"added".to_string()), "{tools:?}");
    stub.wait_for("the ping sent before the answer is answered", |state| {
        state["answers"]
            .as_array()
            .unwrap()
            .contains(&json!("srv-1"))
    })
    .await;
}

#[tokio::test]
async fn an_expired_session_reconnects_and_the_call_goes_through() {
    let Some(stub) = Stub::start("expired") else {
        return;
    };
    let mcp = LocalMcp::from_file(&stub.config(REMOTE));
    assert_eq!(
        mcp.call("remote", "expire", json!({})).await.output,
        "expired"
    );
    let out = mcp.call("remote", "echo", json!({"text": "again"})).await;
    assert!(!out.is_error, "{}", out.output);
    assert_eq!(out.output, "again");
}

#[tokio::test]
async fn the_older_sse_transport_lists_calls_and_follows_tool_changes() {
    let Some(stub) = Stub::start("legacy") else {
        return;
    };
    let mcp = LocalMcp::from_file(&stub.config(
        "[servers.old]\ntype = \"sse\"\nurl = \"{base}/sse\"\nheaders = { \"X-Api-Key\" = \"k-123\" }\ntimeout_seconds = 10\n",
    ));
    assert_eq!(mcp.tools("old").await.unwrap().len(), STUB_TOOLS);
    let out = mcp.call("old", "echo", json!({"text": "via sse"})).await;
    assert_eq!(out.output, "via sse");
    assert_eq!(mcp.call("old", "change", json!({})).await.output, "changed");
    assert!(tool_names(&mcp.tools("old").await.unwrap()).contains(&"added"));
    assert_eq!(mcp.statuses()[0].transport, "sse");
}

#[tokio::test]
async fn a_servers_output_limit_cuts_long_results_and_says_so() {
    let Some(stub) = Stub::start("limit") else {
        return;
    };
    let mcp = LocalMcp::from_file(&stub.config(&format!("{REMOTE}max_output_chars = 1000\n")));
    let out = mcp.call("remote", "big", json!({})).await;
    assert!(out.truncated && !out.is_error);
    assert!(out.output.chars().count() <= 1000, "{}", out.output.len());
    assert!(out.output.contains("max_output_chars"), "{}", out.output);
}

#[tokio::test]
async fn an_unreachable_server_says_it_cannot_be_reached() {
    let Some(stub) = Stub::start("unreachable") else {
        return;
    };
    // Nothing listens on port 9: the connection is refused at once.
    let mcp =
        LocalMcp::from_file(&stub.config(
            "[servers.gone]\nurl = \"http://127.0.0.1:9/mcp\"\nstartup_timeout_seconds = 5\n",
        ));
    let err = mcp.tools("gone").await.unwrap_err();
    assert!(format!("{err:#}").contains("cannot reach"), "{err:#}");
}

/// Follows the sign-in URL the way a browser would: the stub's authorize
/// endpoint redirects straight to gz's localhost callback.
struct FollowLink {
    shown: Vec<String>,
}

impl Browser for FollowLink {
    fn show(&mut self, line: &str) {
        self.shown.push(line.to_string());
    }

    fn open(&mut self, url: &str) -> bool {
        let url = url.to_string();
        tokio::spawn(async move {
            let _ = reqwest::get(url).await;
        });
        true
    }
}

#[tokio::test]
async fn oauth_discovers_registers_signs_in_and_refreshes() {
    let Some(stub) = Stub::start("oauth") else {
        return;
    };
    let config =
        stub.config("[servers.secure]\nurl = \"{base}/secure/mcp\"\ntimeout_seconds = 10\n");
    let mcp = LocalMcp::from_file(&config);
    let err = mcp.tools("secure").await.unwrap_err();
    assert!(
        format!("{err:#}").contains("gz mcp login secure"),
        "{err:#}"
    );
    assert!(matches!(mcp.statuses()[0].phase, Phase::NeedsLogin(_)));
    assert_eq!(mcp.statuses()[0].phase.label(), "needs login");

    let store = AuthStore::at(stub.dir.join(AUTH_FILE));
    let Target::Remote(remote) = mcp.entry("secure").unwrap().spec.expand(&|_| None).unwrap()
    else {
        panic!("remote expected")
    };
    let http = reqwest::Client::new();
    let mut browser = FollowLink { shown: Vec::new() };
    oauth::login(&http, &store, "secure", &remote, &mut browser)
        .await
        .unwrap();
    assert!(
        browser.shown[0].contains("/auth/authorize?") && browser.shown[0].contains("S256"),
        "the URL is printed: {:?}",
        browser.shown
    );
    let state = stub.state().await;
    assert_eq!(state["registrations"], 1);
    let exchange = &state["token_requests"][0];
    assert_eq!(exchange["grant_type"], "authorization_code");
    assert_eq!(
        exchange["resource"],
        format!("{}/secure/mcp", stub.base),
        "the token is bound to the server"
    );

    // Signed in: the next use connects without a restart command.
    assert_eq!(mcp.tools("secure").await.unwrap().len(), STUB_TOOLS);
    assert_eq!(
        mcp.call("secure", "rotate", json!({})).await.output,
        "rotated"
    );
    let out = mcp
        .call("secure", "echo", json!({"text": "refreshed"}))
        .await;
    assert_eq!(out.output, "refreshed", "a 401 is retried after a refresh");
    let saved = store.get("secure", &remote.url).unwrap().unwrap();
    assert_eq!(saved.access_token, "tok-2");
    assert_eq!(saved.refresh_token.as_deref(), Some("ref-1"), "kept");

    // Closing the session carries the sign-in, so the server accepts it.
    drop(mcp);
    stub.wait_for("the session is ended with the token", |state| {
        state["deletes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["path"] == "/secure/mcp" && d["auth"] == "Bearer tok-2")
    })
    .await;

    // A second login reuses the client gz registered.
    oauth::login(&http, &store, "secure", &remote, &mut browser)
        .await
        .unwrap();
    assert_eq!(stub.state().await["registrations"], 1);
}

#[tokio::test]
async fn an_sse_server_set_up_as_http_suggests_the_right_type() {
    let Some(stub) = Stub::start("wrong-type") else {
        return;
    };
    let mcp = LocalMcp::from_file(&stub.config("[servers.old]\nurl = \"{base}/sse\"\n"));
    let err = format!("{:#}", mcp.tools("old").await.unwrap_err());
    assert!(
        err.contains("HTTP 404") && err.contains("type = \"sse\""),
        "{err}"
    );
}

/// A sign-in for `path` on the stub, followed through its authorize link.
async fn sign_in(stub: &Stub, path: &str) -> Result<(LocalMcp, AuthStore, Remote)> {
    let config = stub.config(&format!(
        "[servers.secure]\nurl = \"{{base}}{path}\"\ntimeout_seconds = 10\n"
    ));
    let mcp = LocalMcp::from_file(&config);
    let store = AuthStore::at(stub.dir.join(AUTH_FILE));
    let Target::Remote(remote) = mcp.entry("secure")?.spec.expand(&|_| None)? else {
        panic!("remote expected")
    };
    let mut browser = FollowLink { shown: Vec::new() };
    oauth::login(
        &reqwest::Client::new(),
        &store,
        "secure",
        &remote,
        &mut browser,
    )
    .await?;
    Ok((mcp, store, remote))
}

#[tokio::test]
async fn a_server_that_never_answers_times_out_without_holding_the_server() {
    let Some(stub) = Stub::start("hang") else {
        return;
    };
    let mcp = LocalMcp::from_file(&stub.config(
        "[servers.slow]\nurl = \"{base}/hang/mcp\"\ntimeout_seconds = 2\nstartup_timeout_seconds = 3\n",
    ));
    // tools/list times out after 2 s; the cancel notification that follows
    // is never taken either, and must not be waited for without end.
    let started = std::time::Instant::now();
    let err = tokio::time::timeout(Duration::from_secs(20), mcp.tools("slow"))
        .await
        .expect("the listing gives up instead of hanging")
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("no answer within 2s"),
        "{err:#}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(12),
        "{:?}",
        started.elapsed()
    );
    assert!(
        matches!(mcp.statuses()[0].phase, Phase::Failed(_)),
        "not busy for ever: {:?}",
        mcp.statuses()
    );
    let again = tokio::time::timeout(Duration::from_secs(20), mcp.restart("slow")).await;
    assert!(matches!(again, Ok(Err(_))), "a reconnect fails in time too");
}

#[tokio::test]
async fn the_get_stream_brings_tool_changes_and_pings_and_resumes_after_a_drop() {
    let Some(stub) = Stub::start("listen") else {
        return;
    };
    let mcp = LocalMcp::from_file(
        &stub.config("[servers.live]\nurl = \"{base}/listen/mcp\"\ntimeout_seconds = 10\n"),
    );
    assert_eq!(mcp.tools("live").await.unwrap().len(), STUB_TOOLS);
    stub.wait_for(
        "the ping on the GET stream is answered and the stream resumed",
        |state| {
            state["answers"]
                .as_array()
                .unwrap()
                .contains(&json!("srv-get"))
                && state["resumed_from"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("2"))
        },
    )
    .await;
    let tools = mcp.tools("live").await.unwrap();
    assert!(tool_names(&tools).contains(&"added"), "{tools:?}");
}

#[tokio::test]
async fn a_session_forgotten_during_a_tool_refresh_reconnects_without_counting_a_crash() {
    let Some(stub) = Stub::start("expire-refresh") else {
        return;
    };
    let mcp = LocalMcp::from_file(&stub.config(REMOTE));
    // Twice: two crashes in a row would leave the server failed.
    for round in 0..2 {
        let out = mcp.call("remote", "expire_and_change", json!({})).await;
        assert_eq!(out.output, "expired and changed", "round {round}");
        assert!(
            !matches!(
                mcp.statuses()[0].phase,
                Phase::Crashed(_) | Phase::Failed(_)
            ),
            "round {round}: {:?}",
            mcp.statuses()
        );
        let tools = mcp.tools("remote").await.unwrap();
        assert!(tool_names(&tools).contains(&"added"), "round {round}");
    }
}

#[tokio::test]
async fn redirects_are_followed_within_the_origin_only() {
    let Some(stub) = Stub::start("redirects") else {
        return;
    };
    let mcp = LocalMcp::from_file(&stub.config(
        "[servers.hop]\nurl = \"{base}/hop/mcp\"\nheaders = { \"X-Api-Key\" = \"k-123\" }\n\n\
         [servers.moved]\nurl = \"{base}/moved/mcp\"\nheaders = { \"X-Api-Key\" = \"k-123\" }\n",
    ));
    assert_eq!(mcp.tools("hop").await.unwrap().len(), STUB_TOOLS);
    let reached = stub.state().await["seen_headers"].as_array().unwrap().len();

    let err = format!("{:#}", mcp.tools("moved").await.unwrap_err());
    assert!(
        err.contains("redirected to http://localhost:") && err.contains("another origin"),
        "{err}"
    );
    assert_eq!(
        stub.state().await["seen_headers"].as_array().unwrap().len(),
        reached,
        "the headers never went to the other origin"
    );
}

#[tokio::test]
async fn an_endless_event_is_cut_off_at_the_message_limit() {
    let Some(stub) = Stub::start("flood") else {
        return;
    };
    let mcp = LocalMcp::from_file(
        &stub.config("[servers.flood]\nurl = \"{base}/flood/mcp\"\ntimeout_seconds = 30\n"),
    );
    let out = mcp.call("flood", "anything", json!({})).await;
    assert!(out.is_error, "{}", out.output);
    assert!(out.output.contains("an event over 8 MiB"), "{}", out.output);
}

#[tokio::test]
async fn a_token_about_to_expire_is_refreshed_before_use() {
    let Some(stub) = Stub::start("expiring") else {
        return;
    };
    // Inside the refresh margin from the start.
    stub.control(json!({"expires_in": 30})).await;
    let (mcp, store, remote) = sign_in(&stub, "/secure/mcp").await.unwrap();
    assert_eq!(mcp.tools("secure").await.unwrap().len(), STUB_TOOLS);
    let state = stub.state().await;
    let grants: Vec<&Value> = state["token_requests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| &r["grant_type"])
        .collect();
    assert_eq!(
        grants[..2],
        [&json!("authorization_code"), &json!("refresh_token")]
    );
    let saved = store.get("secure", &remote.url).unwrap().unwrap();
    assert_eq!(saved.access_token, "tok-2");
}

#[tokio::test]
async fn discovery_works_without_a_challenge_through_openid_metadata_and_a_basic_client() {
    let Some(stub) = Stub::start("plain") else {
        return;
    };
    stub.control(json!({"basic": true})).await;
    let (mcp, _, _) = sign_in(&stub, "/plain/mcp").await.unwrap();
    assert_eq!(mcp.tools("secure").await.unwrap().len(), STUB_TOOLS);
    let state = stub.state().await;
    let exchange = &state["token_requests"][0];
    let expected = format!(
        "Basic {}",
        base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            "client-1:secret-1"
        )
    );
    assert_eq!(exchange["_authorization"], expected.as_str());
    assert!(
        exchange.get("client_secret").is_none(),
        "the secret goes only in the header: {exchange}"
    );
}

#[tokio::test]
async fn metadata_for_another_resource_or_issuer_is_refused() {
    let Some(stub) = Stub::start("mismatch") else {
        return;
    };
    let err = format!("{:#}", sign_in(&stub, "/evil/mcp").await.err().unwrap());
    assert!(
        err.contains("is for https://elsewhere.test/mcp") && err.contains("won't sign in"),
        "{err}"
    );
    let err = format!(
        "{:#}",
        sign_in(&stub, "/badissuer/mcp").await.err().unwrap()
    );
    assert!(err.contains("/someone-else as its issuer"), "{err}");
    assert_eq!(
        stub.state().await["registrations"],
        0,
        "nothing was registered with either"
    );
}
