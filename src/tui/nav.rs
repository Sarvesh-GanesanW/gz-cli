//! The TUI's navigation tree as data: modules, their resources, and how each
//! resource loads rows, drills into a selected row, and opens details.
//! Adding a view means adding an entry here, not new key-handling code.

use reqwest::Method;
use serde_json::Value;

use crate::cmd::seg;

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

/// One request template. `{0}`, `{1}`, … are the ids picked so far: the
/// drill trail, then the selected row.
#[derive(Debug, Clone, Copy)]
pub struct Source {
    pub service: &'static str,
    pub verb: Verb,
    pub path: &'static str,
    /// JSON body template for POST sources.
    pub body: Option<&'static str>,
}

impl Source {
    const fn get(service: &'static str, path: &'static str) -> Self {
        Self {
            service,
            verb: Verb::Get,
            path,
            body: None,
        }
    }

    const fn post(service: &'static str, path: &'static str, body: &'static str) -> Self {
        Self {
            service,
            verb: Verb::Post,
            path,
            body: Some(body),
        }
    }

    /// The concrete path and body for these ids. Ids are percent-encoded in
    /// the path and JSON-escaped in the body.
    pub fn resolve(&self, ids: &[String]) -> (String, Option<Value>) {
        let path = fill(self.path, ids, seg);
        let body = self.body.map(|template| {
            let text = fill(template, ids, json_escape);
            serde_json::from_str(&text).unwrap_or(Value::Null)
        });
        (path, body)
    }
}

fn json_escape(id: &str) -> String {
    let quoted = serde_json::to_string(id).unwrap_or_default();
    quoted[1..quoted.len() - 1].to_string()
}

