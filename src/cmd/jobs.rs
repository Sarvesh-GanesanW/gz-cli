use anyhow::{Context, Result};
use owo_colors::OwoColorize;
use reqwest::Method;
use serde_json::{json, Value};
use std::time::Duration;

use crate::cli::{ExternalAction, JobsAction, LineageAction, OntologyAction};
use crate::cmd::{seg, Runtime};
use crate::output::OutputFormat;

const SVC: &str = "etl";
const LOG_POLL: Duration = Duration::from_secs(3);
const LOG_THROTTLE_WAIT: Duration = Duration::from_secs(5);

pub async fn run(rt: &Runtime, action: &JobsAction) -> Result<()> {
    match action {
        JobsAction::Run {
            project,
            job,
            compute,
            rte,
            workers,
            gpu,
            follow,
        } => {
            let choice = RunChoice {
                compute: *compute,
                rte: rte.clone(),
                workers: *workers,
                gpu: *gpu,
            };
            run_saved_job(rt, *project, *job, &choice, *follow).await
        }
        JobsAction::Submit { body } => rt.call(SVC, Method::POST, "/etl/job", body, None).await,
        JobsAction::Abort {
            session,
            execution,
            job_id,
        } => {
            let mut body = json!({"action": "Abort", "sessionId": session});
            if let Some(execution) = execution {
                body["jobIdentifier"] = json!(execution);
            }
            if let Some(job_id) = job_id {
                body["jobId"] = json!(job_id);
            }
            rt.send(SVC, Method::POST, "/etl/job", body).await
        }
        JobsAction::Status { job } => {
            let body = json!({"jobIdentifier": job});
            rt.send(SVC, Method::POST, "/etl/jobStatus", body).await
        }
        JobsAction::Logs {
            job,
            cursor,
            follow,
        } => logs(rt, job, cursor.clone(), *follow).await,
        JobsAction::CurrentRun { job_id } => {
            rt.get(SVC, &format!("/etl/jobs/{job_id}/current-run"), &[])
                .await
        }
        JobsAction::EksLogs { body } => {
            rt.call(SVC, Method::POST, "/etl/ekslogs", body, None).await
        }
        JobsAction::Schedule { body } => {
            rt.call(SVC, Method::POST, "/etl/submitScheduledJob", body, None)
                .await
        }
        JobsAction::Read { body } => rt.call(SVC, Method::POST, "/etl/getdata", body, None).await,
        JobsAction::Metrics { body } => {
            rt.call(SVC, Method::GET, "/etl/consumption-metrics", body, None)
                .await
        }
        JobsAction::ExecutionMetrics { execution } => {
            rt.get(
                SVC,
                &format!("/etl/consumption-metrics/{}", seg(execution)),
                &[],
            )
            .await
        }
        JobsAction::ProjectMetrics => {
            rt.get(SVC, "/etl/consumption-metrics/aggregates/projects", &[])
                .await
        }
        JobsAction::Insights { job_id } => {
            rt.get(
                SVC,
                &format!("/etl/consumption-metrics/jobs/{job_id}/insights"),
                &[],
            )
            .await
        }
        JobsAction::Collect { execution, body } => {
            let path = format!("/etl/observability/executions/{}/collect", seg(execution));
            rt.call(SVC, Method::POST, &path, body, None).await
        }
        JobsAction::LineageHealth => rt.get(SVC, "/etl/lineage/health", &[]).await,
        JobsAction::LineageJob { job_id } => {
            rt.get(SVC, &format!("/etl/lineage/jobs/{job_id}"), &[])
                .await
        }
        JobsAction::LineageDataset { dataset_id } => {
            rt.get(SVC, &format!("/etl/lineage/datasets/{dataset_id}"), &[])
                .await
        }
        JobsAction::LineageAssets { connection } => {
            let query = [("connectionId".to_string(), connection.to_string())];
            rt.get(SVC, "/etl/lineage/assets", &query).await
        }
        JobsAction::Lineage { action } => lineage(rt, action).await,
        JobsAction::Ontology { action } => ontology(rt, action).await,
        JobsAction::External { action } => external(rt, action).await,
    }
}

