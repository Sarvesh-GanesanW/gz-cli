//! Ending a run the user interrupted. A run paused for gz (client tools or
//! a server approval) ignores a cancel, so gz answers the pause first —
//! every open call "Interrupted by the user", every server action denied —
//! which keeps the conversation well-formed, then cancels the run that
//! answer started. A run still working is simply cancelled.

use std::time::Duration;

use reqwest::Method;
use serde_json::{json, Value};

use super::{Leg, Session, CHAT};
use crate::assistant::interrupt::{Interrupt, INTERRUPTED};
use crate::assistant::permissions::Approver;
use crate::assistant::session::AssistantEvent;
use crate::assistant::tools::ToolOutput;
use crate::cmd::seg;

/// How long gz reads a stopped run's last events, looking for a pause it
/// had not seen yet. The run has already ended by then, so its stream only
/// replays what is stored and closes.
const LATE_PAUSE_READ: Duration = Duration::from_secs(10);

/// The run status of a run paused for gz (client tools or an approval);
/// such a run ignores a cancel (gz-chat infrastructure/chat_run_store.py:30,
/// api/routes/chat/run_routes.py cancel_chat_run).
const PAUSED: &str = "interrupted";

impl Session {
    /// The turn was interrupted while `leg` streamed (read up to
    /// `last_id`): answers a pause the leg already reported, else cancels
    /// the run. A run that turns out to be paused already, for a pause gz
    /// had not read yet, gets that pause answered too; otherwise a pending
    /// approval would block the user's next message.
    pub(super) async fn stop_leg(
        &mut self,
        run_id: &str,
        leg: Leg,
        last_id: Option<String>,
        approver: &dyn Approver,
        on_event: &mut (dyn FnMut(AssistantEvent) + Send),
    ) {
        if self.answer_pause(run_id, leg, approver, on_event).await {
            return;
        }
        let reply = self.cancel_run(run_id, on_event).await;
        let is_paused = reply.is_some_and(|run| run["status"].as_str() == Some(PAUSED));
        if is_paused {
            self.answer_late_pause(run_id, last_id, approver, on_event)
                .await;
        }
    }

    /// Answers the pause `leg` ended on, if it did: every open call
    /// "Interrupted by the user", or the server action denied. Returns
    /// whether there was one.
    async fn answer_pause(
        &mut self,
        run_id: &str,
        leg: Leg,
        approver: &dyn Approver,
        on_event: &mut (dyn FnMut(AssistantEvent) + Send),
    ) -> bool {
        if !leg.client_calls.is_empty() {
            let results = interrupted_results(&leg.client_calls);
            let path = format!("/chat/runs/{}/client-tools/resume", seg(run_id));
            self.abandon(path, json!({"toolResults": results}), on_event)
                .await;
            true
        } else if let Some(pause) = leg.approval {
            // The approver is interrupted, so this denies without asking.
            let body = self.decide_server_action(&pause, approver, on_event).await;
            let path = format!("/chat/runs/{}/resume", seg(run_id));
            self.abandon(path, body, on_event).await;
            true
        } else {
            false
        }
    }

    /// Reads the paused run's events after `last_id` to learn what it
    /// waits on, and answers that. Only the cost and context fill of
    /// those events are passed on: the turn is over for the user.
    async fn answer_late_pause(
        &mut self,
        run_id: &str,
        mut last_id: Option<String>,
        approver: &dyn Approver,
        on_event: &mut (dyn FnMut(AssistantEvent) + Send),
    ) {
        let mut leg = Leg::default();
        let mut counted = |event: AssistantEvent| {
            if matches!(event, AssistantEvent::Usage(_) | AssistantEvent::Context(_)) {
                on_event(event);
            }
        };
        let read = self.stream_leg(run_id, &mut last_id, &mut leg, &mut counted);
        let unread = match tokio::time::timeout(LATE_PAUSE_READ, read).await {
            Ok(Ok(())) => None,
            Ok(Err(err)) => Some(format!("{err:#}")),
            Err(_) => Some(format!("no answer in {}s", LATE_PAUSE_READ.as_secs())),
        };
        if let Some(why) = unread {
            on_event(AssistantEvent::Notice(format!(
                "gz stopped, but could not read what the chat was waiting on ({why}). \
                 If your next message is refused, answer or cancel the pending action \
                 in the web app."
            )));
            return;
        }
        self.answer_pause(run_id, leg, approver, on_event).await;
    }

