//! How a session hands checker problems for its edits to the model.

use std::os::unix::fs::PermissionsExt;

use super::*;
use crate::assistant::diagnostics::{settings::Settings, Diagnostics};
use crate::assistant::permissions::DecisionFuture;
use crate::assistant::sandbox::tests::temp_dir;

struct AllowAll;

impl Approver for AllowAll {
    fn decide<'a>(&'a self, _: &'a PermissionRequest) -> DecisionFuture<'a> {
        Box::pin(async { Decision::AllowOnce })
    }
}

/// A session in `dir` whose diagnostics run a fake `ruff` reporting every
/// line with `BAD` on it.
fn checked_session(dir: &Path) -> Session {
    let ruff = "for a; do f=$a; done\ngrep -n BAD \"$f\" | cut -d: -f1 | while read n; do echo \"$f:$n:1: E9 bad\"; done\n";
    session_with_ruff(dir, ruff, Settings::default())
}

/// A session in `dir` whose diagnostics run a fake `ruff` with `body` as
/// its shell script.
fn session_with_ruff(dir: &Path, body: &str, settings: Settings) -> Session {
    let bin = temp_dir("session-diag-bin");
    let ruff = bin.join("ruff");
    std::fs::write(&ruff, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(&ruff, std::fs::Permissions::from_mode(0o755)).unwrap();
    let resolved = crate::config::resolve(
        &crate::config::Config::default(),
        Some("diag-test"),
        Some("http://127.0.0.1:9".into()),
        Some("stub-token".to_string()),
    );
    let http = ApiClient::new(resolved, crate::output::OutputFormat::Json, true, 0, 5).unwrap();
    let mut session = Session::new(http, dir).unwrap();
    session.audit = None;
    session.history = None;
    session.set_diagnostics(Diagnostics::with(settings, vec![bin], None));
    // These tests are not about the read-before-edit rule.
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        if entry.path().is_file() {
            crate::assistant::sandbox::tests::seen_by_model(&session.sandbox, &entry.path());
        }
    }
    session
}

#[tokio::test]
async fn an_edit_result_carries_the_problems_it_introduced() {
    let dir = temp_dir("session-diag");
    std::fs::write(dir.join("app.py"), "x = 1\nBAD kept\n").unwrap();
    let mut session = checked_session(&dir);
    let mut shown = Vec::new();
    let input = json!({"path": "app.py", "old_text": "x = 1", "new_text": "BAD added"});
    let out = session
        .answer_one(
            "run_1",
            "edit_file",
            &input,
            None,
            &AllowAll,
            &mut |event| {
                if let AssistantEvent::ToolFinished { output, .. } = event {
                    shown.push(output);
                }
            },
        )
        .await;
    assert!(!out.is_error, "{}", out.output);
    assert!(
        out.output
            .ends_with("Diagnostics after this edit: ruff found 1 new problem in app.py:\napp.py:1:1: E9 bad\n"),
        "{}",
        out.output
    );
    assert_eq!(shown, [out.output.clone()], "the transcript shows it too");
}

#[tokio::test]
async fn a_written_file_is_checked_and_reads_are_not() {
    let dir = temp_dir("session-diag-write");
    let mut session = checked_session(&dir);
    let input = json!({"path": "new.py", "content": "BAD\n"});
    let out = session
        .answer_one("run_1", "write_file", &input, None, &AllowAll, &mut |_| {})
        .await;
    assert!(
        out.output.contains("ruff found 1 problem in new.py"),
        "{}",
        out.output
    );
    let read = json!({"path": "new.py"});
    let out = session
        .answer_one("run_1", "read_file", &read, None, &AllowAll, &mut |_| {})
        .await;
    assert!(!out.output.contains("Diagnostics"), "{}", out.output);
}

#[tokio::test]
async fn a_script_reports_each_edited_file_once_at_its_end() {
    if !std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        eprintln!("skipped: python3 not installed");
        return;
    }
    let dir = temp_dir("session-diag-script");
    std::fs::write(dir.join("a.py"), "BAD old\nx = 1\n").unwrap();
    let mut session = checked_session(&dir);
    let code = "import gz\n\
                gz.edit_file(path='a.py', old_text='x = 1', new_text='BAD one')\n\
                gz.write_file(path='b.py', content='y = 2\\n')\n\
                gz.edit_file(path='a.py', old_text='BAD one', new_text='BAD two')\n\
                print('done')\n";
    let mut notices = Vec::new();
    let out = session
        .answer_one(
            "run_1",
            "run_script",
            &json!({"language": "python", "code": code}),
            None,
            &AllowAll,
            &mut |event| {
                if let AssistantEvent::Notice(text) = event {
                    notices.push(text);
                }
            },
        )
        .await;
    assert!(
        out.output
            .contains("ruff found 1 new problem in a.py:\na.py:2:1: E9 bad\n"),
        "{}",
        out.output
    );
    assert_eq!(
        out.output.matches("Diagnostics after this edit").count(),
        1,
        "{}",
        out.output
    );
    assert_eq!(
        notices.iter().filter(|n| n.contains("ruff found")).count(),
        1,
        "{notices:?}"
    );
}

#[tokio::test]
async fn esc_skips_the_checks_of_a_script_it_stopped() {
    if !std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        eprintln!("skipped: python3 not installed");
        return;
    }
    let dir = temp_dir("session-diag-stop");
    let settings = Settings {
        timeout: std::time::Duration::from_secs(60),
        ..Settings::default()
    };
    let mut session = session_with_ruff(&dir, "sleep 30\n", settings);
    let code = "import gz, time\n\
                gz.write_file(path='b.py', content='y = 2\\n')\n\
                print('written', flush=True)\n\
                time.sleep(30)\n";
    let interrupt = session.interrupt.clone();
    let stop = async {
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        interrupt.request();
    };
    let input = json!({"language": "python", "code": code});
    let mut ignore = |_| {};
    let started = std::time::Instant::now();
    let answer = session.answer_one("run_1", "run_script", &input, None, &AllowAll, &mut ignore);
    let (out, ()) = tokio::join!(answer, stop);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(8),
        "the stop waited for the checker: {:?}",
        started.elapsed()
    );
    assert!(
        out.output
            .contains(crate::assistant::interrupt::INTERRUPTED),
        "{}",
        out.output
    );
    assert!(!out.output.contains("Diagnostics"), "{}", out.output);
}
