//! `local_mcp_tools` and `local_mcp_call` through the session, against the
//! fake stdio server of [`crate::assistant::mcp_local::tests`].

use std::sync::Mutex;

use serde_json::json;

use super::*;
use crate::assistant::mcp_local::tests::{fake_config, python};
use crate::assistant::permissions::{DecisionFuture, PermissionRequest, SavedRules};
use crate::assistant::saved_rules::{Rule, Store};
use crate::assistant::tools::Access;
use crate::http::ApiClient;

/// Answers prompts from a script, in order, and keeps what it was asked.
struct Answering {
    decisions: Mutex<Vec<Decision>>,
    asked: Mutex<Vec<PermissionRequest>>,
}

impl Answering {
    fn new(decisions: &[Decision]) -> Self {
        Self {
            decisions: Mutex::new(decisions.to_vec()),
            asked: Mutex::new(Vec::new()),
        }
    }

    fn asked(&self) -> Vec<PermissionRequest> {
        self.asked.lock().unwrap().clone()
    }
}

impl Approver for Answering {
    fn decide<'a>(&'a self, request: &'a PermissionRequest) -> DecisionFuture<'a> {
        self.asked.lock().unwrap().push(request.clone());
        let decision = self.decisions.lock().unwrap().remove(0);
        Box::pin(async move { decision })
    }
}

const SERVER_TOOLS: [&str; 3] = ["read_file", "local_mcp_tools", "local_mcp_call"];

/// A session with the fake server configured and the chat server listing
/// both tools; nothing it saves or logs reaches the user's config.
fn mcp_session(name: &str, config: &std::path::Path) -> Session {
    let dir = std::env::temp_dir().join(format!("gz-mcp-session-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let resolved = crate::config::resolve(
        &crate::config::Config::default(),
        Some("session-test"),
        Some("http://127.0.0.1:9".into()),
        Some("stub-token".to_string()),
    );
    let http = ApiClient::new(resolved, crate::output::OutputFormat::Json, true, 0, 5).unwrap();
    let mut session = Session::new(http, &dir).unwrap();
    session.audit = None;
    session.history = None;
    session.saved = Some(Store::at(dir.join("permissions.toml")));
    session.policy = Policy::with_saved(SavedRules::default());
    session.set_server_tools(Some(SERVER_TOOLS.map(str::to_string).to_vec()));
    session.set_local_mcp(Some(LocalMcp::from_file(config)));
    session
}

async fn call(
    session: &mut Session,
    approver: &dyn Approver,
    tool: &str,
    input: Value,
) -> ToolOutput {
    session
        .answer_one("run_1", tool, &input, None, approver, &mut |_| {})
        .await
}

fn echo(text: &str) -> Value {
    json!({"server": "fake", "tool": "echo", "arguments": {"text": text}})
}

#[tokio::test]
async fn listing_runs_without_a_prompt_and_a_call_is_asked_like_a_command() {
    let Some(python) = python() else { return };
    let (_dir, config) = fake_config("session-ask", &python, "");
    let mut session = mcp_session("ask", &config);
    let approver = Answering::new(&[Decision::Deny, Decision::AllowOnce]);

    let listed = call(&mut session, &approver, "local_mcp_tools", json!({})).await;
    assert!(
        !listed.is_error && listed.output.contains("\"echo\""),
        "{}",
        listed.output
    );
    assert!(approver.asked().is_empty(), "listing is read-only");

    let denied = call(&mut session, &approver, "local_mcp_call", echo("hi")).await;
    assert!(
        denied.is_error && denied.output.contains("denied"),
        "{}",
        denied.output
    );
    let asked = approver.asked();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].access, Access::LocalTool);
    assert_eq!(asked[0].summary, "local_mcp_call fake/echo");
    assert_eq!(asked[0].target.as_deref(), Some("fake/echo"));
    assert!(asked[0]
        .preview
        .as_deref()
        .unwrap()
        .contains("\"text\": \"hi\""));
    assert_eq!(asked[0].chat_scope, "fake/echo in this chat");

    let allowed = call(&mut session, &approver, "local_mcp_call", echo("hi")).await;
    assert_eq!((allowed.output.as_str(), allowed.is_error), ("hi", false));
    assert_eq!(
        approver.asked().len(),
        2,
        "allowed once asks again next time"
    );
}

