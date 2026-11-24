//! `gz mcp add | remove | list | get | login | logout | approve`: the MCP
//! servers `gz code` uses, in the user, project and local scopes (see
//! [`crate::assistant::mcp_local::scopes`]). Nothing here needs a gz
//! sign-in; the project is the current folder.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::assistant::mcp_local::config::{OAuthSpec, ServerSpec, Target, TransportKind};
use crate::assistant::mcp_local::http;
use crate::assistant::mcp_local::oauth::{self, Browser, LoginState};
use crate::assistant::mcp_local::scopes::{self, Approval, Entry, Places, Scope};
use crate::assistant::mcp_local::{LocalMcp, Phase};
use crate::cli::{McpAction, McpAddArgs, McpScope, McpTransport};
use crate::cmd::Runtime;
use crate::output::{self, OutputFormat};

/// How long `list` and `get` wait for a server to connect.
const CHECK_WAIT: Duration = Duration::from_secs(15);
/// How long each request of a sign-in may take (not the browser step).
const LOGIN_REQUEST_WAIT: Duration = Duration::from_secs(30);

pub async fn run(rt: &Runtime, action: &McpAction) -> Result<()> {
    let places = Places::for_project(Some(&std::env::current_dir()?))?;
    match action {
        McpAction::Add(args) => add(&places, args),
        McpAction::Remove { name, scope } => {
            let (scope, path) = scopes::remove(&places, scope.map(scope_of), name)?;
            output::success(&format!(
                "Removed {name} from the {} scope ({})",
                scope.as_str(),
                path.display()
            ));
            Ok(())
        }
        McpAction::List => list(rt, places).await,
        McpAction::Get { name } => get(rt, places, name).await,
        McpAction::Login { name, no_browser } => login(&places, name, *no_browser).await,
        McpAction::Logout { name } => logout(&places, name),
        McpAction::Approve { name, decline } => approve(&places, name, !*decline),
        McpAction::Setup { .. } | McpAction::Url => {
            unreachable!("gz mcp setup and url are handled in cmd::mcp")
        }
    }
}

fn scope_of(scope: McpScope) -> Scope {
    match scope {
        McpScope::User => Scope::User,
        McpScope::Project => Scope::Project,
        McpScope::Local => Scope::Local,
    }
}

/// The server spec `gz mcp add` writes, from its flags.
pub fn spec_from_args(args: &McpAddArgs) -> Result<ServerSpec> {
    let first = args.target.first().context("give a command or a URL")?;
    let looks_remote = first.starts_with("http://") || first.starts_with("https://");
    let kind = match args.transport {
        Some(McpTransport::Stdio) => TransportKind::Stdio,
        Some(McpTransport::Http) => TransportKind::Http,
        Some(McpTransport::Sse) => TransportKind::Sse,
        None if looks_remote => TransportKind::Http,
        None => TransportKind::Stdio,
    };
    let mut spec = ServerSpec {
        timeout_seconds: args.request_timeout,
        startup_timeout_seconds: args.startup_timeout,
        max_output_chars: args.max_output_chars,
        ..ServerSpec::default()
    };
    if kind == TransportKind::Stdio {
        spec.command = Some(first.clone());
        spec.args = args.target[1..].to_vec();
        spec.env = pairs(&args.env, '=', "--env", "KEY=VALUE")?;
        if !args.headers.is_empty() || has_oauth_flags(args) {
            bail!("--header and the OAuth flags are for remote servers; give an http(s) URL");
        }
        return Ok(spec);
    }
    if args.target.len() > 1 {
        bail!(
            "a remote server takes one URL, not arguments ({}); put options before the URL",
            args.target[1..].join(" ")
        );
    }
    if !args.env.is_empty() {
        bail!("--env is for stdio servers; send credentials to a remote server with --header");
    }
    spec.transport = (kind == TransportKind::Sse).then_some(kind);
    spec.url = Some(first.clone());
    spec.headers = pairs(&args.headers, ':', "--header", "NAME: VALUE")?;
    if has_oauth_flags(args) {
        spec.oauth = Some(OAuthSpec {
            client_id: args.client_id.clone(),
            client_secret: None,
            scopes: args.oauth_scopes.clone(),
            callback_port: args.callback_port,
        });
    }
    Ok(spec)
}

fn has_oauth_flags(args: &McpAddArgs) -> bool {
    args.client_id.is_some() || args.oauth_scopes.is_some() || args.callback_port.is_some()
}

fn pairs(
    items: &[String],
    separator: char,
    flag: &str,
    shape: &str,
) -> Result<BTreeMap<String, String>> {
    items
        .iter()
        .map(|item| {
            let (key, value) = item
                .split_once(separator)
                .with_context(|| format!("{flag} '{item}' should look like {shape}"))?;
            let key = key.trim();
            if key.is_empty() {
                bail!("{flag} '{item}' has no name before '{separator}'");
            }
            Ok((key.to_string(), value.trim().to_string()))
        })
        .collect()
}

