use anyhow::{Context, Result};
use base64::Engine;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use url::Url;

use crate::config;

pub const READ_SCOPE: &str = "mcp:tools:read";
pub const WRITE_SCOPE: &str = "mcp:tools:write";
const CLIENT_NAME: &str = "gz-cli";

#[derive(Debug, Clone, Deserialize)]
pub struct ServerMetadata {
    #[allow(dead_code)]
    pub issuer: Option<String>,
    pub authorization_endpoint: Option<String>,
    pub token_endpoint: Option<String>,
    pub revocation_endpoint: Option<String>,
    pub registration_endpoint: Option<String>,
    #[allow(dead_code)]
    pub scopes_supported: Option<Vec<String>>,
}

impl ServerMetadata {
    pub fn with_defaults(auth_host: &str, fetched: Option<ServerMetadata>) -> Self {
        let base = auth_host.trim_end_matches('/');
        let empty = Self {
            issuer: None,
            authorization_endpoint: None,
            token_endpoint: None,
            revocation_endpoint: None,
            registration_endpoint: None,
            scopes_supported: None,
        };
        let meta = fetched.unwrap_or(empty);
        Self {
            issuer: meta.issuer,
            authorization_endpoint: meta
                .authorization_endpoint
                .or_else(|| Some(format!("{base}/oauth/authorize"))),
            token_endpoint: meta
                .token_endpoint
                .or_else(|| Some(format!("{base}/oauth/token"))),
            revocation_endpoint: meta
                .revocation_endpoint
                .or_else(|| Some(format!("{base}/oauth/revoke"))),
            registration_endpoint: meta
                .registration_endpoint
                .or_else(|| Some(format!("{base}/oauth/register"))),
            scopes_supported: meta.scopes_supported,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct ProtectedResource {
    resource: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct RegistrationRequest {
    redirect_uris: Vec<String>,
    client_name: String,
    grant_types: Vec<String>,
    response_types: Vec<String>,
    scope: String,
    token_endpoint_auth_method: String,
}

#[derive(Debug, Clone, Deserialize)]
struct RegistrationResponse {
    client_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub token_type: Option<String>,
    pub expires_in: Option<i64>,
    pub scope: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct ManualTokenResponse {
    pub token: String,
    #[serde(rename = "tokenType")]
    pub token_type: Option<String>,
    #[serde(rename = "expiresIn")]
    pub expires_in: Option<i64>,
    pub scope: Option<String>,
    pub site: Option<String>,
    pub username: Option<String>,
}

pub async fn discover(client: &reqwest::Client, auth_host: &str) -> Option<ServerMetadata> {
    let url = config::join_url(auth_host, "/.well-known/oauth-authorization-server");
    let response = client.get(&url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json::<ServerMetadata>().await.ok()
}

pub async fn protected_resource(
    client: &reqwest::Client,
    auth_host: &str,
    site: Option<&str>,
) -> Option<String> {
    let mut candidates = Vec::new();
    if let Some(site) = site {
        candidates.push(format!(
            "/.well-known/oauth-protected-resource/sites/{site}/mcp"
        ));
    }
    candidates.push("/.well-known/oauth-protected-resource/mcp".to_string());
    candidates.push("/.well-known/oauth-protected-resource".to_string());
    for path in candidates {
        let url = config::join_url(auth_host, &path);
        let response = match client.get(&url).send().await {
            Ok(response) => response,
            Err(_) => continue,
        };
        if !response.status().is_success() {
            continue;
        }
        if let Ok(meta) = response.json::<ProtectedResource>().await {
            if let Some(resource) = meta.resource {
                if !resource.trim().is_empty() {
                    return Some(resource);
                }
            }
        }
    }
    None
}

pub async fn register_client(
    client: &reqwest::Client,
    registration_endpoint: &str,
    redirect_uri: &str,
    scope: &str,
) -> Result<String> {
    let payload = RegistrationRequest {
        redirect_uris: vec![redirect_uri.to_string()],
        client_name: CLIENT_NAME.to_string(),
        grant_types: vec!["authorization_code".to_string()],
        response_types: vec!["code".to_string()],
        scope: scope.to_string(),
        token_endpoint_auth_method: "none".to_string(),
    };
    let response = client
        .post(registration_endpoint)
        .json(&payload)
        .send()
        .await
        .context("client registration failed")?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("client registration → HTTP {status}: {text}");
    }
    let parsed: RegistrationResponse =
        serde_json::from_str(&text).context("cannot parse registration response")?;
    Ok(parsed.client_id)
}

pub fn pkce_pair() -> (String, String) {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hasher.finalize());
    (verifier, challenge)
}

pub fn random_state() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub fn scopes(write: bool) -> String {
    if write {
        format!("{READ_SCOPE} {WRITE_SCOPE}")
    } else {
        READ_SCOPE.to_string()
    }
}

pub fn authorize_url(
    authorization_endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    scope: &str,
    state: &str,
    resource: &str,
    challenge: &str,
) -> Result<String> {
    let mut url = Url::parse(authorization_endpoint).context("invalid authorization endpoint")?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", scope)
        .append_pair("state", state)
        .append_pair("resource", resource)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256");
    Ok(url.to_string())
}

pub async fn exchange_code(
    client: &reqwest::Client,
    token_endpoint: &str,
    code: &str,
    client_id: &str,
    redirect_uri: &str,
    verifier: &str,
    resource: &str,
) -> Result<TokenResponse> {
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("client_id", client_id),
        ("redirect_uri", redirect_uri),
        ("code_verifier", verifier),
        ("resource", resource),
    ];
    let response = client
        .post(token_endpoint)
        .form(&form)
        .send()
        .await
        .context("token exchange failed")?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("token exchange → HTTP {status}: {text}");
    }
    serde_json::from_str(&text).context("cannot parse token response")
}

pub async fn revoke_token(
    client: &reqwest::Client,
    revocation_endpoint: &str,
    token: &str,
) -> Result<()> {
    let form = [("token", token)];
    let response = client
        .post(revocation_endpoint)
        .form(&form)
        .send()
        .await
        .context("revocation request failed")?;
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        anyhow::bail!("revocation → HTTP {status}: {text}");
    }
    Ok(())
}

pub struct ManualLogin<'a> {
    pub site: &'a str,
    pub username: &'a str,
    pub password: &'a str,
    pub totp: &'a str,
    pub domain: Option<&'a str>,
    pub allow_write: bool,
}

