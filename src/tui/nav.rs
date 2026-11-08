//! The TUI's navigation tree as data: modules, their resources, and how each
//! resource loads rows, drills into a selected row, and opens details.
//! Each module's entry lives in its own file under `specs/`; this file holds
//! the types and the order modules appear in.

use reqwest::Method;
use serde_json::Value;

use crate::tui::fields;
use crate::tui::screen::{allowed, Cond, ScreenSpec};
use crate::tui::specs;
use crate::tui::template::{self, Encoding, Scope, TemplateError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "spec vocabulary for the screens being converted next"
)]
pub enum Verb {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl Verb {
    pub fn method(self) -> Method {
        match self {
            Verb::Get => Method::GET,
            Verb::Post => Method::POST,
            Verb::Put => Method::PUT,
            Verb::Patch => Method::PATCH,
            Verb::Delete => Method::DELETE,
        }
    }
}

/// One request template; see [`template`] for the placeholder forms.
#[derive(Debug, Clone, Copy)]
pub struct Source {
    pub service: &'static str,
    pub verb: Verb,
    pub path: &'static str,
    /// JSON body template.
    pub body: Option<&'static str>,
}

impl Source {
    pub const fn get(service: &'static str, path: &'static str) -> Self {
        Self::new(service, Verb::Get, path, None)
    }

    pub const fn post(service: &'static str, path: &'static str, body: &'static str) -> Self {
        Self::new(service, Verb::Post, path, Some(body))
    }

    pub const fn new(
        service: &'static str,
        verb: Verb,
        path: &'static str,
        body: Option<&'static str>,
    ) -> Self {
        Self {
            service,
            verb,
            path,
            body,
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
    /// Row fields that hold the id, in lookup order. A screen spec on the
    /// level being drilled from overrides these with its identity.
    pub id_keys: &'static [&'static str],
    pub source: Source,
    /// How the rows this drill loads are shown; None = generic table.
    pub screen: Option<&'static ScreenSpec>,
    /// Only rows where this holds drill in (folders); Enter on the others
    /// opens their details (files).
    pub when: Option<Cond>,
    /// This drill also applies to every deeper level, for trees of any
    /// depth (folders in folders). Only the last drill may repeat.
    pub repeats: bool,
}

/// A table-backed resource.
#[derive(Debug, Clone, Copy)]
pub struct GridSpec {
    pub source: Source,
    /// Drill levels: `drills[d]` applies to rows shown at depth `d`.
    pub drills: &'static [Drill],
    /// Fetched on Enter past the last drill level when the level has no
    /// screen spec; else the raw row is shown.
    pub detail: Option<Source>,
    /// How the root rows are shown; None = generic table.
    pub screen: Option<&'static ScreenSpec>,
}

impl GridSpec {
    /// The drill that applies to rows shown at `depth`: `drills[depth]`, or
    /// the last drill when it repeats.
    pub fn drill_at(&self, depth: usize) -> Option<&'static Drill> {
        let drills: &'static [Drill] = self.drills;
        drills
            .get(depth)
            .or_else(|| drills.last().filter(|last| last.repeats))
    }

    /// The drill that opens `row` at `depth`, if that row drills in.
    pub fn drill_for(&self, depth: usize, row: &Value) -> Option<&'static Drill> {
        self.drill_at(depth)
            .filter(|drill| allowed(drill.when, row))
    }

    /// The screen spec for rows shown at `depth`.
    pub fn screen_at(&self, depth: usize) -> Option<&'static ScreenSpec> {
        match depth {
            0 => self.screen,
            d => self.drill_at(d - 1).and_then(|drill| drill.screen),
        }
    }

    /// The request that loads rows at `depth`.
    pub fn source_at(&self, depth: usize) -> Source {
        match depth {
            0 => self.source,
            d => self
                .drill_at(d - 1)
                .map_or(self.source, |drill| drill.source),
        }
    }
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

/// Row fields that commonly hold an id, for details without a screen spec.
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
            screen: None,
        }),
    }
}

/// A table shown through a screen spec.
pub const fn screen(label: &'static str, source: Source, spec: &'static ScreenSpec) -> Resource {
    Resource {
        label,
        view: View::Grid(GridSpec {
            source,
            drills: &[],
            detail: None,
            screen: Some(spec),
        }),
    }
}

pub const fn drill(noun: &'static str, id_keys: &'static [&'static str], source: Source) -> Drill {
    Drill {
        noun,
        id_keys,
        source,
        screen: None,
        when: None,
        repeats: false,
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

    pub fn grid_spec(self) -> Option<GridSpec> {
        match self.resource().view {
            View::Grid(spec) => Some(spec),
            View::Chat => None,
        }
    }
}

pub fn all_resources() -> impl Iterator<Item = ResourceId> {
    MODULES.iter().enumerate().flat_map(|(module, m)| {
        (0..m.resources.len()).map(move |resource| ResourceId { module, resource })
    })
}

/// The resource titled `module` › `label`, if there is one.
pub fn find_resource(module: &str, label: &str) -> Option<ResourceId> {
    all_resources().find(|id| id.module().title == module && id.resource().label == label)
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
    fn sources_fill_paths_and_bodies() {
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
            let Some(spec) = id.grid_spec() else {
                continue;
            };
            for (depth, drill) in spec.drills.iter().enumerate() {
                let positional = format!("{{{depth}}}");
                let uses_id = drill.source.path.contains(&positional)
                    || drill.source.body.is_some_and(|b| b.contains(&positional))
                    || spec.screen_at(depth).is_some();
                assert!(uses_id, "{} drill {depth} never uses its id", id.title());
            }
        }
    }

    /// A renamed or removed resource fails here, not on a user's keypress.
    #[test]
    fn every_link_target_exists() {
        use crate::tui::screen::ActionKind;
        for id in all_resources() {
            let Some(grid) = id.grid_spec() else { continue };
            for spec in (0..=grid.drills.len()).filter_map(|d| grid.screen_at(d)) {
                let targets =
                    spec.links
                        .iter()
                        .map(|l| l.goto.target)
                        .chain(spec.actions.iter().filter_map(|a| match a.kind {
                            ActionKind::Navigate(goto) => Some(goto.target),
                            _ => None,
                        }));
                for target in targets {
                    assert!(
                        find_resource(target.module, target.resource).is_some(),
                        "{} links to missing {} › {}",
                        id.title(),
                        target.module,
                        target.resource
                    );
                }
            }
        }
    }

    #[test]
    fn resources_are_found_by_title() {
        let id = find_resource("Connections", "Connections").unwrap();
        assert_eq!(id.title(), "Connections › Connections");
        assert!(find_resource("Nope", "Nothing").is_none());
    }
}
