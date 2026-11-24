//! Signing in to remote MCP servers that use OAuth (the MCP authorization
//! spec): protected-resource and authorization-server metadata discovery,
//! dynamic client registration, authorization code with PKCE through a
//! localhost redirect, and refresh. Tokens live in `mcp-auth.json` next to
//! the gz config, owner-only; they are never shown or logged.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use url::Url;

use super::config::Remote;
use crate::assistant::private_file::write_private;

pub const AUTH_FILE: &str = "mcp-auth.json";
/// A token this close to expiry is refreshed before use.
const REFRESH_MARGIN_SECS: i64 = 60;
const CLIENT_NAME: &str = "gz";

/// Where gz keeps MCP sign-ins.
#[derive(Debug, Clone)]
pub struct AuthStore {
    path: PathBuf,
}

/// One server's sign-in: the endpoints and client it used, and its tokens.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Saved {
    /// The server URL signed in to; a server pointed elsewhere signs in again.
    pub url: String,
    pub token_endpoint: String,
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    /// `client_secret_basic` sends the secret as HTTP Basic auth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_method: Option<String>,
    pub redirect_uri: String,
    /// Whether gz registered the client itself (and may register anew).
    #[serde(default)]
    pub registered: bool,
    pub resource: String,
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Unix seconds; none means the server gave no expiry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    servers: BTreeMap<String, Saved>,
}

/// What `gz mcp list` / `get` say about a server's sign-in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginState {
    SignedOut,
    SignedIn {
        expires_at: Option<i64>,
        can_refresh: bool,
    },
}

impl AuthStore {
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// The sign-in for `name`, if it was made for this `url`.
    pub fn get(&self, name: &str, url: &str) -> Result<Option<Saved>> {
        Ok(self
            .load()?
            .servers
            .remove(name)
            .filter(|saved| saved.url == url))
    }

    pub fn state(&self, name: &str, url: &str) -> Result<LoginState> {
        Ok(match self.get(name, url)? {
            Some(saved) => LoginState::SignedIn {
                expires_at: saved.expires_at,
                can_refresh: saved.refresh_token.is_some(),
            },
            None => LoginState::SignedOut,
        })
    }

    pub fn put(&self, name: &str, saved: Saved) -> Result<()> {
        let mut file = self.load()?;
        file.servers.insert(name.to_string(), saved);
        self.save(&file)
    }

    /// Forgets `name`'s sign-in; whether there was one.
    pub fn remove(&self, name: &str) -> Result<bool> {
        let mut file = self.load()?;
        let had = file.servers.remove(name).is_some();
        if had {
            self.save(&file)?;
        }
        Ok(had)
    }

    fn load(&self) -> Result<File> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(File::default()),
            Err(err) => {
                return Err(err).with_context(|| format!("cannot read {}", self.path.display()))
            }
        };
        serde_json::from_str(&text).with_context(|| {
            format!(
                "{} is not valid JSON; delete it and sign in to your MCP servers again",
                self.path.display()
            )
        })
    }

    fn save(&self, file: &File) -> Result<()> {
        let text = serde_json::to_string_pretty(file).context("cannot encode MCP sign-ins")?;
        write_private(&self.path, &text)
    }
}

/// The access token to send to `name`, refreshed first when it is about to
/// expire. `None` when there is no usable sign-in (the server then answers
/// 401 if it needs one).
pub async fn bearer(
    http: &reqwest::Client,
    store: &AuthStore,
    name: &str,
    url: &str,
) -> Option<String> {
    let saved = store.get(name, url).ok().flatten()?;
    let expiring = saved
        .expires_at
        .is_some_and(|at| at - crate::config::now_unix() <= REFRESH_MARGIN_SECS);
    if !expiring {
        return Some(saved.access_token);
    }
    refresh(http, store, name, url).await.ok()
}