/// Runtime choices the web app asks for when you press Run.
pub struct RunChoice {
    pub compute: i64,
    pub rte: String,
    pub workers: Option<u32>,
    pub gpu: Option<u32>,
}

/// Reads the saved job and its project, submits them the way the web app's
/// job editor does, and optionally tails the run's logs.
async fn run_saved_job(
    rt: &Runtime,
    project: i64,
    job: i64,
    choice: &RunChoice,
    follow: bool,
) -> Result<()> {
    // GET /etl/etlProjects/{id} is unreliable, so find the project in the list.
    let projects = rt
        .http
        .request_json("etlprojects", Method::GET, "/etl/etlProjects", &[], None)
        .await?;
    let project_name = projects
        .as_array()
        .and_then(|rows| rows.iter().find(|p| p["Id"].as_i64() == Some(project)))
        .and_then(|p| p["Name"].as_str())
        .with_context(|| format!("project {project} not found; see `gz projects list`"))?
        .to_string();
    let saved = rt
        .http
        .request_json(
            "etlprojects",
            Method::GET,
            &crate::cmd::apps::saved_job_path(&project.to_string(), &job.to_string()),
            &[],
            None,
        )
        .await?;
    let spec = run_spec(&project_name, project, job, &saved, choice)?;
    let started = rt
        .http
        .request_json(SVC, Method::POST, "/etl/job", &[], Some(spec))
        .await?;
    let execution = started["jobIdentifier"].as_str().map(str::to_string);
    if !follow {
        return rt.show(started).await;
    }
    let execution =
        execution.context("the run started but returned no job identifier to follow")?;
    if rt.format == OutputFormat::Table {
        crate::output::info(&format!("run {execution} started; streaming logs…"));
    }
    logs(rt, &execution, None, true).await
}

