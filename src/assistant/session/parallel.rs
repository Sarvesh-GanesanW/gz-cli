//! Running the model's side-effect-free calls at the same time.
//!
//! The model often asks for several reads at once. Each still goes through
//! the same checks one by one, in order (plan, grants, prompts, hooks), so
//! prompts never overlap; only the work itself (reading files, listing,
//! searching, fetching pages) runs concurrently, at most
//! [`MAX_CONCURRENT`] at a time. Results go back, and show, in the order
//! the model asked. Anything that writes, runs a command or a script, or
//! asks the user something stays strictly in order.

use std::panic::AssertUnwindSafe;

use serde_json::Value;
use tokio::task::JoinSet;

use super::{AssistantEvent, Session};
use crate::assistant::checkpoints::{self, EditHistory};
use crate::assistant::interrupt::Interrupt;
use crate::assistant::permissions::Approver;
use crate::assistant::sandbox::Sandbox;
use crate::assistant::tools::{Planned, ToolName, ToolOutput};
use crate::subagents;

/// Reads running at once; past this they wait for a free slot.
pub(super) const MAX_CONCURRENT: usize = 8;

/// A planned call the user (or a grant) allowed and the hooks let through.
pub(super) struct Authorized {
    pub tool_name: String,
    pub input: Value,
    pub planned: Planned,
    /// How it was allowed, for the audit log.
    pub decided: &'static str,
    /// The folders as the asking subagent (or the main conversation) sees
    /// them, with its own record of reads.
    pub reader: Sandbox,
    /// How it runs without asking, when gz allowed it on its own.
    pub automatic: Option<crate::assistant::read_only::Automatic>,
    /// What `pre_tool_use` hooks added for the model.
    pub hook_context: Vec<String>,
}

impl Authorized {
    /// Shows the call as started.
    pub(super) fn announce(&self, on_event: &mut (dyn FnMut(AssistantEvent) + Send)) {
        on_event(AssistantEvent::ToolStarted {
            name: self.tool_name.clone(),
            summary: self.planned.summary.clone(),
            local: true,
        });
    }
}

/// Whether `call` only reads (files, folders, web pages), so it may run
/// alongside others like it. Judged from what each tool does: these never
/// write, run anything or ask a question (`tools::run` for
/// `Action::Read`/`List`/`Find`/`Search`/`Fetch`).
pub(super) fn only_reads(call: &Value) -> bool {
    let name = call["toolName"].as_str().unwrap_or_default();
    matches!(
        ToolName::parse(name),
        Some(
            ToolName::ReadFile
                | ToolName::ListDirectory
                | ToolName::FindFiles
                | ToolName::SearchFiles
                | ToolName::WebFetch
        )
    )
}

impl Session {
    /// Answers `calls` (all [`only_reads`]): checks each in order, runs the
    /// allowed ones together, then finishes each in order. A call whose
    /// result is in `answers` is done; when the turn stops mid-way, reads
    /// that already finished keep their results and the caller answers the
    /// rest as interrupted.
    pub(super) async fn answer_reads(
        &mut self,
        run_id: &str,
        calls: &[Value],
        approver: &dyn Approver,
        on_event: &mut (dyn FnMut(AssistantEvent) + Send),
        answers: &mut [Option<ToolOutput>],
    ) {
        let interrupt = self.interrupt.clone();
        let mut allowed = Vec::new();
        for (slot, call) in calls.iter().enumerate() {
            if interrupt.is_requested() {
                return;
            }
            let tool_name = call["toolName"].as_str().unwrap_or_default();
            if let Err(refused) = self.admit(tool_name, &call["input"]) {
                answers[slot] = Some(refused);
                continue;
            }
            let asked_by = self.subagent_id(call);
            let mut sink = routed(call, on_event);
            let checking = self.authorize(
                run_id,
                tool_name,
                &call["input"],
                asked_by,
                approver,
                &mut sink,
            );
            let Some(checked) = interrupt.unless_stopped(checking).await else {
                return;
            };
            match checked {
                Ok(authorized) => {
                    authorized.announce(&mut sink);
                    allowed.push((slot, authorized));
                }
                Err(out) => answers[slot] = Some(out),
            }
        }
        let reads: Vec<(Sandbox, Planned)> = allowed
            .iter()
            .map(|(_, a)| (a.reader.clone(), a.planned.clone()))
            .collect();
        let ran = run_together(&self.edits, reads, &interrupt).await;
        for ((slot, authorized), ran) in allowed.into_iter().zip(ran) {
            let mut sink = routed(&calls[slot], on_event);
            let Some((out, events)) = ran else {
                let summary = &authorized.planned.summary;
                self.audit(
                    run_id,
                    &authorized.tool_name,
                    summary,
                    ("interrupted", None),
                    true,
                );
                continue;
            };
            events.into_iter().for_each(&mut sink);
            answers[slot] = Some(self.finish(run_id, authorized, out, &mut sink).await);
        }
    }
}

