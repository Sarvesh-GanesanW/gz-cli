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

/// Endpoints from `/.well-known/oauth-authorization-server`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ServerMetadata {
    authorization_endpoint: Option<String>,
    token_endpoint: Option<String>,
    registration_endpoint: Option<String>,
}

/// OAuth endpoints, falling back to gz-chat's default `/oauth/*` paths.
#[derive(Debug, Clone)]
pub struct Endpoints {
    pub authorize: String,
    pub token: String,
    pub register: String,
}

impl Endpoints {
    pub fn resolve(auth_host: &str, fetched: Option<ServerMetadata>) -> Self {
        let base = auth_host.trim_end_matches('/');
        let meta = fetched.unwrap_or_default();
        Self {
            authorize: meta
                .authorization_endpoint
                .unwrap_or_else(|| format!("{base}/oauth/authorize")),
            token: meta
                .token_endpoint
                .unwrap_or_else(|| format!("{base}/oauth/token")),
            register: meta
                .registration_endpoint
                .unwrap_or_else(|| format!("{base}/oauth/register")),
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
    pub expires_in: Option<i64>,
    pub scope: Option<String>,
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

    /// Waits for the browser's `/callback` redirect. Other requests (favicon,
    /// prefetch, stray local connections) get a 404 and are ignored.
    pub async fn wait_for_code(self, expected_state: &str) -> Result<String> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
        loop {
            let (mut socket, _) = tokio::time::timeout_at(deadline, self.listener.accept())
                .await
                .context("login timed out waiting for the browser callback")?
                .context("cannot accept browser callback")?;
            let Ok(target) = read_request_target(&mut socket).await else {
                continue;
            };
            let parsed = Url::parse(&format!("http://localhost{target}"))
                .context("cannot parse callback request")?;
            if parsed.path() != "/callback" {
                Self::respond_not_found(&mut socket).await;
                continue;
            }
            let params: HashMap<String, String> = parsed
                .query_pairs()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            if let Some(error) = params.get("error") {
                let detail = params.get("error_description").cloned().unwrap_or_default();
                Self::respond(&mut socket, false).await;
                anyhow::bail!("authorization failed: {error} {detail}");
            }
            if params.get("state").map(String::as_str) != Some(expected_state) {
                Self::respond_not_found(&mut socket).await;
                continue;
            }
            let code = params.get("code").cloned().unwrap_or_default();
            if code.is_empty() {
                Self::respond(&mut socket, false).await;
                anyhow::bail!("authorization failed: no code returned");
            }
            Self::respond(&mut socket, true).await;
            return Ok(code);
        }
    }

    async fn respond_not_found(socket: &mut tokio::net::TcpStream) {
        let response = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        socket.write_all(response.as_bytes()).await.ok();
        socket.shutdown().await.ok();
    }

    async fn respond(socket: &mut tokio::net::TcpStream, ok: bool) {
        let (title, message) = if ok {
            (
                "Login successful",
                "You can close this tab and return to the terminal.",
            )
        } else {
            (
                "Login failed",
                "Close this tab and retry `gz auth mcp-token`.",
            )
        };
        let body = format!(
            "<html><body style=\"font-family:sans-serif;text-align:center;padding-top:10%\">\
             <h2>{title}</h2><p>{message}</p></body></html>"
        );
        let status = if ok { "200 OK" } else { "400 Bad Request" };
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        socket.write_all(response.as_bytes()).await.ok();
        socket.shutdown().await.ok();
    }
}

/// Reads one HTTP request head and returns its target (`/callback?...`).
async fn read_request_target(socket: &mut tokio::net::TcpStream) -> Result<String> {
    let mut buf = [0u8; 4096];
    let mut raw: Vec<u8> = Vec::new();
    while !raw.windows(4).any(|w| w == b"\r\n\r\n") && raw.len() <= 16384 {
        let n = tokio::time::timeout(Duration::from_secs(10), socket.read(&mut buf))
            .await
            .context("timed out reading the browser callback")?
            .context("cannot read browser callback")?;
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&buf[..n]);
    }
    let text = String::from_utf8_lossy(&raw);
    let line = text.lines().next().unwrap_or_default();
    Ok(line.split_whitespace().nth(1).unwrap_or("/").to_string())
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
        let endpoints = Endpoints::resolve("https://auth.test/", None);
        assert_eq!(endpoints.token, "https://auth.test/oauth/token");
        assert_eq!(endpoints.register, "https://auth.test/oauth/register");
    }
}
