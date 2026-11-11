//! Hooks in the interactive UI: the `/hooks` panel and asking the user to
//! trust a project's hooks file (the same prompt as a team skill's trust).

use std::path::{Path, PathBuf};

use tokio::sync::{mpsc, oneshot};

use crate::assistant::hooks::{HookRow, Hooks, ProjectState, Source, Status, TrustRequest};
use crate::assistant::permissions::{Decision, PermissionRequest};
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
    if status.rows.is_empty() && status.problems.is_empty() {
        panel = panel.section(
            None,
            vec![Row::note(format!(
                "No hooks. Add [[before_tool]], [[after_tool]] or [[turn_end]] entries to {} or {}",
                shown(&status.user_file),
                shown(&status.project_file)
            ))],
        );
    }
    panel.footer("/hooks off|on for this session · /hooks trust reviews the project file")
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
            "These commands run as you, unsandboxed, around gz's tool calls. Only this version is trusted; any change asks again."
                .to_string(),
        ),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant::hooks::test_support::{places, write};

    fn plain(path: &Path) -> String {
        path.display().to_string()
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
