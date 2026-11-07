use anyhow::{Context, Result};
use reqwest::Method;
use serde_json::{json, Value};

use crate::cli::{
    ApiKeyAction, ConnectionsAction, DesignerAction, DesignerKind, PermissionsAction,
    ProjectsAction, ScheduleEmailAction, SchedulesAction, WorkspacesAction,
};
use crate::cmd::{read_data_arg, read_text_arg, seg, Runtime};

/// Columns worth seeing in the gateway list endpoints (they all share this shape).
const NAMED_ROW_COLUMNS: &[&str] = &["Name", "Id", "owner", "type", "modified_by", "modified_on"];

/// The gateways accept numeric ids as JSON numbers; keep anything else as text.
fn id_value(id: &str) -> Value {
    id.parse::<i64>()
        .map(Value::from)
        .unwrap_or_else(|_| json!(id))
}

fn rename_body(name: &str) -> Value {
    json!({"Name": name})
}

async fn list_named(rt: &Runtime, service: &str, path: &str) -> Result<()> {
    let value = rt
        .http
        .request_json(service, Method::GET, path, &[], None)
        .await?;
    rt.show_columns(&value, NAMED_ROW_COLUMNS)
}

pub async fn projects(rt: &Runtime, action: &ProjectsAction) -> Result<()> {
    const SVC: &str = "etlprojects";
    let project = |id: &str| format!("/etl/etlProjects/{}", seg(id));
    let job = |id: &str| format!("/etl/etlJobs/jobs/{}", seg(id));
    match action {
        ProjectsAction::List => list_named(rt, SVC, "/etl/etlProjects").await,
        ProjectsAction::Get { id } => rt.get(SVC, &project(id), &[]).await,
        ProjectsAction::Create { body } => {
            rt.call(SVC, Method::POST, "/etl/etlProjects", body, None)
                .await
        }
        ProjectsAction::Update { id, description } => {
            rt.send(
                SVC,
                Method::PUT,
                &project(id),
                json!({"description": description}),
            )
            .await
        }
        ProjectsAction::Rename { id, name } => {
            rt.send(SVC, Method::PATCH, &project(id), rename_body(name))
                .await
        }
        ProjectsAction::SetOwner { id, owner } => {
            let path = format!("{}/owner", project(id));
            rt.send(SVC, Method::PUT, &path, json!({"owner": owner}))
                .await
        }
        ProjectsAction::Delete { id } => rt.send_empty(SVC, Method::DELETE, &project(id)).await,
        ProjectsAction::Jobs { project } => {
            list_named(rt, SVC, &format!("/etl/etlJobs/{}/jobs", seg(project))).await
        }
        ProjectsAction::Job { project, job } => {
            rt.get(SVC, &saved_job_path(project, job), &[]).await
        }
        ProjectsAction::CreateJob { project, body } => {
            let path = format!("/etl/etlJobs/{}", seg(project));
            rt.call(SVC, Method::POST, &path, body, None).await
        }
        ProjectsAction::UpdateJob {
            project,
            job,
            code,
            config,
        } => update_job(rt, project, job, code.as_deref(), config.as_deref()).await,
        ProjectsAction::RenameJob { job: id, name } => {
            rt.send(SVC, Method::PATCH, &job(id), rename_body(name))
                .await
        }
        ProjectsAction::DeleteJob { job: id } => rt.send_empty(SVC, Method::DELETE, &job(id)).await,
        ProjectsAction::JobRuns { job: id } => rt.get(SVC, &format!("{}/runs", job(id)), &[]).await,
        ProjectsAction::Published => list_named(rt, SVC, "/etl/etlJobs/publish").await,
        ProjectsAction::PublishedJob { name } => {
            rt.get(SVC, &format!("/etl/etlJobs/publish/{}", seg(name)), &[])
                .await
        }
        ProjectsAction::Publish { body } => {
            rt.call(SVC, Method::POST, "/etl/etlJobs/publish", body, None)
                .await
        }
        ProjectsAction::Unpublish { job, name } => {
            let path = format!("/etl/etlJobs/publish/{}/{}", seg(job), seg(name));
            rt.send_empty(SVC, Method::DELETE, &path).await
        }
        ProjectsAction::JobExists { project, name } => {
            let path = format!("/etl/etlJobs/exists/{}/{}", seg(project), seg(name));
            rt.get(SVC, &path, &[]).await
        }
    }
}