/// The `/etl/job` submit body, built from a saved job like the web app's
/// `submitAJob` (etlUtlis.js): job config + project + runtime choice.
pub fn run_spec(
    project_name: &str,
    project: i64,
    job: i64,
    saved: &Value,
    choice: &RunChoice,
) -> Result<Value> {
    let config = &saved["config"];
    let job_name = saved["Name"]
        .as_str()
        .or(saved["name"].as_str())
        .context("the saved job has no name")?;
    let workers = choice.workers.map(Value::from).unwrap_or_else(|| {
        config["minimumWorkers"]
            .as_u64()
            .map(Value::from)
            .unwrap_or(json!(2))
    });
    let gpu = choice.gpu.map(Value::from).unwrap_or_else(|| {
        config["gpuSize"]
            .as_u64()
            .map(Value::from)
            .unwrap_or(json!(0))
    });
    let mut spec = json!({
        "ConnectionNames": config["connectionNames"].as_array().cloned().unwrap_or_default(),
        "projectName": project_name,
        "projectId": project,
        "jobName": job_name.split_whitespace().collect::<String>(),
        "jobId": job,
        "computeId": choice.compute,
        "rteName": choice.rte,
        "minimumWorkers": workers,
        "gpuSize": gpu,
    });
    let sub_jobs = config["selectedJobs"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if !sub_jobs.is_empty() {
        spec["subJobIds"] = sub_jobs.iter().map(|j| j["Id"].clone()).collect();
        spec["subJobNames"] = sub_jobs.iter().map(|j| j["Name"].clone()).collect();
    }
    let models = config["modelNames"].as_array().cloned().unwrap_or_default();
    if !models.is_empty() {
        spec["modelName"] = Value::Array(models);
        spec["importModel"] = json!(true);
    }
    Ok(spec)
}

/// One page of logs, or (`--follow`) every page until the run is done.
/// The server hands back a `cursor` to continue from on each page.
async fn logs(rt: &Runtime, job: &str, mut cursor: Option<String>, follow: bool) -> Result<()> {
    loop {
        let mut body = json!({"jobIdentifier": job});
        if let Some(cursor) = &cursor {
            body["cursor"] = json!(cursor);
        }
        let page = match rt
            .http
            .request_json(SVC, Method::POST, "/etl/logs", &[], Some(body))
            .await
        {
            Ok(page) => page,
            Err(err) if follow && err.to_string().contains("HTTP 429") => {
                tokio::time::sleep(LOG_THROTTLE_WAIT).await;
                continue;
            }
            Err(err) => return Err(err),
        };
        if !follow {
            return rt.show(page).await;
        }
        print_log_events(rt, &page)?;
        cursor = page["cursor"].as_str().map(str::to_string).or(cursor);
        if page["hasMoreLogs"] != Value::Bool(true) {
            if let Some(reason) = page["staleReason"].as_str() {
                crate::output::warn(reason);
            }
            return Ok(());
        }
        tokio::time::sleep(LOG_POLL).await;
    }
}

fn print_log_events(rt: &Runtime, page: &Value) -> Result<()> {
    let events = page["events"].as_array().cloned().unwrap_or_default();
    for event in events {
        if rt.format != OutputFormat::Table {
            println!("{}", serde_json::to_string(&event)?);
            continue;
        }
        let time = event["timestamp"].as_str().unwrap_or_default();
        let message = event["message"].as_str().unwrap_or_default();
        println!("{} {}", time.dimmed(), message.trim_end());
    }
    Ok(())
}

/// The web app's lineage explorer (`apiCatalog/lineageApi.js`).
async fn lineage(rt: &Runtime, action: &LineageAction) -> Result<()> {
    const ASSETS: &str = "/etl/lineage/assets";
    match action {
        LineageAction::Asset { id, depth, run } => {
            let mut query = Vec::new();
            push_opt(&mut query, "depth", depth);
            push_opt(&mut query, "runId", run);
            rt.get(SVC, &format!("{ASSETS}/{id}"), &query).await
        }
        LineageAction::Movement { id } => {
            rt.get(SVC, &format!("{ASSETS}/{id}/movement"), &[]).await
        }
        LineageAction::Impact { id, depth } => {
            let mut query = Vec::new();
            push_opt(&mut query, "depth", depth);
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::GET,
                    &format!("{ASSETS}/{id}/impact"),
                    &query,
                    None,
                )
                .await?;
            if rt.format != OutputFormat::Table {
                return rt.show(value).await;
            }
            rt.show_columns(
                &value["assets"],
                &["displayName", "assetType", "depth", "assetId"],
            )
        }
        LineageAction::Versions { id } => {
            let mut query = Vec::new();
            push_opt(&mut query, "limit", &rt.limit);
            rt.get(SVC, &format!("{ASSETS}/{id}/versions"), &query)
                .await
        }
        LineageAction::Connections => rt.get(SVC, "/etl/lineage/connections", &[]).await,
        LineageAction::Export {
            kind,
            format,
            connection,
            offset,
        } => {
            let mut query = vec![
                ("kind".to_string(), kind.clone()),
                ("format".to_string(), format.as_str().to_string()),
            ];
            push_opt(&mut query, "limit", &rt.limit);
            push_opt(&mut query, "connectionId", connection);
            push_opt(&mut query, "offset", offset);
            let value = rt
                .http
                .request_json(SVC, Method::GET, "/etl/lineage/export", &query, None)
                .await?;
            // CSV and OpenLineage come back as text meant for a file or a pipe.
            match value {
                Value::String(text) => {
                    print!("{text}");
                    Ok(())
                }
                other => rt.show(other).await,
            }
        }
    }
}

fn push_opt<T: ToString>(query: &mut Vec<(String, String)>, key: &str, value: &Option<T>) {
    if let Some(value) = value {
        query.push((key.to_string(), value.to_string()));
    }
}