fn add(places: &Places, args: &McpAddArgs) -> Result<()> {
    let spec = spec_from_args(args)?;
    let scope = scope_of(args.scope);
    let path = scopes::add(places, scope, &args.name, &spec)?;
    output::success(&format!(
        "Added {} ({}) to the {} scope in {}",
        args.name,
        spec.kind().as_str(),
        scope.as_str(),
        path.display()
    ));
    if scope == Scope::Project {
        output::info(&format!(
            "Project servers run only once approved: `gz mcp approve {}` or /mcp approve {} in gz code",
            args.name, args.name
        ));
    }
    if spec.kind() != TransportKind::Stdio {
        output::info(&format!(
            "If it needs a sign-in, `gz mcp login {}` opens it",
            args.name
        ));
    }
    Ok(())
}

/// One row of `list` / `get`.
async fn describe(servers: &LocalMcp, places: &Places, entry: &Entry) -> Value {
    let mut row = json!({
        "name": entry.name,
        "scope": entry.scope.as_str(),
        "transport": entry.spec.kind().as_str(),
        "target": entry.spec.display(),
        "file": entry.source.display().to_string(),
    });
    row["status"] = json!(if entry.shadowed {
        "not used: a higher scope defines the same name".to_string()
    } else {
        status(servers, &entry.name).await
    });
    if let Some(login) = login_label(places, entry) {
        row["login"] = json!(login);
    }
    row
}

/// Connects (without waiting past [`CHECK_WAIT`]) and says how it went.
async fn status(servers: &LocalMcp, name: &str) -> String {
    let phase = servers
        .statuses()
        .into_iter()
        .find(|s| s.name == name)
        .map(|s| s.phase);
    match phase {
        Some(Phase::NeedsApproval) => {
            return format!("needs approval (gz mcp approve {name})");
        }
        Some(Phase::Declined) => return "declined".to_string(),
        _ => {}
    }
    match tokio::time::timeout(CHECK_WAIT, servers.tools(name)).await {
        Ok(Ok(tools)) => format!("connected · {} tool(s)", tools.len()),
        Err(_) => format!("no answer within {}s", CHECK_WAIT.as_secs()),
        Ok(Err(err)) => {
            let needs_login = servers
                .statuses()
                .iter()
                .any(|s| s.name == name && matches!(s.phase, Phase::NeedsLogin(_)));
            if needs_login {
                format!("needs login (gz mcp login {name})")
            } else {
                format!("failed: {err:#}")
            }
        }
    }
}

/// The sign-in, when there is one to speak of; a server that needs one
/// says so in its status instead.
fn login_label(places: &Places, entry: &Entry) -> Option<String> {
    let Ok(Target::Remote(remote)) = entry.spec.expand(&|var| std::env::var(var).ok()) else {
        return None;
    };
    let label = match places.auth().state(&entry.name, &remote.url) {
        Ok(LoginState::SignedOut) => return None,
        Ok(LoginState::SignedIn {
            expires_at,
            can_refresh,
        }) => match (expires_at, can_refresh) {
            (Some(at), true) if at <= crate::config::now_unix() => {
                "signed in (expired; refreshes on use)".to_string()
            }
            (Some(at), false) if at <= crate::config::now_unix() => {
                format!("expired; run gz mcp login {}", entry.name)
            }
            _ => "signed in".to_string(),
        },
        Err(err) => format!("{err:#}"),
    };
    Some(label)
}

async fn list(rt: &Runtime, places: Places) -> Result<()> {
    let loaded = scopes::load(&places);
    for why in &loaded.problems {
        output::warn(why);
    }
    if loaded.entries.is_empty() {
        if rt.format == OutputFormat::Table {
            output::info("No MCP servers yet; add one with `gz mcp add <name> -- <command>`");
            return Ok(());
        }
        return rt.show(json!([])).await;
    }
    let servers = LocalMcp::from_places(places.clone());
    // Every check at once: one slow server doesn't hold up the rest.
    let checks: Vec<_> = loaded
        .entries
        .into_iter()
        .map(|entry| {
            let (servers, places) = (servers.clone(), places.clone());
            tokio::spawn(async move { describe(&servers, &places, &entry).await })
        })
        .collect();
    let mut rows = Vec::with_capacity(checks.len());
    for check in checks {
        rows.push(check.await.context("a server check stopped unexpectedly")?);
    }
    rt.show_columns(
        &Value::Array(rows),
        &["name", "scope", "transport", "target", "status", "login"],
    )
}