/// Trades the refresh token for a new access token and saves it; fails
/// when there is no refresh token or the server refuses it.
pub async fn refresh(
    http: &reqwest::Client,
    store: &AuthStore,
    name: &str,
    url: &str,
) -> Result<String> {
    let mut saved = store
        .get(name, url)?
        .with_context(|| format!("{name} has no sign-in"))?;
    let refresh_token = saved
        .refresh_token
        .clone()
        .with_context(|| format!("{name}'s sign-in can't be refreshed"))?;
    let form = vec![
        ("grant_type", "refresh_token".to_string()),
        ("refresh_token", refresh_token),
        ("resource", saved.resource.clone()),
    ];
    let tokens = token_request(http, &saved, form).await?;
    apply_tokens(&mut saved, tokens);
    store.put(name, saved.clone())?;
    Ok(saved.access_token)
}

/// How a login shows the authorization URL and opens the browser; tests
/// follow the URL themselves.
pub trait Browser {
    fn show(&mut self, line: &str);
    /// Opens `url`; false when no browser could be opened.
    fn open(&mut self, url: &str) -> bool;
}

/// The whole browser sign-in for one server; saves the tokens.
pub async fn login(
    http: &reqwest::Client,
    store: &AuthStore,
    name: &str,
    remote: &Remote,
    browser: &mut dyn Browser,
) -> Result<()> {
    check_secure(&remote.url)?;
    let resource = canonical_resource(&remote.url)?;
    let protected = discover_protected_resource(http, remote, &resource).await?;
    let issuer = protected
        .as_ref()
        .and_then(|meta| {
            meta["authorization_servers"][0]
                .as_str()
                .map(str::to_string)
        })
        .unwrap_or_else(|| origin(&remote.url));
    check_secure(&issuer)?;
    let endpoints = discover_authorization_server(http, &issuer).await?;
    check_secure(&endpoints.authorize)?;
    check_secure(&endpoints.token)?;
    let scope = remote.oauth.scopes.clone().or_else(|| {
        let advertised: Vec<&str> = protected
            .as_ref()
            .and_then(|meta| meta["scopes_supported"].as_array())
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        (!advertised.is_empty()).then(|| advertised.join(" "))
    });

    let earlier = store.get(name, &remote.url).ok().flatten();
    let port = remote
        .oauth
        .callback_port
        .or_else(|| {
            earlier
                .as_ref()
                .and_then(|s| redirect_port(&s.redirect_uri))
        })
        .unwrap_or(0);
    let loopback = match crate::oauth::Loopback::bind(port).await {
        Ok(bound) => bound,
        Err(err) if remote.oauth.callback_port.is_some() => return Err(err),
        Err(_) => crate::oauth::Loopback::bind(0).await?,
    };
    let redirect_uri = loopback.redirect_uri.clone();
    let client = client_for(http, remote, &endpoints, earlier.as_ref(), &redirect_uri).await?;

    let (verifier, challenge) = crate::oauth::pkce_pair();
    let state = crate::oauth::random_state();
    let mut url = Url::parse(&endpoints.authorize).context("invalid authorization endpoint")?;
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("response_type", "code")
            .append_pair("client_id", &client.client_id)
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("state", &state)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("resource", &resource);
        if let Some(scope) = &scope {
            query.append_pair("scope", scope);
        }
    }
    let url = url.to_string();
    browser.show(&format!("Open this URL to sign in to {name}:\n{url}"));
    if !browser.open(&url) {
        browser.show("Could not open a browser; open the URL above yourself.");
    }
    let code = loopback.wait_for_code(&state).await?;

    let mut saved = Saved {
        url: remote.url.clone(),
        token_endpoint: endpoints.token.clone(),
        client_id: client.client_id,
        client_secret: client.client_secret,
        auth_method: client.auth_method,
        redirect_uri: redirect_uri.clone(),
        registered: client.registered,
        resource: resource.clone(),
        scope,
        ..Saved::default()
    };
    let form = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("code_verifier", verifier),
        ("resource", resource),
    ];
    let tokens = token_request(http, &saved, form).await?;
    apply_tokens(&mut saved, tokens);
    store.put(name, saved)
}

/// Tokens go only over https, or plain http to this machine.
fn check_secure(url: &str) -> Result<()> {
    let parsed = Url::parse(url).with_context(|| format!("'{url}' is not a valid URL"))?;
    let loopback = matches!(
        parsed.host_str(),
        Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
    );
    if parsed.scheme() == "https" || (parsed.scheme() == "http" && loopback) {
        return Ok(());
    }
    bail!("refusing to sign in over plain http to {url}; the server must use https")
}