async fn ontology(rt: &Runtime, action: &OntologyAction) -> Result<()> {
    const MODELS: &str = "/etl/ontology/models";
    match action {
        OntologyAction::List => rt.get(SVC, MODELS, &[]).await,
        OntologyAction::Get { id } => rt.get(SVC, &format!("{MODELS}/{id}"), &[]).await,
        OntologyAction::Delete { id } => {
            rt.send_empty(SVC, Method::DELETE, &format!("{MODELS}/{id}"))
                .await
        }
        OntologyAction::Usage { id } => rt.get(SVC, &format!("{MODELS}/{id}/usage"), &[]).await,
        OntologyAction::Calculate { body } => {
            rt.call(
                SVC,
                Method::POST,
                &format!("{MODELS}/calculate"),
                body,
                None,
            )
            .await
        }
        OntologyAction::Releases { id } => {
            rt.get(SVC, &format!("{MODELS}/{id}/releases"), &[]).await
        }
        OntologyAction::Release { id, body } => {
            rt.call(
                SVC,
                Method::POST,
                &format!("{MODELS}/{id}/releases"),
                body,
                None,
            )
            .await
        }
        OntologyAction::Citations { body } => {
            rt.call(
                SVC,
                Method::POST,
                "/etl/ontology/citation-events",
                body,
                None,
            )
            .await
        }
        OntologyAction::Resolve { body } => {
            rt.call(
                SVC,
                Method::POST,
                "/etl/ontology/context:resolve",
                body,
                None,
            )
            .await
        }
        OntologyAction::Define { id, body } => {
            rt.call(
                SVC,
                Method::POST,
                &format!("{MODELS}/{id}/definitions"),
                body,
                None,
            )
            .await
        }
        OntologyAction::PatchDefinition {
            id,
            definition,
            body,
        } => {
            let path = format!("{MODELS}/{id}/definitions/{definition}");
            rt.call(SVC, Method::PATCH, &path, body, None).await
        }
        OntologyAction::PatchDefinitions { id, body } => {
            rt.call(
                SVC,
                Method::PATCH,
                &format!("{MODELS}/{id}/definitions"),
                body,
                None,
            )
            .await
        }
        OntologyAction::Drift { id } => rt.get(SVC, &format!("{MODELS}/{id}/drift"), &[]).await,
        OntologyAction::Imports { id } => rt.get(SVC, &format!("{MODELS}/{id}/imports"), &[]).await,
        OntologyAction::ReadRelease { id, release } => {
            rt.get(
                SVC,
                &format!("{MODELS}/{id}/releases/{}", seg(release)),
                &[],
            )
            .await
        }
    }
}

async fn external(rt: &Runtime, action: &ExternalAction) -> Result<()> {
    match action {
        ExternalAction::Submit { body } => {
            rt.call(SVC, Method::POST, "/external/etl/job", body, None)
                .await
        }
        ExternalAction::Status { body } => {
            rt.call(SVC, Method::GET, "/external/etl/job/status", body, None)
                .await
        }
        ExternalAction::Logs { body } => {
            rt.call(SVC, Method::GET, "/external/etl/job/logs", body, None)
                .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice() -> RunChoice {
        RunChoice {
            compute: 1003,
            rte: "gz-rte-spark-pytorch:1.0.0".into(),
            workers: None,
            gpu: None,
        }
    }

    #[test]
    fn run_spec_matches_the_web_app() {
        let saved = json!({
            "Name": "nightly etl",
            "config": {"connectionNames": ["GZS3"], "minimumWorkers": 1, "gpuSize": 0,
                       "selectedJobs": [], "modelNames": []},
        });
        let spec = run_spec("ETL", 1000, 1006, &saved, &choice()).unwrap();
        assert_eq!(
            spec,
            json!({
                "ConnectionNames": ["GZS3"], "projectName": "ETL", "projectId": 1000,
                "jobName": "nightlyetl", "jobId": 1006, "computeId": 1003,
                "rteName": "gz-rte-spark-pytorch:1.0.0", "minimumWorkers": 1, "gpuSize": 0,
            })
        );
    }

    #[test]
    fn run_spec_adds_sub_jobs_models_and_overrides() {
        let saved = json!({
            "Name": "multi",
            "config": {"connectionNames": [], "selectedJobs": [{"Id": 1006, "Name": "a"}],
                       "modelNames": ["churn"]},
        });
        let mut pick = choice();
        pick.workers = Some(4);
        let spec = run_spec("ETL", 1000, 1063, &saved, &pick).unwrap();
        assert_eq!(spec["subJobIds"], json!([1006]));
        assert_eq!(spec["subJobNames"], json!(["a"]));
        assert_eq!(spec["modelName"], json!(["churn"]));
        assert_eq!(spec["importModel"], true);
        assert_eq!(spec["minimumWorkers"], 4);
        assert_eq!(spec["gpuSize"], 0);
    }
}
