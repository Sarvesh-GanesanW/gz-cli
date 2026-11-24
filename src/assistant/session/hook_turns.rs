//! The session's side of hooks that shape a whole turn: `session_start`
//! and `user_prompt_submit` before a message goes out, `stop` and
//! `subagent_stop` when the agent finishes, `session_end` when the session
//! is left, and `pre_compact`.

use std::collections::HashMap;

use super::{AssistantEvent, Leg, Session, TurnEnd};
use crate::assistant::hooks::{PromptCheck, SubagentEnd};
use crate::subagents::SubagentEvent;

/// How many times `stop` hooks may make the agent carry on in one turn,
/// so a hook that always blocks cannot loop forever.
const MAX_CARRY_ONS: usize = 8;

type Events<'e> = &'e mut (dyn FnMut(AssistantEvent) + Send);

impl Session {
    /// The message to send after `session_start` (first message only) and
    /// `user_prompt_submit` hooks, with what they added for the model;
    /// `Err` is why a hook blocked it. A background subagent's task is not
    /// the user's message, so it runs neither.
    pub(super) async fn hooked_prompt(
        &mut self,
        prompt: &str,
        on_event: Events<'_>,
    ) -> Result<String, String> {
        if self.subagent.is_some() {
            return Ok(prompt.to_string());
        }
        // A stop left from an interrupted turn must not stop this one.
        self.hooks.take_halt();
        let cwd = self.sandbox.cwd();
        let mut context = Vec::new();
        if !self.hooks.is_started() {
            let log = self.hook_log("");
            context.extend(self.hooks.session_start(&cwd, &log, on_event).await);
        }
        let log = self.hook_log("");
        self.hooks.auto_compact_due(&cwd, &log, on_event).await;
        match self
            .hooks
            .user_prompt_submit(prompt, &cwd, &log, on_event)
            .await
        {
            PromptCheck::Blocked(why) => Err(why),
            PromptCheck::Send { context: more } => {
                context.extend(more);
                Ok(with_hook_context(prompt, &context))
            }
        }
    }

    /// After the agent finished: `stop` hooks (or `subagent_stop` in a
    /// background subagent's session); `Some` is the message that makes it
    /// carry on.
    pub(super) async fn carry_on(&self, carried_on: usize, on_event: Events<'_>) -> Option<String> {
        let cwd = self.sandbox.cwd();
        let log = self.hook_log("");
        let is_continuation = carried_on > 0;
        let why = match &self.subagent {
            None => self.hooks.stop(is_continuation, &cwd, &log, on_event).await,
            Some((_, name)) => {
                let end = SubagentEnd {
                    name,
                    status: "completed",
                    summary: "",
                    is_continuation,
                };
                self.hooks.subagent_stop(&end, &cwd, &log, on_event).await
            }
        }?;
        if carried_on >= MAX_CARRY_ONS {
            on_event(AssistantEvent::Notice(format!(
                "⚠ stop hooks asked the agent to carry on {MAX_CARRY_ONS} times in this turn; stopping here (check the hook's stopHookActive input)"
            )));
            return None;
        }
        Some(format!(
            "A stop hook asked you to carry on instead of stopping:\n{why}"
        ))
    }

    /// Runs `subagent_stop` hooks for subagents the chat ran this leg. The
    /// chat runs them, so a hook cannot make them carry on.
    pub(super) async fn chat_subagents_stopped(
        &self,
        finished: &[(String, String, String)],
        on_event: Events<'_>,
    ) {
        let cwd = self.sandbox.cwd();
        let log = self.hook_log("");
        for (name, status, summary) in finished {
            let end = SubagentEnd {
                name,
                status,
                summary,
                is_continuation: false,
            };
            if self
                .hooks
                .subagent_stop(&end, &cwd, &log, on_event)
                .await
                .is_some()
            {
                on_event(AssistantEvent::Notice(format!(
                    "the chat runs subagent {name}, so a hook cannot make it carry on"
                )));
            }
        }
    }

    /// Stops the turn when a hook said `"continue": false`.
    pub(super) fn hook_halt(&self) -> Option<TurnEnd> {
        self.hooks.take_halt().map(|_| TurnEnd::Cancelled)
    }
}

impl Leg {
    /// Notes a subagent the chat ran that finished, for `subagent_stop`.
    pub(super) fn note_finished(
        &mut self,
        event: &SubagentEvent,
        earlier: &HashMap<String, String>,
    ) {
        let SubagentEvent::Finished {
            id,
            status,
            summary,
            ..
        } = event
        else {
            return;
        };
        let name = self
            .subagents
            .get(id)
            .or_else(|| earlier.get(id))
            .cloned()
            .unwrap_or_else(|| id.clone());
        let status = format!("{status:?}").to_lowercase();
        self.finished.push((name, status, summary.clone()));
    }
}

