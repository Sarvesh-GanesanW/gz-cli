use anyhow::Result;
use reqwest::Method;

use crate::cli::{
    ConnectionsAction, DesignerAction, PermissionsAction, ProjectsAction, SchedulesAction,
    WorkspacesAction,
};
use crate::cmd::Runtime;

pub async fn projects(rt: &Runtime, action: &ProjectsAction) -> Result<()> {
    const SVC: &str = "etlprojects";
    match action {
        ProjectsAction::List => {
            let value = rt
                .http
                .request_json(SVC, Method::GET, "/etl/etlProjects", &[], None)
                .await?;
            rt.show(value).await
        }
        ProjectsAction::Get { id } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::GET,
                    &format!("/etl/etlProjects/{id}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        ProjectsAction::Create { body } => {
            rt.call(SVC, Method::POST, "/etl/etlProjects", body, None)
                .await
        }
        ProjectsAction::Update { id, body } => {
            rt.call(
                SVC,
                Method::PATCH,
                &format!("/etl/etlProjects/{id}"),
                body,
                None,
            )
            .await
        }
        ProjectsAction::Delete { id } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::DELETE,
                    &format!("/etl/etlProjects/{id}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        ProjectsAction::Jobs { project } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::GET,
                    &format!("/etl/etlJobs/{project}/jobs"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        ProjectsAction::Job { project, job } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::GET,
                    &format!("/etl/etlJobs/jobs/{project}/{job}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        ProjectsAction::CreateJob { project, body } => {
            rt.call(
                SVC,
                Method::POST,
                &format!("/etl/etlJobs/{project}"),
                body,
                None,
            )
            .await
        }
        ProjectsAction::UpdateJob { job, body } => {
            rt.call(
                SVC,
                Method::PATCH,
                &format!("/etl/etlJobs/jobs/{job}"),
                body,
                None,
            )
            .await
        }
        ProjectsAction::DeleteJob { job } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::DELETE,
                    &format!("/etl/etlJobs/jobs/{job}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        ProjectsAction::Published => {
            let value = rt
                .http
                .request_json(SVC, Method::GET, "/etl/etlJobs/publish", &[], None)
                .await?;
            rt.show(value).await
        }
        ProjectsAction::Publish { body } => {
            rt.call(SVC, Method::POST, "/etl/etlJobs/publish", body, None)
                .await
        }
        ProjectsAction::JobExists { project, name } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::GET,
                    &format!("/etl/etlJobs/exists/{project}/{name}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
    }
}

pub async fn designer(rt: &Runtime, action: &DesignerAction) -> Result<()> {
    match action {
        DesignerAction::Datasets => {
            let value = rt
                .http
                .request_json("datasets", Method::GET, "/dataset", &[], None)
                .await?;
            rt.show(value).await
        }
        DesignerAction::Dataset { id } => {
            let value = rt
                .http
                .request_json(
                    "datasets",
                    Method::GET,
                    &format!("/dataset/{id}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        DesignerAction::CreateDataset { body } => {
            rt.call("datasets", Method::POST, "/dataset", body, None)
                .await
        }
        DesignerAction::UpdateDataset { id, body } => {
            rt.call(
                "datasets",
                Method::PUT,
                &format!("/dataset/{id}"),
                body,
                None,
            )
            .await
        }
        DesignerAction::Dashboards => {
            let value = rt
                .http
                .request_json("dashboards", Method::GET, "/dashboard", &[], None)
                .await?;
            rt.show(value).await
        }
        DesignerAction::Dashboard { id } => {
            let value = rt
                .http
                .request_json(
                    "dashboards",
                    Method::GET,
                    &format!("/dashboard/{id}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        DesignerAction::CreateDashboard { body } => {
            rt.call("dashboards", Method::POST, "/dashboard", body, None)
                .await
        }
        DesignerAction::UpdateDashboard { id, body } => {
            rt.call(
                "dashboards",
                Method::PUT,
                &format!("/dashboard/{id}"),
                body,
                None,
            )
            .await
        }
        DesignerAction::DeleteDashboard { id } => {
            let value = rt
                .http
                .request_json(
                    "dashboards",
                    Method::DELETE,
                    &format!("/dashboard/{id}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        DesignerAction::Published => {
            let value = rt
                .http
                .request_json("dashboards", Method::GET, "/dashboard/publish", &[], None)
                .await?;
            rt.show(value).await
        }
        DesignerAction::Publish { body } => {
            rt.call("dashboards", Method::POST, "/dashboard/publish", body, None)
                .await
        }
        DesignerAction::Unpublish { dashboard, name } => {
            let value = rt
                .http
                .request_json(
                    "dashboards",
                    Method::DELETE,
                    &format!("/dashboard/publish/{dashboard}/{name}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        DesignerAction::Visualizations => {
            let value = rt
                .http
                .request_json("visualizations", Method::GET, "/visualization", &[], None)
                .await?;
            rt.show(value).await
        }
        DesignerAction::Visualization { id } => {
            let value = rt
                .http
                .request_json(
                    "visualizations",
                    Method::GET,
                    &format!("/visualization/{id}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        DesignerAction::CreateVisualization { body } => {
            rt.call("visualizations", Method::POST, "/visualization", body, None)
                .await
        }
        DesignerAction::UpdateVisualization { id, body } => {
            rt.call(
                "visualizations",
                Method::PUT,
                &format!("/visualization/{id}"),
                body,
                None,
            )
            .await
        }
        DesignerAction::DeleteVisualization { id } => {
            let value = rt
                .http
                .request_json(
                    "visualizations",
                    Method::DELETE,
                    &format!("/visualization/{id}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        DesignerAction::Filters => {
            let value = rt
                .http
                .request_json("filters", Method::GET, "/filter", &[], None)
                .await?;
            rt.show(value).await
        }
        DesignerAction::Filter { id } => {
            let value = rt
                .http
                .request_json("filters", Method::GET, &format!("/filter/{id}"), &[], None)
                .await?;
            rt.show(value).await
        }
        DesignerAction::CreateFilter { body } => {
            rt.call("filters", Method::POST, "/filter", body, None)
                .await
        }
        DesignerAction::UpdateFilter { id, body } => {
            rt.call("filters", Method::PUT, &format!("/filter/{id}"), body, None)
                .await
        }
        DesignerAction::DeleteFilter { id } => {
            let value = rt
                .http
                .request_json(
                    "filters",
                    Method::DELETE,
                    &format!("/filter/{id}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
    }
}

pub async fn workspaces(rt: &Runtime, action: &WorkspacesAction) -> Result<()> {
    const SVC: &str = "workspaces";
    match action {
        WorkspacesAction::List => {
            let value = rt
                .http
                .request_json(SVC, Method::GET, "/workspace", &[], None)
                .await?;
            rt.show(value).await
        }
        WorkspacesAction::Contents { id } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::GET,
                    &format!("/workspace/{id}/contents"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        WorkspacesAction::Create { body } => {
            rt.call(SVC, Method::POST, "/workspace", body, None).await
        }
        WorkspacesAction::CreateIn { folder, body } => {
            rt.call(
                SVC,
                Method::POST,
                &format!("/workspace/{folder}"),
                body,
                None,
            )
            .await
        }
        WorkspacesAction::Update { id, body } => {
            rt.call(SVC, Method::PATCH, &format!("/workspace/{id}"), body, None)
                .await
        }
        WorkspacesAction::Delete { id } => {
            let value = rt
                .http
                .request_json(SVC, Method::DELETE, &format!("/workspace/{id}"), &[], None)
                .await?;
            rt.show(value).await
        }
        WorkspacesAction::Remove { workspace, id } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::DELETE,
                    &format!("/workspace/{workspace}/{id}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        WorkspacesAction::Exists { parent, name } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::GET,
                    &format!("/workspace/exists/{parent}/{name}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        WorkspacesAction::AddDashboard { body } => {
            rt.call(SVC, Method::POST, "/workspace/dashboard", body, None)
                .await
        }
        WorkspacesAction::Dashboard { parent, dashboard } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::GET,
                    &format!("/workspace/dashboard/{parent}/{dashboard}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
    }
}

pub async fn schedules(rt: &Runtime, action: &SchedulesAction) -> Result<()> {
    const SVC: &str = "schedules";
    match action {
        SchedulesAction::List => {
            let value = rt
                .http
                .request_json(SVC, Method::GET, "/schedule", &[], None)
                .await?;
            rt.show(value).await
        }
        SchedulesAction::Get { id } => {
            let value = rt
                .http
                .request_json(SVC, Method::GET, &format!("/schedule/{id}"), &[], None)
                .await?;
            rt.show(value).await
        }
        SchedulesAction::Create { body } => {
            rt.call(SVC, Method::POST, "/schedule", body, None).await
        }
        SchedulesAction::Update { id, name, body } => {
            rt.call(
                SVC,
                Method::PUT,
                &format!("/schedule/{id}/{name}"),
                body,
                None,
            )
            .await
        }
        SchedulesAction::Delete { id, name } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::DELETE,
                    &format!("/schedule/{id}/{name}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        SchedulesAction::Start { id, name } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::PUT,
                    &format!("/schedule/start/{id}/{name}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        SchedulesAction::Stop { id, name } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::PUT,
                    &format!("/schedule/stop/{id}/{name}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        SchedulesAction::Run { id, name } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::PUT,
                    &format!("/schedule/run/{id}/{name}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        SchedulesAction::Runs { id } => {
            let value = rt
                .http
                .request_json(SVC, Method::GET, &format!("/schedule/{id}/runs"), &[], None)
                .await?;
            rt.show(value).await
        }
        SchedulesAction::Executions => {
            let value = rt
                .http
                .request_json(SVC, Method::GET, "/schedule/executions", &[], None)
                .await?;
            rt.show(value).await
        }
    }
}

pub async fn permissions(rt: &Runtime, action: &PermissionsAction) -> Result<()> {
    const SVC: &str = "permissions";
    match action {
        PermissionsAction::List { body } => {
            rt.call(SVC, Method::GET, "/permission", body, None).await
        }
        PermissionsAction::Grant { body } => {
            rt.call(SVC, Method::POST, "/permission", body, None).await
        }
        PermissionsAction::Users => {
            let value = rt
                .http
                .request_json(SVC, Method::GET, "/permission/users", &[], None)
                .await?;
            rt.show(value).await
        }
        PermissionsAction::Groups => {
            let value = rt
                .http
                .request_json(SVC, Method::GET, "/permission/groups", &[], None)
                .await?;
            rt.show(value).await
        }
        PermissionsAction::Effects => {
            let value = rt
                .http
                .request_json(SVC, Method::GET, "/permission/effects", &[], None)
                .await?;
            rt.show(value).await
        }
        PermissionsAction::AccessTypes { resource } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::GET,
                    &format!("/permission/{resource}/accessTypes"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        PermissionsAction::Access { resource, access } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::GET,
                    &format!("/permission/{resource}/{access}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        PermissionsAction::Policy {
            resource_type,
            effect,
            permission,
        } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::GET,
                    &format!("/permission/policy/{resource_type}/{effect}/{permission}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        PermissionsAction::ResourcePolicy {
            resource_type,
            resource,
            effect,
            permission,
        } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::GET,
                    &format!(
                        "/permission/resourcePolicy/{resource_type}/{resource}/{effect}/{permission}"
                    ),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        PermissionsAction::RemoveAccess { body } => {
            rt.call(SVC, Method::POST, "/permission/removeAccess", body, None)
                .await
        }
    }
}

pub async fn connections(rt: &Runtime, action: &ConnectionsAction) -> Result<()> {
    const SVC: &str = "connections";
    match action {
        ConnectionsAction::List => {
            let value = rt
                .http
                .request_json(SVC, Method::GET, "/connection", &[], None)
                .await?;
            rt.show(value).await
        }
        ConnectionsAction::Get { id } => {
            let value = rt
                .http
                .request_json(SVC, Method::GET, &format!("/connection/{id}"), &[], None)
                .await?;
            rt.show(value).await
        }
        ConnectionsAction::Create { body } => {
            rt.call(SVC, Method::POST, "/connection", body, None).await
        }
        ConnectionsAction::Update { id, body } => {
            rt.call(SVC, Method::PUT, &format!("/connection/{id}"), body, None)
                .await
        }
        ConnectionsAction::Delete { id } => {
            let value = rt
                .http
                .request_json(SVC, Method::DELETE, &format!("/connection/{id}"), &[], None)
                .await?;
            rt.show(value).await
        }
    }
}
