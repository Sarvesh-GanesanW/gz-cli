//! Compute left running that costs money while it waits: the lakehouse SQL
//! session gz keeps for this profile, and your running notebooks. Shared by
//! `gz status` and `gz usage compute`.
//!
//! Neither service can list everything: the lakehouse provider keeps no
//! list of open sessions (groundzero-iceberg-provider-api has only
//! per-session routes, app_routes/sessions.py and session_state.py; the web
//! app keeps its session id in browser storage), so only the session gz
//! itself kept is known. Notebooks are listed per project and per user
//! (groundzero-mlops-provider-api app.py:311, label selector
//! `isNotebook=true,userName=…,projectId=…`), without a start time.

use anyhow::{Context, Result};
use reqwest::Method;
use serde::Serialize;
use serde_json::{json, Value};

use crate::config::Profile;
use crate::http::ApiClient;
use crate::impact::lookup::parallel;
use crate::sql::session::Lakehouse;

const PROJECTS: &str = "etlprojects";
const MLOPS: &str = "mlops";
/// What `/lakehouse/sessionstate` says of a session that is gone.
const STOPPED: &str = "Stopped";
/// A statement's status while it runs (iceberg/ledger_answers.py:19).
const RUNNING_STATEMENT: &str = "RUNNING";

/// Said wherever SQL sessions are listed, so an empty list isn't read as
/// "nothing is running".
pub const SQL_SESSIONS_SCOPE: &str =
    "only the session gz keeps is known; the lakehouse service has no list of open sessions, so sessions started in the web app aren't shown";

/// The lakehouse session this profile keeps (`gz sql --keep-session`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SqlSession {
    pub session_id: String,
    pub warehouse: Option<String>,
    pub compute: Option<String>,
    /// `Starting`, `Restarting`, `Running` or `Stopping`.
    pub state: String,
    /// `None` for a session started before gz recorded start times, or by
    /// the TUI, which doesn't record them.
    pub started_ms: Option<i64>,
}

/// The session the profile keeps, when the server says it is not stopped.
pub async fn kept_sql_session(http: &ApiClient) -> Result<Option<SqlSession>> {
    let profile = &http.resolved().profile;
    let Some(session) = profile.sql_session.clone() else {
        return Ok(None);
    };
    let state = Lakehouse::new(http.clone())
        .session_state(&session)
        .await
        .with_context(|| format!("cannot ask the lakehouse about session {session}"))?;
    Ok((state != STOPPED).then(|| SqlSession {
        warehouse: profile.sql_session_warehouse.clone(),
        compute: profile.sql_compute.clone(),
        started_ms: started_ms(profile, &session),
        session_id: session,
        state,
    }))
}

/// Whether one of your statements is running in `session` right now
/// (`GET /lakehouse/sessionqueries/{id}`, app_routes/session_state.py:65;
/// web app Utils/lakehouseSessionApi.js:44). It lists running statements
/// and recently interrupted ones; none running means the session is idle.
pub async fn is_busy(http: &ApiClient, session: &str) -> Result<bool> {
    let path = format!("/lakehouse/sessionqueries/{}", crate::cmd::seg(session));
    let reply = http
        .request_json("lakehouse", Method::GET, &path, &[], None)
        .await?;
    Ok(has_running_statement(&reply))
}

fn has_running_statement(reply: &Value) -> bool {
    reply["response"]["queries"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|query| query["status"].as_str() == Some(RUNNING_STATEMENT))
}

/// When `session` started, if the profile recorded it for this session.
fn started_ms(profile: &Profile, session: &str) -> Option<i64> {
    profile
        .sql_session_started
        .as_ref()
        .filter(|start| start.session == session)
        .map(|start| start.at_ms)
}

/// One of your notebooks with a running pod.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunningNotebook {
    pub project_id: i64,
    pub project: String,
    pub notebook_id: Option<String>,
    pub name: Option<String>,
    /// The pod's phase (`Running`, `Pending`).
    pub status: String,
    pub runtime: Option<String>,
}

/// Your running notebooks across every project you can see, and the
/// projects that could not be asked.
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotebookScan {
    pub running: Vec<RunningNotebook>,
    pub unchecked_projects: Vec<String>,
}

#[derive(Debug, Clone)]
struct ProjectRef {
    id: i64,
    name: String,
}

