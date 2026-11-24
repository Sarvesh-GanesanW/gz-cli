//! How the session runs what [`crate::assistant::read_only`] lets through
//! without asking.

use super::*;
use crate::assistant::permissions::DecisionFuture;
use crate::assistant::read_only::SCRIPT_WROTE_NOTE;
use crate::assistant::sandbox::tests::temp_dir;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Denies every prompt and counts them.
#[derive(Default)]
struct DenyAll {
    asked: AtomicUsize,
    accepts_edits: bool,
}

impl Approver for DenyAll {
    fn decide<'a>(&'a self, _: &'a PermissionRequest) -> DecisionFuture<'a> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Decision::Deny })
    }

    fn accepts_edits(&self) -> bool {
        self.accepts_edits
    }
}

fn session_in(dir: &Path) -> Session {
    let resolved = crate::config::resolve(
        &crate::config::Config::default(),
        Some("read-only-test"),
        Some("http://127.0.0.1:9".into()),
        Some("stub-token".to_string()),
    );
    let http = ApiClient::new(resolved, crate::output::OutputFormat::Json, true, 0, 5).unwrap();
    let mut session = Session::new(http, dir).unwrap();
    session.audit = None;
    session.history = None;
    session
}

fn sandbox_here() -> bool {
    !matches!(
        crate::assistant::os_sandbox::mechanism(),
        crate::assistant::os_sandbox::Mechanism::Unavailable(_)
    )
}

async fn call(session: &mut Session, approver: &DenyAll, tool: &str, input: Value) -> ToolOutput {
    session
        .answer_one("run_1", tool, &input, None, approver, &mut |_| {})
        .await
}

#[tokio::test]
async fn reads_in_the_open_folders_never_prompt() {
    let dir = temp_dir("session-ro-reads");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/a.txt"), "hello\n").unwrap();
    let mut session = session_in(&dir);
    let approver = DenyAll::default();
    let read = call(
        &mut session,
        &approver,
        "read_file",
        json!({"path": "src/a.txt"}),
    )
    .await;
    assert!(
        !read.is_error && read.output.contains("hello"),
        "{}",
        read.output
    );
    let listed = call(
        &mut session,
        &approver,
        "list_directory",
        json!({"path": "src"}),
    )
    .await;
    assert!(!listed.is_error, "{}", listed.output);
    assert_eq!(approver.asked.load(Ordering::SeqCst), 0);
    let secret = call(
        &mut session,
        &approver,
        "read_file",
        json!({"path": ".env"}),
    )
    .await;
    assert!(secret.is_error, "secret files stay blocked");
}

#[tokio::test]
async fn a_read_only_command_runs_without_a_prompt_and_cannot_write() {
    if !sandbox_here() {
        eprintln!("skipped: no OS sandbox here");
        return;
    }
    let dir = temp_dir("session-ro-command");
    std::fs::write(dir.join("a.txt"), "hello\n").unwrap();
    let mut session = session_in(&dir);
    let approver = DenyAll::default();
    let out = call(
        &mut session,
        &approver,
        "run_command",
        json!({"command": "cat a.txt"}),
    )
    .await;
    assert!(
        !out.is_error && out.output.contains("hello"),
        "{}",
        out.output
    );
    assert_eq!(approver.asked.load(Ordering::SeqCst), 0);
    let write = call(
        &mut session,
        &approver,
        "run_command",
        json!({"command": "touch b.txt"}),
    )
    .await;
    assert!(
        write.is_error,
        "touch asks outside accept-edits: {}",
        write.output
    );
    assert_eq!(approver.asked.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn accept_edits_runs_safe_file_commands_in_the_folder() {
    if !sandbox_here() {
        eprintln!("skipped: no OS sandbox here");
        return;
    }
    let dir = temp_dir("session-ro-files");
    let mut session = session_in(&dir);
    let approver = DenyAll {
        accepts_edits: true,
        ..DenyAll::default()
    };
    let out = call(
        &mut session,
        &approver,
        "run_command",
        json!({"command": "mkdir -p x/y"}),
    )
    .await;
    assert!(!out.is_error, "{}", out.output);
    assert!(dir.join("x/y").is_dir());
    let rm = call(
        &mut session,
        &approver,
        "run_command",
        json!({"command": "rm -r x"}),
    )
    .await;
    assert!(rm.is_error, "rm still asks");
    assert!(dir.join("x/y").is_dir());
    assert_eq!(approver.asked.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_script_that_writes_runs_read_only_then_asks() {
    let has_python = std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    let scripts = session_in(&temp_dir("session-ro-probe"))
        .jobs
        .confinement()
        .and_then(|c| c.read_only_support())
        .is_some_and(|s| s.scripts);
    if !has_python || !scripts {
        eprintln!("skipped: no python3 or no read-only scripts here");
        return;
    }
    let dir = temp_dir("session-ro-script");
    let mut session = session_in(&dir);
    let approver = DenyAll::default();
    let input = json!({"language": "python", "code": "open('out.txt', 'w').write('x')"});
    let first = call(&mut session, &approver, "run_script", input.clone()).await;
    assert!(
        first.output.ends_with(SCRIPT_WROTE_NOTE),
        "{}",
        first.output
    );
    assert!(!dir.join("out.txt").exists());
    assert_eq!(approver.asked.load(Ordering::SeqCst), 0);
    let second = call(&mut session, &approver, "run_script", input).await;
    assert!(second.is_error);
    assert_eq!(
        approver.asked.load(Ordering::SeqCst),
        1,
        "the second run asks"
    );
}
