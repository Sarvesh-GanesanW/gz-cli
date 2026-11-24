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

/// A session whose user `permissions.toml` holds `toml`.
fn session_with_user_rules(dir: &Path, toml: &str) -> Session {
    use crate::assistant::hooks::trust::TrustStore;
    use crate::assistant::rules::{FlagRules, Places, RuleSet};
    let mut session = session_in(dir);
    let config = temp_dir("session-ro-rules-config");
    std::fs::write(config.join("permissions.toml"), toml).unwrap();
    let places = Places {
        folder: dir.to_path_buf(),
        home: None,
        user_file: Some(config.join("permissions.toml")),
        trust: Some(TrustStore::at(config.join("trust.json"))),
    };
    session
        .policy
        .set_rules(RuleSet::load(places, FlagRules::default()));
    session
}

async fn command(session: &mut Session, approver: &DenyAll, line: &str) -> ToolOutput {
    call(session, approver, "run_command", json!({"command": line})).await
}

#[tokio::test]
async fn a_deny_read_rule_makes_read_only_commands_and_scripts_ask() {
    if !sandbox_here() {
        return;
    }
    let dir = temp_dir("session-ro-deny-read");
    std::fs::create_dir_all(dir.join("secrets")).unwrap();
    std::fs::write(dir.join("secrets/prod.txt"), "TOPSECRET-42\n").unwrap();
    let mut session = session_with_user_rules(&dir, "deny = [\"read(secrets/**)\"]\n");
    let approver = DenyAll::default();
    let mut outputs = Vec::new();
    for line in [
        "cat secrets/prod.txt",
        "grep -r TOPSECRET .",
        "git show HEAD:secrets/prod.txt",
    ] {
        outputs.push(command(&mut session, &approver, line).await);
    }
    let script = json!({"language": "python", "code": "print(open('secrets/prod.txt').read())"});
    outputs.push(call(&mut session, &approver, "run_script", script).await);
    for out in &outputs {
        assert!(!out.output.contains("TOPSECRET-42"), "{}", out.output);
    }
    assert_eq!(approver.asked.load(Ordering::SeqCst), outputs.len());
}

