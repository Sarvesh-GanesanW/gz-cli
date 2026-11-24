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
    let read = json!({"path": path});
    let out = session
        .answer_one("run_1", "read_file", &read, None, &AllowAll, &mut |_| {})
        .await;
    assert!(!out.is_error, "{}", out.output);
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
    let code = "import gz\ngz.read_file('a.txt')\ngz.edit_file('a.txt', 'one', 'two')\ngz.write_file('n.txt', 'new')\n";
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

async fn call(
    session: &mut Session,
    tool: &str,
    input: Value,
    asked_by: Option<&str>,
) -> ToolOutput {
    session
        .answer_one(
            "run_1",
            tool,
            &input,
            asked_by.map(str::to_string),
            &AllowAll,
            &mut |_| {},
        )
        .await
}

#[tokio::test]
async fn reads_are_per_conversation_and_per_reader() {
    let dir = temp_dir("edits-ledger");
    std::fs::write(dir.join("a.txt"), "one\n").unwrap();
    let history = EditHistory::at(temp_dir("edits-ledger-store"), "conv");
    let mut session = session_in(&dir, &history);
    let change = json!({"path": "a.txt", "old_text": "one", "new_text": "two"});

    let unread = call(&mut session, "edit_file", change.clone(), None).await;
    assert!(unread.is_error && unread.output.contains("read it with read_file first"));

    let read = call(&mut session, "read_file", json!({"path": "a.txt"}), None).await;
    assert!(!read.is_error, "{}", read.output);
    let by_subagent = call(&mut session, "edit_file", change.clone(), Some("Helper")).await;
    assert!(
        by_subagent.is_error,
        "a subagent reads for itself: {}",
        by_subagent.output
    );

    session.new_conversation();
    let after_new = call(&mut session, "edit_file", change.clone(), None).await;
    assert!(
        after_new.is_error,
        "a new conversation starts with no reads"
    );

    call(&mut session, "read_file", json!({"path": "a.txt"}), None).await;
    let id = session.conversation_id.clone();
    session.continue_conversation(&id);
    let resumed = call(&mut session, "edit_file", change.clone(), None).await;
    assert!(resumed.is_error, "a resumed conversation re-reads first");

    call(&mut session, "read_file", json!({"path": "a.txt"}), None).await;
    let edited = call(&mut session, "edit_file", change, None).await;
    assert!(!edited.is_error, "{}", edited.output);
    let again = json!({"path": "a.txt", "old_text": "two", "new_text": "three"});
    let own_write = call(&mut session, "edit_file", again, None).await;
    assert!(
        !own_write.is_error,
        "its own edit counts as read: {}",
        own_write.output
    );
}

/// Allows commands and denies every read it is asked about, like a
/// headless run without `--yes-reads`.
struct DenyReads;

impl Approver for DenyReads {
    fn decide<'a>(&'a self, request: &'a PermissionRequest) -> DecisionFuture<'a> {
        let decision = match request.access {
            Access::Read => Decision::Deny,
            _ => Decision::AllowOnce,
        };
        Box::pin(async move { decision })
    }
}

/// The saved file's path, as the spill note names it.
fn saved_path(output: &str) -> String {
    let start = output.find(" is saved in ").expect("spill note") + " is saved in ".len();
    let rest = &output[start..];
    rest[..rest.find(": page through it").unwrap()].to_string()
}

