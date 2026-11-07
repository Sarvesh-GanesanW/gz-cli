//! Site administration: Cognito users and groups (AdminQueries gateway),
//! chat budgets (chat service), sites, billing (admin gateway), and sender
//! emails (notifications gateway) — the same calls the web app's admin pages
//! make (`user/User Console/Users & Roles/userActions.js`, `home/utils.js`,
//! `Utils/adminUtils.js`, `Utils/billingUtils.js`, `Utils/notificationUtils.js`).

use anyhow::{Context, Result};
use reqwest::Method;
use serde_json::{json, Map, Value};

use crate::cli::{
    AdminAction, AdminBudgetsAction, AdminEmailsAction, AdminGroupsAction, AdminUsersAction,
    BudgetSubject, Confirm,
};
use crate::cmd::{seg, Runtime};
use crate::http::ADMIN_QUERIES;
use crate::output::{self, OutputFormat};

const CHAT: &str = "chat";
const ADMIN: &str = "admin";
const NOTIFICATIONS: &str = "notifications";

pub async fn run(rt: &Runtime, action: &AdminAction) -> Result<()> {
    match action {
        AdminAction::Users { action } => users(rt, action).await,
        AdminAction::Groups { action } => groups(rt, action).await,
        AdminAction::Budgets { action } => budgets(rt, action).await,
        AdminAction::Sites => rt.get(ADMIN, "/admin/site", &[]).await,
        AdminAction::Emails { action } => emails(rt, action).await,
        AdminAction::Client => rt.get(ADMIN, "/admin/client", &[]).await,
        AdminAction::Billing { year, month, site } => {
            let mut body = json!({"year": year, "month": month});
            if let Some(site) = site {
                body["siteName"] = json!(site);
            }
            rt.send(ADMIN, Method::POST, "/admin/client/bill", body)
                .await
        }
    }
}

/// Asks before changing someone else's access, unless `--yes` was given.
fn confirm(confirm: Confirm, question: &str) -> Result<()> {
    crate::cmd::confirm(confirm.yes, question)
}

async fn require_admin_session(rt: &Runtime) -> Result<()> {
    if rt.http.has_access_token().await {
        return Ok(());
    }
    anyhow::bail!(
        "user admin needs a fresh sign-in (it uses a different token); run `gz auth login`"
    )
}

fn page_query(limit: Option<usize>, token: Option<&str>) -> Vec<(String, String)> {
    let mut query = Vec::new();
    if let Some(limit) = limit {
        query.push(("limit".to_string(), limit.to_string()));
    }
    if let Some(token) = token {
        query.push(("token".to_string(), token.to_string()));
    }
    query
}

async fn users(rt: &Runtime, action: &AdminUsersAction) -> Result<()> {
    require_admin_session(rt).await?;
    let post = |path: &'static str, body: Value| rt.send(ADMIN_QUERIES, Method::POST, path, body);
    match action {
        AdminUsersAction::List { next } => {
            let value = rt
                .http
                .request_json(
                    ADMIN_QUERIES,
                    Method::GET,
                    "/listUsers",
                    &page_query(rt.limit, next.as_deref()),
                    None,
                )
                .await?;
            show_page(rt, &value, "Users", user_rows)
        }
        AdminUsersAction::Get { username } => {
            let query = [("username".to_string(), username.clone())];
            let value = rt
                .http
                .request_json(ADMIN_QUERIES, Method::GET, "/getUser", &query, None)
                .await?;
            if rt.format != OutputFormat::Table {
                return rt.show(value).await;
            }
            rt.show(user_row(&value)).await
        }
        AdminUsersAction::Create {
            username,
            email,
            site,
        } => {
            let mut body = json!({"username": username, "email": email});
            if let Some(site) = site.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                body["site"] = json!(site);
            }
            post("/registerUserAsAdmin", body).await
        }
        AdminUsersAction::Delete {
            username,
            confirm: c,
        } => {
            confirm(*c, &format!("Delete user {username}?"))?;
            post("/deleteUserAsAdmin", json!({"username": username})).await
        }
        AdminUsersAction::Disable {
            username,
            confirm: c,
        } => {
            confirm(*c, &format!("Block {username} from signing in?"))?;
            post("/disableUser", json!({"username": username})).await
        }
        AdminUsersAction::Enable { username } => {
            post("/enableUser", json!({"username": username})).await
        }
        AdminUsersAction::Groups { username } => {
            let query = [("username".to_string(), username.clone())];
            let value = rt
                .http
                .request_json(
                    ADMIN_QUERIES,
                    Method::GET,
                    "/listGroupsForUser",
                    &query,
                    None,
                )
                .await?;
            show_page(rt, &value, "Groups", group_rows)
        }
        AdminUsersAction::AddGroup { username, group } => {
            post(
                "/addUserToGroup",
                json!({"username": username, "groupname": group}),
            )
            .await
        }
        AdminUsersAction::RemoveGroup {
            username,
            group,
            confirm: c,
        } => {
            confirm(*c, &format!("Remove {username} from {group}?"))?;
            post(
                "/removeUserFromGroup",
                json!({"username": username, "groupname": group}),
            )
            .await
        }
    }
}

