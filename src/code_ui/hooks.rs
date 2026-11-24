//! Hooks in the interactive UI: the `/hooks` panel and asking the user to
//! trust a project's hooks file (the same prompt as a team skill's trust).

use std::path::{Path, PathBuf};

use tokio::sync::{mpsc, oneshot};

use owo_colors::OwoColorize;

use crate::assistant::hooks::{
    HookEvent, HookRow, Hooks, Log, ProjectState, Source, Status, TrustRequest,
};
use crate::assistant::permissions::{Decision, PermissionRequest};
use crate::assistant::session::AssistantEvent;
use crate::assistant::tools::Access;
use crate::code_ui::approver::Ask;
use crate::code_ui::panel::{Panel, Row};

/// Marks a trust prompt as being about project hooks (the renderer words
/// it for them).
pub const TRUST_ACTION: &str = "project_hooks";

/// The `/hooks` panel: each file's hooks, whether they run, and how each
/// last ended.
pub fn panel(status: &Status, show: &dyn Fn(&Path) -> String) -> Panel {
    let aside = if status.enabled {
        "on"
    } else {
        "off for this session"
    };
    let mut panel = Panel::new("Hooks").aside(aside);
    let shown = |path: &Option<PathBuf>| path.as_deref().map(show).unwrap_or_default();
    let user_file = shown(&status.user_file);
    panel = panel.section(Some("User"), rows(status, Source::User, &user_file));
    let project_heading = match status.project {
        Some(ProjectState::Trusted) => "Project · trusted",
        Some(ProjectState::Declined) => "Project · off, you declined this version (/hooks trust)",
        Some(ProjectState::Untrusted) | None => "Project · not trusted (/hooks trust)",
    };
    let project_file = shown(&status.project_file);
    panel = panel.section(
        Some(project_heading),
        rows(status, Source::Project, &project_file),
    );
    let problems = status.problems.iter().map(Row::note).collect();
    panel = panel.section(Some("Problems"), problems);
    panel = panel.section(Some("Events"), event_rows(status));
    if status.rows.is_empty() && status.problems.is_empty() {
        panel = panel.section(
            None,
            vec![Row::note(format!(
                "No hooks. Add [[event]] entries (e.g. [[pre_tool_use]]) to {} or {}",
                shown(&status.user_file),
                shown(&status.project_file)
            ))],
        );
    }
    panel.footer("/hooks off|on for this session · /hooks trust reviews the project file")
}

/// Every event in one line, with how many hooks run for each that has
/// some (`stop ×1`); the README says when each runs.
fn event_rows(status: &Status) -> Vec<Row> {
    let events: Vec<String> = HookEvent::ALL
        .iter()
        .map(|event| {
            let count = status
                .rows
                .iter()
                .filter(|row| row.event == *event && row.runs)
                .count();
            match count {
                0 => event.as_str().to_string(),
                n => format!("{} ×{n}", event.as_str()),
            }
        })
        .collect();
    vec![Row::note(events.join(" · "))]
}

/// `source`'s file, then its hooks; nothing when it has none.
fn rows(status: &Status, source: Source, file: &str) -> Vec<Row> {
    let hooks: Vec<Row> = status
        .rows
        .iter()
        .filter(|row| row.source == source)
        .map(row)
        .collect();
    if hooks.is_empty() {
        return hooks;
    }
    std::iter::once(Row::field("file", file))
        .chain(hooks)
        .collect()
}

fn row(hook: &HookRow) -> Row {
    let last = match (&hook.last, hook.runs) {
        (_, false) => "does not run".to_string(),
        (Some(last), true) => format!("last: {last}"),
        (None, true) => "not run yet".to_string(),
    };
    Row::entry(hook.event.as_str(), format!("{} · {last}", hook.action))
}

/// The prompt for trusting one version of a project hooks file.
pub fn trust_request(request: &TrustRequest) -> PermissionRequest {
    PermissionRequest {
        access: Access::Trust,
        summary: format!("trust the hooks in {}", request.path.display()),
        preview: Some(request.lines().join("\n")),
        chat_scope: String::new(),
        always_scope: None,
        action: Some(TRUST_ACTION.to_string()),
        asked_by: None,
        target: Some(request.path.display().to_string()),
        note: Some(
            "Every command is listed in full above. They run as you, unsandboxed, at the points of the session they name (tool calls, messages, start and end). Only this version is trusted; any change asks again."
                .to_string(),
        ),
    }
}

/// Every hook a trust prompt is about, in full, for scrollback: the prompt
/// box has room for only part of a long file, and the user must be able to
/// read all of what they trust. `None` for any other prompt.
pub fn trust_listing(request: &PermissionRequest) -> Option<Panel> {
    if request.action.as_deref() != Some(TRUST_ACTION) {
        return None;
    }
    let rows = request.preview.as_deref()?.lines().map(Row::note).collect();
    let mut panel = Panel::new("Project hooks to trust").section(None, rows);
    if let Some(path) = &request.target {
        panel = panel.aside(path.clone());
    }
    Some(panel)
}

/// Reads the project hooks file, asks the user to trust it, and applies
/// the answer; what happened (or why it couldn't) comes back on `done`.
pub fn start_trust(
    hooks: Hooks,
    asks: mpsc::UnboundedSender<Ask>,
    done: mpsc::UnboundedSender<Result<String, String>>,
) {
    tokio::spawn(async move {
        let request = match hooks.review_project() {
            Ok(request) => request,
            Err(why) => {
                done.send(Err(why)).ok();
                return;
            }
        };
        let (reply, answer) = oneshot::channel();
        let ask = Ask {
            request: trust_request(&request),
            reply,
            background: None,
        };
        if asks.send(ask).is_err() {
            return;
        }
        let trusted = matches!(answer.await, Ok((Decision::AllowOnce, _)));
        done.send(hooks.answer_trust(request, trusted)).ok();
    });
}

