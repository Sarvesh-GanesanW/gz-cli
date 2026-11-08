//! Screen specs: static data that says how a table of API rows is shown
//! (columns and formats), which field identifies a row, what its detail
//! view holds, and which actions and links it offers. A resource or drill
//! level without a spec keeps the generic table.

mod action;
mod form;
mod link;

pub use action::{ActionKind, ActionSpec, After, RequestSpec};
pub use form::{Choices, FieldKind, FormField, FormSpec};
pub use link::{Goto, LinkSpec, Select, Target};

use std::time::Duration;

use serde_json::Value;

use crate::tui::fields::{self, FieldPath};
use crate::tui::format::{Format, StatusMap};
use crate::tui::nav::Source;

/// One table column.
#[derive(Debug, Clone, Copy)]
pub struct Column {
    pub header: &'static str,
    pub field: FieldPath,
    pub format: Format,
    /// Fixed width in cells; None sizes the column from its content.
    pub width: Option<u16>,
}

impl Column {
    pub const fn new(header: &'static str, field: FieldPath, format: Format) -> Self {
        Self {
            header,
            field,
            format,
            width: None,
        }
    }

    pub const fn width(self, width: u16) -> Self {
        Self {
            width: Some(width),
            ..self
        }
    }
}

/// The order rows arrive in, by a raw field (not the formatted text).
#[derive(Debug, Clone, Copy)]
pub struct Sort {
    pub field: FieldPath,
    pub descending: bool,
}

/// Which fields identify a row: `id` for requests, drills, actions and
/// links; `title` for people (breadcrumbs, confirm prompts, detail titles).
#[derive(Debug, Clone, Copy)]
pub struct Identity {
    pub id: FieldPath,
    pub title: FieldPath,
}

impl Identity {
    pub fn id_of(&self, row: &Value) -> Option<String> {
        fields::lookup_any(row, self.id).and_then(fields::id_text)
    }

    pub fn title_of(&self, row: &Value) -> Option<String> {
        fields::lookup_any(row, self.title)
            .map(crate::tui::format::text_of)
            .filter(|t| !t.is_empty())
            .or_else(|| self.id_of(row))
    }
}

/// A row condition: the field's text equals one of `any_of` (ignoring case).
#[derive(Debug, Clone, Copy)]
pub struct Cond {
    pub field: FieldPath,
    pub any_of: &'static [&'static str],
}

impl Cond {
    pub fn holds(&self, row: &Value) -> bool {
        fields::lookup_any(row, self.field)
            .map(crate::tui::format::text_of)
            .is_some_and(|text| self.any_of.iter().any(|v| v.eq_ignore_ascii_case(&text)))
    }
}

/// `true` when there is no condition or it holds.
pub fn allowed(when: Option<Cond>, row: &Value) -> bool {
    when.is_none_or(|cond| cond.holds(row))
}

/// One key and what it does, for the help overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyHelp {
    pub keys: &'static str,
    pub what: &'static str,
}

/// A labelled field in a detail section.
#[derive(Debug, Clone, Copy)]
pub struct DetailField {
    pub label: &'static str,
    pub field: FieldPath,
    pub format: Format,
    /// Leave the line out when the value is missing instead of showing "—".
    pub optional: bool,
}

impl DetailField {
    pub const fn new(label: &'static str, field: FieldPath, format: Format) -> Self {
        Self {
            label,
            field,
            format,
            optional: false,
        }
    }