/// What became of one read run with others.
#[derive(Debug, PartialEq)]
enum Ran<R> {
    Done(R),
    /// It panicked inside gz.
    Failed,
    /// The turn stopped before it finished.
    Stopped,
}

/// Runs each planned read on its own thread, [`MAX_CONCURRENT`] at a time,
/// and returns the results (and the events each raised) in `planned`'s
/// order; `None` for a read the stop cut short.
async fn run_together(
    edits: &EditHistory,
    reads: Vec<(Sandbox, Planned)>,
    stop: &Interrupt,
) -> Vec<Option<(ToolOutput, Vec<AssistantEvent>)>> {
    let edits = edits.clone();
    let ran = run_bounded(reads, MAX_CONCURRENT, stop, move |(reader, planned)| {
        run_one(&edits, &reader, &planned)
    })
    .await;
    ran.into_iter()
        .map(|result| match result {
            Ran::Done(done) => Some(done),
            Ran::Failed => {
                let out = ToolOutput::error("gz failed while running this call; try it again");
                Some((out, Vec::new()))
            }
            Ran::Stopped => None,
        })
        .collect()
}

/// Runs `work` on every item on blocking threads, at most `limit` at once;
/// results come back in the items' order. Once `stop` is requested it
/// returns at once: items not finished by then are [`Ran::Stopped`] (a
/// thread already running is left to end on its own, its result unused).
async fn run_bounded<T, R>(
    items: Vec<T>,
    limit: usize,
    stop: &Interrupt,
    work: impl Fn(T) -> R + Send + Sync + 'static,
) -> Vec<Ran<R>>
where
    T: Send + 'static,
    R: Send + 'static,
{
    let work = std::sync::Arc::new(work);
    let mut done: Vec<Ran<R>> = items.iter().map(|_| Ran::Stopped).collect();
    let mut waiting = items.into_iter().enumerate();
    let mut running = JoinSet::new();
    loop {
        while running.len() < limit && !stop.is_requested() {
            let Some((index, item)) = waiting.next() else {
                break;
            };
            let work = work.clone();
            running.spawn_blocking(move || {
                let result = std::panic::catch_unwind(AssertUnwindSafe(|| work(item)));
                (index, result)
            });
        }
        let Some(Some(finished)) = stop.unless_stopped(running.join_next()).await else {
            break;
        };
        // Panics are caught inside the task, and nothing aborts the set
        // while it is polled here, so a join error cannot happen.
        let Ok((index, result)) = finished else {
            continue;
        };
        done[index] = match result {
            Ok(result) => Ran::Done(result),
            Err(_) => {
                crate::output::warn("a read failed inside gz");
                Ran::Failed
            }
        };
    }
    done
}

/// One read, on a blocking thread: file reads and searches are plain
/// blocking I/O, so each gets its own thread rather than holding up the
/// runtime.
fn run_one(
    edits: &EditHistory,
    sandbox: &Sandbox,
    planned: &Planned,
) -> (ToolOutput, Vec<AssistantEvent>) {
    let mut events = Vec::new();
    let mut collect = |event| events.push(event);
    let run = checkpoints::run_tool(edits, sandbox, planned, &mut collect);
    let out = tokio::runtime::Handle::current().block_on(run);
    (out, events)
}