/// The server URL as the token's audience: no fragment, no trailing slash.
fn canonical_resource(url: &str) -> Result<String> {
    let mut parsed = Url::parse(url).with_context(|| format!("'{url}' is not a valid URL"))?;
    parsed.set_fragment(None);
    Ok(parsed.to_string().trim_end_matches('/').to_string())
}

fn origin(url: &str) -> String {
    Url::parse(url)
        .map(|u| u.origin().ascii_serialization())
        .unwrap_or_else(|_| url.to_string())
}

fn redirect_port(redirect_uri: &str) -> Option<u16> {
    Url::parse(redirect_uri).ok()?.port()
}

/// Protected-resource metadata: from the `resource_metadata` the server
/// names in its 401, else the well-known paths (path-specific first).
/// A document must name this server as its `resource` (RFC 9728 section
/// 3.3): one for another resource is refused, so a 401 can't send the
/// sign-in to an authorization server the server doesn't use.
async fn discover_protected_resource(
    http: &reqwest::Client,
    remote: &Remote,
    resource: &str,
) -> Result<Option<Value>> {
    let mut candidates = Vec::new();
    if let Some(named) = challenge_metadata_url(http, remote).await {
        candidates.push(named);
    }
    candidates.extend(well_known(&remote.url, "oauth-protected-resource"));
    for candidate in candidates {
        let Some(meta) = get_json(http, &candidate).await else {
            continue;
        };
        let Some(named) = meta["resource"].as_str() else {
            continue;
        };
        if !covers(named, resource) {
            bail!(
                "the protected-resource metadata at {candidate} is for {named}, not {resource}; \
                 gz won't sign in through it. Check the server's url, or ask its owner to fix \
                 the metadata"
            );
        }
        return Ok(Some(meta));
    }
    Ok(None)
}

/// Whether the metadata's `resource` is this server: the same URL, or a
/// path above it on the same origin.
fn covers(named: &str, resource: &str) -> bool {
    let Ok(named) = canonical_resource(named) else {
        return false;
    };
    resource == named
        || (resource.starts_with(&named)
            && resource[named.len()..].starts_with('/')
            && origin(resource) == origin(&named))
}

/// Asks the server without a token and reads `resource_metadata` from its
/// `WWW-Authenticate` challenge.
async fn challenge_metadata_url(http: &reqwest::Client, remote: &Remote) -> Option<String> {
    let mut request = if remote.sse {
        http.get(&remote.url).header("Accept", "text/event-stream")
    } else {
        http.post(&remote.url)
            .header("Accept", "application/json, text/event-stream")
            .json(&json!({"jsonrpc": "2.0", "id": 0, "method": "ping"}))
    };
    for (key, value) in &remote.headers {
        request = request.header(key, value);
    }
    let response = request.send().await.ok()?;
    if response.status() != reqwest::StatusCode::UNAUTHORIZED {
        return None;
    }
    let header = response
        .headers()
        .get(reqwest::header::WWW_AUTHENTICATE)?
        .to_str()
        .ok()?;
    resource_metadata_param(header)
}

/// `resource_metadata="..."` from a `WWW-Authenticate: Bearer ...` value.
pub fn resource_metadata_param(header: &str) -> Option<String> {
    let at = header.find("resource_metadata=")?;
    let rest = &header[at + "resource_metadata=".len()..];
    let value = match rest.strip_prefix('"') {
        Some(quoted) => quoted.split('"').next()?,
        None => rest.split([',', ' ']).next()?,
    };
    (!value.is_empty()).then(|| value.to_string())
}

/// `{origin}/.well-known/{kind}{path}` then `{origin}/.well-known/{kind}`.
fn well_known(url: &str, kind: &str) -> Vec<String> {
    let Ok(parsed) = Url::parse(url) else {
        return Vec::new();
    };
    let origin = parsed.origin().ascii_serialization();
    let path = parsed.path().trim_end_matches('/');
    let mut out = Vec::new();
    if !path.is_empty() {
        out.push(format!("{origin}/.well-known/{kind}{path}"));
    }
    out.push(format!("{origin}/.well-known/{kind}"));
    out
}

