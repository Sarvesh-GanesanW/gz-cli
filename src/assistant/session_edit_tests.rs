//! How a session's tool calls reach its edit history (`/rewind`, `/diff`).

use super::*;
use crate::assistant::permissions::DecisionFuture;
use crate::assistant::sandbox::tests::temp_dir;

struct AllowAll;

impl Approver for AllowAll {
    fn decide<'a>(&'a self, _: &'a PermissionRequest) -> DecisionFuture<'a> {
        Box::pin(async { Decision::AllowOnce })
    }
}

/// A session in a fresh folder whose edits go to `history`.
fn session_in(dir: &Path, history: &EditHistory) -> Session {
    let resolved = crate::config::resolve(
        &crate::config::Config::default(),
        Some("edit-test"),
        Some("http://127.0.0.1:9".into()),
        Some("stub-token".to_string()),
    );
    let http = ApiClient::new(resolved, crate::output::OutputFormat::Json, true, 0, 5).unwrap();
    let mut session = Session::new(http, dir)
        .unwrap()
        .with_edit_history(history.clone());
    session.audit = None;
    session.history = None;
    session
}

async fn edit(session: &mut Session, path: &str, old: &str, new: &str) {
    let input = json!({"path": path, "old_text": old, "new_text": new});
    let out = session
        .answer_one("run_1", "edit_file", &input, None, &AllowAll, &mut |_| {})
        .await;
    assert!(!out.is_error, "{}", out.output);
}

#[tokio::test]
async fn a_subagents_edits_join_the_main_conversations_history() {
    let dir = temp_dir("edits-subagent");
    std::fs::write(dir.join("a.txt"), "one\n").unwrap();
    std::fs::write(dir.join("b.txt"), "bee\n").unwrap();
    let history = EditHistory::at(temp_dir("edits-subagent-store"), "main-conv");
    let mut main = session_in(&dir, &history);
    main.edits.begin_turn("fix both files").unwrap();
    edit(&mut main, "a.txt", "one", "two").await;

    // As the launcher starts one: a fresh session sharing the main history.
    let mut child = Session::with_sandbox(main.http(), main.sandbox().fork(), main.local_context())
        .into_subagent("general", "Helper", main.shared_policy())
        .with_edit_history(main.edit_history());
    child.audit = None;
    edit(&mut child, "b.txt", "bee", "wasp").await;

    let messages = history.messages().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(
        (messages[0].excerpt.as_str(), messages[0].files),
        ("fix both files", 2)
    );
    history.rewind(messages[0].turn, false).unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "bee\n");
}

#[tokio::test]
async fn a_later_session_continuing_the_conversation_finds_its_history() {
    let dir = temp_dir("edits-resume");
    std::fs::write(dir.join("a.txt"), "one\n").unwrap();
    let store = temp_dir("edits-resume-store");
    let mut first = session_in(&dir, &EditHistory::at(store.clone(), "unused"));
    first.new_conversation();
    let id = first.conversation_id().to_string();
    first.edits.begin_turn("change a").unwrap();
    edit(&mut first, "a.txt", "one", "two").await;
    drop(first);

    let mut later = session_in(&dir, &EditHistory::at(store, "another"));
    assert!(later.edit_history().messages().unwrap().is_empty());
    later.continue_conversation(&id);
    let messages = later.edit_history().messages().unwrap();
    assert_eq!(
        messages.len(),
        1,
        "gz code -c / resume sees the earlier edits"
    );
    later
        .edit_history()
        .rewind(messages[0].turn, false)
        .unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "one\n");
}

#[tokio::test]
async fn subagent_turns_do_not_start_messages_of_their_own() {
    let dir = temp_dir("edits-subagent-turn");
    let history = EditHistory::at(temp_dir("edits-subagent-turn-store"), "main");
    let main = session_in(&dir, &history);
    main.edits.begin_turn("the user's message").unwrap();
    let mut child = Session::with_sandbox(main.http(), main.sandbox().fork(), main.local_context())
        .into_subagent("general", "Helper", main.shared_policy())
        .with_edit_history(main.edit_history());
    child.audit = None;
    // The run itself fails (no server), after the point a main session
    // would have started a new message.
    let _ = child
        .run_turn("the subagent's task", &AllowAll, &mut |_| {})
        .await;
    std::fs::write(dir.join("c.txt"), "c\n").unwrap();
    edit(&mut child, "c.txt", "c", "see").await;
    assert_eq!(history.messages().unwrap()[0].excerpt, "the user's message");
}

#[tokio::test]
async fn script_helper_writes_are_kept_too() {
    let has_python = std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !has_python {
        return;
    }
    let dir = temp_dir("edits-script");
    std::fs::write(dir.join("a.txt"), "one\n").unwrap();
    let history = EditHistory::at(temp_dir("edits-script-store"), "conv");
    let mut session = session_in(&dir, &history);
    session.edits.begin_turn("script it").unwrap();
    let code = "import gz\ngz.edit_file('a.txt', 'one', 'two')\ngz.write_file('n.txt', 'new')\n";
    let input = json!({"language": "python", "code": code});
    let out = session
        .answer_one("run_1", "run_script", &input, None, &AllowAll, &mut |_| {})
        .await;
    assert!(!out.is_error, "{}", out.output);
    let turn = history.messages().unwrap()[0].turn;
    let done = history.rewind(turn, false).unwrap();
    assert_eq!(done.changed_files(), 2, "{done:?}");
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "one\n");
    assert!(!dir.join("n.txt").exists());
}
