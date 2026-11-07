use anyhow::{Context, Result};
use serde_json::json;

use crate::apikey::{self, KeySource};
use crate::cli::AuthAction;
use crate::cmd::{prompt_line, Runtime};
use crate::cognito::{self, Step};
use crate::config::{self, load_config, redact_token, save_config};
use crate::oauth;
use crate::output::{self, OutputFormat};
use crate::webapp;

pub async fn run(rt: &Runtime, action: &AuthAction) -> Result<()> {
    match action {
        AuthAction::Login { api_key: true, .. } => save_api_key(rt).await,
        AuthAction::Login {
            username,
            password_stdin,
            mfa_code,
            ..
        } => login(rt, username.clone(), *password_stdin, mfa_code.clone()).await,
        AuthAction::Status => status(rt).await,
        AuthAction::Logout { local_only } => logout(rt, *local_only).await,
        AuthAction::McpToken {
            write,
            no_open,
            port,
        } => mcp_token(rt, *write, *no_open, *port).await,
    }
}

async fn login(
    rt: &Runtime,
    username: Option<String>,
    password_stdin: bool,
    mfa_code: Option<String>,
) -> Result<()> {
    let resolved = rt.http.resolved();
    let web_url = resolved
        .web_url()
        .context("no client or site configured — run `gz configure` first")?;
    let http = rt.http.raw();
    if rt.format == OutputFormat::Table {
        output::info(&format!("loading sign-in settings from {web_url}"));
    }
    let web = webapp::discover(http, &web_url).await?;
    let pool = web.pool.clone();
    let username = match username {
        Some(name) => name,
        None => prompt_with_default("Username", resolved.profile.username.as_deref())?,
    };
    if username.is_empty() {
        anyhow::bail!("a username is required");
    }
    let password = read_password(password_stdin)?;
    let tokens = match cognito::sign_in(http, &pool, &username, &password).await? {
        Step::Done(tokens) => tokens,
        Step::NeedsMfa {
            session,
            username: srp_user,
        } => {
            let code = match mfa_code {
                Some(code) => code,
                None => prompt_line("Authenticator code: ")?,
            };
            cognito::answer_mfa(http, &pool, &session, &srp_user, code.trim()).await?
        }
    };

    let name = resolved.profile_name.clone();
    let mut config = load_config()?;
    let entry = config.profile_mut(&name);
    entry.token = Some(tokens.id_token);
    entry.expires_at = Some(config::now_unix() + tokens.expires_in);
    entry.refresh_token = tokens.refresh_token;
    entry.access_token = tokens.access_token;
    entry.username = Some(username.clone());
    entry.cognito_pool_id = Some(pool.pool_id);
    entry.cognito_client_id = Some(pool.client_id);
    entry.gateways = web.gateways;
    if config.current_profile.is_none() {
        config.current_profile = Some(name.clone());
    }
    save_config(&config)?;
    output::success(&format!(
        "Signed in as {username} · {} (profile '{name}')",
        web_url.trim_start_matches("https://")
    ));
    Ok(())
}

fn prompt_with_default(label: &str, current: Option<&str>) -> Result<String> {
    let answer = match current {
        Some(value) => prompt_line(&format!("{label} [{value}]: "))?,
        None => prompt_line(&format!("{label}: "))?,
    };
    Ok(if answer.is_empty() {
        current.unwrap_or_default().to_string()
    } else {
        answer
    })
}

/// Passwords are used exactly as typed; only the line ending is dropped.
fn read_password(from_stdin: bool) -> Result<String> {
    read_secret("Password", from_stdin)
}

/// Prompts without echo on a terminal, else reads one line from stdin.
/// Secrets never come from argv, where other processes can see them.
fn read_secret(label: &str, from_stdin: bool) -> Result<String> {
    use std::io::{BufRead, IsTerminal};
    if !from_stdin && std::io::stdin().is_terminal() {
        return rpassword::prompt_password(format!("{label}: "))
            .with_context(|| format!("cannot read {}", label.to_lowercase()));
    }
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .with_context(|| format!("cannot read {} from stdin", label.to_lowercase()))?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

/// Stores a platform API key in the profile. Keys skip Cognito, but the
/// data gateway URLs still come from the web app, so those are refreshed too.
async fn save_api_key(rt: &Runtime) -> Result<()> {
    let resolved = rt.http.resolved();
    let web_url = resolved
        .web_url()
        .context("no client or site configured — run `gz configure` first")?;
    let key = read_secret("API key", false)?.trim().to_string();
    if !apikey::looks_like_key(&key) {
        anyhow::bail!("that doesn't look like a Groundzero API key (sk_…); create one with `gz permissions api-key create`");
    }
    let web = webapp::discover(rt.http.raw(), &web_url).await?;
    let name = resolved.profile_name.clone();
    let mut config = load_config()?;
    let entry = config.profile_mut(&name);
    entry.api_key = Some(key.clone());
    entry.gateways = web.gateways;
    if config.current_profile.is_none() {
        config.current_profile = Some(name.clone());
    }
    let path = save_config(&config)?;
    output::success(&format!(
        "API key {} saved to profile '{name}' ({})",
        redact_token(&key),
        path.display()
    ));
    output::info(&format!(
        "{}. Check it with `gz doctor`.",
        apikey::KEYS_ONLY_HINT
    ));
    Ok(())
}

fn key_source_label(source: KeySource) -> &'static str {
    match source {
        KeySource::Env => "GZ_API_KEY",
        KeySource::Profile => "config",
    }
}

