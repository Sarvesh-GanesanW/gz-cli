//! The named things of each kind, one list call per kind (and per project
//! for jobs and notebooks), kept for the length of the command.
//!
//! Routes, as the web app and the backends use them:
//! - projects `GET etlprojects /etl/etlProjects` → `[{Id, Name}]`; a
//!   project's jobs `GET /etl/etlJobs/{project}/jobs` → `[{Id, Name}]`
//!   (ground-zero src/pages/Utils/etlUtlis.js; `gz projects jobs`)
//! - notebooks `GET mlops /jupyter/{project}/notebook` → `{notebooks: [{id, name}]}`
//! - designer `GET /dataset`, `/dashboard`, `/visualization`, `/filter`,
//!   workspaces `GET /workspace` (top-level folders only), connections
//!   `GET /connection` → `[{Id, Name}]`
//! - a workspace folder's contents `GET /workspace/{id}/contents` →
//!   `[{id, Name, isDashboard}]` (Workspace Lambda `getContentsByParentId`,
//!   site DB `fn_r_workspacecontents`; web app `getFolderContent`)
//! - agents `GET agents /v1/agents?limit&offset` → `{items: [{id, name}]}`
//!   (gz-agent-builder-provider-api api.py `list_agents`, limit ≤ 100)
//! - compute sizes and warehouses: [`Lakehouse::computes`], [`Lakehouse::warehouses`]
//! - saved queries `GET lakehouse /lakehouse/query` → `[{Id, Name}]`
//!   (iceberg provider saved_queries.py `get_iceberg_queries`)
//! - ontology models `GET etl /etl/ontology/models?limit&offset` →
//!   `{items: [{id, name}]}` (etl provider app.py `etlOntologyModelsHandler`, limit ≤ 100)

use std::collections::{HashMap, HashSet};

use anyhow::{bail, Context, Result};
use reqwest::Method;
use serde_json::Value;

use super::Kind;
use crate::cmd::{jobs, seg, Runtime};
use crate::impact::lookup::parallel;
use crate::sql::session::Lakehouse;

/// Agents and ontology models are listed a page at a time.
const PAGE: usize = 100;
/// Stops a paged list that never ends.
const MAX_PAGES: usize = 50;
/// Stops walking a workspace tree past this many folders.
const MAX_FOLDERS: usize = 2000;

/// One named thing to choose among.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub id: String,
    pub name: String,
    /// The project a job or notebook is in: id and name.
    pub project: Option<(i64, String)>,
    /// How to tell it apart when no project does (a lineage asset's type
    /// and connection); shown instead of the name.
    pub detail: Option<String>,
}

impl Candidate {
    pub fn id_number(&self) -> Result<i64> {
        self.id
            .parse()
            .with_context(|| format!("{} has a non-numeric id {}", self.name, self.id))
    }

    /// `DQC (id 3013) in project Sales (1999)`.
    pub fn describe(&self) -> String {
        if let Some(detail) = &self.detail {
            return detail.clone();
        }
        match &self.project {
            Some((id, name)) if name.is_empty() => {
                format!("{} (id {}) in project {id}", self.name, self.id)
            }
            Some((id, name)) => format!("{} (id {}) in project {name} ({id})", self.name, self.id),
            None => format!("{} (id {})", self.name, self.id),
        }
    }
}

/// Lists, fetched once each and kept for the command.
pub struct Catalog {
    rt: Runtime,
    lists: HashMap<(Kind, Option<i64>), Vec<Candidate>>,
}

impl Catalog {
    pub fn new(rt: &Runtime) -> Self {
        Self {
            rt: rt.clone(),
            lists: HashMap::new(),
        }
    }

    /// Every `kind` you can see; jobs and notebooks in `project`, or in
    /// every project when it is `None`.
    pub async fn list(&mut self, kind: Kind, project: Option<i64>) -> Result<Vec<Candidate>> {
        let project = project.filter(|_| kind.in_project());
        if let Some(found) = self.lists.get(&(kind, project)) {
            return Ok(found.clone());
        }
        let found = match (kind, project) {
            (Kind::LineageAsset, _) => bail!("lineage assets are searched by name, not listed"),
            // The project was named on the line, so its name isn't read again.
            (Kind::Job | Kind::Notebook, Some(project)) => {
                in_project(&self.rt, kind, project, String::new()).await?
            }
            (Kind::Job | Kind::Notebook, None) => self.everywhere(kind).await?,
            _ => fetch(&self.rt, kind).await?,
        };
        self.lists.insert((kind, project), found.clone());
        Ok(found)
    }

    pub fn runtime(&self) -> &Runtime {
        &self.rt
    }

