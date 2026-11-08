//! The TUI's navigation tree as data: modules, their resources, and how each
//! resource loads rows, drills into a selected row, and opens details.
//! Each module's entry lives in its own file under `specs/`; this file holds
//! the types and the order modules appear in.

use reqwest::Method;
use serde_json::Value;

use crate::tui::fields;
use crate::tui::specs;
use crate::tui::template::{self, Encoding, Scope, TemplateError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Get,
    Post,
}

impl Verb {
    pub fn method(self) -> Method {
        match self {
            Verb::Get => Method::GET,
            Verb::Post => Method::POST,
        }
    }
}

/// One request template; see [`template`] for the placeholder forms.
#[derive(Debug, Clone, Copy)]
pub struct Source {
    pub service: &'static str,
    pub verb: Verb,
    pub path: &'static str,
    /// JSON body template for POST sources.
    pub body: Option<&'static str>,
}

impl Source {
    pub const fn get(service: &'static str, path: &'static str) -> Self {
        Self {
            service,
            verb: Verb::Get,
            path,
            body: None,
        }
    }

    pub const fn post(service: &'static str, path: &'static str, body: &'static str) -> Self {
        Self {
            service,
            verb: Verb::Post,
            path,
            body: Some(body),
        }
    }

    /// The concrete path and body for `scope`.
    pub fn resolve(&self, scope: &Scope) -> Result<(String, Option<Value>), TemplateError> {
        let path = template::fill(self.path, scope, Encoding::Path)?;
        let body = self
            .body
            .map(|body| template::fill_json(body, scope))
            .transpose()?;
        Ok((path, body))
    }
}

/// What selecting a row at a given depth opens.
#[derive(Debug, Clone, Copy)]
pub struct Drill {
    /// Singular name for messages ("Select a warehouse first").
    pub noun: &'static str,
    /// Row fields that hold the id, in lookup order.
    pub id_keys: &'static [&'static str],
    pub source: Source,
}

/// A table-backed resource.
#[derive(Debug, Clone, Copy)]
pub struct GridSpec {
    pub source: Source,
    /// Drill levels: `drills[d]` applies to rows shown at depth `d`.
    pub drills: &'static [Drill],
    /// Fetched on Enter past the last drill level; else the raw row is shown.
    pub detail: Option<Source>,
}

#[derive(Debug, Clone, Copy)]
pub enum View {
    Grid(GridSpec),
    Chat,
}

#[derive(Debug, Clone, Copy)]
pub struct Resource {
    pub label: &'static str,
    pub view: View,
}

#[derive(Debug, Clone, Copy)]
pub struct Module {
    pub title: &'static str,
    pub icon: &'static str,
    pub resources: &'static [Resource],
}

/// Row fields that commonly hold an id, for details without a drill.
pub const ID_KEYS: &[&str] = &[
    "id",
    "Id",
    "ID",
    "conversationId",
    "runId",
    "scheduleId",
    "projectId",
    "connectionId",
    "name",
    "Name",
];

/// A resource with a plain table and no drills.
pub const fn grid(label: &'static str, source: Source) -> Resource {
    Resource {
        label,
        view: View::Grid(GridSpec {
            source,
            drills: &[],
            detail: None,
        }),
    }
}

/// A plain table whose rows open `detail` on Enter.
pub const fn grid_with_detail(label: &'static str, source: Source, detail: Source) -> Resource {
    Resource {
        label,
        view: View::Grid(GridSpec {
            source,
            drills: &[],
            detail: Some(detail),
        }),
    }
}

pub const fn drill(noun: &'static str, id_keys: &'static [&'static str], source: Source) -> Drill {
    Drill {
        noun,
        id_keys,
        source,
    }
}

/// Modules in sidebar order. Each lives in its own `specs/` file.
pub static MODULES: &[Module] = &[
    specs::chat::MODULE,
    specs::lakehouse::MODULE,
    specs::deml::MODULE,
    specs::agents::MODULE,
    specs::designer::MODULE,
    specs::workspaces::MODULE,
    specs::connections::MODULE,
    specs::files::MODULE,
    specs::catalog::MODULE,
    specs::schedules::MODULE,
];

/// Address of one resource in [`MODULES`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResourceId {
    pub module: usize,
    pub resource: usize,
}

impl ResourceId {
    pub fn module(self) -> &'static Module {
        &MODULES[self.module]
    }

    pub fn resource(self) -> &'static Resource {
        &MODULES[self.module].resources[self.resource]
    }

    /// "Lakehouse › Warehouses".
    pub fn title(self) -> String {
        format!("{} › {}", self.module().title, self.resource().label)
    }
}

pub fn all_resources() -> impl Iterator<Item = ResourceId> {
    MODULES.iter().enumerate().flat_map(|(module, m)| {
        (0..m.resources.len()).map(move |resource| ResourceId { module, resource })
    })
}

/// The id of a row: the first present key, or the row itself when it is a
/// bare string (folder lists).
pub fn row_id(row: &Value, keys: &[&str]) -> Option<String> {
    if let Value::String(s) = row {
        return Some(s.clone());
    }
    fields::lookup_any(row, keys).and_then(fields::id_text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fills_templates_with_encoded_ids() {
        let source = Source::get("lakehouse", "/lakehouse/schema/{0}/{1}");
        let scope = Scope::with_ids(&["wh".to_string(), "my db".to_string()]);
        assert_eq!(
            source.resolve(&scope).unwrap().0,
            "/lakehouse/schema/wh/my%20db"
        );
        let post = Source::post("data", "/x", r#"{"ConnectionName":"{0}","prefix":"{1}"}"#);
        let scope = Scope::with_ids(&["c\"1".to_string(), "raw/".to_string()]);
        let (_, body) = post.resolve(&scope).unwrap();
        assert_eq!(
            body.unwrap(),
            json!({"ConnectionName": "c\"1", "prefix": "raw/"})
        );
    }

    #[test]
    fn row_ids_come_from_keys_or_bare_strings() {
        assert_eq!(
            row_id(&json!({"Id": 7}), &["id", "Id"]).as_deref(),
            Some("7")
        );
        assert_eq!(row_id(&json!("folder"), &[]).as_deref(), Some("folder"));
        assert_eq!(row_id(&json!({"x": 1}), &["id"]), None);
    }

    #[test]
    fn every_drill_template_has_its_ids() {
        for id in all_resources() {
            let View::Grid(spec) = id.resource().view else {
                continue;
            };
            for (depth, drill) in spec.drills.iter().enumerate() {
                assert!(
                    drill.source.path.contains(&format!("{{{depth}}}"))
                        || drill
                            .source
                            .body
                            .is_some_and(|b| b.contains(&format!("{{{depth}}}"))),
                    "{} drill {depth} never uses its id",
                    id.title()
                );
            }
        }
    }
}
