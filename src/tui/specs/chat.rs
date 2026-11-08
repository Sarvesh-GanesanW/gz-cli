//! Chat: conversations, budget and metrics.

use crate::tui::nav::{grid, Module, Resource, Source, View};

pub const MODULE: Module = Module {
    title: "Chat",
    icon: "◎",
    resources: &[
        Resource {
            label: "Conversations",
            view: View::Chat,
        },
        grid("Budget", Source::get("chat", "/chat/budget-status")),
        grid("Metrics", Source::get("chat", "/chat/metrics")),
    ],
};
