use std::os::unix::fs::PermissionsExt;

use serde_json::json;

use super::*;
use crate::assistant::sandbox::tests::temp_workspace;
use crate::assistant::sandbox::Sandbox;
use crate::assistant::tools::{self, ToolName};

/// A fake `ruff` that reports every line containing `BAD`.
const FAKE_RUFF: &str = r#"#!/bin/sh
for arg; do file=$arg; done
found=$(grep -n BAD "$file" | cut -d: -f1)
[ -z "$found" ] && exit 0
for n in $found; do echo "$file:$n:1: E999 bad thing"; done
exit 1
"#;

fn bin_with(name: &str, script: &str) -> PathBuf {
    let bin = crate::assistant::sandbox::tests::temp_dir("diag-fake-bin");
    let path = bin.join(name);
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

fn with_ruff(script: &str, settings: Settings) -> Diagnostics {
    Diagnostics::with(settings, vec![bin_with("ruff", script)], None)
}

fn write_plan(sandbox: &Sandbox, path: &str, content: &str) -> Planned {
    tools::plan(
        sandbox,
        ToolName::WriteFile,
        &json!({"path": path, "content": content}),
    )
    .unwrap()
}

/// Runs a write through diagnostics like the session does; returns the
/// result and the notices.
async fn write_checked(
    diagnostics: &Diagnostics,
    sandbox: &Sandbox,
    path: &str,
    content: &str,
) -> (ToolOutput, Vec<String>) {
    let planned = write_plan(sandbox, path, content);
    let mut notices = Vec::new();
    let mut on_event = |event: AssistantEvent| {
        if let AssistantEvent::Notice(text) = event {
            notices.push(text);
        }
    };
    let pending = diagnostics
        .before(
            &planned,
            &sandbox.cwd(),
            |file| sandbox.display(file),
            &mut on_event,
        )
        .await;
    let out = tools::run(sandbox, &planned).await;
    let out = match pending {
        Some(pending) => pending.after(out, &mut on_event).await,
        None => out,
    };
    (out, notices)
}

#[tokio::test]
async fn only_problems_the_edit_added_reach_the_model() {
    let (dir, sandbox) = temp_workspace("diag-new");
    std::fs::write(dir.join("app.py"), "x = 1\nBAD old\n").unwrap();
    let diagnostics = with_ruff(FAKE_RUFF, Settings::default());
    let (out, notices) = write_checked(
        &diagnostics,
        &sandbox,
        "app.py",
        "BAD new\nx = 1\nBAD old\n",
    )
    .await;
    assert!(!out.is_error, "{}", out.output);
    assert!(
        out.output.contains(
            "Diagnostics after this edit: ruff found 1 new problem in app.py:\napp.py:1:1: E999 bad thing\n"
        ),
        "{}",
        out.output
    );
    assert!(
        !out.output.contains("app.py:3:1"),
        "the old problem is not new"
    );
    assert_eq!(
        notices,
        ["ruff found 1 new problem in app.py; gz passed them to the model"]
    );
}

#[tokio::test]
async fn a_clean_edit_adds_nothing() {
    let (dir, sandbox) = temp_workspace("diag-clean");
    std::fs::write(dir.join("app.py"), "x = 1\n").unwrap();
    let diagnostics = with_ruff(FAKE_RUFF, Settings::default());
    let plain = tools::run(&sandbox, &write_plan(&sandbox, "other.txt", "y")).await;
    let (out, notices) = write_checked(&diagnostics, &sandbox, "app.py", "x = 2\n").await;
    assert!(!out.output.contains("Diagnostics"), "{}", out.output);
    assert_eq!(out.output.replace("app.py", "other.txt"), plain.output);
    assert!(notices.is_empty());
}

#[tokio::test]
async fn a_new_file_reports_every_problem() {
    let (_dir, sandbox) = temp_workspace("diag-created");
    let diagnostics = with_ruff(FAKE_RUFF, Settings::default());
    let (out, _) = write_checked(&diagnostics, &sandbox, "pkg/new.py", "BAD\nBAD\n").await;
    assert!(
        out.output.contains(
            "ruff found 2 problems in pkg/new.py:\npkg/new.py:1:1: E999 bad thing\npkg/new.py:2:1:"
        ),
        "{}",
        out.output
    );
}

#[tokio::test]
async fn long_lists_are_capped() {
    let (_dir, sandbox) = temp_workspace("diag-capped");
    let diagnostics = with_ruff(FAKE_RUFF, Settings::default());
    let content = "BAD\n".repeat(MAX_SHOWN + 5);
    let (out, _) = write_checked(&diagnostics, &sandbox, "many.py", &content).await;
    assert_eq!(out.output.matches("E999 bad thing").count(), MAX_SHOWN);
    assert!(
        out.output
            .contains("… and 5 more; fix these and the next check shows the rest"),
        "{}",
        out.output
    );
}

#[tokio::test]
async fn a_hung_checker_is_stopped_at_its_timeout_and_the_edit_goes_through() {
    let (dir, sandbox) = temp_workspace("diag-hung");
    let settings = Settings {
        timeout: Duration::from_millis(400),
        ..Settings::default()
    };
    let diagnostics = with_ruff("#!/bin/sh\nsleep 30\n", settings);
    let started = std::time::Instant::now();
    let (out, notices) = write_checked(&diagnostics, &sandbox, "slow.py", "x = 1\n").await;
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "took {:?}",
        started.elapsed()
    );
    assert!(!out.is_error);
    assert!(!out.output.contains("Diagnostics"));
    assert_eq!(
        std::fs::read_to_string(dir.join("slow.py")).unwrap(),
        "x = 1\n"
    );
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(
        notices[0].starts_with("ruff did not finish checking slow.py within 0.4 s"),
        "{}",
        notices[0]
    );
    assert!(notices[0].contains("diagnostics.toml"));
}

