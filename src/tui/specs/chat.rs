//! Chat: the conversation page, conversation history, budget and metrics.
//!
//! Verified contracts (TUI research notes, "Chat"; gz-chat-eks):
//! - list `GET chat /chat/conversations?assistantMode=chat` →
//!   `{conversations: [{conversationId, conversationName, groundzeroMode, …}]}`
//!   newest first, no timestamps (`api/routes/conversations.py:658-734`)
//! - rename `POST chat /chat/conversations {conversationId, conversationName}`
//!   (`conversations.py:737-800`, web `pages/home/utils.js:48-62`)
//! - delete `DELETE chat /chat/conversations/{id}`; 409 while a run is live
//!   (`conversations.py:1229-1290`)
//! - metrics `GET chat /chat/metrics` → one object; tenant-wide and
//!   including agent-builder spend, so it is never added to agent usage

use crate::tui::format::{Format, TimeUnit};
use crate::tui::nav::{grid, screen, Module, Resource, Source, Verb, View};
use crate::tui::screen::{
    ActionKind, ActionSpec, After, Column, DetailField, DetailSpec, FieldKind, FormField, FormSpec,
    Identity, RequestSpec, ScreenSpec, Section, SectionBody, Sort,
};
use crate::tui::views::transcript::Transcript;
use crate::tui::views::usage::Usage;

const CHAT: &str = "chat";
const CONVERSATION_ID: &[&str] = &["conversationId"];
const TITLE: &[&str] = &["conversationName"];

const OPEN_TRANSCRIPT: ActionSpec = ActionSpec::new(
    't',
    "Read the transcript",
    ActionKind::Custom(|row, ctx| ctx.push(Box::new(Transcript::new(row)))),
);

const CONTINUE_IN_CODE: ActionSpec = ActionSpec::new(
    'c',
    "Continue in gz code (copy the command)",
    ActionKind::Custom(|row, ctx| {
        let command = Transcript::new(row).continue_command();
        ctx.copy(command.clone(), format!("`{command}`"));
    }),
);

static RENAME_FORM: FormSpec = FormSpec {
    title: "Rename conversation",
    submit: "Rename",
    fields: &[FormField::new("name", "Name", FieldKind::Text)
        .required()
        .default("{conversationName}")
        .max_len(256)],
};

const RENAME: ActionSpec = ActionSpec::new(
    'n',
    "Rename",
    ActionKind::Form(
        &RENAME_FORM,
        RequestSpec {
            source: Source::new(
                CHAT,
                Verb::Post,
                "/chat/conversations",
                Some(r#"{"conversationId":"{conversationId}","conversationName":"{name}"}"#),
            ),
            success: "Conversation renamed",
            after: After::Refresh,
        },
    ),
);

const DELETE: ActionSpec = ActionSpec::new(
    'D',
    "Delete",
    ActionKind::Request(RequestSpec {
        source: Source::new(
            CHAT,
            Verb::Delete,
            "/chat/conversations/{conversationId}",
            None,
        ),
        success: "Conversation deleted",
        after: After::RefreshAndClose,
    }),
)
.destructive(
    "Delete the conversation {conversationName|conversationId}? Its messages can't be recovered.",
);

pub static HISTORY: ScreenSpec = ScreenSpec {
    noun: "conversation",
    columns: &[
        Column::new("Title", TITLE, Format::Text),
        Column::new(
            "Model",
            &["groundzeroMode", "modelConnectionName"],
            Format::Text,
        ),
    ],
    identity: Identity {
        id: CONVERSATION_ID,
        title: TITLE,
    },
    actions: &[OPEN_TRANSCRIPT, CONTINUE_IN_CODE, RENAME, DELETE],
    empty: "No conversations yet. Open Chat › Conversations and ask a question.",
    rows: Some(&["conversations"]),
    ..ScreenSpec::BLANK
};

const USAGE_VIEW: ActionSpec = ActionSpec::new(
    'u',
    "Usage & cost by day",
    ActionKind::Custom(|_, ctx| ctx.push(Box::new(Usage::new()))),
);

/// Shared by Chat › Metrics (per user) and Catalog › AI usage (per model).
pub const METRIC_SECTIONS: &[Section] = &[
    Section {
        title: "Tokens",
        body: SectionBody::Fields(&[
            DetailField::new("Input", &["inputTokens"], Format::Number),
            DetailField::new("Output", &["outputTokens"], Format::Number),
            DetailField::new("Cache read", &["cacheReadInputTokens"], Format::Number),
            DetailField::new("Cache write", &["cacheWriteInputTokens"], Format::Number),
            DetailField::new("Total", &["totalTokens"], Format::Number),
            DetailField::new("RAG embedding", &["ragEmbeddingTokens"], Format::Number).optional(),
            DetailField::new("Rerank queries", &["ragRerankQueries"], Format::Number).optional(),
        ]),
    },
    Section {
        title: "Cost",
        body: SectionBody::Fields(&[
            DetailField::new("Input", &["inputCostUsd"], Format::Usd),
            DetailField::new("Output", &["outputCostUsd"], Format::Usd),
            DetailField::new("Cache read", &["cacheReadCostUsd"], Format::Usd),
            DetailField::new("Cache write", &["cacheWriteCostUsd"], Format::Usd),
            DetailField::new("Queries", &["queryCostUsd"], Format::Usd),
            DetailField::new("Total", &["totalCostUsd"], Format::Usd),
        ]),
    },
    Section {
        title: "Activity",
        body: SectionBody::Fields(&[
            DetailField::new("Requests", &["requestCount"], Format::Number),
            DetailField::new("Invocations", &["invocationCount"], Format::Number),
            DetailField::new(
                "Usage not reported",
                &["usageUnknownInvocationCount"],
                Format::Number,
            ),
            DetailField::new(
                "First token (median)",
                &["medianTimeToFirstTokenMs"],
                Format::Duration(TimeUnit::Millis),
            ),
            DetailField::new(
                "Speed (tok/s)",
                &["avgOutputTokensPerSecond"],
                Format::Number,
            ),
        ]),
    },
];

pub static USER_METRICS: ScreenSpec = ScreenSpec {
    noun: "user",
    columns: &[
        Column::new("User", &["username"], Format::Text),
        Column::new("Requests", &["requestCount"], Format::Number),
        Column::new("Invocations", &["invocationCount"], Format::Number),
        Column::new("Tokens", &["totalTokens"], Format::Compact),
        Column::new(
            "First token",
            &["medianTimeToFirstTokenMs"],
            Format::Duration(TimeUnit::Millis),
        ),
        Column::new("Cost", &["totalCostUsd"], Format::Usd),
    ],
    sort: Some(Sort {
        field: &["totalCostUsd"],
        descending: true,
    }),
    identity: Identity {
        id: &["username"],
        title: &["username"],
    },
    detail: Some(DetailSpec {
        sections: METRIC_SECTIONS,
        ..DetailSpec::BLANK
    }),
    actions: &[USAGE_VIEW],
    empty: "No usage in the last 30 days",
    rows: Some(&["userMetrics"]),
    ..ScreenSpec::BLANK
};

pub const MODULE: Module = Module {
    title: "Chat",
    icon: "◎",
    resources: &[
        Resource {
            label: "Conversations",
            view: View::Chat,
        },
        screen(
            "History",
            Source::get(CHAT, "/chat/conversations?assistantMode=chat"),
            &HISTORY,
        ),
        grid("Budget", Source::get(CHAT, "/chat/budget-status")),
        screen("Metrics", Source::get(CHAT, "/chat/metrics"), &USER_METRICS),
    ],
};

#[cfg(test)]
mod tests;
