//! Row actions: what a key on a selected row (or in its detail view) does.

#![allow(
    dead_code,
    reason = "spec vocabulary for the screens being converted next"
)]

use serde_json::Value;

use super::form::FormSpec;
use super::link::Goto;
use super::Cond;
use crate::tui::nav::Source;
use crate::tui::views::Ctx;

/// Code for actions that data can't describe (downloads, live views). It
/// gets the row (merged with its details when the detail view is open) and
/// can fetch, push views, toast and notify through `ctx`.
pub type CustomAction = fn(row: &Value, ctx: &mut Ctx);

/// What happens after a request succeeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum After {
    /// Reload the screen the action came from.
    Refresh,
    /// Reload it and close the detail view (the row is gone or changed).
    RefreshAndClose,
    /// Leave everything as it is (tests, previews).
    Stay,
}

/// A request an action sends.
#[derive(Debug, Clone, Copy)]
pub struct RequestSpec {
    /// Filled from the form values (when there is a form), then the row,
    /// then its parents; `{$site}` is the signed-in site.
    pub source: Source,
    /// Toast on success when the server sends no message of its own.
    pub success: &'static str,
    pub after: After,
}

#[derive(Clone, Copy)]
pub enum ActionKind {
    Request(RequestSpec),
    /// Opens a form; its values fill the request template by field name.
    Form(&'static FormSpec, RequestSpec),
    /// Opens another screen (see `LinkSpec` for keys of their own).
    Navigate(Goto),
    Custom(CustomAction),
}

impl std::fmt::Debug for ActionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ActionKind::Request(req) => f.debug_tuple("Request").field(req).finish(),
            ActionKind::Form(form, req) => {
                f.debug_tuple("Form").field(&form.title).field(req).finish()
            }
            ActionKind::Navigate(goto) => f.debug_tuple("Navigate").field(goto).finish(),
            ActionKind::Custom(_) => f.write_str("Custom"),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ActionSpec {
    pub key: char,
    pub label: &'static str,
    pub kind: ActionKind,
    /// Ask first, with this text (a template over the row, e.g.
    /// "Delete connection {Name}?"). Destructive actions always ask.
    pub confirm: Option<&'static str>,
    /// Destructive actions make the user type the row's title (or press
    /// `D` when the row has none) before they run.
    pub destructive: bool,
    /// Only offered for rows where this holds.
    pub when: Option<Cond>,
}

impl ActionSpec {
    pub const fn new(key: char, label: &'static str, kind: ActionKind) -> Self {
        Self {
            key,
            label,
            kind,
            confirm: None,
            destructive: false,
            when: None,
        }
    }

    pub const fn confirm(self, text: &'static str) -> Self {
        Self {
            confirm: Some(text),
            ..self
        }
    }

    pub const fn destructive(self, text: &'static str) -> Self {
        Self {
            confirm: Some(text),
            destructive: true,
            ..self
        }
    }

    pub const fn when(self, cond: Cond) -> Self {
        Self {
            when: Some(cond),
            ..self
        }
    }
}