async fn get_json(http: &reqwest::Client, url: &str) -> Option<Value> {
    let response = http
        .get(url)
        .header("Accept", "application/json")
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json::<Value>().await.ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Endpoints {
    authorize: String,
    token: String,
    register: Option<String>,
}

/// The authorization server's endpoints from its OAuth or OpenID metadata;
/// without either, the spec's default paths on its origin.
async fn discover_authorization_server(http: &reqwest::Client, issuer: &str) -> Result<Endpoints> {
    let mut candidates = well_known(issuer, "oauth-authorization-server");
    candidates.extend(well_known(issuer, "openid-configuration"));
    candidates.push(format!(
        "{}/.well-known/openid-configuration",
        issuer.trim_end_matches('/')
    ));
    candidates.dedup();
    for candidate in candidates {
        let Some(meta) = get_json(http, &candidate).await else {
            continue;
        };
        let (Some(authorize), Some(token)) = (
            meta["authorization_endpoint"].as_str(),
            meta["token_endpoint"].as_str(),
        ) else {
            continue;
        };
        // RFC 8414 section 3.3: the metadata must be the issuer's own.
        let named = meta["issuer"].as_str().unwrap_or("no issuer");
        if named.trim_end_matches('/') != issuer.trim_end_matches('/') {
            bail!(
                "the authorization server metadata at {candidate} names {named} as its issuer, \
                 not {issuer}; gz won't sign in with it"
            );
        }
        if let Some(methods) = meta["code_challenge_methods_supported"].as_array() {
            if !methods.iter().any(|m| m == "S256") {
                bail!(
                    "the authorization server at {issuer} doesn't support PKCE with S256, \
                     which gz requires"
                );
            }
        }
        return Ok(Endpoints {
            authorize: authorize.to_string(),
            token: token.to_string(),
            register: meta["registration_endpoint"].as_str().map(str::to_string),
        });
    }
    let base = origin(issuer);
    Ok(Endpoints {
        authorize: format!("{base}/authorize"),
        token: format!("{base}/token"),
        register: Some(format!("{base}/register")),
    })
}

struct Client {
    client_id: String,
    client_secret: Option<String>,
    auth_method: Option<String>,
    registered: bool,
}

/// The configured client id; else the one gz registered before for this
/// redirect; else a fresh dynamic registration.
async fn client_for(
    http: &reqwest::Client,
    remote: &Remote,
    endpoints: &Endpoints,
    earlier: Option<&Saved>,
    redirect_uri: &str,
) -> Result<Client> {
    if let Some(client_id) = &remote.oauth.client_id {
        return Ok(Client {
            client_id: client_id.clone(),
            client_secret: remote.oauth.client_secret.clone(),
            auth_method: None,
            registered: false,
        });
    }
    if let Some(saved) = earlier.filter(|s| {
        s.registered && s.redirect_uri == redirect_uri && s.token_endpoint == endpoints.token
    }) {
        return Ok(Client {
            client_id: saved.client_id.clone(),
            client_secret: saved.client_secret.clone(),
            auth_method: saved.auth_method.clone(),
            registered: true,
        });
    }
    let Some(register) = &endpoints.register else {
        bail!(
            "the authorization server offers no dynamic client registration; register gz there \
             with redirect URI {redirect_uri}, then set oauth.client_id (and oauth.callback_port) \
             for this server"
        );
    };
    let body = json!({
        "client_name": CLIENT_NAME,
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    let response = http
        .post(register)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("cannot reach the registration endpoint {register}"))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!(
            "client registration at {register} failed (HTTP {status}): {}; set oauth.client_id \
             to use a client registered by hand",
            excerpt(&text)
        );
    }
    let reply: Value = serde_json::from_str(&text).context("the registration reply is not JSON")?;
    let client_id = reply["client_id"]
        .as_str()
        .context("the registration reply has no client_id")?;
    Ok(Client {
        client_id: client_id.to_string(),
        client_secret: reply["client_secret"].as_str().map(str::to_string),
        auth_method: reply["token_endpoint_auth_method"]
            .as_str()
            .map(str::to_string),
        registered: true,
    })
}

#[derive(Deserialize)]
struct Tokens {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    scope: Option<String>,
}

async fn token_request(
    http: &reqwest::Client,
    saved: &Saved,
    mut form: Vec<(&'static str, String)>,
) -> Result<Tokens> {
    let basic = saved.auth_method.as_deref() == Some("client_secret_basic");
    let mut request = http.post(&saved.token_endpoint);
    match (&saved.client_secret, basic) {
        (Some(secret), true) => request = request.basic_auth(&saved.client_id, Some(secret)),
        (Some(secret), false) => {
            form.push(("client_id", saved.client_id.clone()));
            form.push(("client_secret", secret.clone()));
        }
        (None, _) => form.push(("client_id", saved.client_id.clone())),
    }
    let response = request
        .header("Accept", "application/json")
        .form(&form)
        .send()
        .await
        .with_context(|| format!("cannot reach the token endpoint {}", saved.token_endpoint))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!(
            "the token endpoint refused (HTTP {status}): {}",
            excerpt(&text)
        );
    }
    serde_json::from_str(&text).context("the token reply has no access_token")
}

fn apply_tokens(saved: &mut Saved, tokens: Tokens) {
    saved.access_token = tokens.access_token;
    if tokens.refresh_token.is_some() {
        saved.refresh_token = tokens.refresh_token;
    }
    saved.expires_at = tokens
        .expires_in
        .map(|secs| crate::config::now_unix() + secs);
    if tokens.scope.is_some() {
        saved.scope = tokens.scope;
    }
}

/// The start of a server's error body: enough to explain, never a page.
pub fn excerpt(text: &str) -> String {
    const MAX: usize = 300;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "no details".to_string();
    }
    let mut cut: String = trimmed.chars().take(MAX).collect();
    if trimmed.chars().count() > MAX {
        cut.push('…');
    }
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_metadata_url_is_read_from_a_bearer_challenge() {
        assert_eq!(
            resource_metadata_param(
                r#"Bearer error="invalid_token", resource_metadata="https://x/.well-known/oauth-protected-resource/mcp""#
            )
            .as_deref(),
            Some("https://x/.well-known/oauth-protected-resource/mcp")
        );
        assert_eq!(
            resource_metadata_param("Bearer resource_metadata=https://x/m, realm=a").as_deref(),
            Some("https://x/m")
        );
        assert_eq!(resource_metadata_param("Bearer realm=\"x\""), None);
    }

    #[test]
    fn well_known_paths_try_the_path_specific_one_first() {
        assert_eq!(
            well_known("https://a.test/tenant/mcp/", "oauth-protected-resource"),
            [
                "https://a.test/.well-known/oauth-protected-resource/tenant/mcp",
                "https://a.test/.well-known/oauth-protected-resource",
            ]
        );
        assert_eq!(
            well_known("https://a.test", "oauth-authorization-server"),
            ["https://a.test/.well-known/oauth-authorization-server"]
        );
    }

    #[test]
    fn tokens_go_only_over_https_or_to_this_machine() {
        assert!(check_secure("https://a.test/mcp").is_ok());
        assert!(check_secure("http://127.0.0.1:9/mcp").is_ok());
        assert!(check_secure("http://localhost:9/mcp").is_ok());
        let err = check_secure("http://a.test/mcp").unwrap_err();
        assert!(format!("{err:#}").contains("plain http"), "{err:#}");
    }

    #[test]
    fn the_store_is_private_and_forgets_a_server_pointed_elsewhere() {
        let dir = std::env::temp_dir().join(format!("gz-mcp-auth-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = AuthStore::at(dir.join(AUTH_FILE));
        assert_eq!(
            store.state("a", "https://a/mcp").unwrap(),
            LoginState::SignedOut
        );
        store
            .put(
                "a",
                Saved {
                    url: "https://a/mcp".into(),
                    access_token: "tok".into(),
                    refresh_token: Some("r".into()),
                    ..Saved::default()
                },
            )
            .unwrap();
        assert_eq!(
            store.state("a", "https://a/mcp").unwrap(),
            LoginState::SignedIn {
                expires_at: None,
                can_refresh: true
            }
        );
        assert!(store.get("a", "https://elsewhere/mcp").unwrap().is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join(AUTH_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(store.remove("a").unwrap());
        assert!(!store.remove("a").unwrap());
    }
}