async fn groups(rt: &Runtime, action: &AdminGroupsAction) -> Result<()> {
    require_admin_session(rt).await?;
    match action {
        AdminGroupsAction::List { next } => {
            let value = rt
                .http
                .request_json(
                    ADMIN_QUERIES,
                    Method::GET,
                    "/listGroups",
                    &page_query(rt.limit, next.as_deref()),
                    None,
                )
                .await?;
            show_page(rt, &value, "Groups", group_rows)
        }
        AdminGroupsAction::Members { group, next } => {
            let mut query = vec![("groupname".to_string(), group.clone())];
            query.extend(page_query(None, next.as_deref()));
            let value = rt
                .http
                .request_json(
                    ADMIN_QUERIES,
                    Method::GET,
                    "/listUsersInGroup",
                    &query,
                    None,
                )
                .await?;
            show_page(rt, &value, "Users", user_rows)
        }
        AdminGroupsAction::Create {
            name,
            description,
            precedence,
        } => {
            let body =
                json!({"groupName": name, "description": description, "precedence": precedence});
            rt.send(ADMIN_QUERIES, Method::POST, "/createGroup", body)
                .await
        }
        AdminGroupsAction::Delete { name, confirm: c } => {
            confirm(*c, &format!("Delete group {name}?"))?;
            let path = format!("/deleteGroup/{}", seg(name));
            rt.send(ADMIN_QUERIES, Method::POST, &path, json!({})).await
        }
        AdminGroupsAction::System => rt.get(ADMIN_QUERIES, "/listSystemGroups", &[]).await,
        AdminGroupsAction::Workspace => rt.get(ADMIN_QUERIES, "/listWorkspaceGroups", &[]).await,
    }
}

/// Prints one page of a Cognito list and how to fetch the next one.
fn show_page(rt: &Runtime, value: &Value, key: &str, rows: fn(&Value) -> Value) -> Result<()> {
    if rt.format != OutputFormat::Table {
        return output::print_value(value, rt.format, rt.limit);
    }
    output::print_value(&rows(&value[key]), rt.format, rt.limit)?;
    if let Some(next) = value["NextToken"].as_str() {
        output::info(&format!("more results: add --next {next}"));
    }
    Ok(())
}

/// Cognito users (`Attributes` or `UserAttributes` as name/value pairs)
/// flattened into one readable row each.
fn user_rows(users: &Value) -> Value {
    Value::Array(
        users
            .as_array()
            .into_iter()
            .flatten()
            .map(user_row)
            .collect(),
    )
}

fn user_row(user: &Value) -> Value {
    let attributes = user["Attributes"]
        .as_array()
        .or(user["UserAttributes"].as_array())
        .into_iter()
        .flatten();
    let attribute = |name: &str| {
        attributes
            .clone()
            .find(|a| a["Name"] == name)
            .and_then(|a| a["Value"].as_str())
            .map(str::to_string)
    };
    let mut row = Map::new();
    row.insert("username".into(), user["Username"].clone());
    row.insert("email".into(), json!(attribute("email")));
    row.insert("status".into(), user["UserStatus"].clone());
    row.insert("enabled".into(), user["Enabled"].clone());
    row.insert("site".into(), json!(attribute("custom:site")));
    row.insert("createdAt".into(), user["UserCreateDate"].clone());
    Value::Object(row)
}

fn group_rows(groups: &Value) -> Value {
    let rows = groups.as_array().into_iter().flatten().map(|group| {
        json!({
            "name": group["GroupName"],
            "description": group["Description"],
            "precedence": group["Precedence"],
            "createdAt": group["CreationDate"],
        })
    });
    Value::Array(rows.collect())
}