/// Projects without notebooks can't have a notebook pod, so only projects
/// whose notebook list (fast) is not empty are asked for running pods
/// (slow: the provider runs kubectl for each).
pub async fn running_notebooks(http: &ApiClient) -> Result<NotebookScan> {
    let projects = projects(http).await?;
    let lists = parallel(
        projects
            .iter()
            .map(|p| notebook_list(http.clone(), p.id))
            .collect::<Vec<_>>(),
    )
    .await?;
    let mut scan = NotebookScan::default();
    let mut with_notebooks = Vec::new();
    for (project, list) in projects.into_iter().zip(lists) {
        match list {
            Ok(names) if names.is_empty() => {}
            Ok(names) => with_notebooks.push((project, names)),
            Err(_) => scan.unchecked_projects.push(project.name),
        }
    }
    let asks = with_notebooks
        .iter()
        .map(|(project, _)| running_pods(http.clone(), project.id));
    let answers = parallel(asks.collect::<Vec<_>>()).await?;
    for ((project, names), answer) in with_notebooks.iter().zip(answers) {
        match answer {
            Ok(answer) => scan.running.extend(pods(project, &answer, names)),
            Err(_) => scan.unchecked_projects.push(project.name.clone()),
        }
    }
    Ok(scan)
}

/// A project's notebooks as (id, name): `GET /jupyter/{project}/notebook`,
/// `{notebooks: [{id, name}]}` (mlops provider; web app Utils/notebookUtils.js).
async fn notebook_list(http: ApiClient, project: i64) -> Result<Vec<(String, String)>> {
    let path = format!("/jupyter/{project}/notebook");
    let list = http
        .request_json(MLOPS, Method::GET, &path, &[], None)
        .await?;
    Ok(list["notebooks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| Some((id_text(&row["id"])?, row["name"].as_str()?.to_string())))
        .collect())
}

/// `POST /jupyter/getRunningNotebooks {projectId}` (web app
/// Utils/notebookUtils.js:172): the caller's own pods in the project.
async fn running_pods(http: ApiClient, project: i64) -> Result<Value> {
    let body = json!({"projectId": project});
    http.request_json(
        MLOPS,
        Method::POST,
        "/jupyter/getRunningNotebooks",
        &[],
        Some(body),
    )
    .await
}

async fn projects(http: &ApiClient) -> Result<Vec<ProjectRef>> {
    let rows = http
        .request_json(PROJECTS, Method::GET, "/etl/etlProjects", &[], None)
        .await
        .context("cannot list your projects")?;
    Ok(rows
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| {
            Some(ProjectRef {
                id: row["Id"].as_i64()?,
                name: row["Name"].as_str().unwrap_or_default().to_string(),
            })
        })
        .collect())
}

/// `{success, data: [{notebookId, status, podId, rteName, …}]}`.
fn pods(project: &ProjectRef, answer: &Value, names: &[(String, String)]) -> Vec<RunningNotebook> {
    answer["data"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|pod| {
            let notebook_id = id_text(&pod["notebookId"]);
            let name = names
                .iter()
                .find(|(id, _)| Some(id) == notebook_id.as_ref())
                .map(|(_, name)| name.clone());
            RunningNotebook {
                project_id: project.id,
                project: project.name.clone(),
                notebook_id,
                name,
                status: pod["status"].as_str().unwrap_or("Running").to_string(),
                runtime: pod["rteName"].as_str().map(str::to_string),
            }
        })
        .collect()
}

fn id_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SessionStart;

    #[test]
    fn a_start_time_counts_only_for_its_own_session() {
        let mut profile = Profile {
            sql_session_started: Some(SessionStart {
                session: "old".into(),
                at_ms: 5,
            }),
            ..Profile::default()
        };
        assert_eq!(started_ms(&profile, "new"), None);
        profile.sql_session_started.as_mut().unwrap().session = "new".into();
        assert_eq!(started_ms(&profile, "new"), Some(5));
    }

    #[test]
    fn a_session_is_busy_only_while_a_statement_runs() {
        let busy = json!({"status": 200, "response": {"queries": [{"status": "RUNNING"}]}});
        let idle = json!({"status": 200, "response": {"queries": [{"status": "FAILED", "interrupted": true}]}});
        assert!(has_running_statement(&busy));
        assert!(!has_running_statement(&idle));
    }

    #[test]
    fn running_pods_keep_their_project_and_runtime() {
        let project = ProjectRef {
            id: 2098,
            name: "Home-Health-Rcm".into(),
        };
        let answer = json!({"success": true, "data": [
            {"notebookId": "1177", "status": "Running", "podId": "p1", "rteName": "gz-rte-sklearn:1.0.0"}
        ]});
        let names = [("1177".to_string(), "RCM-Denial-Risk".to_string())];
        let found = pods(&project, &answer, &names);
        assert_eq!(found[0].project, "Home-Health-Rcm");
        assert_eq!(found[0].name.as_deref(), Some("RCM-Denial-Risk"));
        assert_eq!(found[0].notebook_id.as_deref(), Some("1177"));
        assert_eq!(found[0].runtime.as_deref(), Some("gz-rte-sklearn:1.0.0"));
        assert!(pods(&project, &json!({"success": true, "data": []}), &names).is_empty());
    }
}
