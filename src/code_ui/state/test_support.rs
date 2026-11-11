//! Helpers the state tests share.

use std::sync::{Arc, Mutex};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::assistant::models::Catalog;
use crate::assistant::tools::Access;
use crate::assistant::{AssistantEvent, PermissionRequest};
use crate::code_ui::approver::{Answer, Ask, Mode};

use super::UiState;

pub(crate) fn state() -> UiState {
    UiState::new(Arc::new(Mutex::new(Mode::Ask)))
}

pub(crate) fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

pub(crate) fn type_text(s: &mut UiState, text: &str) {
    for c in text.chars() {
        s.on_key(key(KeyCode::Char(c)));
    }
}

pub(super) fn started(name: &str, summary: &str) -> AssistantEvent {
    AssistantEvent::ToolStarted {
        name: name.into(),
        summary: summary.into(),
        local: true,
    }
}

pub(super) fn finished(
    name: &str,
    summary: &str,
    detail: &str,
    diff: Option<&str>,
) -> AssistantEvent {
    AssistantEvent::ToolFinished {
        name: name.into(),
        summary: summary.into(),
        is_error: false,
        detail: detail.into(),
        output: detail.into(),
        diff: diff.map(str::to_string),
    }
}

pub(crate) fn with_catalog() -> UiState {
    use crate::assistant::models::{GroundzeroMode, LlmConnection};
    let mut s = state();
    s.catalog = Some(Catalog {
        modes: vec![
            GroundzeroMode {
                value: "opus".into(),
                label: "Claude Opus 5".into(),
                efforts: vec!["low".into(), "high".into()],
                context_window: Some(872_000),
                long_context: None,
            },
            GroundzeroMode {
                value: "sonnet55".into(),
                label: "Claude Sonnet 5.5".into(),
                efforts: vec![],
                context_window: Some(200_000),
                long_context: None,
            },
        ],
        default_mode: Some("sonnet55".into()),
        default_effort: None,
        connections: vec![LlmConnection {
            provider: "OPENAI".into(),
            name: "my-gpt".into(),
            model_id: "gpt-5".into(),
        }],
        subagent_profiles: vec![crate::assistant::models::SubagentProfile {
            id: "analyst".into(),
            label: "Analyst".into(),
            description: "lakehouse and SQL reads".into(),
        }],
        action_review: true,
        action_review_authorizations: true,
        background_subagents: true,
        live_steering: true,
        client_tools: None,
        subagent_settings: Some(Default::default()),
        local_context: Some(Default::default()),
        team_skills: None,
    });
    s
}

/// Live steering on, with a run that is never reached: notes stay pending
/// until a test takes them.
pub(crate) fn with_steering(s: &mut UiState) {
    s.catalog = with_catalog().catalog;
    let resolved = crate::config::resolve(
        &crate::config::Config::default(),
        Some("steering-test"),
        Some("http://127.0.0.1:9".into()),
        Some("stub-token".to_string()),
    );
    let http = crate::http::ApiClient::new(resolved, crate::output::OutputFormat::Json, true, 0, 5)
        .unwrap();
    s.steering = Some(crate::assistant::steering::Steering::new(
        http,
        Arc::new(Mutex::new(None)),
    ));
}

pub(super) fn subagent(event: crate::subagents::SubagentEvent) -> AssistantEvent {
    AssistantEvent::Subagent(event)
}

pub(super) fn ask(
    access: Access,
    summary: &str,
    preview: Option<&str>,
) -> (Ask, tokio::sync::oneshot::Receiver<Answer>) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let request = PermissionRequest {
        access,
        summary: summary.into(),
        preview: preview.map(str::to_string),
        chat_scope: "all edits this session".into(),
        always_scope: Some("always scope".into()),
        action: None,
        asked_by: None,
        target: None,
        note: None,
    };
    (
        Ask {
            request,
            reply: tx,
            background: None,
        },
        rx,
    )
}