    /// Answers a pause with `body` (POST `path`) and cancels the run the
    /// answer starts, so the model does not carry on.
    pub(super) async fn abandon(
        &mut self,
        path: String,
        body: Value,
        on_event: &mut (dyn FnMut(AssistantEvent) + Send),
    ) {
        match self.post_run(path, body).await {
            Ok(resumed) => {
                if let Some(next) = resumed["runId"].as_str() {
                    self.cancel_run(next, on_event).await;
                }
            }
            Err(err) => on_event(AssistantEvent::Notice(format!(
                "gz stopped, but could not tell the chat which calls were interrupted: \
                 {err:#}. Your next message starts a new turn as usual."
            ))),
        }
    }

    /// Asks the chat to stop `run_id`; a finished or paused run ignores
    /// it. Returns the run as the chat reports it, if the call went through.
    pub(super) async fn cancel_run(
        &self,
        run_id: &str,
        on_event: &mut (dyn FnMut(AssistantEvent) + Send),
    ) -> Option<Value> {
        let path = format!("/chat/runs/{}/cancel", seg(run_id));
        let cancelled = self
            .http
            .request_json(CHAT, Method::POST, &path, &[], None)
            .await;
        match cancelled {
            Ok(run) => Some(run),
            Err(err) => {
                on_event(AssistantEvent::Notice(format!(
                    "gz stopped, but the chat may still finish this answer: {err:#}"
                )));
                None
            }
        }
    }
}

/// An "Interrupted by the user" error result for every call in `calls`.
fn interrupted_results(calls: &[Value]) -> Vec<Value> {
    calls
        .iter()
        .map(|call| {
            json!({
                "interruptId": call["interruptId"],
                "toolName": call["toolName"],
                "result": {"output": INTERRUPTED, "isError": true, "truncated": false},
            })
        })
        .collect()
}

/// Whether `out` is the result of a call the stop cut short (a command
/// reports it with what it printed so far).
pub(super) fn was_cut_short(out: &ToolOutput, interrupt: &Interrupt) -> bool {
    interrupt.is_requested() && out.is_error && out.output.starts_with(INTERRUPTED)
}