#[tokio::test]
async fn a_checker_that_keeps_timing_out_stops_holding_up_edits() {
    let (dir, sandbox) = temp_workspace("diag-backoff");
    std::fs::write(dir.join("a.py"), "x = 1\n").unwrap();
    let settings = Settings {
        timeout: Duration::from_millis(300),
        ..Settings::default()
    };
    let diagnostics = with_ruff("#!/bin/sh\nsleep 30\n", settings);
    // Before and after the first edit: both time out.
    let (_, first) = write_checked(&diagnostics, &sandbox, "a.py", "x = 2\n").await;
    assert_eq!(
        first.len(),
        1,
        "the run before the edit is not told: {first:?}"
    );
    assert!(
        first[0].contains("gz now runs it only after edits"),
        "{}",
        first[0]
    );
    // The second edit skips the run before it; its run after times out.
    let started = std::time::Instant::now();
    let (_, second) = write_checked(&diagnostics, &sandbox, "a.py", "x = 3\n").await;
    assert!(started.elapsed() < Duration::from_millis(1_500));
    assert_eq!(second.len(), 1, "{second:?}");
    assert!(
        second[0].contains("gz stops running it for this session"),
        "{}",
        second[0]
    );
    // Off now: the third edit runs nothing and says nothing.
    let started = std::time::Instant::now();
    let (out, third) = write_checked(&diagnostics, &sandbox, "a.py", "x = 4\n").await;
    assert!(
        started.elapsed() < Duration::from_millis(250),
        "{:?}",
        started.elapsed()
    );
    assert!(third.is_empty(), "{third:?}");
    assert!(!out.is_error);
}

#[tokio::test]
async fn a_broken_checker_tells_the_user_not_the_model() {
    let (_dir, sandbox) = temp_workspace("diag-broken");
    let diagnostics = with_ruff(
        "#!/bin/sh\necho 'ruff: error: invalid pyproject.toml' >&2\nexit 2\n",
        Settings::default(),
    );
    let (out, notices) = write_checked(&diagnostics, &sandbox, "a.py", "x = 1\n").await;
    assert!(!out.output.contains("Diagnostics"));
    assert_eq!(
        notices,
        ["ruff could not check a.py (exit 2): ruff: error: invalid pyproject.toml; fix the checker or its entry in diagnostics.toml"]
    );
}