    pub const fn optional(self) -> Self {
        Self {
            optional: true,
            ..self
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum SectionBody {
    Fields(&'static [DetailField]),
    /// Every key of a nested object, labels humanized ("bucketName" →
    /// "Bucket name"); secret-looking keys are masked.
    Object(FieldPath),
    /// Long text (a prompt, instructions, an answer) wrapped to the view's
    /// width with its own line breaks kept. Left out when empty.
    Text(FieldPath),
}

#[derive(Debug, Clone, Copy)]
pub struct Section {
    pub title: &'static str,
    pub body: SectionBody,
}

/// Extra rows a detail view loads on open and shows as a small table.
#[derive(Debug, Clone, Copy)]
pub struct RelatedSpec {
    pub title: &'static str,
    /// Filled from the (merged) detail row and its parents.
    pub source: Source,
    pub columns: &'static [Column],
    /// Only load for rows where this holds.
    pub when: Option<Cond>,
    pub empty: &'static str,
    /// Rows shown at most.
    pub limit: usize,
}

/// A second thing `Y` copies besides the id (a name, a URL).
#[derive(Debug, Clone, Copy)]
pub struct CopySpec {
    pub label: &'static str,
    /// Template filled from the row (see `template`).
    pub template: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub struct DetailSpec {
    /// Fetched when the view opens; its object is laid over the list row.
    /// Paths and bodies can use the row's fields (`{Id}`).
    pub load: Option<Source>,
    /// Shown when `load` answers with nothing (`[]`).
    pub missing: &'static str,
    pub badge: Option<(FieldPath, &'static StatusMap)>,
    /// Shown under the title, joined with " · ".
    pub subtitle: &'static [(FieldPath, Format)],
    pub sections: &'static [Section],
    pub related: &'static [RelatedSpec],
    pub copy: Option<CopySpec>,
    /// Reload the details this often while the view is open.
    pub poll: Option<Duration>,
}

impl DetailSpec {
    pub const BLANK: DetailSpec = DetailSpec {
        load: None,
        missing: "Not found. It may have been deleted.",
        badge: None,
        subtitle: &[],
        sections: &[],
        related: &[],
        copy: None,
        poll: None,
    };
}

/// How one table of rows is shown and what it can do.
#[derive(Debug, Clone, Copy)]
pub struct ScreenSpec {
    /// Singular, lower case ("connection"), for messages.
    pub noun: &'static str,
    pub columns: &'static [Column],
    pub sort: Option<Sort>,
    pub identity: Identity,
    /// Enter (or `i` on drillable rows) opens this; None keeps raw JSON.
    pub detail: Option<DetailSpec>,
    pub actions: &'static [ActionSpec],
    pub links: &'static [LinkSpec],
    /// Shown when the list is empty.
    pub empty: &'static str,
    /// Extra keys this screen handles, for the help overlay.
    pub keys: &'static [KeyHelp],
    /// Reload the list this often while it is on screen.
    pub poll: Option<Duration>,
    /// Where the rows sit in the answer when it isn't a list or a known
    /// envelope (`/chat/metrics` → `modelMetrics`).
    pub rows: Option<FieldPath>,
}

impl ScreenSpec {
    /// Defaults for struct-update syntax: `ScreenSpec { noun, …, ..ScreenSpec::BLANK }`.
    pub const BLANK: ScreenSpec = ScreenSpec {
        noun: "item",
        columns: &[],
        sort: None,
        identity: Identity {
            id: &["id", "Id"],
            title: &["name", "Name"],
        },
        detail: None,
        actions: &[],
        links: &[],
        empty: "Nothing here yet",
        keys: &[],
        poll: None,
        rows: None,
    };

    /// Actions offered for `row`.
    pub fn actions_for<'a>(
        &'a self,
        row: &'a Value,
    ) -> impl Iterator<Item = &'static ActionSpec> + 'a {
        self.actions.iter().filter(move |a| allowed(a.when, row))
    }

    /// Links offered for `row`.
    pub fn links_for<'a>(&'a self, row: &'a Value) -> impl Iterator<Item = &'static LinkSpec> + 'a {
        self.links.iter().filter(move |l| allowed(l.when, row))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn identity_reads_ids_and_falls_back_to_them_for_titles() {
        let identity = Identity {
            id: &["Id"],
            title: &["Name", "name"],
        };
        let row = json!({"Id": 1000, "name": "GZS3"});
        assert_eq!(identity.id_of(&row).as_deref(), Some("1000"));
        assert_eq!(identity.title_of(&row).as_deref(), Some("GZS3"));
        assert_eq!(identity.title_of(&json!({"Id": 5})).as_deref(), Some("5"));
    }

    #[test]
    fn conditions_match_any_value_ignoring_case() {
        let cond = Cond {
            field: &["type"],
            any_of: &["GZLAKEHOUSE", "ONTOLOGY"],
        };
        assert!(cond.holds(&json!({"type": "gzlakehouse"})));
        assert!(!cond.holds(&json!({"type": "AWSS3"})));
        assert!(!cond.holds(&json!({})));
        assert!(allowed(None, &json!({})));
    }
}
