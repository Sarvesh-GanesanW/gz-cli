//! Helpers the state tests share.

use std::sync::{Arc, Mutex};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::assistant::models::Catalog;
use crate::assistant::tools::Access;
use crate::assistant::{AssistantEvent, PermissionRequest};
use crate::code_ui::approver::{Answer, Ask, Mode};

use super::UiState;

pub(super) fn state() -> UiState {
    UiState::new(Arc::new(Mutex::new(Mode::Ask)))
}

pub(super) fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

pub(super) fn type_text(s: &mut UiState, text: &str) {
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

pub(super) fn with_catalog() -> UiState {
    use crate::assistant::models::{GroundzeroMode, LlmConnection};
    let mut s = state();
    s.catalog = Some(Catalog {
        modes: vec![
            GroundzeroMode {
                value: "opus".into(),
                label: "Claude Opus 5".into(),
                efforts: vec!["low".into(), "high".into()],
            },
            GroundzeroMode {
                value: "sonnet55".into(),
                label: "Claude Sonnet 5.5".into(),
                efforts: vec![],
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
    });
    s
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
    (Ask { request, reply: tx }, rx)
}