fn fill(template: &str, ids: &[String], encode: impl Fn(&str) -> String) -> String {
    let mut out = template.to_string();
    for (i, id) in ids.iter().enumerate() {
        out = out.replace(&format!("{{{i}}}"), &encode(id));
    }
    out
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

const fn grid(label: &'static str, source: Source) -> Resource {
    Resource {
        label,
        view: View::Grid(GridSpec {
            source,
            drills: &[],
            detail: None,
        }),
    }
}

const fn grid_with_detail(label: &'static str, source: Source, detail: Source) -> Resource {
    Resource {
        label,
        view: View::Grid(GridSpec {
            source,
            drills: &[],
            detail: Some(detail),
        }),
    }
}

const fn drill(noun: &'static str, id_keys: &'static [&'static str], source: Source) -> Drill {
    Drill {
        noun,
        id_keys,
        source,
    }
}

const PROJECT_IDS: &[&str] = &["Id", "id", "projectId"];
const NAME_IDS: &[&str] = &["name", "Name"];

pub static MODULES: &[Module] = &[
    Module {
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
    },
    Module {
        title: "Lakehouse",
        icon: "≋",
        resources: &[
            Resource {
                label: "Warehouses",
                view: View::Grid(GridSpec {
                    source: Source::get("lakehouse", "/lakehouse/warehouse"),
                    drills: &[
                        drill(
                            "warehouse",
                            &["name", "warehouseName", "warehouse"],
                            Source::get("lakehouse", "/lakehouse/warehouse/{0}/namespaces"),
                        ),
                        drill(
                            "namespace",
                            &["name", "namespace", "namespaceName"],
                            Source::get("lakehouse", "/lakehouse/schema/{0}/{1}"),
                        ),
                    ],
                    detail: None,
                }),
            },
            grid_with_detail(
                "Saved queries",
                Source::get("lakehouse", "/lakehouse/query"),
                Source::get("lakehouse", "/lakehouse/query/{0}"),
            ),
        ],
    },
    Module {
        title: "DE/ML",
        icon: "◈",
        resources: &[
            Resource {
                label: "Projects",
                view: View::Grid(GridSpec {
                    source: Source::get("etlprojects", "/etl/etlProjects"),
                    drills: &[drill(
                        "project",
                        PROJECT_IDS,
                        Source::get("etlprojects", "/etl/etlJobs/{0}/jobs"),
                    )],
                    detail: None,
                }),
            },
            grid(
                "Published jobs",
                Source::get("etlprojects", "/etl/etlJobs/publish"),
            ),
            Resource {
                label: "Notebooks",
                view: View::Grid(GridSpec {
                    source: Source::get("etlprojects", "/etl/etlProjects"),
                    drills: &[drill(
                        "project",
                        PROJECT_IDS,
                        Source::get("mlops", "/jupyter/{0}/notebook"),
                    )],
                    detail: None,
                }),
            },
            grid("Models", Source::get("mlops", "/mlflow/listAllModels")),
            grid(
                "Experiments",
                Source::get("mlops", "/mlflow/listAllExperiments"),
            ),
        ],
    },
    Module {
        title: "Agents",
        icon: "◉",
        resources: &[
            Resource {
                label: "Agents",
                view: View::Grid(GridSpec {
                    source: Source::get("agents", "/v1/agents"),
                    drills: &[drill(
                        "agent",
                        &["id"],
                        Source::get("agents", "/v1/agents/{0}/runs"),
                    )],
                    detail: Some(Source::get("agents", "/v1/agents/{0}/runs/{1}")),
                }),
            },
            grid(
                "Approval requests",
                Source::get("agents", "/v1/approval-requests"),
            ),
            grid("Tools", Source::get("agents", "/v1/tools")),
            grid("Models", Source::get("agents", "/v1/models")),
        ],
    },
    Module {
        title: "Designer",
        icon: "◧",
        resources: &[
            grid_with_detail(
                "Datasets",
                Source::get("datasets", "/dataset"),
                Source::get("datasets", "/dataset/{0}"),
            ),
            grid_with_detail(
                "Dashboards",
                Source::get("dashboards", "/dashboard"),
                Source::get("dashboards", "/dashboard/{0}"),
            ),
            grid_with_detail(
                "Visualizations",
                Source::get("visualizations", "/visualization"),
                Source::get("visualizations", "/visualization/{0}"),
            ),
            grid_with_detail(
                "Filters",
                Source::get("filters", "/filter"),
                Source::get("filters", "/filter/{0}"),
            ),
        ],
    },
    Module {
        title: "Workspaces",
        icon: "▣",
        resources: &[Resource {
            label: "Workspaces",
            view: View::Grid(GridSpec {
                source: Source::get("workspaces", "/workspace"),
                drills: &[drill(
                    "workspace",
                    &["Id", "id", "workspaceId"],
                    Source::get("workspaces", "/workspace/{0}/contents"),
                )],
                detail: None,
            }),
        }],
    },
    Module {
        title: "Connections",
        icon: "⇄",
        resources: &[grid_with_detail(
            "Connections",
            Source::get("connections", "/connection"),
            Source::get("connections", "/connection/{0}"),
        )],
    },
    Module {
        title: "Files",
        icon: "▤",
        resources: &[Resource {
            label: "Storage",
            view: View::Grid(GridSpec {
                source: Source::post("data", "/storage", r#"{"ConnectionName":"","prefix":""}"#),
                drills: &[
                    drill(
                        "connection",
                        NAME_IDS,
                        Source::post(
                            "data",
                            "/storage/listFolders",
                            r#"{"ConnectionName":"{0}","prefix":""}"#,
                        ),
                    ),
                    drill(
                        "folder",
                        &[],
                        Source::post(
                            "data",
                            "/storage/listFiles",
                            r#"{"ConnectionName":"{0}","prefix":"{1}"}"#,
                        ),
                    ),
                ],
                detail: None,
            }),
        }],
    },
    Module {
        title: "Catalog",
        icon: "▦",
        resources: &[
            Resource {
                label: "Data catalog",
                view: View::Grid(GridSpec {
                    source: Source::get("lakehouse", "/lakehouse/warehouse"),
                    drills: &[
                        drill(
                            "warehouse",
                            &["name", "warehouseName"],
                            Source::get("lakehouse", "/lakehouse/warehouse/{0}/namespaces"),
                        ),
                        drill(
                            "database",
                            &["name", "databaseName", "namespace"],
                            Source::get("lakehouse", "/lakehouse/schema/{0}/{1}"),
                        ),
                    ],
                    detail: None,
                }),
            },
            grid("AI usage", Source::get("chat", "/chat/metrics")),
            grid("Agent usage", Source::get("agents", "/v1/agents/metrics")),
            grid("Job usage", Source::get("etl", "/etl/consumption-metrics")),
            grid(
                "Project usage",
                Source::get("etl", "/etl/consumption-metrics/aggregates/projects"),
            ),
            Resource {
                label: "Data lineage",
                view: View::Grid(GridSpec {
                    source: Source::get("connections", "/connection"),
                    drills: &[drill(
                        "connection",
                        &["Id", "id"],
                        Source::get("etl", "/etl/lineage/assets?connectionId={0}"),
                    )],
                    detail: Some(Source::get("etl", "/etl/lineage/assets/{1}")),
                }),
            },
            grid(
                "Live streams",
                Source::post("etl", "/etl/dataset-stream/list", "{}"),
            ),
            grid_with_detail(
                "Ontology models",
                Source::get("etl", "/etl/ontology/models"),
                Source::get("etl", "/etl/ontology/models/{0}"),
            ),
            grid_with_detail(
                "Log exports",
                Source::get("logexports", "/log-export/runs"),
                Source::get("logexports", "/log-export/runs/{0}"),
            ),
            grid_with_detail(
                "Export schedules",
                Source::get("logexports", "/log-export/schedules"),
                Source::get("logexports", "/log-export/schedules/{0}"),
            ),
        ],
    },
    Module {
        title: "Schedules",
        icon: "◷",
        resources: &[
            grid_with_detail(
                "Schedules",
                Source::get("schedules", "/schedule"),
                Source::get("schedules", "/schedule/{0}"),
            ),
            grid(
                "Executions",
                Source::get("schedules", "/schedule/executions"),
            ),
        ],
    },
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
    keys.iter().find_map(|k| match row.get(k)? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fills_templates_with_encoded_ids() {
        let source = Source::get("lakehouse", "/lakehouse/schema/{0}/{1}");
        let ids = vec!["wh".to_string(), "my db".to_string()];
        assert_eq!(source.resolve(&ids).0, "/lakehouse/schema/wh/my%20db");
        let post = Source::post("data", "/x", r#"{"ConnectionName":"{0}","prefix":"{1}"}"#);
        let ids = vec!["c\"1".to_string(), "raw/".to_string()];
        let (_, body) = post.resolve(&ids);
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