pub fn saved_job_path(project: &str, job: &str) -> String {
    format!("/etl/etlJobs/jobs/{}/{}", seg(project), seg(job))
}

/// The job update endpoint replaces config and code together, so read the
/// saved job first and only change what was asked for.
async fn update_job(
    rt: &Runtime,
    project: &str,
    job: &str,
    code: Option<&str>,
    config: Option<&str>,
) -> Result<()> {
    if code.is_none() && config.is_none() {
        anyhow::bail!("nothing to change: pass --code and/or --config");
    }
    let path = saved_job_path(project, job);
    let current = rt
        .http
        .request_json("etlprojects", Method::GET, &path, &[], None)
        .await?;
    let code = code.map(read_text_arg).transpose()?;
    let config_patch = read_data_arg(config)?;
    let body = job_update_body(&current, project, code, config_patch)?;
    rt.send("etlprojects", Method::PUT, &path, body).await
}

/// `{config, code, projectId}` as the web app's job editor saves it.
pub fn job_update_body(
    current: &Value,
    project: &str,
    code: Option<String>,
    config_patch: Option<Value>,
) -> Result<Value> {
    let mut config = current["config"].clone();
    if !config.is_object() {
        config = json!({});
    }
    if let Some(patch) = config_patch {
        let patch = patch
            .as_object()
            .context("--config must be a JSON object of fields to change")?;
        for (key, value) in patch {
            config[key] = value.clone();
        }
    }
    let code = code.unwrap_or_else(|| {
        current["fileContent"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    });
    Ok(json!({"config": config, "code": code, "projectId": id_value(project)}))
}

/// `(gateway service, path prefix)` of each designer resource.
fn designer_route(kind: DesignerKind) -> (&'static str, &'static str) {
    match kind {
        DesignerKind::Dataset => ("datasets", "/dataset"),
        DesignerKind::Dashboard => ("dashboards", "/dashboard"),
        DesignerKind::Visualization => ("visualizations", "/visualization"),
        DesignerKind::Filter => ("filters", "/filter"),
    }
}

pub async fn designer(rt: &Runtime, action: &DesignerAction) -> Result<()> {
    let one = |kind: DesignerKind, id: &str| {
        let (service, prefix) = designer_route(kind);
        (service, format!("{prefix}/{}", seg(id)))
    };
    match action {
        DesignerAction::Datasets => list_named(rt, "datasets", "/dataset").await,
        DesignerAction::Dataset { id } => {
            let (service, path) = one(DesignerKind::Dataset, id);
            rt.get(service, &path, &[]).await
        }
        DesignerAction::CreateDataset { body } => {
            rt.call("datasets", Method::POST, "/dataset", body, None)
                .await
        }
        DesignerAction::UpdateDataset { id, body } => {
            let (service, path) = one(DesignerKind::Dataset, id);
            rt.call(service, Method::PUT, &path, body, None).await
        }
        DesignerAction::Dashboards => list_named(rt, "dashboards", "/dashboard").await,
        DesignerAction::Dashboard { id } => {
            let (service, path) = one(DesignerKind::Dashboard, id);
            rt.get(service, &path, &[]).await
        }
        DesignerAction::CreateDashboard { body } => {
            rt.call("dashboards", Method::POST, "/dashboard", body, None)
                .await
        }
        DesignerAction::UpdateDashboard { id, body } => {
            let (service, path) = one(DesignerKind::Dashboard, id);
            rt.call(service, Method::PUT, &path, body, None).await
        }
        DesignerAction::Published => list_named(rt, "dashboards", "/dashboard/publish").await,
        DesignerAction::PublishedDashboard { name } => {
            rt.get(
                "dashboards",
                &format!("/dashboard/publish/{}", seg(name)),
                &[],
            )
            .await
        }
        DesignerAction::Publish { body } => {
            rt.call("dashboards", Method::POST, "/dashboard/publish", body, None)
                .await
        }
        DesignerAction::Unpublish { dashboard, name } => {
            let path = format!("/dashboard/publish/{}/{}", seg(dashboard), seg(name));
            rt.send_empty("dashboards", Method::DELETE, &path).await
        }
        DesignerAction::DashboardExists { name } => {
            rt.get(
                "dashboards",
                &format!("/dashboard/exists/{}", seg(name)),
                &[],
            )
            .await
        }
        DesignerAction::Visualizations => list_named(rt, "visualizations", "/visualization").await,
        DesignerAction::Visualization { id } => {
            let (service, path) = one(DesignerKind::Visualization, id);
            rt.get(service, &path, &[]).await
        }
        DesignerAction::CreateVisualization { body } => {
            rt.call("visualizations", Method::POST, "/visualization", body, None)
                .await
        }
        DesignerAction::UpdateVisualization { id, body } => {
            let (service, path) = one(DesignerKind::Visualization, id);
            rt.call(service, Method::PUT, &path, body, None).await
        }
        DesignerAction::Filters => list_named(rt, "filters", "/filter").await,
        DesignerAction::Filter { id } => {
            let (service, path) = one(DesignerKind::Filter, id);
            rt.get(service, &path, &[]).await
        }
        DesignerAction::CreateFilter { body } => {
            rt.call("filters", Method::POST, "/filter", body, None)
                .await
        }
        DesignerAction::UpdateFilter { id, body } => {
            let (service, path) = one(DesignerKind::Filter, id);
            rt.call(service, Method::PUT, &path, body, None).await
        }
        DesignerAction::Rename { kind, id, name } => {
            let (service, path) = one(*kind, id);
            rt.send(service, Method::PATCH, &path, rename_body(name))
                .await
        }
        DesignerAction::Delete { kind, id } => {
            let (service, path) = one(*kind, id);
            rt.send_empty(service, Method::DELETE, &path).await
        }
    }
}

pub async fn workspaces(rt: &Runtime, action: &WorkspacesAction) -> Result<()> {
    const SVC: &str = "workspaces";
    let workspace = |id: &str| format!("/workspace/{}", seg(id));
    match action {
        WorkspacesAction::List => list_named(rt, SVC, "/workspace").await,
        WorkspacesAction::Get { id } => rt.get(SVC, &workspace(id), &[]).await,
        WorkspacesAction::Contents { id } => {
            list_named(rt, SVC, &format!("{}/contents", workspace(id))).await
        }
        WorkspacesAction::Create { body } => {
            rt.call(SVC, Method::POST, "/workspace", body, None).await
        }
        WorkspacesAction::CreateIn { folder, body } => {
            rt.call(SVC, Method::POST, &workspace(folder), body, None)
                .await
        }
        WorkspacesAction::Rename { id, name } => {
            rt.send(SVC, Method::PATCH, &workspace(id), rename_body(name))
                .await
        }
        WorkspacesAction::Move { id, parent } => {
            rt.send(
                SVC,
                Method::PUT,
                &workspace(id),
                json!({"parentId": parent}),
            )
            .await
        }
        WorkspacesAction::SetOwner { id, owner } => {
            let path = format!("{}/owner", workspace(id));
            rt.send(SVC, Method::PUT, &path, json!({"owner": owner}))
                .await
        }
        WorkspacesAction::Delete { id } => rt.send_empty(SVC, Method::DELETE, &workspace(id)).await,
        WorkspacesAction::Remove { workspace: ws, id } => {
            let path = format!("{}/{}", workspace(ws), seg(id));
            rt.send_empty(SVC, Method::DELETE, &path).await
        }
        WorkspacesAction::Exists { parent, name } => {
            let path = format!("/workspace/exists/{}/{}", seg(parent), seg(name));
            rt.get(SVC, &path, &[]).await
        }
        WorkspacesAction::AddDashboard { body } => {
            rt.call(SVC, Method::POST, "/workspace/dashboard", body, None)
                .await
        }
        WorkspacesAction::Dashboard { parent, dashboard } => {
            let path = format!("/workspace/dashboard/{}/{}", seg(parent), seg(dashboard));
            rt.get(SVC, &path, &[]).await
        }
    }
}

pub async fn schedules(rt: &Runtime, action: &SchedulesAction) -> Result<()> {
    const SVC: &str = "schedules";
    let named = |verb: &str, id: &str, name: &str| {
        let verb = if verb.is_empty() {
            String::new()
        } else {
            format!("/{verb}")
        };
        format!("/schedule{verb}/{}/{}", seg(id), seg(name))
    };
    match action {
        SchedulesAction::List => rt.get(SVC, "/schedule", &[]).await,
        SchedulesAction::Get { id } => rt.get(SVC, &format!("/schedule/{}", seg(id)), &[]).await,
        SchedulesAction::Create { body } => {
            rt.call(SVC, Method::POST, "/schedule", body, None).await
        }
        SchedulesAction::Update { id, name, body } => {
            rt.call(SVC, Method::PUT, &named("", id, name), body, None)
                .await
        }
        SchedulesAction::Delete { id, name } => {
            rt.send_empty(SVC, Method::DELETE, &named("", id, name))
                .await
        }
        SchedulesAction::Start { id, name } => {
            rt.send_empty(SVC, Method::PUT, &named("start", id, name))
                .await
        }
        SchedulesAction::Stop { id, name } => {
            rt.send_empty(SVC, Method::PUT, &named("stop", id, name))
                .await
        }
        SchedulesAction::Run { id, name, body } => {
            rt.call(SVC, Method::PUT, &named("run", id, name), body, None)
                .await
        }
        SchedulesAction::Runs { id } => {
            rt.get(SVC, &format!("/schedule/{}/runs", seg(id)), &[])
                .await
        }
        SchedulesAction::Executions => rt.get(SVC, "/schedule/executions", &[]).await,
        SchedulesAction::Email { action } => schedule_email(rt, action).await,
    }
}

async fn schedule_email(rt: &Runtime, action: &ScheduleEmailAction) -> Result<()> {
    const SVC: &str = "schedules";
    let emails = |schedule: &str| format!("/schedule/{}/email", seg(schedule));
    match action {
        ScheduleEmailAction::Types => rt.get(SVC, "/schedule/notificationTypes", &[]).await,
        ScheduleEmailAction::Parameters => {
            rt.get(SVC, "/schedule/notificationParameters", &[]).await
        }
        ScheduleEmailAction::List { schedule } => rt.get(SVC, &emails(schedule), &[]).await,
        ScheduleEmailAction::Add { schedule, body } => {
            rt.call(SVC, Method::POST, &emails(schedule), body, None)
                .await
        }
        ScheduleEmailAction::Update {
            schedule,
            notification,
            body,
        } => {
            let path = format!("{}/{}", emails(schedule), seg(notification));
            rt.call(SVC, Method::PUT, &path, body, None).await
        }
        ScheduleEmailAction::Delete {
            schedule,
            notification,
        } => {
            let path = format!("{}/{}", emails(schedule), seg(notification));
            rt.send_empty(SVC, Method::DELETE, &path).await
        }
    }
}

pub async fn permissions(rt: &Runtime, action: &PermissionsAction) -> Result<()> {
    const SVC: &str = "permissions";
    match action {
        PermissionsAction::List => rt.get(SVC, "/permission", &[]).await,
        PermissionsAction::Resources { resource_type } => {
            rt.get(SVC, &format!("/permission/{}", seg(resource_type)), &[])
                .await
        }
        PermissionsAction::Grant { body } => {
            rt.call(SVC, Method::POST, "/permission", body, None).await
        }
        PermissionsAction::Effects => rt.get(SVC, "/permission/effects", &[]).await,
        PermissionsAction::AccessTypes { resource_type } => {
            let path = format!("/permission/{}/accessTypes", seg(resource_type));
            rt.get(SVC, &path, &[]).await
        }
        PermissionsAction::Policy {
            resource_type,
            effect,
            permission,
        } => {
            let path = format!(
                "/permission/policy/{}/{}/{}",
                seg(resource_type),
                seg(effect),
                seg(permission)
            );
            rt.get(SVC, &path, &[]).await
        }
        PermissionsAction::ResourcePolicy {
            resource_type,
            resource,
            effect,
            permission,
        } => {
            let path = format!(
                "/permission/resourcePolicy/{}/{}/{}/{}",
                seg(resource_type),
                seg(resource),
                seg(effect),
                seg(permission)
            );
            rt.get(SVC, &path, &[]).await
        }
        PermissionsAction::UserCache {
            username,
            resource_type,
        } => {
            let path = format!("/permission/cache/{}/{}", seg(username), seg(resource_type));
            rt.get(SVC, &path, &[]).await
        }
        PermissionsAction::RemoveAccess { body } => {
            rt.call(SVC, Method::POST, "/permission/removeAccess", body, None)
                .await
        }
        PermissionsAction::ApiKey { action } => api_keys(rt, action).await,
    }
}

async fn api_keys(rt: &Runtime, action: &ApiKeyAction) -> Result<()> {
    const SVC: &str = "permissions";
    let key = |id: &str| format!("/permission/apiKey/{}", seg(id));
    match action {
        ApiKeyAction::List { username } => rt.get(SVC, &key(username), &[]).await,
        ApiKeyAction::Create { username, use_case } => {
            let body = json!({"userName": username, "useCase": use_case});
            rt.send(SVC, Method::POST, "/permission/apiKey", body).await
        }
        ApiKeyAction::Enable { id } => {
            rt.send(SVC, Method::PUT, &key(id), json!({"isActive": true}))
                .await
        }
        ApiKeyAction::Disable { id } => {
            rt.send(SVC, Method::PUT, &key(id), json!({"isActive": false}))
                .await
        }
    }
}

pub async fn connections(rt: &Runtime, action: &ConnectionsAction) -> Result<()> {
    const SVC: &str = "connections";
    let connection = |id: &str| format!("/connection/{}", seg(id));
    match action {
        ConnectionsAction::List => list_named(rt, SVC, "/connection").await,
        ConnectionsAction::Get { id } => rt.get(SVC, &connection(id), &[]).await,
        ConnectionsAction::Create { body } => {
            rt.call(SVC, Method::POST, "/connection", body, None).await
        }
        ConnectionsAction::Update { id, body } => {
            rt.call(SVC, Method::PUT, &connection(id), body, None).await
        }
        ConnectionsAction::Rename { id, name } => {
            rt.send(SVC, Method::PATCH, &connection(id), rename_body(name))
                .await
        }
        ConnectionsAction::Delete { id } => {
            rt.send_empty(SVC, Method::DELETE, &connection(id)).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_update_keeps_unchanged_parts() {
        let current = json!({
            "config": {"connectionNames": ["GZS3"], "minimumWorkers": 1},
            "fileContent": "print(1)",
        });
        let body =
            job_update_body(&current, "1000", None, Some(json!({"minimumWorkers": 3}))).unwrap();
        assert_eq!(body["config"]["connectionNames"], json!(["GZS3"]));
        assert_eq!(body["config"]["minimumWorkers"], 3);
        assert_eq!(body["code"], "print(1)");
        assert_eq!(body["projectId"], 1000);

        let body = job_update_body(&current, "1000", Some("print(2)".into()), None).unwrap();
        assert_eq!(body["code"], "print(2)");
        assert_eq!(body["config"]["minimumWorkers"], 1);
    }

    #[test]
    fn job_update_rejects_non_object_config() {
        assert!(job_update_body(&json!({}), "1", None, Some(json!([1]))).is_err());
    }

    #[test]
    fn designer_kinds_map_to_their_gateways() {
        assert_eq!(
            designer_route(DesignerKind::Dataset),
            ("datasets", "/dataset")
        );
        assert_eq!(designer_route(DesignerKind::Filter), ("filters", "/filter"));
    }

    #[test]
    fn numeric_ids_become_json_numbers() {
        assert_eq!(id_value("42"), json!(42));
        assert_eq!(id_value("abc"), json!("abc"));
    }
}