#[tokio::test]
async fn a_tool_the_server_lacks_is_refused_before_anyone_is_asked() {
    let Some(python) = python() else { return };
    let (_dir, config) = fake_config("session-unknown", &python, "");
    let mut session = mcp_session("unknown", &config);
    let approver = Answering::new(&[]);
    let out = call(
        &mut session,
        &approver,
        "local_mcp_call",
        json!({"server": "fake", "tool": "rm_rf"}),
    )
    .await;
    assert!(out.is_error);
    assert!(
        out.output
            .contains("fake has no tool 'rm_rf'; its tools: echo, sleep"),
        "{}",
        out.output
    );
    assert!(approver.asked().is_empty());
}

#[tokio::test]
async fn for_this_chat_allows_that_servers_tool_until_a_new_chat() {
    let Some(python) = python() else { return };
    let (_dir, config) = fake_config("session-chat", &python, "");
    let mut session = mcp_session("chat", &config);
    let approver = Answering::new(&[
        Decision::AllowChat,
        Decision::AllowOnce,
        Decision::AllowOnce,
    ]);

    call(&mut session, &approver, "local_mcp_call", echo("one")).await;
    let again = call(&mut session, &approver, "local_mcp_call", echo("two")).await;
    assert_eq!(again.output, "two");
    assert_eq!(
        approver.asked().len(),
        1,
        "same tool, any arguments, this chat"
    );

    let other = json!({"server": "fake", "tool": "env", "arguments": {"name": "HOME"}});
    call(&mut session, &approver, "local_mcp_call", other).await;
    assert_eq!(approver.asked().len(), 2, "another tool of the server asks");

    session.new_conversation();
    call(&mut session, &approver, "local_mcp_call", echo("three")).await;
    assert_eq!(approver.asked().len(), 3, "the grant ended with the chat");
}

#[tokio::test]
async fn always_saves_a_rule_per_server_and_tool_that_can_be_listed_and_revoked() {
    let Some(python) = python() else { return };
    let (_dir, config) = fake_config("session-always", &python, "");
    let mut session = mcp_session("always", &config);
    let approver = Answering::new(&[Decision::AllowAlways]);

    call(&mut session, &approver, "local_mcp_call", echo("one")).await;
    let store = session.saved.clone().unwrap();
    let listed = store.list(None).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].rule, Rule::LocalTool("fake/echo".into()));
    assert_eq!(listed[0].rule.to_string(), "MCP tool `fake/echo`");
    assert_eq!(listed[0].workspace, session.sandbox.cwd());

    // A later session in this folder runs it without asking.
    let mut later = mcp_session("always", &config);
    later.policy = Policy::with_saved(store.all().unwrap());
    let silent = Answering::new(&[]);
    let out = call(&mut later, &silent, "local_mcp_call", echo("two")).await;
    assert_eq!(out.output, "two");

    assert!(store.revoke(&listed[0]).unwrap());
    assert!(store.list(None).unwrap().is_empty());
    let mut revoked = mcp_session("always", &config);
    revoked.policy = Policy::with_saved(store.all().unwrap());
    let asks = Answering::new(&[Decision::Deny]);
    call(&mut revoked, &asks, "local_mcp_call", echo("three")).await;
    assert_eq!(asks.asked().len(), 1, "revoked rules ask again");
}

