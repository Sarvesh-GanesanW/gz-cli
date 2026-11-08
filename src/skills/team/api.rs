//! The chat server's team skill routes:
//! `GET /chat/team-skills` → `{items: [IndexEntry]}`,
//! `GET /chat/team-skills/{name}/file?path=` → `{path, content, sha256}`,
//! and, for site admins, `PUT|DELETE /admin/chat-team-skills/{name}`.

use std::future::Future;

use anyhow::{bail, Context, Result};
use reqwest::Method;
use serde_json::{json, Value};

use crate::cmd::seg;
use crate::http::ApiClient;
use crate::skills::team::{limits_from, IndexEntry, ServedFile, TeamLimits};
use crate::tui::errors::Failure;

const CHAT: &str = "chat";

/// Where a sync reads team skills from (the chat server, or a test fake).
pub trait TeamSource {
    fn index(&self) -> impl Future<Output = Result<Vec<IndexEntry>>> + Send;
    fn file(&self, name: &str, path: &str) -> impl Future<Output = Result<ServedFile>> + Send;
}

/// The chat server's team skill routes for the signed-in user.
#[derive(Debug, Clone)]
pub struct ChatTeamApi {
    http: ApiClient,
}

impl ChatTeamApi {
    pub fn new(http: ApiClient) -> Self {
        Self { http }
    }

    /// The team skill limits, or `None` when the server doesn't offer them.
    pub async fn limits(&self) -> Result<Option<TeamLimits>> {
        let capabilities = self
            .http
            .request_json(CHAT, Method::GET, "/chat/capabilities", &[], None)
            .await
            .context("cannot read the chat server's capabilities")?;
        Ok(limits_from(&capabilities))
    }

    /// Like [`Self::limits`], failing with a plain reason when the server
    /// has no team skills.
    pub async fn require(&self) -> Result<TeamLimits> {
        self.limits().await?.context(
            "this site's chat server doesn't offer team skills yet (no features.teamSkills in /chat/capabilities)",
        )
    }

    /// Publishes `files` (path, content) as the team skill `name`.
    pub async fn publish(&self, name: &str, files: &[(String, String)]) -> Result<IndexEntry> {
        let body = json!({
            "files": files
                .iter()
                .map(|(path, content)| json!({"path": path, "content": content}))
                .collect::<Vec<_>>(),
        });
        let path = format!("/admin/chat-team-skills/{}", seg(name));
        let reply = self
            .http
            .request_json(CHAT, Method::PUT, &path, &[], Some(body))
            .await
            .map_err(|err| admins_only(err, "publish"))?;
        serde_json::from_value(reply)
            .context("the server's reply to the publish isn't a team skill")
    }

    pub async fn unpublish(&self, name: &str) -> Result<()> {
        let path = format!("/admin/chat-team-skills/{}", seg(name));
        self.http
            .request_json(CHAT, Method::DELETE, &path, &[], None)
            .await
            .map_err(|err| admins_only(err, "unpublish"))?;
        Ok(())
    }
}

impl TeamSource for ChatTeamApi {
    async fn index(&self) -> Result<Vec<IndexEntry>> {
        let reply = self
            .http
            .request_json(CHAT, Method::GET, "/chat/team-skills", &[], None)
            .await
            .map_err(refused_reader)
            .context("cannot read the site's team skills")?;
        parse_index(reply)
    }

    async fn file(&self, name: &str, path: &str) -> Result<ServedFile> {
        let route = format!("/chat/team-skills/{}/file", seg(name));
        let query = [("path".to_string(), path.to_string())];
        let reply = self
            .http
            .request_json(CHAT, Method::GET, &route, &query, None)
            .await
            .map_err(refused_reader)
            .with_context(|| format!("cannot download {path}"))?;
        serde_json::from_value(reply)
            .with_context(|| format!("the server's reply for {path} isn't a file"))
    }
}

fn parse_index(reply: Value) -> Result<Vec<IndexEntry>> {
    let items = reply
        .get("items")
        .cloned()
        .context("the team skill index has no `items`")?;
    serde_json::from_value(items).context("the team skill index isn't in the expected shape")
}

/// A 401 from a read route: the chat server answers it to users without
/// AI chat access, as well as to expired sign-ins.
fn refused_reader(err: anyhow::Error) -> anyhow::Error {
    match crate::tui::errors::parse(&format!("{err:#}")) {
        Failure::Http { status: 401, .. } => anyhow::anyhow!(
            "the chat server turned you away (401): team skills need AI chat access on this site; ask a site admin for it, or sign in again with `gz auth login`"
        ),
        _ => err,
    }
}

/// A 403 from an admin route, said plainly.
fn admins_only(err: anyhow::Error, verb: &str) -> anyhow::Error {
    match crate::tui::errors::parse(&format!("{err:#}")) {
        Failure::Http { status: 403, .. } => anyhow::anyhow!(
            "only site admins can {verb} team skills; ask an admin of this site, or keep it as a personal skill"
        ),
        _ => err,
    }
}

/// Fails when a publish would go past the site's skill count.
pub fn check_room(index: &[IndexEntry], name: &str, limits: &TeamLimits) -> Result<()> {
    let replaces = index.iter().any(|e| e.name == name);
    if !replaces && index.len() >= limits.skills {
        bail!(
            "the site already has {} team skills, its limit; unpublish one first",
            index.len()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_403_from_an_admin_route_says_only_site_admins_can_publish() {
        let forbidden = anyhow::anyhow!(
            "PUT https://x/admin/chat-team-skills/a → HTTP 403 Forbidden: Site admin access required"
        );
        let said = admins_only(forbidden, "publish").to_string();
        assert!(
            said.starts_with("only site admins can publish team skills"),
            "{said}"
        );
        let other = anyhow::anyhow!(
            "PUT https://x/admin/chat-team-skills/a → HTTP 413 Payload Too Large: big"
        );
        assert!(admins_only(other, "publish").to_string().contains("413"));
    }

    #[test]
    fn a_new_skill_needs_room_but_a_replacement_does_not() {
        let (entry, _) = crate::skills::team::test_support::published("a", "x", &[]);
        let limits = TeamLimits {
            skills: 1,
            ..TeamLimits::default()
        };
        assert!(check_room(std::slice::from_ref(&entry), "a", &limits).is_ok());
        assert!(check_room(&[entry], "b", &limits)
            .unwrap_err()
            .to_string()
            .contains("its limit"));
    }
}
