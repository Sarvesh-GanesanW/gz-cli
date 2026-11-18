//! `gz open <kind> <id-or-name>`: the web app page for a resource, opened
//! with the system opener (`open`, `xdg-open` or `start`), or printed with
//! `--print`. With no arguments, the site's home page.
//!
//! Routes come from the web app's routers (ground-zero, branch `stage`):
//! - `/` home, `/conversation/:conversationId` chat: src/AppRoutes.js:149-151
//! - `/designer/datasets/:slug`, `/designer/dashboards/:slug`: src/Designer.js:47,61
//! - `/etl/projectId/:id/projectName/:name`, `/etl/code-editor/:slug`,
//!   `/etl/notebook/:slug`, `…/agents/:agentId`: src/pages/etl/Jobs/JobsContainer.js:145,173,195,267
//! - `/lakehouse/code-editor/:slug`: src/pages/lakehouse/Databases/LakehouseContainer.js:272
//!
//! and their query strings and slugs from the links the web app builds:
//! src/components/global/finder/finderKinds.js (project, job, agent,
//! dataset, dashboard, warehouse) and src/pages/etl/Jobs/NotebooksList.js:758.

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use clap::{Args, ValueEnum};
use reqwest::Method;
use serde_json::{json, Value};

use crate::cmd::{job_fix, seg, Runtime};
use crate::impact::lookup::parallel;
use crate::output::{self, OutputFormat};
use crate::sql::session::Lakehouse;
use crate::tui::chat::parse_conversations;

const SEARCH_RESULTS: &str = "20";

#[derive(Args, Debug)]
#[command(about = "Open a resource's page in the web app (or print its URL with --print)")]
pub struct OpenArgs {
    #[arg(value_enum, help = "What to open; leave out for the site's home page")]
    pub kind: Option<PageKind>,
    #[arg(help = "Its id or name; a table is warehouse.namespace.table, a run its execution id")]
    pub target: Option<String>,
    #[arg(
        long,
        help = "The project of a job or notebook, when the name is in several"
    )]
    pub project: Option<i64>,
    #[arg(long, help = "Print the URL instead of opening it")]
    pub print: bool,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageKind {
    Job,
    Project,
    Table,
    Run,
    Dashboard,
    Dataset,
    Notebook,
    Agent,
    Chat,
}

/// A path on the web app, and what to tell the user when it is not
/// exactly the page asked for.
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub path: String,
    pub note: Option<String>,
}

impl Page {
    fn at(path: String) -> Self {
        Self { path, note: None }
    }

    fn with_note(path: String, note: &str) -> Self {
        Self {
            path,
            note: Some(note.to_string()),
        }
    }
}

pub async fn run(rt: &Runtime, args: &OpenArgs) -> Result<()> {
    let origin = rt.http.resolved().web_url().context(
        "gz doesn't know this site's web address; set the site and client with `gz configure`",
    )?;
    let page = match (args.kind, args.target.as_deref()) {
        // clap fills the kind before the target, so no kind means neither.
        (None, _) => Page::at("/".into()),
        (Some(kind), None) => bail!(
            "say which {}: gz open {} <id-or-name>",
            noun(kind),
            noun(kind)
        ),
        (Some(kind), Some(target)) => resolve(rt, kind, target, args.project).await?,
    };
    let url = format!("{origin}{}", page.path);
    let print = args.print || rt.format != OutputFormat::Table;
    deliver(rt.format, &url, &page, print, system_open)
}

/// Prints or opens `url`. `opener` is the seam tests replace, so no test
/// ever starts a browser.
fn deliver(
    format: OutputFormat,
    url: &str,
    page: &Page,
    print: bool,
    opener: impl FnOnce(&str) -> Result<()>,
) -> Result<()> {
    if format != OutputFormat::Table {
        return output::print_value(&json!({"url": url, "note": page.note}), format, None);
    }
    if let Some(note) = &page.note {
        output::info(note);
    }
    if print {
        println!("{url}");
        return Ok(());
    }
    opener(url).with_context(|| format!("cannot open a browser; the page is {url}"))?;
    output::info(&format!("opened {url}"));
    Ok(())
}

fn system_open(url: &str) -> Result<()> {
    open::that(url).context("the system opener failed")
}