async fn get(rt: &Runtime, places: Places, name: &str) -> Result<()> {
    let loaded = scopes::load(&places);
    let entry = loaded.find(name).cloned().with_context(|| {
        format!("there is no MCP server '{name}'; `gz mcp list` shows them all")
    })?;
    let servers = LocalMcp::from_places(places.clone());
    let mut row = describe(&servers, &places, &entry).await;
    row["settings"] = masked_settings(&entry.spec);
    if entry.scope == Scope::Project {
        row["approval"] = json!(match entry.approval {
            Approval::Approved => "approved",
            Approval::Declined => "declined",
            Approval::Pending | Approval::NotNeeded => "pending",
        });
    }
    if let Ok(tools) = servers.tools(name).await {
        row["tools"] = json!(tools.iter().map(|t| t.name.clone()).collect::<Vec<_>>());
    }
    rt.show(row).await
}

/// The settings as written, with env and header values masked (they may
/// be literal secrets) and `${VAR}` references left as they are.
fn masked_settings(spec: &ServerSpec) -> Value {
    let mut shown = spec.clone();
    for value in shown.env.values_mut().chain(shown.headers.values_mut()) {
        *value = "****".to_string();
    }
    if let Some(oauth) = shown.oauth.as_mut() {
        if oauth.client_secret.is_some() {
            oauth.client_secret = Some("****".to_string());
        }
    }
    json!(shown)
}

/// Prints the sign-in URL on stderr and opens the browser unless told not to.
struct Terminal {
    no_browser: bool,
}

impl Browser for Terminal {
    fn show(&mut self, line: &str) {
        eprintln!("{line}");
    }

    fn open(&mut self, url: &str) -> bool {
        self.no_browser || open::that(url).is_ok()
    }
}

async fn login(places: &Places, name: &str, no_browser: bool) -> Result<()> {
    let entry = remote_entry(places, name)?;
    let Target::Remote(remote) = entry.spec.expand(&|var| std::env::var(var).ok())? else {
        unreachable!("remote_entry returns remote servers only")
    };
    let http = http::client(Some(LOGIN_REQUEST_WAIT)).context("cannot build an HTTP client")?;
    let mut terminal = Terminal { no_browser };
    oauth::login(&http, &places.auth(), name, &remote, &mut terminal).await?;
    output::success(&format!(
        "Signed in to {name}; in a running gz code, /mcp reconnect {name}"
    ));
    Ok(())
}

fn remote_entry(places: &Places, name: &str) -> Result<Entry> {
    let loaded = scopes::load(places);
    let entry = loaded.find(name).cloned().with_context(|| {
        format!("there is no MCP server '{name}'; `gz mcp list` shows them all")
    })?;
    if entry.spec.kind() == TransportKind::Stdio {
        bail!("{name} runs locally over stdio, so it has no sign-in");
    }
    check_approved(&entry)?;
    Ok(entry)
}

/// A project server is contacted only once the user approved it: its
/// URL and headers (with `${VAR}` filled from the user's environment)
/// come from a checked-in file.
fn check_approved(entry: &Entry) -> Result<()> {
    let name = &entry.name;
    match entry.approval {
        Approval::NotNeeded | Approval::Approved => Ok(()),
        Approval::Pending => bail!(
            "{name} is a project server (from {}) you haven't approved yet, so gz won't \
             contact it; review it with `gz mcp get {name}`, then `gz mcp approve {name}`",
            entry.source.display()
        ),
        Approval::Declined => bail!(
            "you declined the project server {name}, so gz won't contact it; \
             `gz mcp approve {name}` turns it on"
        ),
    }
}

fn logout(places: &Places, name: &str) -> Result<()> {
    if places.auth().remove(name)? {
        output::success(&format!("Forgot the sign-in for {name}"));
    } else {
        output::info(&format!("{name} had no sign-in"));
    }
    Ok(())
}