/// `on_event`, or for a call a subagent asked for, events sent to that
/// subagent's page.
pub(super) fn routed<'a>(
    call: &Value,
    on_event: &'a mut (dyn FnMut(AssistantEvent) + Send),
) -> impl FnMut(AssistantEvent) + Send + 'a {
    let page = subagents::asked_by(call);
    move |event| match &page {
        Some(id) => on_event(AssistantEvent::ForSubagent {
            id: id.clone(),
            event: Box::new(event),
        }),
        None => on_event(event),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reads_run_together_within_the_limit_and_keep_their_order() {
        let started = Instant::now();
        // Later items finish first; the results still follow the input.
        let waits = vec![300u64, 200, 100, 50];
        let done = run_bounded(waits, 4, &Interrupt::default(), |ms| {
            std::thread::sleep(Duration::from_millis(ms));
            ms
        })
        .await;
        let done: Vec<Ran<u64>> = done;
        assert_eq!(done, [300, 200, 100, 50].map(Ran::Done));
        assert!(
            started.elapsed() < Duration::from_millis(550),
            "ran one after another: {:?}",
            started.elapsed()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_more_than_the_limit_run_at_once() {
        let busy = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let most = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (b, m) = (busy.clone(), most.clone());
        run_bounded(
            (0..10).collect::<Vec<u32>>(),
            3,
            &Interrupt::default(),
            move |_| {
                use std::sync::atomic::Ordering::SeqCst;
                let now = b.fetch_add(1, SeqCst) + 1;
                m.fetch_max(now, SeqCst);
                std::thread::sleep(Duration::from_millis(30));
                b.fetch_sub(1, SeqCst);
            },
        )
        .await;
        assert!(most.load(std::sync::atomic::Ordering::SeqCst) <= 3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stop_returns_at_once_keeping_the_reads_already_done() {
        let stop = Interrupt::default();
        let raise = stop.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            raise.request();
        });
        let started = Instant::now();
        let done = run_bounded(vec![20u64, 3000, 20], 2, &stop, |ms| {
            std::thread::sleep(Duration::from_millis(ms));
            ms
        })
        .await;
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(done, vec![Ran::Done(20), Ran::Stopped, Ran::Done(20)]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_read_that_panics_fails_alone() {
        let done = run_bounded(vec![1u32, 2, 3], 3, &Interrupt::default(), |n| {
            assert_ne!(n, 2, "boom");
            n
        })
        .await;
        assert_eq!(done, vec![Ran::Done(1), Ran::Failed, Ran::Done(3)]);
    }

    #[test]
    fn only_tools_that_just_read_run_together() {
        let call = |name: &str| serde_json::json!({"toolName": name});
        for name in [
            "read_file",
            "list_directory",
            "find_files",
            "search_files",
            "web_fetch",
        ] {
            assert!(only_reads(&call(name)), "{name}");
        }
        for name in [
            "edit_file",
            "write_file",
            "run_command",
            "run_script",
            "notebook_edit",
            "ask_user_question",
            "memory",
            "change_directory",
            "local_mcp_call",
        ] {
            assert!(!only_reads(&call(name)), "{name}");
        }
    }

    struct ApproveAll;

    impl Approver for ApproveAll {
        fn decide<'a>(
            &'a self,
            _: &'a crate::assistant::permissions::PermissionRequest,
        ) -> crate::assistant::permissions::DecisionFuture<'a> {
            Box::pin(async { crate::assistant::permissions::Decision::AllowOnce })
        }
    }

    #[tokio::test]
    async fn a_mixed_batch_answers_every_call_in_the_order_asked() {
        use crate::assistant::session::stub_chat::{self, call, events, Answer};
        use serde_json::json;
        let calls = [
            call("i1", "read_file", json!({"path": "a.txt"})),
            call("i2", "read_file", json!({"path": "b.txt"})),
            call(
                "i3",
                "write_file",
                json!({"path": "c.txt", "content": "new"}),
            ),
            call("i4", "read_file", json!({"path": "c.txt"})),
            call("i5", "list_directory", json!({"path": "."})),
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
            (
                "GET /chat/runs/r2/events",
                Answer::Events(events(&[], "completed")),
            ),
        ]);
        let (mut session, dir) = stub_chat::session(base, "parallel-batch");
        std::fs::write(dir.join("a.txt"), "alpha\n").unwrap();
        std::fs::write(dir.join("b.txt"), "beta\n").unwrap();
        std::fs::remove_file(dir.join("c.txt")).ok();
        let mut finished = Vec::new();
        let end = session
            .send("go", &ApproveAll, &mut |event| {
                if let AssistantEvent::ToolFinished { summary, .. } = event {
                    finished.push(summary);
                }
            })
            .await
            .unwrap();
        assert_eq!(end, crate::assistant::session::TurnEnd::Completed);
        let body = seen.body("POST /chat/runs/r1/client-tools/resume").unwrap();
        let results = body["toolResults"].as_array().unwrap();
        let ids: Vec<&str> = results
            .iter()
            .map(|r| r["interruptId"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["i1", "i2", "i3", "i4", "i5"]);
        let output = |i: usize| results[i]["result"]["output"].as_str().unwrap().to_string();
        assert!(output(0).contains("alpha"), "{}", output(0));
        assert!(output(1).contains("beta"), "{}", output(1));
        assert!(
            output(3).contains("new"),
            "the read after the write sees it: {}",
            output(3)
        );
        assert!(output(4).contains("c.txt"), "{}", output(4));
        assert_eq!(finished.len(), 5, "{finished:?}");
        assert!(
            finished[0].contains("a.txt") && finished[1].contains("b.txt"),
            "{finished:?}"
        );
        std::fs::remove_file(dir.join("c.txt")).ok();
    }

    /// The tool events, as "started a.txt" / "finished a.txt".
    fn tool_events(event: AssistantEvent, log: &mut Vec<String>) {
        match event {
            AssistantEvent::ToolStarted { summary, .. } => log.push(format!("started {summary}")),
            AssistantEvent::ToolFinished { summary, .. } => log.push(format!("finished {summary}")),
            _ => {}
        }
    }

    #[tokio::test]
    async fn a_batch_of_reads_starts_together_and_finishes_in_order() {
        use crate::assistant::session::stub_chat::{self, call, events, Answer};
        use serde_json::json;
        let calls = [
            call("i1", "read_file", json!({"path": "a.txt"})),
            call("i2", "read_file", json!({"path": "b.txt"})),
            call("i3", "list_directory", json!({"path": "."})),
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
            (
                "GET /chat/runs/r2/events",
                Answer::Events(events(&[], "completed")),
            ),
        ]);
        let (mut session, dir) = stub_chat::session(base, "parallel-order");
        std::fs::write(dir.join("a.txt"), "alpha\n").unwrap();
        std::fs::write(dir.join("b.txt"), "beta\n").unwrap();
        let mut log = Vec::new();
        session
            .send("go", &ApproveAll, &mut |event| tool_events(event, &mut log))
            .await
            .unwrap();
        let kinds: Vec<&str> = log.iter().map(|l| l.split(' ').next().unwrap()).collect();
        assert_eq!(
            kinds,
            ["started", "started", "started", "finished", "finished", "finished"],
            "every read starts before any finishes (one after another would \
             alternate): {log:?}"
        );
        assert!(
            log[3].contains("a.txt") && log[4].contains("b.txt"),
            "{log:?}"
        );
    }

    /// Allows the first request; leaves every later prompt open.
    struct FirstOnly(std::sync::atomic::AtomicBool);

    impl Approver for FirstOnly {
        fn decide<'a>(
            &'a self,
            _: &'a crate::assistant::permissions::PermissionRequest,
        ) -> crate::assistant::permissions::DecisionFuture<'a> {
            let first = !self.0.swap(true, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move {
                if first {
                    crate::assistant::permissions::Decision::AllowOnce
                } else {
                    std::future::pending().await
                }
            })
        }
    }

    #[tokio::test]
    async fn a_stop_during_a_read_batch_answers_every_read_as_interrupted() {
        use crate::assistant::interrupt::INTERRUPTED;
        use crate::assistant::session::stub_chat::{self, call, events, Answer};
        use serde_json::json;
        // Reads in the open folders never ask, so an ask rule makes each
        // one ask; the approver allows the first once and leaves the second
        // prompt open until the stop.
        let calls = [
            call("i1", "read_file", json!({"path": "a.txt"})),
            call("i2", "read_file", json!({"path": "sub/b.txt"})),
            call("i3", "read_file", json!({"path": "a.txt"})),
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
        let (mut session, dir) = stub_chat::session(base, "parallel-stop");
        std::fs::write(dir.join("a.txt"), "alpha\n").unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/b.txt"), "beta\n").unwrap();
        let rules = dir.join("rules-user.toml");
        std::fs::write(&rules, "ask = [\"read_file\"]").unwrap();
        let places = crate::assistant::rules::Places {
            folder: dir.clone(),
            home: None,
            user_file: Some(rules),
            trust: None,
        };
        session
            .policy
            .set_rules(crate::assistant::rules::RuleSet::load(
                places,
                crate::assistant::rules::FlagRules::default(),
            ));
        let canceller = session.canceller();
        canceller.expect_turn();
        let stop = async {
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            canceller.interrupt();
        };
        let approver = FirstOnly(Default::default());
        let mut ignore = |_| {};
        let (end, ()) = tokio::join!(session.send("go", &approver, &mut ignore), stop);
        assert_eq!(end.unwrap(), crate::assistant::session::TurnEnd::Cancelled);
        let body = seen.body("POST /chat/runs/r1/client-tools/resume").unwrap();
        let results = body["toolResults"].as_array().unwrap();
        assert_eq!(results.len(), 3);
        for result in results {
            assert_eq!(result["result"]["output"], INTERRUPTED, "{result}");
        }
        assert!(seen.has("POST /chat/runs/r2/cancel"), "{:?}", seen.lines());
    }
}