fn noun(kind: PageKind) -> &'static str {
    match kind {
        PageKind::Job => "job",
        PageKind::Project => "project",
        PageKind::Table => "table",
        PageKind::Run => "run",
        PageKind::Dashboard => "dashboard",
        PageKind::Dataset => "dataset",
        PageKind::Notebook => "notebook",
        PageKind::Agent => "agent",
        PageKind::Chat => "chat",
    }
}

async fn resolve(rt: &Runtime, kind: PageKind, target: &str, project: Option<i64>) -> Result<Page> {
    match kind {
        PageKind::Project => {
            let found = project_by(rt, target).await?;
            Ok(Page::at(project_path(id_number(&found)?, &found.name)))
        }
        PageKind::Job => {
            let (project, job) = job_fix::resolve_job(rt, target, project).await?;
            Ok(Page::at(job_path(project, job.id)))
        }
        PageKind::Dataset => designer_page(rt, &DATASETS, target).await,
        PageKind::Dashboard => designer_page(rt, &DASHBOARDS, target).await,
        PageKind::Agent => agent_page(rt, target).await,
        PageKind::Notebook => notebook_page(rt, target, project).await,
        PageKind::Chat => chat_page(rt, target).await,
        PageKind::Run => run_page(rt, target).await,
        PageKind::Table => table_page(rt, target).await,
    }
}

// ---------- routes ----------

fn project_path(id: i64, name: &str) -> String {
    format!("/etl/projectId/{id}/projectName/{}", seg(name))
}

fn job_path(project: i64, job: i64) -> String {
    format!("/etl/code-editor/{job}?projectId={project}&jobId={job}")
}

fn notebook_path(project: i64, id: i64, name: &str) -> String {
    format!(
        "/etl/notebook/{}?projectId={project}&notebookId={id}",
        seg(name)
    )
}

fn agent_path(project: i64, project_name: &str, agent: &str) -> String {
    format!(
        "/etl/projectId/{project}/projectName/{}/agents/{}",
        seg(project_name),
        seg(agent)
    )
}

fn designer_path(section: &str, id: i64) -> String {
    format!("/designer/{section}/{id}")
}

fn chat_path(id: &str) -> String {
    format!("/conversation/{}", seg(id))
}

/// The warehouse's SQL editor: `btoa("name&id")`, as the web app's finder
/// builds it.
fn warehouse_path(name: &str, id: i64) -> String {
    format!(
        "/lakehouse/code-editor/{}",
        B64.encode(format!("{name}&{id}"))
    )
}

// ---------- matching ----------

/// One named thing to choose among.
#[derive(Debug, Clone, PartialEq)]
struct Named {
    id: String,
    name: String,
    row: Value,
}

/// The item whose id or name is `wanted`: an exact match first, then a
/// name that differs only in case. Several matches is an error that lists
/// them, so the user can pass the id.
fn pick(items: Vec<Named>, wanted: &str, what: &str) -> Result<Named> {
    let exact: Vec<&Named> = items
        .iter()
        .filter(|item| item.id == wanted || item.name == wanted)
        .collect();
    let matches = if exact.is_empty() {
        items
            .iter()
            .filter(|item| item.name.eq_ignore_ascii_case(wanted))
            .collect()
    } else {
        exact
    };
    match matches.as_slice() {
        [] => bail!("no {what} named or numbered '{wanted}' that you can see"),
        [one] => Ok((*one).clone()),
        several => {
            let listed: Vec<String> = several
                .iter()
                .map(|item| format!("{} (id {})", item.name, item.id))
                .collect();
            bail!(
                "several {what}s match '{wanted}': {}; pass the id",
                listed.join(", ")
            )
        }
    }
}