    /// The lineage assets named `name`, from the search `gz jobs lineage
    /// graph` uses.
    pub async fn lineage_assets(&self, name: &str) -> Result<Vec<Candidate>> {
        let found = crate::cmd::lineage_graph::find_assets(&self.rt, name).await?;
        Ok(found
            .into_iter()
            .map(|(id, about)| Candidate {
                id: id.to_string(),
                name: name.to_string(),
                project: None,
                detail: Some(about),
            })
            .collect())
    }

    /// Jobs or notebooks of every project, read side by side.
    async fn everywhere(&mut self, kind: Kind) -> Result<Vec<Candidate>> {
        let projects = Box::pin(self.list(Kind::Project, None)).await?;
        let reads = projects.iter().map(|project| {
            let rt = self.rt.clone();
            let project = project.clone();
            async move {
                let id = project.id_number()?;
                in_project(&rt, kind, id, project.name).await
            }
        });
        let mut found = Vec::new();
        for answer in parallel(reads.collect::<Vec<_>>()).await? {
            found.extend(answer?);
        }
        Ok(found)
    }
}

async fn in_project(
    rt: &Runtime,
    kind: Kind,
    project: i64,
    project_name: String,
) -> Result<Vec<Candidate>> {
    let rows = match kind {
        Kind::Job => jobs::project_jobs(rt, project)
            .await?
            .into_iter()
            .map(|job| (job.id.to_string(), job.name))
            .collect(),
        _ => {
            let path = format!("/jupyter/{project}/notebook");
            let list = get(rt, "mlops", &path, &[]).await?;
            pairs(
                list["notebooks"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or(&[]),
            )
        }
    };
    Ok(rows
        .into_iter()
        .map(|(id, name)| Candidate {
            id,
            name,
            project: Some((project, project_name.clone())),
            detail: None,
        })
        .collect())
}

async fn fetch(rt: &Runtime, kind: Kind) -> Result<Vec<Candidate>> {
    let rows = match kind {
        Kind::Project => rows_of(get(rt, "etlprojects", "/etl/etlProjects", &[]).await?),
        Kind::Dataset => rows_of(get(rt, "datasets", "/dataset", &[]).await?),
        Kind::Dashboard => rows_of(get(rt, "dashboards", "/dashboard", &[]).await?),
        Kind::Visualization => rows_of(get(rt, "visualizations", "/visualization", &[]).await?),
        Kind::Filter => rows_of(get(rt, "filters", "/filter", &[]).await?),
        Kind::Workspace => return workspaces(rt).await,
        Kind::Connection => rows_of(get(rt, "connections", "/connection", &[]).await?),
        Kind::SavedQuery => rows_of(get(rt, "lakehouse", "/lakehouse/query", &[]).await?),
        Kind::Agent => paged(rt, "agents", "/v1/agents").await?,
        Kind::OntologyModel => paged(rt, "etl", "/etl/ontology/models").await?,
        Kind::Compute => {
            let sizes = Lakehouse::new(rt.http.clone()).computes().await?;
            sizes
                .into_iter()
                .map(|c| (c.id.to_string(), c.name))
                .collect()
        }
        Kind::Warehouse => {
            let list = Lakehouse::new(rt.http.clone()).warehouses().await?;
            list.into_iter()
                .map(|w| (w.id.to_string(), w.name))
                .collect()
        }
        Kind::Job | Kind::Notebook | Kind::LineageAsset => {
            unreachable!("Catalog::list reads these itself")
        }
    };
    Ok(rows
        .into_iter()
        .map(|(id, name)| Candidate {
            id,
            name,
            project: None,
            detail: None,
        })
        .collect())
}

/// A workspace folder and, for a nested one, the folders above it.
struct Folder {
    id: String,
    name: String,
    /// `Sales / EMEA` for a folder inside EMEA inside Sales.
    parent: Option<String>,
}

impl Folder {
    fn path(&self) -> String {
        match &self.parent {
            Some(parent) => format!("{parent} / {}", self.name),
            None => self.name.clone(),
        }
    }
}

/// Every workspace folder, nested ones too. `GET /workspace` lists only the
/// top level, so each level's contents are read in turn; dashboards inside
/// folders are left out. A nested folder is described with its path, so
/// two folders of the same name can be told apart.
async fn workspaces(rt: &Runtime) -> Result<Vec<Candidate>> {
    let top = rows_of(get(rt, "workspaces", "/workspace", &[]).await?);
    let mut seen: HashSet<String> = top.iter().map(|(id, _)| id.clone()).collect();
    let mut level: Vec<Folder> = top
        .into_iter()
        .map(|(id, name)| Folder {
            id,
            name,
            parent: None,
        })
        .collect();
    let mut found = Vec::new();
    while !level.is_empty() {
        if found.len() + level.len() > MAX_FOLDERS {
            crate::output::warn(&format!(
                "stopped reading workspace folders after {MAX_FOLDERS}; use a folder's id for deeper ones"
            ));
            found.extend(level.into_iter().map(folder_candidate));
            break;
        }
        let replies = parallel(level.iter().map(|folder| {
            let rt = rt.clone();
            let path = format!("/workspace/{}/contents", seg(&folder.id));
            async move { get(&rt, "workspaces", &path, &[]).await }
        }))
        .await?;
        let mut next = Vec::new();
        for (folder, reply) in level.into_iter().zip(replies) {
            let path = folder.path();
            let reply =
                reply.with_context(|| format!("listing the folders inside workspace {path}"))?;
            next.extend(subfolders(&reply, &path, &mut seen));
            found.push(folder_candidate(folder));
        }
        level = next;
    }
    Ok(found)
}

/// The folders (not dashboards) in one folder's contents, each seen once.
fn subfolders(reply: &Value, parent: &str, seen: &mut HashSet<String>) -> Vec<Folder> {
    rows_in(reply)
        .iter()
        .filter(|row| row["isDashboard"].as_bool() != Some(true))
        .filter_map(pair)
        .filter(|(id, _)| seen.insert(id.clone()))
        .map(|(id, name)| Folder {
            id,
            name,
            parent: Some(parent.to_string()),
        })
        .collect()
}

fn folder_candidate(folder: Folder) -> Candidate {
    let detail = folder
        .parent
        .as_ref()
        .map(|parent| format!("{} (id {}) in {parent}", folder.name, folder.id));
    Candidate {
        id: folder.id,
        name: folder.name,
        project: None,
        detail,
    }
}

async fn get(rt: &Runtime, service: &str, path: &str, query: &[(String, String)]) -> Result<Value> {
    rt.http
        .request_json(service, Method::GET, path, query, None)
        .await
}

async fn paged(rt: &Runtime, service: &str, path: &str) -> Result<Vec<(String, String)>> {
    Ok(pairs(&paged_rows(rt, service, path).await?))
}

/// Every page of a `{items}` list read with `limit` and `offset`.
async fn paged_rows(rt: &Runtime, service: &str, path: &str) -> Result<Vec<Value>> {
    let mut rows = Vec::new();
    for page in 0..MAX_PAGES {
        let query = vec![
            ("limit".to_string(), PAGE.to_string()),
            ("offset".to_string(), (page * PAGE).to_string()),
        ];
        let reply = get(rt, service, path, &query).await?;
        let items = rows_in(&reply);
        let full = items.len() == PAGE;
        rows.extend(items);
        if !full {
            break;
        }
    }
    Ok(rows)
}

/// The rows of a bare list, `{items}`, or `{response}`.
fn rows_in(reply: &Value) -> Vec<Value> {
    [&reply["items"], &reply["response"], reply]
        .into_iter()
        .find_map(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn rows_of(reply: Value) -> Vec<(String, String)> {
    pairs(&rows_in(&reply))
}

fn pairs(rows: &[Value]) -> Vec<(String, String)> {
    rows.iter().filter_map(pair).collect()
}

/// `(id, name)` of a row keyed `Id`/`Name` or `id`/`name`.
fn pair(row: &Value) -> Option<(String, String)> {
    let id = ["Id", "id"].iter().find_map(|key| match &row[*key] {
        Value::Number(n) => Some(n.to_string()),
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        _ => None,
    })?;
    let name = ["Name", "name"]
        .iter()
        .find_map(|key| row[*key].as_str())
        .unwrap_or_default()
        .to_string();
    Some((id, name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_rows_in_every_envelope() {
        let bare = json!([{"Id": 1, "Name": "a"}]);
        let items = json!({"items": [{"id": "u-1", "name": "b"}]});
        let response = json!({"status": 200, "response": [{"id": 2, "name": "c"}]});
        assert_eq!(rows_of(bare), vec![("1".into(), "a".into())]);
        assert_eq!(rows_of(items), vec![("u-1".into(), "b".into())]);
        assert_eq!(rows_of(response), vec![("2".into(), "c".into())]);
        assert!(rows_of(json!({"x": 1})).is_empty());
    }

    #[test]
    fn describes_a_candidate_with_its_project() {
        let job = Candidate {
            id: "3013".into(),
            name: "DQC".into(),
            project: Some((1999, "Sales".into())),
            detail: None,
        };
        assert_eq!(job.describe(), "DQC (id 3013) in project Sales (1999)");
    }
}