/// `prompt` with what hooks added for the model after it.
fn with_hook_context(prompt: &str, context: &[String]) -> String {
    if context.is_empty() {
        return prompt.to_string();
    }
    format!(
        "{prompt}\n\n<hook_context>\n{}\n</hook_context>",
        context.join("\n\n")
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use serde_json::{json, Value};

    use crate::assistant::hooks::test_support::user_hooks;
    use crate::assistant::permissions::{Approver, Decision, DecisionFuture, PermissionRequest};
    use crate::assistant::sandbox::Sandbox;
    use crate::http::ApiClient;

    #[test]
    fn hook_context_follows_the_message() {
        assert_eq!(with_hook_context("hi", &[]), "hi");
        assert_eq!(
            with_hook_context("hi", &["a".into(), "b".into()]),
            "hi\n\n<hook_context>\na\n\nb\n</hook_context>"
        );
    }

    /// Answers every prompt with `decision`, keeping what it was asked.
    struct Asked {
        decision: Decision,
        requests: Mutex<Vec<PermissionRequest>>,
        /// How many requests came through `ask_anyway`.
        forced: Mutex<usize>,
        plan_mode: bool,
    }

    impl Asked {
        fn new(decision: Decision) -> Self {
            Self {
                decision,
                requests: Mutex::new(Vec::new()),
                forced: Mutex::new(0),
                plan_mode: false,
            }
        }
    }

    impl Approver for Asked {
        fn decide<'a>(&'a self, request: &'a PermissionRequest) -> DecisionFuture<'a> {
            self.requests.lock().unwrap().push(request.clone());
            Box::pin(async move { self.decision })
        }

        fn ask_anyway<'a>(&'a self, request: &'a PermissionRequest) -> DecisionFuture<'a> {
            *self.forced.lock().unwrap() += 1;
            self.decide(request)
        }

        fn in_plan_mode(&self) -> bool {
            self.plan_mode
        }
    }

    /// A chat server where every run completes at once with "done"; keeps
    /// each run's `userInput`.
    fn chat_server() -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let inputs = Arc::new(Mutex::new(Vec::new()));
        let seen = inputs.clone();
        let done = "id: 1\ndata: {\"type\":\"llm_chunk\",\"text\":\"done\"}\n\nid: 2\ndata: {\"type\":\"run_status\",\"status\":\"completed\"}\n\ndata: [DONE]\n\n";
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let (path, body) = read_request(&mut stream);
                let (kind, text) = if path == "/chat/runs" {
                    let mut inputs = seen.lock().unwrap();
                    inputs.push(body["userInput"].as_str().unwrap_or_default().to_string());
                    let run = format!("{{\"runId\":\"r{}\"}}", inputs.len());
                    ("application/json", run)
                } else if path.ends_with("/events") {
                    ("text/event-stream", done.to_string())
                } else {
                    ("application/json", "{}".to_string())
                };
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                    text.len()
                );
                stream.write_all(reply.as_bytes()).ok();
            }
        });
        (base, inputs)
    }

    /// The path (without query) and JSON body of one request.
    fn read_request(stream: &mut std::net::TcpStream) -> (String, Value) {
        let mut raw = Vec::new();
        let mut buf = [0u8; 8192];
        let head_end = loop {
            let n = stream.read(&mut buf).unwrap();
            raw.extend_from_slice(&buf[..n]);
            if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
        };
        let head = String::from_utf8_lossy(&raw[..head_end]).to_string();
        let length: usize = head
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.eq_ignore_ascii_case("content-length")
                    .then(|| v.trim().parse().ok())?
            })
            .unwrap_or(0);
        while raw.len() < head_end + length {
            let n = stream.read(&mut buf).unwrap();
            raw.extend_from_slice(&buf[..n]);
        }
        let path = head
            .split_whitespace()
            .nth(1)
            .unwrap_or_default()
            .split('?')
            .next()
            .unwrap_or_default()
            .to_string();
        let body = serde_json::from_slice(&raw[head_end..]).unwrap_or(Value::Null);
        (path, body)
    }

    /// A session against `base`, in a temp folder with `hooks_toml` as
    /// the user's hooks.
    fn session(name: &str, base: &str, hooks_toml: &str) -> (PathBuf, Session) {
        let (dir, hooks) = user_hooks(name, hooks_toml);
        let resolved = crate::config::resolve(
            &crate::config::Config::default(),
            Some("session-test"),
            Some(base.to_string()),
            Some("stub-token".to_string()),
        );
        let http =
            ApiClient::new(resolved, crate::output::OutputFormat::Json, true, 0, 30).unwrap();
        let mut session = Session::new(http, &dir).unwrap();
        session.audit = None;
        session.history = None;
        session.sandbox = Sandbox::new(&dir).unwrap();
        session.set_hooks(hooks);
        (dir, session)
    }

    #[tokio::test]
    async fn a_blocked_message_never_reaches_the_chat() {
        let (base, inputs) = chat_server();
        let (_dir, mut session) = session(
            "hook-turn-block",
            &base,
            "[[user_prompt_submit]]\ncommand = \"echo 'not on Fridays' >&2; exit 2\"",
        );
        let end = session
            .send("ship it", &Asked::new(Decision::AllowOnce), &mut |_| {})
            .await;
        match end.unwrap() {
            TurnEnd::Failed(why) => assert!(
                why.ends_with("blocked this message: not on Fridays"),
                "{why}"
            ),
            other => panic!("{other:?}"),
        }
        assert!(inputs.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn start_and_prompt_context_go_with_the_message_and_start_runs_once() {
        let (base, inputs) = chat_server();
        let (_dir, mut session) = session(
            "hook-turn-context",
            &base,
            "[[session_start]]\ncommand = \"echo 'on branch main'\"\n[[user_prompt_submit]]\ncommand = \"echo '{\\\"additionalContext\\\":\\\"tests: make test\\\"}'\"",
        );
        let approver = Asked::new(Decision::AllowOnce);
        session
            .send("fix it", &approver, &mut |_| {})
            .await
            .unwrap();
        session.send("again", &approver, &mut |_| {}).await.unwrap();
        let inputs = inputs.lock().unwrap().clone();
        assert_eq!(
            inputs,
            vec![
                "fix it\n\n<hook_context>\non branch main\n\ntests: make test\n</hook_context>",
                "again\n\n<hook_context>\ntests: make test\n</hook_context>",
            ]
        );
    }

    #[tokio::test]
    async fn a_stop_hook_makes_the_agent_carry_on_until_it_is_satisfied() {
        let (base, inputs) = chat_server();
        let (_dir, mut session) = session(
            "hook-turn-stop",
            &base,
            "[[stop]]\ncommand = \"grep -q '\\\"stopHookActive\\\":true' && exit 0; echo 'run the tests' >&2; exit 2\"",
        );
        let end = session
            .send("fix it", &Asked::new(Decision::AllowOnce), &mut |_| {})
            .await
            .unwrap();
        assert_eq!(end, TurnEnd::Completed);
        let inputs = inputs.lock().unwrap().clone();
        assert_eq!(inputs.len(), 2, "{inputs:?}");
        assert_eq!(
            inputs[1],
            "A stop hook asked you to carry on instead of stopping:\nrun the tests"
        );
    }

    #[tokio::test]
    async fn a_stop_hook_that_always_blocks_is_cut_off() {
        let (base, inputs) = chat_server();
        let (_dir, mut session) =
            session("hook-turn-loop", &base, "[[stop]]\ncommand = \"exit 2\"");
        let mut notices = Vec::new();
        session
            .send("fix it", &Asked::new(Decision::AllowOnce), &mut |event| {
                if let AssistantEvent::Notice(text) = event {
                    notices.push(text);
                }
            })
            .await
            .unwrap();
        assert_eq!(inputs.lock().unwrap().len(), MAX_CARRY_ONS + 1);
        assert!(notices.iter().any(|n| n.contains("stopping here")));
    }

    #[tokio::test]
    async fn continue_false_skips_the_rest_of_the_calls() {
        let text = "[[post_tool_use]]\ncommand = \"echo '{\\\"continue\\\":false,\\\"stopReason\\\":\\\"red build\\\"}'\"";
        let (dir, mut session) = session("hook-turn-halt", "http://127.0.0.1:9", text);
        let approver = Asked::new(Decision::AllowOnce);
        let write = |name: &str| json!({"path": name, "content": "x"});
        session
            .answer_one(
                "r",
                "write_file",
                &write("a.txt"),
                None,
                &approver,
                &mut |_| {},
            )
            .await;
        let skipped = session
            .answer_one(
                "r",
                "write_file",
                &write("b.txt"),
                None,
                &approver,
                &mut |_| {},
            )
            .await;
        assert_eq!(
            skipped.output,
            "Not run: a hook stopped the turn (red build)"
        );
        assert!(dir.join("a.txt").exists() && !dir.join("b.txt").exists());
        assert_eq!(session.hook_halt(), Some(TurnEnd::Cancelled));
        assert_eq!(session.hook_halt(), None);
    }

    #[tokio::test]
    async fn pre_tool_use_decides_before_the_permission_prompt() {
        let text = "[[pre_tool_use]]\nmatcher = \"run_command\"\ncommand = \"echo '{\\\"decision\\\":\\\"deny\\\",\\\"reason\\\":\\\"use make\\\"}'\"\n[[pre_tool_use]]\nmatcher = \"write_file\"\ncommand = \"echo '{\\\"decision\\\":\\\"allow\\\"}'\"\n[[pre_tool_use]]\nmatcher = \"list_directory\"\ncommand = \"echo '{\\\"decision\\\":\\\"ask\\\",\\\"reason\\\":\\\"big folder\\\"}'\"";
        let (dir, mut session) = session("hook-turn-pre", "http://127.0.0.1:9", text);
        let refusing = Asked::new(Decision::Deny);

        let denied = session
            .answer_one(
                "r",
                "run_command",
                &json!({"command": "touch made"}),
                None,
                &refusing,
                &mut |_| {},
            )
            .await;
        assert!(denied.is_error);
        assert_eq!(
            denied.output,
            "A pre_tool_use hook denied this call: use make"
        );
        assert!(!dir.join("made").exists());

        let written = session
            .answer_one(
                "r",
                "write_file",
                &json!({"path": "a.txt", "content": "x"}),
                None,
                &refusing,
                &mut |_| {},
            )
            .await;
        assert!(!written.is_error, "{}", written.output);
        assert!(dir.join("a.txt").exists(), "allowed without a prompt");
        assert!(
            refusing.requests.lock().unwrap().is_empty(),
            "nobody was asked"
        );

        session.allow_all_reads();
        let listed = session
            .answer_one(
                "r",
                "list_directory",
                &json!({"path": "."}),
                None,
                &refusing,
                &mut |_| {},
            )
            .await;
        assert!(listed.is_error, "asked, and the user said no");
        let asked = refusing.requests.lock().unwrap().clone();
        assert_eq!(asked.len(), 1);
        assert_eq!(
            asked[0].note.as_deref(),
            Some("A pre_tool_use hook asks you to confirm: big folder")
        );
        assert_eq!(*refusing.forced.lock().unwrap(), 1, "asked past the mode");
    }

    #[tokio::test]
    async fn pre_tool_use_covers_folders_memory_and_session_tools() {
        let seen = "post_seen";
        let text = format!(
            "[[pre_tool_use]]\nmatcher = \"change_directory\"\ncommand = \"echo 'stay here' >&2; exit 2\"\n[[post_tool_use_failure]]\nmatcher = \"todo_write\"\ncommand = \"cat > {seen}; echo 'todos noted'\"\n[[post_tool_use_failure]]\nmatcher = \"todo_write\"\ncommand = \"echo '{{\\\"additionalContext\\\":\\\"keep it short\\\"}}'\""
        );
        let (dir, mut session) = session("hook-turn-side", "http://127.0.0.1:9", &text);
        session.set_local_context(Some(crate::assistant::local_context::Limits::default()));
        session.set_server_tools(Some(vec!["todo_write".to_string()]));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let approver = Asked::new(Decision::AllowOnce);
        let moved = session
            .answer_one(
                "r",
                "change_directory",
                &json!({"path": "sub"}),
                None,
                &approver,
                &mut |_| {},
            )
            .await;
        assert_eq!(
            moved.output,
            "A pre_tool_use hook denied this call: stay here"
        );
        assert_eq!(session.sandbox.cwd(), dir, "gz stayed where it was");

        let todos = json!({"todos": [{"content": "a", "status": "pending"}]});
        let out = session
            .answer_one("r", "todo_write", &todos, None, &approver, &mut |_| {})
            .await;
        assert!(out.is_error, "todo_write is the server's: {}", out.output);
        assert!(
            out.output
                .ends_with("[post_tool_use_failure hook]\nkeep it short"),
            "{}",
            out.output
        );
        let payload: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join(seen)).unwrap()).unwrap();
        assert_eq!(payload["event"], "post_tool_use_failure");
        assert_eq!(payload["tool"], "todo_write");
    }

    #[tokio::test]
    async fn a_hook_allow_does_not_get_past_plan_mode() {
        let text = "[[pre_tool_use]]\nmatcher = \"write_file\"\ncommand = \"echo '{\\\"decision\\\":\\\"allow\\\"}'\"";
        let (dir, mut session) = session("hook-turn-plan", "http://127.0.0.1:9", text);
        let planning = Asked {
            plan_mode: true,
            ..Asked::new(Decision::Deny)
        };
        let written = session
            .answer_one(
                "r",
                "write_file",
                &json!({"path": "a.txt", "content": "x"}),
                None,
                &planning,
                &mut |_| {},
            )
            .await;
        assert!(written.is_error, "{}", written.output);
        assert!(!dir.join("a.txt").exists());
        assert_eq!(
            planning.requests.lock().unwrap().len(),
            1,
            "the mode decided"
        );
    }
}