/// Runs `session_end` hooks as gz code exits; what they print goes to
/// stderr, the screen being the plain terminal again.
pub async fn on_exit(hooks: &Hooks, cwd: &Path) {
    hooks
        .exit(cwd, &mut |event| {
            if let AssistantEvent::Notice(text) = event {
                eprintln!("{}", text.dimmed());
            }
        })
        .await;
}

/// Runs `notification` hooks on their own task when gz puts a prompt
/// (`kind`: permission or question) in front of the user; the prompt does
/// not wait for them. What they print (or how they failed) goes to
/// `notices` as the same dim line other hooks get; `/hooks` keeps how they
/// ended.
pub fn notify(
    hooks: Option<&Hooks>,
    cwd: Option<PathBuf>,
    conversation: &str,
    kind: &'static str,
    message: String,
    notices: mpsc::UnboundedSender<AssistantEvent>,
) {
    let (Some(hooks), Some(cwd)) = (hooks.cloned(), cwd) else {
        return;
    };
    let conversation = conversation.to_string();
    tokio::spawn(async move {
        let log = Log {
            audit: None,
            conversation_id: &conversation,
            run_id: "",
        };
        let mut show = |event| {
            // The UI may have closed meanwhile; the hook still ran.
            notices.send(event).ok();
        };
        hooks
            .notification(kind, &message, &cwd, &log, &mut show)
            .await;
    });
}

/// Runs `pre_compact` hooks before a `/compact`; `Err` is why a hook
/// blocked it.
pub async fn before_compact(
    hooks: Option<Hooks>,
    cwd: Option<PathBuf>,
    conversation: &str,
    focus: Option<&str>,
) -> Result<(), String> {
    let (Some(hooks), Some(cwd)) = (hooks, cwd) else {
        return Ok(());
    };
    let log = Log {
        audit: None,
        conversation_id: conversation,
        run_id: "",
    };
    hooks
        .pre_compact("manual", focus, &cwd, &log, &mut |_| {})
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant::hooks::test_support::{places, write};

    fn plain(path: &Path) -> String {
        path.display().to_string()
    }

    #[tokio::test]
    async fn notification_hook_output_reaches_the_screen() {
        let (dir, hooks) = crate::assistant::hooks::test_support::user_hooks(
            "hooks-notify-shown",
            "[[notification]]\ncommand = \"echo pinged the team\"\n",
        );
        let (tx, mut rx) = mpsc::unbounded_channel();
        let message = "gz needs your permission: ls".to_string();
        notify(Some(&hooks), Some(dir), "c", "permission", message, tx);
        let shown = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
            .await
            .expect("the hook's output arrives")
            .expect("one notice");
        let AssistantEvent::Notice(text) = shown else {
            panic!("{shown:?}");
        };
        assert!(text.ends_with("pinged the team"), "{text}");
    }

    #[test]
    fn the_panel_shows_sources_trust_and_last_results() {
        let dir = crate::assistant::sandbox::tests::temp_dir("hooks-panel");
        write(
            &dir.join("config/hooks.toml"),
            "[[after_tool]]\ntools = [\"edit_file\"]\npath = \"*.rs\"\ncommand = \"cargo fmt\"",
        );
        write(
            &dir.join("project/.gz/hooks.toml"),
            "[[turn_end]]\ncommand = \"make check\"",
        );
        let hooks = Hooks::load(places(&dir));
        let text = panel(&hooks.status(), &plain).plain_lines().join("\n");
        assert!(
            text.contains("edit_file *.rs → cargo fmt (30s) · not run yet"),
            "{text}"
        );
        assert!(text.contains("not trusted (/hooks trust)"), "{text}");
        for event in HookEvent::ALL {
            assert!(
                text.contains(event.as_str()),
                "{} missing: {text}",
                event.as_str()
            );
        }
        assert!(text.contains("· after_tool ×1 ·"), "{text}");
        assert!(
            text.contains("session_start · user_prompt_submit"),
            "{text}"
        );
        assert!(
            text.contains("each turn → make check (30s) · does not run"),
            "{text}"
        );

        hooks.set_enabled(false);
        assert_eq!(
            panel(&hooks.status(), &plain).aside.as_deref(),
            Some("off for this session")
        );
    }

    #[tokio::test]
    async fn trusting_through_the_prompt_turns_project_hooks_on() {
        let dir = crate::assistant::sandbox::tests::temp_dir("hooks-trust-ui");
        write(
            &dir.join("project/.gz/hooks.toml"),
            "[[turn_end]]\ncommand = \"make check\"",
        );
        let hooks = Hooks::load(places(&dir));
        let (ask_tx, mut asks) = mpsc::unbounded_channel();
        let (done_tx, mut done) = mpsc::unbounded_channel();
        start_trust(hooks.clone(), ask_tx, done_tx);
        let ask = asks.recv().await.unwrap();
        assert_eq!(ask.request.access, Access::Trust);
        assert_eq!(
            ask.request.preview.as_deref(),
            Some("turn_end each turn (30s):\n  make check")
        );
        ask.reply.send((Decision::AllowOnce, None)).unwrap();
        assert!(done
            .recv()
            .await
            .unwrap()
            .unwrap()
            .starts_with("Trusted 1 project hook"));
        assert_eq!(hooks.status().project, Some(ProjectState::Trusted));
    }
}