#[tokio::test]
async fn an_ask_read_rule_makes_read_only_commands_ask() {
    if !sandbox_here() {
        return;
    }
    let dir = temp_dir("session-ro-ask-read");
    std::fs::write(dir.join("a.txt"), "hello\n").unwrap();
    let mut session = session_with_user_rules(&dir, "ask = [\"read_file(private/**)\"]\n");
    let approver = DenyAll::default();
    let out = command(&mut session, &approver, "cat a.txt").await;
    assert!(out.is_error, "{}", out.output);
    assert_eq!(approver.asked.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn read_only_commands_still_run_unasked_beside_unrelated_rules() {
    if !sandbox_here() {
        return;
    }
    let dir = temp_dir("session-ro-other-rules");
    std::fs::write(dir.join("a.txt"), "hello\n").unwrap();
    let rules = "deny = [\"run_command(git push:*)\", \"web_fetch(domain:example.com)\"]\n";
    let mut session = session_with_user_rules(&dir, rules);
    let approver = DenyAll::default();
    let out = command(&mut session, &approver, "cat a.txt").await;
    assert!(out.output.contains("hello"), "{}", out.output);
    assert_eq!(approver.asked.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_chain_of_read_only_commands_runs_without_a_prompt() {
    if !sandbox_here() {
        eprintln!("skipped: no OS sandbox here");
        return;
    }
    let dir = temp_dir("session-ro-chain");
    std::fs::write(dir.join("a.txt"), "hello\nworld\n").unwrap();
    let mut session = session_in(&dir);
    let approver = DenyAll::default();
    let line = "cat a.txt && wc -l a.txt 2>/dev/null | head -1; ls nothing-here 2>&1 || echo none";
    let out = command(&mut session, &approver, line).await;
    assert!(
        out.output.contains("hello")
            && out.output.contains("2 a.txt")
            && out.output.contains("none"),
        "{}",
        out.output
    );
    assert_eq!(approver.asked.load(Ordering::SeqCst), 0);
    let write = command(&mut session, &approver, "cat a.txt && touch b.txt").await;
    assert!(write.is_error, "{}", write.output);
    assert_eq!(approver.asked.load(Ordering::SeqCst), 1);
    assert!(!dir.join("b.txt").exists());
}

#[tokio::test]
async fn a_deny_rule_on_one_command_of_a_chain_still_refuses_it() {
    if !sandbox_here() {
        return;
    }
    let dir = temp_dir("session-ro-chain-deny");
    let mut session = session_with_user_rules(&dir, "deny = [\"run_command(git push:*)\"]\n");
    let approver = DenyAll::default();
    let out = command(&mut session, &approver, "git status && git push").await;
    assert!(out.is_error, "{}", out.output);
    assert_eq!(
        approver.asked.load(Ordering::SeqCst),
        0,
        "refused, not asked"
    );
}

#[tokio::test]
async fn a_deny_edit_rule_makes_accept_edits_file_commands_ask() {
    if !sandbox_here() {
        return;
    }
    let dir = temp_dir("session-ro-deny-edit");
    std::fs::create_dir_all(dir.join("config")).unwrap();
    std::fs::write(dir.join("config/prod.toml"), "keep = true\n").unwrap();
    std::fs::write(dir.join("a.txt"), "hello\n").unwrap();
    let mut session = session_with_user_rules(&dir, "deny = [\"edit(config/**)\"]\n");
    let approver = DenyAll {
        accepts_edits: true,
        ..DenyAll::default()
    };
    let lines = [
        "cp a.txt config/new.txt",
        "mv config/prod.toml config/moved.toml",
        "mkdir config/sub",
    ];
    for line in lines {
        let out = command(&mut session, &approver, line).await;
        assert!(out.is_error, "{line}: {}", out.output);
    }
    assert_eq!(approver.asked.load(Ordering::SeqCst), lines.len());
    assert!(dir.join("config/prod.toml").exists());
    assert!(!dir.join("config/new.txt").exists());
    assert!(!dir.join("config/sub").exists());
}

#[test]
fn instruction_files_and_imports_obey_deny_read_rules() {
    use crate::assistant::local_context::Limits;
    let dir = temp_dir("session-ro-instructions");
    std::fs::create_dir_all(dir.join("secrets")).unwrap();
    std::fs::create_dir_all(dir.join("docs")).unwrap();
    std::fs::write(dir.join("secrets/prod.txt"), "TOPSECRET-42\n").unwrap();
    std::fs::write(dir.join("secrets/GZ.md"), "SECRET-FOLDER-RULES\n").unwrap();
    std::fs::write(dir.join("secrets/x.txt"), "x").unwrap();
    std::fs::write(dir.join("docs/GZ.md"), "docs @../secrets/prod.txt\n").unwrap();
    std::fs::write(dir.join("docs/a.txt"), "a").unwrap();
    let session = session_with_user_rules(&dir, "deny = [\"read(secrets/**)\"]\n");
    for touched in ["docs/a.txt", "secrets/x.txt"] {
        session
            .context
            .note_tool_call(&session.sandbox, "read_file", &json!({"path": touched}));
    }
    let found = session
        .context
        .instruction_report(&session.sandbox, &Limits::default());
    let sent: String = found.sent.iter().map(|entry| entry.text.as_str()).collect();
    assert!(sent.contains("docs "), "the docs file still goes: {sent}");
    assert!(!sent.contains("TOPSECRET-42"), "{sent}");
    assert!(!sent.contains("SECRET-FOLDER-RULES"), "{sent}");
    assert!(
        found
            .left_out
            .iter()
            .any(|left| left.why.contains("`read(secrets/**)`")),
        "{:?}",
        found.left_out
    );
}

#[tokio::test]
async fn reads_a_stop_cuts_short_together_are_audited_once_each() {
    let dir = temp_dir("session-ro-stop-reads");
    std::fs::write(dir.join("a.txt"), "a\n").unwrap();
    std::fs::write(dir.join("b.txt"), "b\n").unwrap();
    let mut session = session_in(&dir);
    let log = dir.join(".audit.log");
    session.audit = Some(crate::assistant::audit::AuditLog::at(log.clone()));
    let calls = ["a.txt", "b.txt"]
        .iter()
        .enumerate()
        .map(|(n, path)| {
            json!({"interruptId": format!("i{n}"), "toolName": "read_file", "input": {"path": path}})
        })
        .collect();
    let stop = session.interrupt.clone();
    let mut started = 0;
    // The stop lands once both reads are allowed, before they run.
    let answers = session
        .answer_client_calls("run_1", calls, &DenyAll::default(), &mut |event| {
            if matches!(event, AssistantEvent::ToolStarted { .. }) {
                started += 1;
                if started == 2 {
                    stop.request();
                }
            }
        })
        .await;
    for answer in &answers {
        assert_eq!(answer["result"]["isError"], true, "{answer}");
    }
    let entries: Vec<Value> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let decisions: Vec<&str> = entries
        .iter()
        .map(|entry| entry["decision"].as_str().unwrap())
        .collect();
    assert_eq!(decisions, ["interrupted", "interrupted"], "{entries:?}");
}

/// Denies every prompt, counting them in a shared counter.
struct Counted(std::sync::Arc<AtomicUsize>);

impl Approver for Counted {
    fn decide<'a>(&'a self, _: &'a PermissionRequest) -> DecisionFuture<'a> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Decision::Deny })
    }
}

async fn write_with(session: &mut Session, approver: &dyn Approver, path: &str) -> ToolOutput {
    let input = json!({"path": path, "content": "deny = []\n"});
    session
        .answer_one("run_1", "write_file", &input, None, approver, &mut |_| {})
        .await
}

#[tokio::test]
async fn accept_edits_still_asks_before_editing_gz_project_settings() {
    use crate::cmd::code_rules::apply_mode;
    use crate::code_ui::approver::Mode;
    let dir = temp_dir("session-ro-gz-settings");
    std::fs::create_dir_all(dir.join(".gz")).unwrap();
    let rules = "deny = [\"read(secrets/**)\"]\n";
    std::fs::write(dir.join(".gz/permissions.toml"), rules).unwrap();
    let mut session = session_in(&dir);
    let asked = std::sync::Arc::new(AtomicUsize::new(0));
    let approver = apply_mode(Mode::AcceptEdits, Box::new(Counted(asked.clone())));
    let read = json!({"path": ".gz/permissions.toml"});
    let seen = session
        .answer_one(
            "run_1",
            "read_file",
            &read,
            None,
            approver.as_ref(),
            &mut |_| {},
        )
        .await;
    assert!(!seen.is_error, "{}", seen.output);
    let plain = write_with(&mut session, approver.as_ref(), "notes.toml").await;
    assert!(!plain.is_error, "{}", plain.output);
    assert_eq!(
        asked.load(Ordering::SeqCst),
        0,
        "accept-edits writes unasked"
    );
    for path in [".gz/permissions.toml", ".gz/hooks.toml", ".mcp.json"] {
        let out = write_with(&mut session, approver.as_ref(), path).await;
        assert!(out.is_error, "{path}: {}", out.output);
    }
    assert_eq!(asked.load(Ordering::SeqCst), 3);
    let kept = std::fs::read_to_string(dir.join(".gz/permissions.toml")).unwrap();
    assert_eq!(kept, rules);
}