/// `/admin/chat-budgets/{site}/{user|group}` on the chat service.
fn budget_path(rt: &Runtime, site: Option<&str>, subject: BudgetSubject) -> Result<String> {
    let site = match site {
        Some(site) => site.to_string(),
        None => rt
            .http
            .resolved()
            .site()
            .context("no site: pass --site or set one with `gz configure --site`")?,
    };
    Ok(format!(
        "/admin/chat-budgets/{}/{}",
        seg(&site),
        subject.as_str()
    ))
}

async fn budgets(rt: &Runtime, action: &AdminBudgetsAction) -> Result<()> {
    let subject_query = |name: &str| [("subjectName".to_string(), name.to_string())];
    match action {
        AdminBudgetsAction::List { subject, site } => {
            let path = format!(
                "{}/assignments",
                budget_path(rt, site.as_deref(), *subject)?
            );
            rt.get(CHAT, &path, &[]).await
        }
        AdminBudgetsAction::Get {
            name,
            subject,
            site,
        } => {
            let path = budget_path(rt, site.as_deref(), *subject)?;
            rt.get(CHAT, &path, &subject_query(name)).await
        }
        AdminBudgetsAction::Set {
            name,
            limit_cents,
            subject,
            site,
        } => {
            let path = budget_path(rt, site.as_deref(), *subject)?;
            let body = json!({"subjectName": name, "limitCents": limit_cents});
            rt.send(CHAT, Method::PUT, &path, body).await
        }
        AdminBudgetsAction::Remove {
            name,
            subject,
            site,
            confirm: c,
        } => {
            confirm(*c, &format!("Remove the chat budget for {name}?"))?;
            let path = budget_path(rt, site.as_deref(), *subject)?;
            rt.http
                .request_json(CHAT, Method::DELETE, &path, &subject_query(name), None)
                .await?;
            output::success(&format!("removed the chat budget for {name}"));
            Ok(())
        }
    }
}

async fn emails(rt: &Runtime, action: &AdminEmailsAction) -> Result<()> {
    match action {
        AdminEmailsAction::List => {
            rt.get(NOTIFICATIONS, "/notification/identity/list", &[])
                .await
        }
        AdminEmailsAction::Add { email } => {
            rt.send(
                NOTIFICATIONS,
                Method::POST,
                "/notification/identity/add",
                json!({"email": email}),
            )
            .await
        }
        AdminEmailsAction::Remove { email, confirm: c } => {
            confirm(*c, &format!("Remove sender email {email}?"))?;
            rt.send(
                NOTIFICATIONS,
                Method::POST,
                "/notification/identity/remove",
                json!({"email": email}),
            )
            .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flattens_cognito_users() {
        let users = json!([{
            "Username": "ana",
            "Enabled": true,
            "UserStatus": "CONFIRMED",
            "Attributes": [{"Name": "email", "Value": "ana@x.test"}, {"Name": "custom:site", "Value": "preview"}]
        }]);
        let rows = user_rows(&users);
        assert_eq!(rows[0]["username"], "ana");
        assert_eq!(rows[0]["email"], "ana@x.test");
        assert_eq!(rows[0]["site"], "preview");
        assert_eq!(rows[0]["status"], "CONFIRMED");
    }

    #[test]
    fn reads_get_user_attributes_too() {
        let user =
            json!({"Username": "bo", "UserAttributes": [{"Name": "email", "Value": "bo@x.test"}]});
        assert_eq!(user_row(&user)["email"], "bo@x.test");
    }

    #[test]
    fn maps_cognito_groups() {
        let rows = group_rows(&json!([{"GroupName": "admin", "Precedence": 1}]));
        assert_eq!(rows[0]["name"], "admin");
        assert_eq!(rows[0]["precedence"], 1);
    }

    #[test]
    fn page_query_only_sends_given_flags() {
        assert!(page_query(None, None).is_empty());
        assert_eq!(
            page_query(Some(10), Some("t")),
            vec![
                ("limit".to_string(), "10".to_string()),
                ("token".to_string(), "t".to_string())
            ]
        );
    }

    #[test]
    fn refuses_without_yes_when_not_interactive() {
        if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            return;
        }
        assert!(confirm(Confirm { yes: false }, "Delete?").is_err());
        assert!(confirm(Confirm { yes: true }, "Delete?").is_ok());
    }
}
