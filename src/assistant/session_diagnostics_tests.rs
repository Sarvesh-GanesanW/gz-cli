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
    let bin = temp_dir("session-diag-bin");
    let ruff = bin.join("ruff");
    std::fs::write(
        &ruff,
        "#!/bin/sh\nfor a; do f=$a; done\ngrep -n BAD \"$f\" | cut -d: -f1 | while read n; do echo \"$f:$n:1: E9 bad\"; done\n",
    )
    .unwrap();
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
    session.set_diagnostics(Diagnostics::with(Settings::default(), vec![bin], None));
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
