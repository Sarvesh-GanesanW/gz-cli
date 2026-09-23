use anyhow::{Context, Result};
use serde_json::Value;

use crate::cli::AuthAction;
use crate::cmd::Runtime;
use crate::config::{self, load_config, redact_token, save_config};
use crate::oauth;
use crate::output::{self, OutputFormat};

pub async fn run(rt: &Runtime, action: &AuthAction) -> Result<()> {
    match action {
        AuthAction::Login {
            manual,
            write,
            site,
            username,
            domain,
            no_open,
            port,
        } => {
            if *manual {
                manual_login(rt, *write, site.clone(), username.clone(), domain.clone()).await
            } else {
                browser_login(rt, *write, *no_open, *port).await
            }
        }
        AuthAction::Status => status(rt).await,
        AuthAction::Logout { local_only } => logout(rt, *local_only).await,
    }
}

async fn browser_login(rt: &Runtime, write: bool, no_open: bool, port: u16) -> Result<()> {
    let resolved = rt.http.resolved();
    let auth_host = resolved.auth_host().context(
        "no client configured — run `gz configure` first (it will prompt for client, site, domain)",
    )?;
    warn_unless_secure(&auth_host);
    let site = resolved.site();
    let client = rt.http.raw();
    let scope = oauth::scopes(write);

    if !rt.quiet && rt.format == OutputFormat::Table {
        output::info(&format!("auth server: {auth_host}"));
    }
    let fetched = oauth::discover(client, &auth_host).await;
    let meta = oauth::ServerMetadata::with_defaults(&auth_host, fetched);
    let resource = oauth::protected_resource(client, &auth_host, site.as_deref())
        .await
        .unwrap_or_else(|| format!("{}/mcp", auth_host.trim_end_matches('/')));

    let loopback = oauth::Loopback::bind(port).await?;
    let client_id = oauth::register_client(
        client,
        meta.registration_endpoint.as_deref().unwrap_or_default(),
        &loopback.redirect_uri,
        &scope,
    )
    .await?;
    let (verifier, challenge) = oauth::pkce_pair();
    let state = oauth::random_state();
    let url = oauth::authorize_url(
        meta.authorization_endpoint.as_deref().unwrap_or_default(),
        &client_id,
        &loopback.redirect_uri,
        &scope,
        &state,
        &resource,
        &challenge,
    )?;

    if no_open {
        println!("{url}");
    } else {
        println!("Opening the browser for Groundzero login…");
        println!("{url}");
        if open::that(&url).is_err() {
            output::warn("could not open a browser; paste the URL above manually");
        }
    }
    let redirect_uri = loopback.redirect_uri.clone();
    let code = loopback.wait_for_code(&state).await?;
    let token = oauth::exchange_code(
        client,
        meta.token_endpoint.as_deref().unwrap_or_default(),
        &code,
        &client_id,
        &redirect_uri,
        &verifier,
        &resource,
    )
    .await?;

    store_token(
        resolved.profile_name.clone(),
        token.access_token,
        token.token_type,
        token.expires_in,
        token.scope.or(Some(scope)),
    )?;
    output::success(&format!("logged in (profile '{}')", resolved.profile_name));
    Ok(())
}