#[test]
fn the_tools_are_advertised_only_when_the_chat_server_lists_them() {
    let dir = std::env::temp_dir().join(format!("gz-mcp-advertise-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("mcp.toml");
    std::fs::write(&config, "[servers.files]\ncommand = \"srv\"\n").unwrap();
    let mut session = mcp_session("advertise", &config);
    let tools = session.client_tools();
    assert!(tools.contains(&"local_mcp_tools") && tools.contains(&"local_mcp_call"));
    assert_eq!(
        session.start_body("hi")["clientTools"]
            .as_array()
            .unwrap()
            .len(),
        tools.len()
    );

    session.set_server_tools(Some(vec!["read_file".to_string(), "web_fetch".to_string()]));
    let older = session.client_tools();
    assert!(!older.contains(&"local_mcp_tools") && !older.contains(&"local_mcp_call"));

    session.set_server_tools(Some(SERVER_TOOLS.map(str::to_string).to_vec()));
    session.set_local_mcp(Some(LocalMcp::from_file(&dir.join("absent.toml"))));
    assert!(
        !session.client_tools().contains(&"local_mcp_tools"),
        "no server configured"
    );
}

#[tokio::test]
async fn a_call_gz_did_not_offer_is_not_run() {
    let dir = std::env::temp_dir().join(format!("gz-mcp-unoffered-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("mcp.toml");
    std::fs::write(&config, "[servers.files]\ncommand = \"srv\"\n").unwrap();
    let mut session = mcp_session("unoffered", &config);
    session.set_server_tools(Some(vec!["read_file".to_string()]));
    let approver = Answering::new(&[]);
    let out = call(&mut session, &approver, "local_mcp_tools", json!({})).await;
    assert!(
        out.is_error
            && out
                .output
                .contains("gz cannot run the tool 'local_mcp_tools'")
    );
}

/// Blocks every action and keeps what auto mode sent for review.
struct BlockingReviewer {
    seen: Mutex<Vec<crate::assistant::action_review::ReviewRequest>>,
}

impl crate::assistant::action_review::ActionReviewer for BlockingReviewer {
    fn review<'a>(
        &'a self,
        request: &'a crate::assistant::action_review::ReviewRequest,
    ) -> crate::assistant::action_review::ReviewFuture<'a> {
        self.seen.lock().unwrap().push(request.clone());
        Box::pin(async {
            Ok(crate::assistant::action_review::Verdict::Block(
                "Not asked for.".into(),
            ))
        })
    }
}

#[tokio::test]
async fn auto_mode_sends_a_call_to_the_review_as_a_local_tool_with_its_arguments() {
    use crate::code_ui::approver::{AutoReview, ChannelApprover, Mode};
    use crate::code_ui::auto_mode::AutoContext;
    use std::sync::Arc;

    let Some(python) = python() else { return };
    let (dir, config) = fake_config("session-auto", &python, "");
    let mut session = mcp_session("auto", &config);
    let reviewer = Arc::new(BlockingReviewer {
        seen: Mutex::new(Vec::new()),
    });
    let context = Arc::new(Mutex::new(AutoContext::default()));
    context
        .lock()
        .unwrap()
        .user_said("echo hi with the fake server");
    let (asks, _prompts) = tokio::sync::mpsc::unbounded_channel();
    let approver =
        ChannelApprover::new(asks, Arc::new(Mutex::new(Mode::Auto))).with_auto(AutoReview {
            reviewer: reviewer.clone(),
            context,
            store: crate::assistant::auto_rules::AutoStore::in_folder(&dir.join("auto")),
            sandbox: session.sandbox.clone(),
        });

    let out = call(&mut session, &approver, "local_mcp_call", echo("hi")).await;
    assert!(
        out.is_error && out.output.starts_with("Blocked by auto mode"),
        "{}",
        out.output
    );
    let seen = reviewer.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    let sent = serde_json::to_value(&seen[0]).unwrap();
    assert_eq!(sent["action"]["kind"], "local_tool");
    assert_eq!(sent["action"]["summary"], "local_mcp_call fake/echo");
    let detail: Value = serde_json::from_str(sent["action"]["detail"].as_str().unwrap()).unwrap();
    assert_eq!(detail, json!({"text": "hi"}));

    // Listing is read-only: auto mode neither reviews nor asks.
    let listed = call(
        &mut session,
        &approver,
        "local_mcp_tools",
        json!({"server": "fake"}),
    )
    .await;
    assert!(!listed.is_error);
    assert_eq!(reviewer.seen.lock().unwrap().len(), 1);
}

#[test]
fn plan_mode_refuses_local_tool_calls_like_commands() {
    use crate::code_ui::approver::Mode;
    assert_eq!(
        Mode::Plan.auto_decision(Access::LocalTool),
        Some(Decision::Deny)
    );
    assert_eq!(Mode::AcceptEdits.auto_decision(Access::LocalTool), None);
}

#[cfg(unix)]
#[tokio::test]
async fn the_users_hooks_run_around_a_local_tool_call() {
    let Some(python) = python() else { return };
    let (_dir, config) = fake_config("session-hooks", &python, "");
    let mut session = mcp_session("hooks", &config);
    let (_hooks_dir, hooks) = crate::assistant::hooks::test_support::user_hooks(
        "mcp-hooks",
        "[[before_tool]]\ntools = [\"local_mcp_call\"]\n\
         command = \"grep -q secret && { echo 'no secrets to MCP tools' >&2; exit 2; } || exit 0\"\n\
         [[after_tool]]\ntools = [\"local_mcp_call\"]\ncommand = \"echo 'checked by hook' >&2; exit 2\"",
    );
    session.set_hooks(hooks);
    let approver = Answering::new(&[Decision::AllowOnce, Decision::AllowOnce]);

    let blocked = call(&mut session, &approver, "local_mcp_call", echo("a secret")).await;
    assert!(
        blocked.is_error && blocked.output.contains("no secrets to MCP tools"),
        "{}",
        blocked.output
    );

    let ran = call(&mut session, &approver, "local_mcp_call", echo("hi")).await;
    assert!(
        ran.output.starts_with("hi") && ran.output.ends_with("[after_tool hook]\nchecked by hook"),
        "{}",
        ran.output
    );
}