#[tokio::test]
async fn the_off_switch_and_other_tools_skip_checking() {
    let (_dir, sandbox) = temp_workspace("diag-off");
    let off = with_ruff(
        FAKE_RUFF,
        Settings {
            enabled: false,
            ..Settings::default()
        },
    );
    let (out, _) = write_checked(&off, &sandbox, "a.py", "BAD\n").await;
    assert!(!out.output.contains("Diagnostics"), "{}", out.output);
    let (out, _) = write_checked(&Diagnostics::default(), &sandbox, "b.py", "BAD\n").await;
    assert!(!out.output.contains("Diagnostics"));
    let on = with_ruff(FAKE_RUFF, Settings::default());
    let read = tools::plan(&sandbox, ToolName::ReadFile, &json!({"path": "a.py"})).unwrap();
    let pending = on
        .before(
            &read,
            &sandbox.cwd(),
            |file| sandbox.display(file),
            &mut |_| {},
        )
        .await;
    assert!(pending.is_none(), "reads are not checked");
}

#[tokio::test]
async fn a_failed_edit_is_not_checked() {
    let (_dir, sandbox) = temp_workspace("diag-failed");
    let diagnostics = with_ruff(FAKE_RUFF, Settings::default());
    let planned = write_plan(&sandbox, "a.py", "BAD\n");
    let pending = diagnostics
        .before(
            &planned,
            &sandbox.cwd(),
            |file| sandbox.display(file),
            &mut |_| {},
        )
        .await
        .unwrap();
    let failed = ToolOutput::error("disk full");
    let out = pending.after(failed.clone(), &mut |_| {}).await;
    assert_eq!(out, failed);
}

#[tokio::test]
async fn a_broken_settings_file_is_told_once() {
    let (_dir, sandbox) = temp_workspace("diag-told");
    let diagnostics = Diagnostics::with(
        Settings::default(),
        Vec::new(),
        Some("diagnostics.toml has a problem".into()),
    );
    let (_, first) = write_checked(&diagnostics, &sandbox, "a.py", "x\n").await;
    let (_, second) = write_checked(&diagnostics, &sandbox, "a.py", "y\n").await;
    assert_eq!(first, ["diagnostics.toml has a problem"]);
    assert!(second.is_empty());
}

#[tokio::test]
async fn notebook_edits_are_checked_too() {
    let (dir, sandbox) = temp_workspace("diag-notebook");
    let notebook = json!({
        "cells": [{"id": "c1", "cell_type": "code", "source": ["x = 1\n"], "metadata": {}, "outputs": [], "execution_count": null}],
        "metadata": {}, "nbformat": 4, "nbformat_minor": 5
    });
    std::fs::write(dir.join("n.ipynb"), notebook.to_string()).unwrap();
    let planned = tools::plan(
        &sandbox,
        ToolName::NotebookEdit,
        &json!({"path": "n.ipynb", "cell_id": "c1", "new_source": "BAD = 1\n", "edit_mode": "replace"}),
    );
    let Ok(planned) = planned else {
        panic!("notebook plan failed: {:#}", planned.unwrap_err());
    };
    // ruff names the cell after the path: `n.ipynb:cell 1:1:1: …`.
    let diagnostics = with_ruff(
        "#!/bin/sh\nfor arg; do file=$arg; done\ngrep -q BAD \"$file\" || exit 0\necho \"$file:cell 1:1:1: F821 Undefined name BAD\"\nexit 1\n",
        Settings::default(),
    );
    let mut notices = Vec::new();
    let mut on_event = |event: AssistantEvent| {
        if let AssistantEvent::Notice(text) = event {
            notices.push(text);
        }
    };
    let pending = diagnostics
        .before(
            &planned,
            &sandbox.cwd(),
            |file| sandbox.display(file),
            &mut on_event,
        )
        .await
        .expect("ruff checks notebooks");
    let out = tools::run(&sandbox, &planned).await;
    let out = pending.after(out, &mut on_event).await;
    assert!(
        out.output
            .contains("n.ipynb cell 1 line 1 col 1: F821 Undefined name BAD"),
        "{}",
        out.output
    );
    assert_eq!(
        notices,
        ["ruff found 1 new problem in n.ipynb; gz passed them to the model"]
    );
}