async fn manual_login(
    rt: &Runtime,
    write: bool,
    site: Option<String>,
    username: Option<String>,
    domain: Option<String>,
) -> Result<()> {
    let resolved = rt.http.resolved();
    let auth_host = resolved.auth_host().context(
        "no client configured — run `gz configure` first (it will prompt for client, site, domain)",
    )?;
    warn_unless_secure(&auth_host);
    let site = site
        .or_else(|| resolved.site())
        .context("no site: pass --site or set it via `gz configure --site`")?;
    let username = match username {
        Some(name) => name,
        None => prompt_line("Username: ")?,
    };
    let password = match env_secret("GZ_PASSWORD") {
        Some(secret) => secret,
        None => read_password()?,
    };
    let totp = match env_secret("GZ_TOTP") {
        Some(code) => code,
        None => prompt_line("TOTP code: ")?,
    };
    let login = oauth::ManualLogin {
        site: &site,
        username: username.trim(),
        password: password.trim(),
        totp: totp.trim(),
        domain: domain.as_deref(),
        allow_write: write,
    };
    let token = oauth::manual_token(rt.http.raw(), &auth_host, &login).await?;
    store_token(
        resolved.profile_name.clone(),
        token.token,
        token.token_type,
        token.expires_in,
        token.scope.or_else(|| Some(oauth::scopes(write))),
    )?;
    output::success(&format!("logged in (profile '{}')", resolved.profile_name));
    Ok(())
}

fn warn_unless_secure(auth_host: &str) {
    if !config::is_secure_base(auth_host) {
        output::warn(
            "auth host is plain http — tokens can be intercepted; use https outside localhost",
        );
    }
}

fn env_secret(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn read_password() -> Result<String> {
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() {
        return rpassword::prompt_password("Password: ").context("cannot read password");
    }
    prompt_line("Password (stdin): ")
}

fn prompt_line(label: &str) -> Result<String> {
    use std::io::{BufRead, Write};
    print!("{label}");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .context("cannot read input")?;
    Ok(line.trim().to_string())
}

fn store_token(
    profile: String,
    token: String,
    token_type: Option<String>,
    expires_in: Option<i64>,
    scope: Option<String>,
) -> Result<()> {
    let mut config = load_config()?;
    let entry = config.profile_mut(&profile);
    entry.token = Some(token);
    entry.token_type = token_type.or_else(|| Some("Bearer".to_string()));
    entry.expires_at = expires_in.map(|ttl| config::now_unix() + ttl);
    entry.scope = scope;
    save_config(&config)?;
    Ok(())
}

async fn status(rt: &Runtime) -> Result<()> {
    let resolved = rt.http.resolved();
    let token = resolved.token();
    let value = serde_json::json!({
        "profile": resolved.profile_name,
        "authHost": resolved.auth_host(),
        "site": resolved.site(),
        "authenticated": token.is_some(),
        "tokenSource": resolved.token_source,
        "token": token.as_deref().map(redact_token),
        "tokenType": resolved.profile.token_type,
        "scope": resolved.profile.scope,
        "expiresAt": resolved.profile.expires_at,
        "expired": token.is_some() && resolved.token_expired(),
    });
    rt.show(value).await
}

async fn logout(rt: &Runtime, local_only: bool) -> Result<()> {
    let resolved = rt.http.resolved();
    let name = resolved.profile_name.clone();
    let mut config = load_config()?;
    let had_token = config.profile(&name).token.clone();
    let mut revoked = false;
    if !local_only {
        if let (Some(token), Some(auth_host)) = (had_token.as_deref(), resolved.auth_host()) {
            let fetched = oauth::discover(rt.http.raw(), &auth_host).await;
            let meta = oauth::ServerMetadata::with_defaults(&auth_host, fetched);
            if let Some(endpoint) = meta.revocation_endpoint.as_deref() {
                match oauth::revoke_token(rt.http.raw(), endpoint, token).await {
                    Ok(()) => revoked = true,
                    Err(err) => {
                        output::warn(&format!("revoke failed ({err}); clearing locally anyway"))
                    }
                }
            }
        }
    }
    if let Some(entry) = config.profiles.get_mut(&name) {
        entry.token = None;
        entry.token_type = None;
        entry.expires_at = None;
        entry.scope = None;
    }
    save_config(&config)?;
    if rt.format == OutputFormat::Table {
        output::success(&format!(
            "logged out (profile '{name}'){}",
            if revoked { ", token revoked" } else { "" }
        ));
    } else {
        let value: Value = serde_json::json!({"profile": name, "revoked": revoked});
        rt.show(value).await?;
    }
    Ok(())
}