fn approve(places: &Places, name: &str, approved: bool) -> Result<()> {
    let loaded = scopes::load(places);
    let entry = loaded.find(name).with_context(|| {
        format!("there is no MCP server '{name}'; `gz mcp list` shows them all")
    })?;
    if entry.scope == Scope::Project {
        eprintln!(
            "{} from {}:\n  {}",
            entry.name,
            entry.source.display(),
            serde_json::to_string(&entry.spec)?
        );
    }
    scopes::approve(places, entry, approved)?;
    if approved {
        output::success(&format!(
            "Approved {name}; gz asks again if its entry changes"
        ));
    } else {
        output::info(&format!(
            "{name} stays off; `gz mcp approve {name}` turns it on"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parsed(line: &[&str]) -> McpAddArgs {
        let mut argv = vec!["gz", "mcp", "add"];
        argv.extend_from_slice(line);
        let cli = crate::cli::Cli::try_parse_from(argv).unwrap();
        match cli.command {
            crate::cli::Command::Mcp(args) => match args.action {
                McpAction::Add(add) => *add,
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_command_becomes_a_stdio_server_with_its_flags() {
        let args = parsed(&[
            "files",
            "-e",
            "TOKEN=${TOKEN}",
            "--request-timeout",
            "90",
            "--",
            "npx",
            "-y",
            "@scope/server",
        ]);
        let spec = spec_from_args(&args).unwrap();
        assert_eq!(spec.kind(), TransportKind::Stdio);
        assert_eq!(spec.command.as_deref(), Some("npx"));
        assert_eq!(spec.args, ["-y", "@scope/server"]);
        assert_eq!(spec.env["TOKEN"], "${TOKEN}");
        assert_eq!(spec.timeout_seconds, Some(90));
        assert_eq!(args.scope, McpScope::Local);
    }

    #[test]
    fn a_url_becomes_a_remote_server_with_headers() {
        let args = parsed(&[
            "tracker",
            "--scope",
            "user",
            "-H",
            "X-Api-Key: ${KEY}",
            "--client-id",
            "abc",
            "https://t.example.com/mcp",
        ]);
        let spec = spec_from_args(&args).unwrap();
        assert_eq!(spec.kind(), TransportKind::Http);
        assert_eq!(spec.transport, None, "http is implied by the url");
        assert_eq!(spec.headers["X-Api-Key"], "${KEY}");
        assert_eq!(spec.oauth.unwrap().client_id.as_deref(), Some("abc"));
        let sse = spec_from_args(&parsed(&["old", "-t", "sse", "http://127.0.0.1:9/sse"])).unwrap();
        assert_eq!(sse.kind(), TransportKind::Sse);
    }

    #[test]
    fn flags_that_dont_fit_the_transport_say_so() {
        for (line, why) in [
            (vec!["a", "-H", "X: y", "--", "npx"], "remote servers"),
            (
                vec!["a", "-e", "A=1", "https://x/mcp"],
                "--env is for stdio",
            ),
            (vec!["a", "-H", "novalue", "https://x/mcp"], "NAME: VALUE"),
            (vec!["a", "https://x/mcp", "extra"], "one URL"),
        ] {
            let err = spec_from_args(&parsed(&line)).unwrap_err();
            assert!(format!("{err:#}").contains(why), "{line:?}: {err:#}");
        }
    }

    #[test]
    fn get_masks_env_header_and_secret_values() {
        let spec = ServerSpec {
            url: Some("https://x/mcp".into()),
            headers: BTreeMap::from([("Authorization".into(), "Bearer literal".into())]),
            oauth: Some(OAuthSpec {
                client_secret: Some("shh".into()),
                ..OAuthSpec::default()
            }),
            ..ServerSpec::default()
        };
        let shown = masked_settings(&spec).to_string();
        assert!(
            !shown.contains("literal") && !shown.contains("shh"),
            "{shown}"
        );
        assert!(shown.contains("Authorization"), "{shown}");
    }

    #[test]
    fn the_login_column_shows_a_sign_in_and_whether_it_still_works() {
        use crate::assistant::mcp_local::oauth::Saved;
        let dir = std::env::temp_dir().join(format!("gz-mcp-login-label-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let places = Places::at(dir.clone(), None);
        let entry = Entry {
            name: "docs".into(),
            spec: ServerSpec {
                url: Some("https://docs.test/mcp".into()),
                ..ServerSpec::default()
            },
            scope: Scope::User,
            source: dir.join("mcp.toml"),
            approval: Approval::NotNeeded,
            shadowed: false,
        };
        assert_eq!(login_label(&places, &entry), None);
        let saved = |refresh: Option<&str>, expires_at: i64| Saved {
            url: "https://docs.test/mcp".into(),
            access_token: "t".into(),
            refresh_token: refresh.map(str::to_string),
            expires_at: Some(expires_at),
            ..Saved::default()
        };
        let now = crate::config::now_unix();
        places
            .auth()
            .put("docs", saved(Some("r"), now + 600))
            .unwrap();
        assert_eq!(login_label(&places, &entry).as_deref(), Some("signed in"));
        places
            .auth()
            .put("docs", saved(Some("r"), now - 1))
            .unwrap();
        assert_eq!(
            login_label(&places, &entry).as_deref(),
            Some("signed in (expired; refreshes on use)")
        );
        places.auth().put("docs", saved(None, now - 1)).unwrap();
        assert_eq!(
            login_label(&places, &entry).as_deref(),
            Some("expired; run gz mcp login docs")
        );
    }
}
