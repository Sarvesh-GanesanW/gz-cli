use anyhow::Result;
use reqwest::Method;
use serde_json::json;

use crate::cli::{ModelsAction, ModelsAdminAction, NotebooksAction};
use crate::cmd::{object_body, Runtime};

const SVC: &str = "mlops";
const ADMIN_SVC: &str = "mlopsadmin";

pub async fn notebooks(rt: &Runtime, action: &NotebooksAction) -> Result<()> {
    match action {
        NotebooksAction::List { project_id } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::GET,
                    &format!("/jupyter/{project_id}/notebook"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        NotebooksAction::Create {
            project_id,
            name,
            template,
        } => {
            let mut pairs = vec![
                ("projectId", json!(project_id)),
                ("notebookName", json!(name)),
            ];
            if let Some(template) = template {
                pairs.push(("publishedNotebookPath", json!(template)));
            }
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::POST,
                    "/jupyter/notebook",
                    &[],
                    Some(object_body(pairs)),
                )
                .await?;
            rt.show(value).await
        }
        NotebooksAction::Update {
            project_id,
            notebook_id,
            path,
        } => {
            let body = object_body(vec![("publishedNotebookPath", json!(path))]);
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::PUT,
                    &format!("/jupyter/notebook/{project_id}/{notebook_id}"),
                    &[],
                    Some(body),
                )
                .await?;
            rt.show(value).await
        }
        NotebooksAction::Patch { notebook_id, body } => {
            rt.call(
                SVC,
                Method::PATCH,
                &format!("/jupyter/notebook/{notebook_id}"),
                body,
                None,
            )
            .await
        }
        NotebooksAction::Delete {
            project_id,
            notebook_id,
        } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::DELETE,
                    &format!("/jupyter/notebook/{project_id}/{notebook_id}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        NotebooksAction::Exists { project_id, name } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::GET,
                    &format!("/jupyter/notebook/exists/{project_id}/{name}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        NotebooksAction::Start { body } => {
            rt.call(SVC, Method::POST, "/jupyter/start", body, None)
                .await
        }
        NotebooksAction::Stop { body } => {
            rt.call(SVC, Method::POST, "/jupyter/stop", body, None)
                .await
        }
        NotebooksAction::Running { body } => {
            rt.call(
                SVC,
                Method::POST,
                "/jupyter/getRunningNotebooks",
                body,
                None,
            )
            .await
        }
        NotebooksAction::Publish { body } => {
            rt.call(SVC, Method::POST, "/jupyter/publish", body, None)
                .await
        }
        NotebooksAction::Published => {
            let value = rt
                .http
                .request_json(SVC, Method::GET, "/jupyter/publish", &[], None)
                .await?;
            rt.show(value).await
        }
        NotebooksAction::GetPublished { name } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::GET,
                    &format!("/jupyter/publish/{name}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        NotebooksAction::Unpublish { notebook_id, name } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::DELETE,
                    &format!("/jupyter/publish/{notebook_id}/{name}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
    }
}

pub async fn models(rt: &Runtime, action: &ModelsAction) -> Result<()> {
    match action {
        ModelsAction::Start { body } => {
            rt.call(SVC, Method::POST, "/mlflow/start", body, None)
                .await
        }
        ModelsAction::Stop { body } => rt.call(SVC, Method::POST, "/mlflow/stop", body, None).await,
        ModelsAction::List { body } => {
            rt.call(SVC, Method::POST, "/mlflow/listmodels", body, None)
                .await
        }
        ModelsAction::ListAll => {
            let value = rt
                .http
                .request_json(SVC, Method::GET, "/mlflow/listAllModels", &[], None)
                .await?;
            rt.show(value).await
        }
        ModelsAction::Experiments => {
            let value = rt
                .http
                .request_json(SVC, Method::GET, "/mlflow/listAllExperiments", &[], None)
                .await?;
            rt.show(value).await
        }
        ModelsAction::Publish { body } => {
            rt.call(SVC, Method::POST, "/mlflow/publish", body, None)
                .await
        }
        ModelsAction::Published => {
            let value = rt
                .http
                .request_json(SVC, Method::GET, "/mlflow/publish", &[], None)
                .await?;
            rt.show(value).await
        }
        ModelsAction::Import { body } => {
            rt.call(SVC, Method::POST, "/mlflow/importModel", body, None)
                .await
        }
        ModelsAction::Unpublish { id, name } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::DELETE,
                    &format!("/mlflow/publish/{id}/{name}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        ModelsAction::Running => {
            let value = rt
                .http
                .request_json(SVC, Method::GET, "/mlflow/getRunningMlflowPods", &[], None)
                .await?;
            rt.show(value).await
        }
        ModelsAction::Admin { action } => admin(rt, action).await,
    }
}

async fn admin(rt: &Runtime, action: &ModelsAdminAction) -> Result<()> {
    match action {
        ModelsAdminAction::CreateUser { body } => {
            rt.call(ADMIN_SVC, Method::POST, "/create_user", body, None)
                .await
        }
        ModelsAdminAction::DeleteUser { body } => {
            rt.call(ADMIN_SVC, Method::POST, "/delete_user", body, None)
                .await
        }
        ModelsAdminAction::SetAdmin { body } => {
            rt.call(ADMIN_SVC, Method::POST, "/set_admin", body, None)
                .await
        }
        ModelsAdminAction::UpdatePass { body } => {
            rt.call(ADMIN_SVC, Method::POST, "/update_pass", body, None)
                .await
        }
        ModelsAdminAction::ExperimentPerm { body } => {
            rt.call(ADMIN_SVC, Method::POST, "/set_experiment_perm", body, None)
                .await
        }
        ModelsAdminAction::ModelPerm { body } => {
            rt.call(ADMIN_SVC, Method::POST, "/set_model_perm", body, None)
                .await
        }
        ModelsAdminAction::Experiments { body } => {
            rt.call(ADMIN_SVC, Method::POST, "/list_experiments", body, None)
                .await
        }
        ModelsAdminAction::Runs { body } => {
            rt.call(ADMIN_SVC, Method::POST, "/list_exp_runs", body, None)
                .await
        }
        ModelsAdminAction::List { body } => {
            rt.call(ADMIN_SVC, Method::POST, "/list_models", body, None)
                .await
        }
        ModelsAdminAction::Deploy { body } => {
            rt.call(ADMIN_SVC, Method::POST, "/deploy_mlops", body, None)
                .await
        }
    }
}