pub async fn manual_token(
    client: &reqwest::Client,
    auth_host: &str,
    login: &ManualLogin<'_>,
) -> Result<ManualTokenResponse> {
    let url = config::join_url(auth_host, "/mcp/auth/token");
    let mut payload = serde_json::json!({
        "site": login.site,
        "username": login.username,
        "password": login.password,
        "totp": login.totp,
        "allowWrite": login.allow_write,
    });
    if let Some(domain) = login.domain {
        payload["domain"] = serde_json::Value::String(domain.to_string());
    }
    let response = client
        .post(&url)
        .json(&payload)
        .send()
        .await
        .context("manual login failed")?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("manual login → HTTP {status}: {text}");
    }
    serde_json::from_str(&text).context("cannot parse login response")
}

pub struct Loopback {
    pub redirect_uri: String,
    listener: tokio::net::TcpListener,
}

impl Loopback {
    pub async fn bind(port: u16) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}"))
            .await
            .context("cannot bind loopback callback (is the port busy?)")?;
        let addr = listener
            .local_addr()
            .context("cannot read loopback address")?;
        Ok(Self {
            redirect_uri: format!("http://127.0.0.1:{}/callback", addr.port()),
            listener,
        })
    }

    pub async fn wait_for_code(self, expected_state: &str) -> Result<String> {
        let (mut socket, _) =
            tokio::time::timeout(Duration::from_secs(300), self.listener.accept())
                .await
                .context("login timed out waiting for the browser callback")?
                .context("cannot accept browser callback")?;
        let mut buf = vec![0u8; 8192];
        let mut raw: Vec<u8> = Vec::new();
        loop {
            let n = tokio::time::timeout(Duration::from_secs(60), socket.read(&mut buf))
                .await
                .context("login timed out reading the browser callback")?
                .context("cannot read browser callback")?;
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&buf[..n]);
            if raw.windows(4).any(|w| w == b"\r\n\r\n") || raw.len() > 16384 {
                break;
            }
        }
        let text = String::from_utf8_lossy(&raw);
        let request_line = text.lines().next().unwrap_or_default().to_string();
        let target = request_line.split_whitespace().nth(1).unwrap_or("/");
        let full = format!("http://localhost{target}");
        let parsed = Url::parse(&full).context("cannot parse callback request")?;
        let params: HashMap<String, String> = parsed
            .query_pairs()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        if let Some(error) = params.get("error") {
            let detail = params.get("error_description").cloned().unwrap_or_default();
            Self::respond(&mut socket, false).await?;
            anyhow::bail!("authorization failed: {error} {detail}");
        }
        let state = params.get("state").cloned().unwrap_or_default();
        if state != expected_state {
            Self::respond(&mut socket, false).await?;
            anyhow::bail!("authorization failed: state mismatch (possible CSRF)");
        }
        let code = params.get("code").cloned().unwrap_or_default();
        if code.is_empty() {
            Self::respond(&mut socket, false).await?;
            anyhow::bail!("authorization failed: no code returned");
        }
        Self::respond(&mut socket, true).await?;
        Ok(code)
    }

    async fn respond(socket: &mut tokio::net::TcpStream, ok: bool) -> Result<()> {
        let (title, message) = if ok {
            (
                "Login successful",
                "You can close this tab and return to the terminal.",
            )
        } else {
            ("Login failed", "Close this tab and retry `gz auth login`.")
        };
        let body = format!(
            "<html><body style=\"font-family:sans-serif;text-align:center;padding-top:10%\">\
             <h2>{title}</h2><p>{message}</p></body></html>"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        socket.write_all(response.as_bytes()).await.ok();
        socket.shutdown().await.ok();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_matches_verifier() {
        let (verifier, challenge) = pkce_pair();
        let mut hasher = Sha256::new();
        hasher.update(verifier.as_bytes());
        let expected = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hasher.finalize());
        assert_eq!(challenge, expected);
        assert_eq!(verifier.len(), 43);
    }

    #[test]
    fn scopes_default_to_read() {
        assert_eq!(scopes(false), "mcp:tools:read");
        assert_eq!(scopes(true), "mcp:tools:read mcp:tools:write");
    }

    #[test]
    fn authorize_url_carries_mcp_params() {
        let url = authorize_url(
            "https://auth.test/oauth/authorize",
            "cid",
            "http://127.0.0.1:9/callback",
            "mcp:tools:read",
            "state1",
            "https://auth.test/mcp",
            "challenge1",
        )
        .unwrap();
        for part in [
            "response_type=code",
            "client_id=cid",
            "code_challenge=challenge1",
            "code_challenge_method=S256",
            "resource=",
            "state=state1",
        ] {
            assert!(url.contains(part), "missing {part} in {url}");
        }
    }

    #[test]
    fn metadata_defaults_point_at_oauth_paths() {
        let meta = ServerMetadata::with_defaults("https://auth.test/", None);
        assert_eq!(
            meta.token_endpoint.unwrap(),
            "https://auth.test/oauth/token"
        );
        assert_eq!(
            meta.registration_endpoint.unwrap(),
            "https://auth.test/oauth/register"
        );
    }
}