#[tokio::test]
async fn saved_command_output_is_readable_without_a_prompt() {
    let dir = temp_dir("edits-spill-read");
    let history = EditHistory::at(temp_dir("edits-spill-read-store"), "conv");
    let mut session = session_in(&dir, &history);
    let rows = json!({"command": "i=0; while [ $i -lt 30000 ]; do echo row $i; i=$((i+1)); done"});
    let out = session
        .answer_one("run_1", "run_command", &rows, None, &DenyReads, &mut |_| {})
        .await;
    assert!(!out.is_error && out.truncated, "{}", out.output);
    let saved = saved_path(&out.output);

    for (tool, input) in [
        (
            "read_file",
            json!({"path": saved, "offset": 29990, "limit": 5}),
        ),
        (
            "search_files",
            json!({"path": saved, "pattern": "row 29999$", "output_mode": "content"}),
        ),
    ] {
        let out = session
            .answer_one("run_1", tool, &input, None, &DenyReads, &mut |_| {})
            .await;
        assert!(!out.is_error, "{tool}: {}", out.output);
        assert!(out.output.contains("row 2999"), "{tool}: {}", out.output);
    }

    let sibling = std::path::Path::new(&saved).with_file_name("other.txt");
    std::fs::write(&sibling, "not saved by gz\n").unwrap();
    let out = session
        .answer_one(
            "run_1",
            "read_file",
            &json!({"path": sibling}),
            None,
            &DenyReads,
            &mut |_| {},
        )
        .await;
    assert!(
        out.is_error,
        "a file next to a saved output is not saved output"
    );
    let folder = sibling.parent().unwrap();
    let out = session
        .answer_one(
            "run_1",
            "search_files",
            &json!({"path": folder, "pattern": "row"}),
            None,
            &DenyReads,
            &mut |_| {},
        )
        .await;
    assert!(
        out.is_error,
        "the saved-output folder is not open: {}",
        out.output
    );
}

#[tokio::test]
async fn subagents_with_the_same_name_keep_their_own_reads() {
    let dir = temp_dir("edits-ledger-ids");
    std::fs::write(dir.join("a.txt"), "one\n").unwrap();
    let history = EditHistory::at(temp_dir("edits-ledger-ids-store"), "conv");
    let mut session = session_in(&dir, &history);
    session.subagent_names.extend(
        [("sa-1", "explorer"), ("sa-2", "explorer")].map(|(id, name)| (id.into(), name.into())),
    );
    let by = |id: &str, tool: &str, input: Value| json!({"interruptId": format!("{id}-{tool}"), "toolName": tool, "input": input, "subagentId": id});
    let change = json!({"path": "a.txt", "old_text": "one", "new_text": "two"});
    let calls = vec![
        by("sa-1", "read_file", json!({"path": "a.txt"})),
        by("sa-2", "edit_file", change.clone()),
        by("sa-1", "edit_file", change),
    ];
    let results = session
        .answer_client_calls("run_1", calls, &AllowAll, &mut |_| {})
        .await;
    let result = |i: usize| &results[i]["result"];
    assert_eq!(result(0)["isError"], false, "{}", result(0));
    assert_eq!(
        result(1)["isError"],
        true,
        "another 'explorer' never read it: {}",
        result(1)
    );
    assert!(result(1)["output"]
        .as_str()
        .unwrap()
        .contains("read it with read_file first"));
    assert_eq!(result(2)["isError"], false, "{}", result(2));
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "two\n");
}

#[tokio::test]
async fn files_the_user_mentions_count_as_read() {
    let dir = temp_dir("edits-mention");
    std::fs::write(dir.join("a.txt"), "one\n").unwrap();
    std::fs::create_dir(dir.join("docs")).unwrap();
    let history = EditHistory::at(temp_dir("edits-mention-store"), "conv");
    let mut session = session_in(&dir, &history);
    let turn = session.prepare("@a.txt @docs fix it", &[]).unwrap();
    assert_eq!(turn.reads, ["a.txt", "docs"]);
    // No chat server answers here; the mention is recorded before sending.
    let _ = session.send_prepared(&turn, &AllowAll, &mut |_| {}).await;
    let change = json!({"path": "a.txt", "old_text": "one", "new_text": "two"});
    let edited = call(&mut session, "edit_file", change, None).await;
    assert!(!edited.is_error, "{}", edited.output);

    std::fs::write(dir.join("a.txt"), "changed\n").unwrap();
    let change = json!({"path": "a.txt", "old_text": "changed", "new_text": "x"});
    let stale = call(&mut session, "edit_file", change, None).await;
    assert!(stale.is_error && stale.output.contains("changed on disk"));
}