fn expires_in_words(expires_at: Option<i64>) -> String {
    let Some(exp) = expires_at else {
        return "unknown".to_string();
    };
    let left = exp - config::now_unix();
    match left {
        l if l <= 0 => "expired".to_string(),
        l if l < 60 => format!("in {l}s"),
        l if l < 3600 => format!("in {}m", l / 60),
        l => format!("in {}h {}m", l / 3600, l % 3600 / 60),
    }
}

async fn status(rt: &Runtime) -> Result<()> {
    let resolved = rt.http.resolved();
    let profile = &resolved.profile;
    let token = resolved.token();
    let refreshable = profile.refresh_token.is_some() && profile.pool().is_some();
    let signed_in = token.is_some() && (!resolved.token_expired() || refreshable);
    let api_key = resolved.api_key();
    let key_json = api_key.as_ref().map(
        |(key, source)| json!({"key": redact_token(key), "source": key_source_label(*source)}),
    );
    let value = json!({
        "apiKey": key_json,
        "profile": resolved.profile_name,
        "signedIn": signed_in,
        "username": profile.username,
        "site": resolved.web_url(),
        "tokenSource": resolved.token_source,
        "token": token.as_deref().map(redact_token),
        "expires": expires_in_words(profile.expires_at),
        "autoRefresh": refreshable,
    });
    if rt.format != OutputFormat::Table {
        return rt.show(value).await;
    }
    if let Some((key, source)) = &api_key {
        output::info(&format!(
            "API key {} (from {}) · {}",
            redact_token(key),
            key_source_label(*source),
            apikey::KEYS_ONLY_HINT
        ));
    }
    if !signed_in {
        output::warn("Not signed in. Run `gz auth login`.");
        return Ok(());
    }
    output::success(&format!(
        "Signed in as {} on {}",
        profile.username.as_deref().unwrap_or("(token)"),
        resolved
            .web_url()
            .unwrap_or_default()
            .trim_start_matches("https://"),
    ));
    let renews = if refreshable {
        ", renews automatically"
    } else {
        ""
    };
    output::info(&format!(
        "profile '{}' · token {} · expires {}{renews}",
        resolved.profile_name,
        token.as_deref().map(redact_token).unwrap_or_default(),
        expires_in_words(profile.expires_at),
    ));
    Ok(())
}

async fn logout(rt: &Runtime, local_only: bool) -> Result<()> {
    let name = rt.http.resolved().profile_name.clone();
    let mut config = load_config()?;
    let entry = config.profile_mut(&name);
    let mut revoked = false;
    if !local_only {
        if let (Some(refresh), Some(pool)) = (entry.refresh_token.clone(), entry.pool()) {
            match cognito::revoke(rt.http.raw(), &pool, &refresh).await {
                Ok(()) => revoked = true,
                Err(err) => output::warn(&format!("revoke failed ({err:#}); clearing locally")),
            }
        }
    }
    entry.clear_session();
    entry.api_key = None;
    save_config(&config)?;
    if rt.format != OutputFormat::Table {
        return rt.show(json!({"profile": name, "revoked": revoked})).await;
    }
    let note = if revoked { ", session revoked" } else { "" };
    output::success(&format!("Signed out (profile '{name}'){note}"));
    Ok(())
}

/// Browser OAuth against gz-chat's MCP server. The token only works on the
/// MCP endpoint, so it is printed for an AI tool to use, never stored.
async fn mcp_token(rt: &Runtime, write: bool, no_open: bool, port: u16) -> Result<()> {
    let resolved = rt.http.resolved();
    let auth_host = resolved
        .auth_host()
        .context("no client configured — run `gz configure` first")?;
    if !config::is_secure_base(&auth_host) {
        anyhow::bail!("refusing to sign in over plain http: {auth_host}");
    }
    let http = rt.http.raw();
    let scope = oauth::scopes(write);
    let endpoints = oauth::Endpoints::resolve(&auth_host, oauth::discover(http, &auth_host).await);
    let resource = oauth::protected_resource(http, &auth_host, resolved.site().as_deref())
        .await
        .unwrap_or_else(|| format!("{}/mcp", auth_host.trim_end_matches('/')));

    let loopback = oauth::Loopback::bind(port).await?;
    let client_id =
        oauth::register_client(http, &endpoints.register, &loopback.redirect_uri, &scope).await?;
    let (verifier, challenge) = oauth::pkce_pair();
    let state = oauth::random_state();
    let url = oauth::authorize_url(
        &endpoints.authorize,
        &client_id,
        &loopback.redirect_uri,
        &scope,
        &state,
        &resource,
        &challenge,
    )?;
    eprintln!("Open this URL to approve MCP access:\n{url}");
    if !no_open && open::that(&url).is_err() {
        output::warn("could not open a browser; paste the URL above manually");
    }
    let redirect_uri = loopback.redirect_uri.clone();
    let code = loopback.wait_for_code(&state).await?;
    let token = oauth::exchange_code(
        http,
        &endpoints.token,
        &code,
        &client_id,
        &redirect_uri,
        &verifier,
        &resource,
    )
    .await?;
    if rt.format == OutputFormat::Table {
        output::info(&format!("MCP endpoint: {resource}"));
        println!("{}", token.access_token);
        return Ok(());
    }
    rt.show(json!({
        "resource": resource,
        "token": token.access_token,
        "expiresIn": token.expires_in,
        "scope": token.scope.unwrap_or(scope),
    }))
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_expiry_in_words() {
        let now = config::now_unix();
        assert_eq!(expires_in_words(None), "unknown");
        assert_eq!(expires_in_words(Some(now - 5)), "expired");
        assert_eq!(expires_in_words(Some(now + 30)), "in 30s");
        assert_eq!(expires_in_words(Some(now + 600)), "in 10m");
        assert_eq!(expires_in_words(Some(now + 3_720)), "in 1h 2m");
    }
}