/// What the audit log names as the target of a call that never ran: the
/// path, command, address or pattern it asked for, never file contents.
pub(super) fn skipped_target(input: &Value) -> &str {
    ["path", "command", "url", "pattern", "query"]
        .iter()
        .find_map(|key| input[*key].as_str())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::assistant::permissions::{Decision, DecisionFuture, PermissionRequest};
    use crate::assistant::session::stub_chat::{self, call, events, Answer};
    use crate::assistant::session::{Session, TurnEnd};

    #[test]
    fn every_open_call_is_answered_as_interrupted() {
        let calls = vec![
            json!({"interruptId": "i1", "toolName": "read_file", "input": {}}),
            json!({"interruptId": "i2", "toolName": "run_command", "input": {}}),
        ];
        let results = interrupted_results(&calls);
        assert_eq!(results.len(), 2);
        assert_eq!(results[1]["interruptId"], "i2");
        assert_eq!(results[1]["toolName"], "run_command");
        assert_eq!(results[1]["result"]["output"], INTERRUPTED);
        assert_eq!(results[1]["result"]["isError"], true);
    }

    /// Allows commands; never answers anything else (a prompt left open).
    struct CommandsOnly;

    impl Approver for CommandsOnly {
        fn decide<'a>(&'a self, request: &'a PermissionRequest) -> DecisionFuture<'a> {
            let is_command = request.summary.starts_with("run_command");
            Box::pin(async move {
                if is_command {
                    Decision::AllowOnce
                } else {
                    std::future::pending().await
                }
            })
        }
    }

    /// Sends `prompt` and interrupts the turn after `after`; returns how
    /// it ended and how long the stop took.
    async fn interrupted_send(session: &mut Session, after: Duration) -> (TurnEnd, Duration) {
        let canceller = session.canceller();
        canceller.expect_turn();
        let stop = async {
            tokio::time::sleep(after).await;
            canceller.interrupt();
            Instant::now()
        };
        let mut ignore = |_| {};
        let send = session.send("go", &CommandsOnly, &mut ignore);
        let (end, stopped_at) = tokio::join!(send, stop);
        (end.unwrap(), stopped_at.elapsed())
    }

    #[tokio::test]
    async fn esc_stops_a_running_command_skips_the_rest_and_answers_every_call() {
        let calls = [
            call("i1", "run_command", json!({"command": "sleep 30"})),
            call("i2", "read_file", json!({"path": "a.txt"})),
        ];
        let (base, seen) = stub_chat::serve(vec![
            ("POST /chat/runs", Answer::Json(r#"{"runId":"r1"}"#)),
            (
                "GET /chat/runs/r1/events",
                Answer::Events(events(&calls, "interrupted")),
            ),
            (
                "POST /chat/runs/r1/client-tools/resume",
                Answer::Json(r#"{"runId":"r2"}"#),
            ),
        ]);
        let (mut session, _dir) = stub_chat::session(base, "stop-batch");
        let (end, took) = interrupted_send(&mut session, Duration::from_millis(600)).await;
        assert_eq!(end, TurnEnd::Cancelled);
        assert!(took < Duration::from_secs(5), "stopped in {took:?}");
        let body = seen
            .body("POST /chat/runs/r1/client-tools/resume")
            .expect("the open calls were answered");
        let results = body["toolResults"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        for result in results {
            assert_eq!(result["result"]["output"], INTERRUPTED);
            assert_eq!(result["result"]["isError"], true);
        }
        assert!(seen.has("POST /chat/runs/r2/cancel"), "{:?}", seen.lines());
        assert!(!seen.has("GET /chat/runs/r2/events"), "gz stops reading");
    }

    fn audit_lines(path: &std::path::Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn an_interrupted_call_is_audited_with_its_target() {
        let calls = [
            call("i1", "run_command", json!({"command": "sleep 31"})),
            call("i2", "read_file", json!({"path": "a.txt"})),
        ];
        let (base, _seen) = stub_chat::serve(vec![
            ("POST /chat/runs", Answer::Json(r#"{"runId":"r1"}"#)),
            (
                "GET /chat/runs/r1/events",
                Answer::Events(events(&calls, "interrupted")),
            ),
            (
                "POST /chat/runs/r1/client-tools/resume",
                Answer::Json(r#"{"runId":"r2"}"#),
            ),
        ]);
        let (mut session, dir) = stub_chat::session(base, "stop-audit");
        let log = dir.join("audit.log");
        std::fs::remove_file(&log).ok();
        session.audit = Some(crate::assistant::audit::AuditLog::at(log.clone()));
        interrupted_send(&mut session, Duration::from_millis(600)).await;
        let lines = audit_lines(&log);
        let entry = |tool: &str| {
            lines
                .iter()
                .rfind(|l| l["tool"] == tool)
                .unwrap_or_else(|| panic!("no {tool} entry in {lines:?}"))
                .clone()
        };
        let command = entry("run_command");
        assert_eq!(command["decision"], "interrupted");
        assert!(
            command["target"].as_str().unwrap().contains("sleep 31"),
            "{command}"
        );
        let read = entry("read_file");
        assert_eq!(read["decision"], "skipped");
        assert_eq!(read["target"], "a.txt");
        std::fs::remove_file(&log).ok();
    }

    #[tokio::test]
    async fn a_pause_the_stop_beat_to_the_screen_is_still_answered() {
        // The run pauses for an approval while gz is not reading (the
        // stream stays silent); Esc then finds the run already paused.
        let approval = json!({
            "type": "interrupt",
            "interruptId": "a1",
            "toolName": "delete_dataset",
        });
        let (base, seen) = stub_chat::serve(vec![
            ("POST /chat/runs", Answer::Json(r#"{"runId":"r1"}"#)),
            (
                "GET /chat/runs/r1/events",
                Answer::InTurn(vec![
                    Answer::OpenEvents(String::new()),
                    Answer::Events(events(&[approval], "interrupted")),
                ]),
            ),
            (
                "POST /chat/runs/r1/cancel",
                Answer::Json(r#"{"runId":"r1","status":"interrupted"}"#),
            ),
            (
                "POST /chat/runs/r1/resume",
                Answer::Json(r#"{"runId":"r2"}"#),
            ),
        ]);
        let (mut session, _dir) = stub_chat::session(base, "stop-late-pause");
        let (end, took) = interrupted_send(&mut session, Duration::from_millis(300)).await;
        assert_eq!(end, TurnEnd::Cancelled);
        assert!(took < Duration::from_secs(5), "stopped in {took:?}");
        let body = seen
            .body("POST /chat/runs/r1/resume")
            .unwrap_or_else(|| panic!("the approval was not answered: {:?}", seen.lines()));
        assert_eq!(body["interruptId"], "a1");
        assert_eq!(body["approved"], false);
        assert!(seen.has("POST /chat/runs/r2/cancel"), "{:?}", seen.lines());
    }

    #[tokio::test]
    async fn a_cancelled_run_that_was_still_working_is_not_read_again() {
        let chunk = json!({"type": "llm_chunk", "text": "Hello"});
        let (base, seen) = stub_chat::serve(vec![
            ("POST /chat/runs", Answer::Json(r#"{"runId":"r1"}"#)),
            (
                "GET /chat/runs/r1/events",
                Answer::OpenEvents(format!("id: 1\ndata: {chunk}\n\n")),
            ),
            (
                "POST /chat/runs/r1/cancel",
                Answer::Json(r#"{"runId":"r1","status":"running"}"#),
            ),
        ]);
        let (mut session, _dir) = stub_chat::session(base, "stop-running");
        interrupted_send(&mut session, Duration::from_millis(300)).await;
        let reads = seen
            .lines()
            .iter()
            .filter(|l| *l == "GET /chat/runs/r1/events")
            .count();
        assert_eq!(reads, 1, "{:?}", seen.lines());
    }

    struct AllowAll;

    impl Approver for AllowAll {
        fn decide<'a>(&'a self, _: &'a PermissionRequest) -> DecisionFuture<'a> {
            Box::pin(async { Decision::AllowOnce })
        }
    }

    #[tokio::test]
    async fn a_call_that_already_ran_keeps_its_result_when_the_stop_lands_in_its_after_hooks() {
        let calls = [call(
            "i1",
            "write_file",
            json!({"path": "done.txt", "content": "x"}),
        )];
        let (base, seen) = stub_chat::serve(vec![
            ("POST /chat/runs", Answer::Json(r#"{"runId":"r1"}"#)),
            (
                "GET /chat/runs/r1/events",
                Answer::Events(events(&calls, "interrupted")),
            ),
            (
                "POST /chat/runs/r1/client-tools/resume",
                Answer::Json(r#"{"runId":"r2"}"#),
            ),
        ]);
        let (mut session, dir) = stub_chat::session(base, "stop-after-hook");
        std::fs::remove_file(dir.join("done.txt")).ok();
        let (_hook_dir, hooks) = crate::assistant::hooks::test_support::user_hooks(
            "stop-after-hook",
            "[[after_tool]]\ntools = [\"write_file\"]\ncommand = \"sleep 5\"\n",
        );
        session.hooks = hooks;
        let canceller = session.canceller();
        canceller.expect_turn();
        let stop = async {
            tokio::time::sleep(Duration::from_millis(800)).await;
            canceller.interrupt();
        };
        let mut ignore = |_| {};
        let (end, ()) = tokio::join!(session.send("go", &AllowAll, &mut ignore), stop);
        assert_eq!(end.unwrap(), TurnEnd::Cancelled);
        assert!(dir.join("done.txt").exists());
        let body = seen.body("POST /chat/runs/r1/client-tools/resume").unwrap();
        let result = &body["toolResults"][0]["result"];
        assert_ne!(result["output"], INTERRUPTED, "{result}");
        assert_eq!(result["isError"], false, "{result}");
        std::fs::remove_file(dir.join("done.txt")).ok();
    }

    #[tokio::test]
    async fn an_interrupted_command_reports_what_it_printed_before_the_stop() {
        let calls = [call(
            "i1",
            "run_command",
            json!({"command": "echo warming-up; sleep 30"}),
        )];
        let (base, seen) = stub_chat::serve(vec![
            ("POST /chat/runs", Answer::Json(r#"{"runId":"r1"}"#)),
            (
                "GET /chat/runs/r1/events",
                Answer::Events(events(&calls, "interrupted")),
            ),
            (
                "POST /chat/runs/r1/client-tools/resume",
                Answer::Json(r#"{"runId":"r2"}"#),
            ),
        ]);
        let (mut session, _dir) = stub_chat::session(base, "stop-output");
        let (end, took) = interrupted_send(&mut session, Duration::from_millis(800)).await;
        assert_eq!(end, TurnEnd::Cancelled);
        assert!(took < Duration::from_secs(5), "stopped in {took:?}");
        let body = seen.body("POST /chat/runs/r1/client-tools/resume").unwrap();
        let result = &body["toolResults"][0]["result"];
        let output = result["output"].as_str().unwrap();
        assert!(output.starts_with(INTERRUPTED), "{output}");
        assert!(output.contains("warming-up"), "{output}");
        assert_eq!(result["isError"], true);
        assert!(seen.has("POST /chat/runs/r2/cancel"), "{:?}", seen.lines());
    }

    #[tokio::test]
    async fn esc_closes_an_open_prompt() {
        let calls = [call(
            "i1",
            "write_file",
            json!({"path": "b.txt", "content": "x"}),
        )];
        let (base, seen) = stub_chat::serve(vec![
            ("POST /chat/runs", Answer::Json(r#"{"runId":"r1"}"#)),
            (
                "GET /chat/runs/r1/events",
                Answer::Events(events(&calls, "interrupted")),
            ),
            (
                "POST /chat/runs/r1/client-tools/resume",
                Answer::Json(r#"{"runId":"r2"}"#),
            ),
        ]);
        let (mut session, dir) = stub_chat::session(base, "stop-prompt");
        let (end, _) = interrupted_send(&mut session, Duration::from_millis(400)).await;
        assert_eq!(end, TurnEnd::Cancelled);
        let body = seen.body("POST /chat/runs/r1/client-tools/resume").unwrap();
        assert_eq!(body["toolResults"][0]["result"]["output"], INTERRUPTED);
        assert!(!dir.join("b.txt").exists(), "nothing was written");
        assert!(seen.has("POST /chat/runs/r2/cancel"));
    }

    #[tokio::test]
    async fn esc_before_the_run_id_arrives_cancels_the_run_once_it_does() {
        let (base, seen) = stub_chat::serve(vec![(
            "POST /chat/runs",
            Answer::SlowJson(Duration::from_millis(500), r#"{"runId":"r1"}"#),
        )]);
        let (mut session, _dir) = stub_chat::session(base, "stop-early");
        let (end, _) = interrupted_send(&mut session, Duration::from_millis(100)).await;
        assert_eq!(end, TurnEnd::Cancelled);
        assert!(seen.has("POST /chat/runs/r1/cancel"), "{:?}", seen.lines());
        assert!(!seen.has("GET /chat/runs/r1/events"));
    }

    #[tokio::test]
    async fn esc_while_the_answer_streams_stops_reading_and_cancels() {
        let chunk = json!({"type": "llm_chunk", "text": "Hello"});
        let (base, seen) = stub_chat::serve(vec![
            ("POST /chat/runs", Answer::Json(r#"{"runId":"r1"}"#)),
            (
                "GET /chat/runs/r1/events",
                Answer::OpenEvents(format!("id: 1\ndata: {chunk}\n\n")),
            ),
        ]);
        let (mut session, _dir) = stub_chat::session(base, "stop-stream");
        let (end, took) = interrupted_send(&mut session, Duration::from_millis(400)).await;
        assert_eq!(end, TurnEnd::Cancelled);
        assert!(took < Duration::from_secs(3), "stopped in {took:?}");
        assert!(seen.has("POST /chat/runs/r1/cancel"));
    }

    #[tokio::test]
    async fn a_stop_for_one_turn_does_not_stop_the_next() {
        let (base, _seen) = stub_chat::serve(vec![
            ("POST /chat/runs", Answer::Json(r#"{"runId":"r1"}"#)),
            (
                "GET /chat/runs/r1/events",
                Answer::Events(events(&[], "completed")),
            ),
        ]);
        let (mut session, _dir) = stub_chat::session(base, "stop-next");
        session.canceller().interrupt();
        let end = session
            .send("go", &CommandsOnly, &mut |_| {})
            .await
            .unwrap();
        assert_eq!(end, TurnEnd::Completed);
    }
}
