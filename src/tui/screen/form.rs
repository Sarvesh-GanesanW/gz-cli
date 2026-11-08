//! Forms as data: the fields an action asks for before it sends a request.

#![allow(
    dead_code,
    reason = "spec vocabulary for the screens being converted next"
)]

use crate::tui::fields::FieldPath;
use crate::tui::nav::Source;

#[derive(Debug, Clone, Copy)]
pub struct FormSpec {
    pub title: &'static str,
    pub fields: &'static [FormField],
    pub submit: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub enum Choices {
    /// `(value, label)` pairs.
    Static(&'static [(&'static str, &'static str)]),
    /// Loaded when the form opens; each row gives a value and a label.
    Endpoint {
        source: Source,
        value: FieldPath,
        label: FieldPath,
    },
}

#[derive(Debug, Clone, Copy)]
pub enum FieldKind {
    Text,
    /// Enter adds a line; Ctrl+S submits.
    Multiline,
    Number {
        min: Option<f64>,
        max: Option<f64>,
    },
    Select(Choices),
    Checkbox,
}

/// Checks run on submit; the first failure shows under the field.
#[derive(Debug, Clone, Copy, Default)]
pub struct Validation {
    /// `(pattern, message)`; the whole value must match.
    pub pattern: Option<(&'static str, &'static str)>,
    pub max_len: Option<usize>,
}

#[derive(Debug, Clone, Copy)]
pub struct FormField {
    /// Key in the submitted values (`{name}` in the request template).
    pub name: &'static str,
    pub label: &'static str,
    pub kind: FieldKind,
    pub required: bool,
    /// Initial value, a template over the row (`{Name}` to rename).
    pub default: Option<&'static str>,
    pub hint: Option<&'static str>,
    pub validation: Validation,
}

impl FormField {
    pub const fn new(name: &'static str, label: &'static str, kind: FieldKind) -> Self {
        Self {
            name,
            label,
            kind,
            required: false,
            default: None,
            hint: None,
            validation: Validation {
                pattern: None,
                max_len: None,
            },
        }
    }

    pub const fn required(self) -> Self {
        Self {
            required: true,
            ..self
        }
    }

    pub const fn default(self, template: &'static str) -> Self {
        Self {
            default: Some(template),
            ..self
        }
    }

    pub const fn hint(self, hint: &'static str) -> Self {
        Self {
            hint: Some(hint),
            ..self
        }
    }

    pub const fn pattern(self, pattern: &'static str, message: &'static str) -> Self {
        Self {
            validation: Validation {
                pattern: Some((pattern, message)),
                ..self.validation
            },
            ..self
        }
    }

    pub const fn max_len(self, max: usize) -> Self {
        Self {
            validation: Validation {
                max_len: Some(max),
                ..self.validation
            },
            ..self
        }
    }
}