fn text_of(value: &Value) -> Option<String> {
    match value {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn named(rows: &[Value], id_key: &str, name_key: &str) -> Vec<Named> {
    rows.iter()
        .filter_map(|row| {
            Some(Named {
                id: text_of(&row[id_key])?,
                name: row[name_key].as_str().unwrap_or_default().to_string(),
                row: row.clone(),
            })
        })
        .collect()
}

fn id_number(named: &Named) -> Result<i64> {
    named
        .id
        .parse()
        .with_context(|| format!("{} has a non-numeric id {}", named.name, named.id))
}

// ---------- lookups ----------

/// The web app's finder search (gz-chat-eks routes/search.py): names only,
/// with the project as `parentId`/`context` and an agent's UUID as `routeKey`.
async fn search(rt: &Runtime, kind: &str, wanted: &str) -> Result<Vec<Value>> {
    let query = vec![
        ("q".to_string(), wanted.to_string()),
        ("kinds".to_string(), kind.to_string()),
        ("limit".to_string(), SEARCH_RESULTS.to_string()),
    ];
    let reply = rt
        .http
        .request_json("chat", Method::GET, "/search", &query, None)
        .await?;
    Ok(reply["results"].as_array().cloned().unwrap_or_default())
}

async fn projects(rt: &Runtime) -> Result<Vec<Named>> {
    let rows = rt
        .http
        .request_json("etlprojects", Method::GET, "/etl/etlProjects", &[], None)
        .await?;
    Ok(named(
        rows.as_array().map(Vec::as_slice).unwrap_or(&[]),
        "Id",
        "Name",
    ))
}

async fn project_by(rt: &Runtime, wanted: &str) -> Result<Named> {
    pick(projects(rt).await?, wanted, "project")
}

/// A dataset or dashboard by id or name, from the designer's own list
/// (`GET /dataset`, `GET /dashboard`: `[{Id, Name}]`), so an id that
/// doesn't exist is refused rather than opened as a blank page.
async fn designer_page(rt: &Runtime, kind: &DesignerKind, target: &str) -> Result<Page> {
    let rows = rt
        .http
        .request_json(kind.service, Method::GET, kind.list, &[], None)
        .await?;
    let rows = rows.as_array().map(Vec::as_slice).unwrap_or(&[]);
    let found = pick(named(rows, "Id", "Name"), target, kind.noun)?;
    Ok(Page::at(designer_path(kind.section, id_number(&found)?)))
}

/// A designer resource: its gateway, list path, and web app section.
struct DesignerKind {
    noun: &'static str,
    service: &'static str,
    list: &'static str,
    section: &'static str,
}

const DATASETS: DesignerKind = DesignerKind {
    noun: "dataset",
    service: "datasets",
    list: "/dataset",
    section: "datasets",
};

const DASHBOARDS: DesignerKind = DesignerKind {
    noun: "dashboard",
    service: "dashboards",
    list: "/dashboard",
    section: "dashboards",
};

/// An agent's page sits under its project. The finder search gives the
/// project (`parentId`, `context`) and the agent's UUID (`routeKey`); an
/// agent asked for by UUID is first named from `GET /v1/agents/{id}`.
async fn agent_page(rt: &Runtime, target: &str) -> Result<Page> {
    let (name, uuid) = if looks_like_uuid(target) {
        let path = format!("/v1/agents/{}", seg(target));
        let agent = rt
            .http
            .request_json("agents", Method::GET, &path, &[], None)
            .await?;
        let name = agent["name"]
            .as_str()
            .context("the agent builder sent the agent without a name")?;
        (name.to_string(), Some(target.to_string()))
    } else {
        (target.to_string(), None)
    };
    let hits = search(rt, "agent", &name).await?;
    let hit = match &uuid {
        Some(uuid) => hits
            .iter()
            .find(|hit| hit["routeKey"].as_str() == Some(uuid))
            .cloned()
            .with_context(|| format!("search can't find agent {uuid} ({name})"))?,
        None => pick(named(&hits, "id", "name"), &name, "agent")?.row,
    };
    agent_path_of(&hit).map(Page::at)
}

fn agent_path_of(hit: &Value) -> Result<String> {
    let name = hit["name"].as_str().unwrap_or("the agent");
    let project = hit["parentId"].as_i64().with_context(|| {
        format!("{name} belongs to no project, and the web app opens agents from their project")
    })?;
    let project_name = hit["context"].as_str().unwrap_or_default();
    let uuid = hit["routeKey"]
        .as_str()
        .with_context(|| format!("search sent {name} without its agent id"))?;
    Ok(agent_path(project, project_name, uuid))
}

fn looks_like_uuid(text: &str) -> bool {
    text.len() == 36 && text.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// A notebook in `project`, or looked for in every project you can see
/// (`GET /jupyter/{project}/notebook`, `{notebooks: [{id, name}]}`).
async fn notebook_page(rt: &Runtime, target: &str, project: Option<i64>) -> Result<Page> {
    let projects: Vec<i64> = match project {
        Some(id) => vec![id],
        None => projects(rt)
            .await?
            .iter()
            .map(id_number)
            .collect::<Result<_>>()?,
    };
    let lists = projects.iter().map(|&project| {
        let http = rt.http.clone();
        async move {
            let path = format!("/jupyter/{project}/notebook");
            let list = http
                .request_json("mlops", Method::GET, &path, &[], None)
                .await?;
            anyhow::Ok((project, list))
        }
    });
    let mut candidates = Vec::new();
    for answer in parallel(lists.collect::<Vec<_>>()).await? {
        let (project, list) = answer?;
        let rows = list["notebooks"].as_array().cloned().unwrap_or_default();
        candidates.extend(named(&rows, "id", "name").into_iter().map(|mut notebook| {
            notebook.row = json!(project);
            notebook
        }));
    }
    let found = pick(candidates, target, "notebook")?;
    let project = found
        .row
        .as_i64()
        .context("the notebook lost its project")?;
    Ok(Page::at(notebook_path(
        project,
        id_number(&found)?,
        &found.name,
    )))
}

/// One of your conversations by id or title (`GET /chat/conversations`),
/// so an id that isn't yours is refused rather than opened blank.
async fn chat_page(rt: &Runtime, target: &str) -> Result<Page> {
    let list = rt
        .http
        .request_json("chat", Method::GET, "/chat/conversations", &[], None)
        .await?;
    let chats: Vec<Named> = parse_conversations(&list)
        .into_iter()
        .map(|chat| Named {
            id: chat.id,
            name: chat.title,
            row: Value::Null,
        })
        .collect();
    let found = pick(chats, target, "chat")?;
    Ok(Page::at(chat_path(&found.id)))
}

const RUN_NOTE: &str =
    "the web app has no page for a single run; this is its job, whose run history lists it";
const DATASET_RUN_NOTE: &str =
    "the web app has no page for a single run; this is the dataset it refreshed";
const TABLE_NOTE: &str =
    "the web app has no page for one table; this is its warehouse's SQL editor, where the table is in the schema tree";

/// A run's job (or dataset), read from the run's own record
/// (`GET /etl/consumption-metrics/{executionId}`, `identity`).
async fn run_page(rt: &Runtime, execution: &str) -> Result<Page> {
    let path = format!("/etl/consumption-metrics/{}", seg(execution));
    let detail = rt
        .http
        .request_json("etl", Method::GET, &path, &[], None)
        .await?;
    run_page_of(&detail["identity"], execution)
}

fn run_page_of(identity: &Value, execution: &str) -> Result<Page> {
    let number = |key: &str| -> Option<i64> {
        match &identity[key] {
            Value::Number(n) => n.as_i64(),
            Value::String(text) => text.parse().ok(),
            _ => None,
        }
    };
    if let (Some(job), Some(project)) = (number("jobId"), number("projectId")) {
        return Ok(Page::with_note(job_path(project, job), RUN_NOTE));
    }
    if let Some(dataset) = number("datasetId") {
        return Ok(Page::with_note(
            designer_path("datasets", dataset),
            DATASET_RUN_NOTE,
        ));
    }
    bail!("run {execution} names no job or dataset to open")
}

/// `warehouse` or `warehouse.namespace.table`: the warehouse's editor.
async fn table_page(rt: &Runtime, target: &str) -> Result<Page> {
    let warehouse = target.split('.').next().unwrap_or(target);
    let warehouses = Lakehouse::new(rt.http.clone()).warehouses().await?;
    let found = warehouses
        .iter()
        .find(|w| w.name == warehouse)
        .or_else(|| {
            warehouses
                .iter()
                .find(|w| w.name.eq_ignore_ascii_case(warehouse))
        })
        .with_context(|| format!("no warehouse named '{warehouse}'; see `gz warehouse list`"))?;
    let path = warehouse_path(&found.name, found.id);
    Ok(if warehouse == target {
        Page::at(path)
    } else {
        Page::with_note(path, TABLE_NOTE)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, name: &str) -> Named {
        Named {
            id: id.into(),
            name: name.into(),
            row: Value::Null,
        }
    }

    #[test]
    fn routes_match_the_web_app() {
        assert_eq!(
            project_path(2098, "Home Health"),
            "/etl/projectId/2098/projectName/Home%20Health"
        );
        assert_eq!(
            job_path(1510, 2125),
            "/etl/code-editor/2125?projectId=1510&jobId=2125"
        );
        assert_eq!(
            notebook_path(2098, 1177, "RCM-Denial-Risk"),
            "/etl/notebook/RCM-Denial-Risk?projectId=2098&notebookId=1177"
        );
        assert_eq!(
            agent_path(2098, "Home-Health-Rcm", "1a37e97d-893f-47ea-af39-ef5ba85b6128"),
            "/etl/projectId/2098/projectName/Home-Health-Rcm/agents/1a37e97d-893f-47ea-af39-ef5ba85b6128"
        );
        assert_eq!(designer_path("dashboards", 9), "/designer/dashboards/9");
        assert_eq!(chat_path("ab02-11"), "/conversation/ab02-11");
        // btoa("Lineage&1108")
        assert_eq!(
            warehouse_path("Lineage", 1108),
            "/lakehouse/code-editor/TGluZWFnZSYxMTA4"
        );
    }

    #[test]
    fn picks_by_id_then_name_then_case() {
        let items = vec![item("1", "Sales"), item("2", "sales"), item("3", "Ops")];
        assert_eq!(pick(items.clone(), "3", "project").unwrap().name, "Ops");
        assert_eq!(pick(items.clone(), "sales", "project").unwrap().id, "2");
        assert_eq!(pick(items.clone(), "ops", "project").unwrap().id, "3");
        let err = pick(items.clone(), "SALES", "project")
            .unwrap_err()
            .to_string();
        assert!(err.contains("Sales (id 1), sales (id 2)"), "{err}");
        assert!(pick(items, "nope", "project").is_err());
    }

    #[test]
    fn a_run_opens_its_job_or_dataset_and_says_why() {
        let page = run_page_of(
            &json!({"jobId": 1111, "projectId": 1021, "workloadType": "job"}),
            "r",
        )
        .unwrap();
        assert_eq!(page.path, "/etl/code-editor/1111?projectId=1021&jobId=1111");
        assert_eq!(page.note.as_deref(), Some(RUN_NOTE));
        let page = run_page_of(&json!({"datasetId": "1837", "jobId": null}), "r").unwrap();
        assert_eq!(page.path, "/designer/datasets/1837");
        assert!(run_page_of(&json!({}), "r").is_err());
    }

    #[test]
    fn an_agent_needs_its_project() {
        let hit = json!({"name": "bot", "parentId": 2098, "context": "Home-Health-Rcm",
                         "routeKey": "1a37e97d-893f-47ea-af39-ef5ba85b6128"});
        assert!(agent_path_of(&hit)
            .unwrap()
            .ends_with("/agents/1a37e97d-893f-47ea-af39-ef5ba85b6128"));
        let orphan = json!({"name": "bot", "parentId": null, "routeKey": "x"});
        assert!(agent_path_of(&orphan)
            .unwrap_err()
            .to_string()
            .contains("belongs to no project"));
    }

    #[test]
    fn print_never_opens_and_open_uses_the_opener() {
        let page = Page::at("/".into());
        let mut opened = None;
        deliver(OutputFormat::Table, "https://s.c.d/", &page, true, |_| {
            panic!("--print must not open anything")
        })
        .unwrap();
        deliver(OutputFormat::Json, "https://s.c.d/", &page, false, |_| {
            panic!("JSON output must not open anything")
        })
        .unwrap();
        deliver(OutputFormat::Table, "https://s.c.d/", &page, false, |url| {
            opened = Some(url.to_string());
            Ok(())
        })
        .unwrap();
        assert_eq!(opened.as_deref(), Some("https://s.c.d/"));
    }

    #[test]
    fn a_failed_opener_still_gives_the_url() {
        let page = Page::at("/".into());
        let err = deliver(OutputFormat::Table, "https://s.c.d/x", &page, false, |_| {
            bail!("no display")
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("https://s.c.d/x"));
    }
}
