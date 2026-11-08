//! Links between screens: a key on a row opens another resource with the
//! matching row selected, drilled into, or filtered for. Back returns.

#![allow(
    dead_code,
    reason = "spec vocabulary for the screens being converted next"
)]

use super::Cond;
use crate::tui::fields::FieldPath;

/// A resource by its sidebar titles; checked by a test, so a renamed
/// resource fails the build's tests instead of a user's keypress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    pub module: &'static str,
    pub resource: &'static str,
}

/// What to do in the target once its rows load.
#[derive(Debug, Clone, Copy)]
pub enum Select {
    /// Just open it.
    Open,
    /// Select the row whose `field` equals this row's `from`.
    Row { field: FieldPath, from: FieldPath },
    /// Select that row and drill into it (Enter).
    Drill { field: FieldPath, from: FieldPath },
    /// Filter the target's rows by this row's `from`.
    Filter { from: FieldPath },
}

#[derive(Debug, Clone, Copy)]
pub struct Goto {
    pub target: Target,
    pub select: Select,
}

#[derive(Debug, Clone, Copy)]
pub struct LinkSpec {
    pub key: char,
    pub label: &'static str,
    pub goto: Goto,
    pub when: Option<Cond>,
}
