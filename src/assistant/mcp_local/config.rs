//! One MCP server's settings, as written in any of the config scopes
//! (see [`super::scopes`]). The user file is TOML:
//!
//! ```toml
//! [servers.files]
//! command = "npx"
//! args = ["-y", "@scope/server"]
//! env = { KEY = "${KEY}" }
//! timeout_seconds = 60
//!
//! [servers.tracker]
//! url = "https://tracker.example.com/mcp"
//! headers = { "X-Api-Key" = "${TRACKER_KEY}" }
//! ```
//!
//! and the project and local files are JSON with the same fields under
//! `mcpServers`. `${NAME}` and `${NAME:-default}` in `command`, `args`,
//! `env`, `url`, `headers` and the OAuth client settings are filled from
//! gz's own environment when the server starts, so the files need no
//! secrets.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Seconds one request may take when the server sets none.
const DEFAULT_TIMEOUT_SECS: u64 = 60;
const MAX_TIMEOUT_SECS: u64 = 600;
/// Starting can mean `npx` downloading the server first.
const DEFAULT_STARTUP_SECS: u64 = 120;
/// The chat server applies the same rule to the names the model sends
/// (gz-chat-eks agent/client_tools.py `LocalMcpServerName`).
const MAX_SERVER_NAME_CHARS: usize = 64;
/// The smallest output cap a server may set: below it, error messages and
/// the truncation note itself would not fit.
const MIN_OUTPUT_CHARS: usize = 1_000;

/// How gz talks to a server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportKind {
    /// A local process, JSON-RPC over its stdin and stdout.
    Stdio,
    /// Streamable HTTP: one endpoint, POST per message, JSON or SSE replies.
    #[serde(
        alias = "streamable-http",
        alias = "streamableHttp",
        alias = "streamable_http"
    )]
    Http,
    /// The older HTTP+SSE transport: a GET event stream plus a POST endpoint.
    Sse,
}

impl TransportKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TransportKind::Stdio => "stdio",
            TransportKind::Http => "http",
            TransportKind::Sse => "sse",
        }
    }
}

/// OAuth client settings for a remote server; all optional, since gz
/// discovers the authorization server and registers itself when it can.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OAuthSpec {
    /// A client id registered by hand, for servers without dynamic
    /// client registration.
    #[serde(default, alias = "clientId", skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Its secret, if it has one; use `${VAR}` rather than the secret itself.
    #[serde(
        default,
        alias = "clientSecret",
        skip_serializing_if = "Option::is_none"
    )]
    pub client_secret: Option<String>,
    /// Space-separated scopes to ask for; default: what the server advertises.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes: Option<String>,
    /// A fixed port for the localhost redirect, when the client id was
    /// registered with one; default: any free port.
    #[serde(
        default,
        alias = "callbackPort",
        skip_serializing_if = "Option::is_none"
    )]
    pub callback_port: Option<u16>,
}

/// One server's table, as written (before `${VAR}` expansion).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerSpec {
    /// `stdio`, `http` or `sse`; default: `http` with a `url`, else `stdio`.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<TransportKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Sent with every HTTP request to the server.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// Seconds each request may take (1-600; default 60).
    #[serde(
        default,
        alias = "timeoutSeconds",
        skip_serializing_if = "Option::is_none"
    )]
    pub timeout_seconds: Option<u64>,
    /// Seconds starting may take (1-600; default 120, or `timeout_seconds`
    /// when that is longer).
    #[serde(
        default,
        alias = "startupTimeoutSeconds",
        skip_serializing_if = "Option::is_none"
    )]
    pub startup_timeout_seconds: Option<u64>,
    /// Most characters of one tool result the model gets (at least 1,000;
    /// default and at most gz's own cap, 40,000).
    #[serde(
        default,
        alias = "maxOutputChars",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_output_chars: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<OAuthSpec>,
}

impl ServerSpec {
    /// The transport, stated or implied by `url` / `command`.
    pub fn kind(&self) -> TransportKind {
        self.transport.unwrap_or(if self.url.is_some() {
            TransportKind::Http
        } else {
            TransportKind::Stdio
        })
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_secs(
            self.timeout_seconds
                .unwrap_or(DEFAULT_TIMEOUT_SECS)
                .clamp(1, MAX_TIMEOUT_SECS),
        )
    }

    /// How long starting (and `initialize`) may take.
    pub fn startup_timeout(&self) -> Duration {
        let stated = self
            .startup_timeout_seconds
            .map(|secs| Duration::from_secs(secs.clamp(1, MAX_TIMEOUT_SECS)));
        stated.unwrap_or_else(|| Duration::from_secs(DEFAULT_STARTUP_SECS).max(self.timeout()))
    }

    /// Most characters of one tool result the server's entry allows, if it
    /// sets a limit. gz's own byte cap on every tool result applies on top.
    pub fn output_limit(&self) -> Option<usize> {
        self.max_output_chars
            .map(|chars| chars.max(MIN_OUTPUT_CHARS))
    }

    /// What the server is, as the user wrote it, with every env and header
    /// value masked: safe to show, since `${VAR}` references stay unexpanded.
    pub fn display(&self) -> String {
        match self.kind() {
            TransportKind::Stdio => {
                let mut words: Vec<String> =
                    self.env.keys().map(|key| format!("{key}=****")).collect();
                words.extend(self.command.iter().cloned());
                words.extend(self.args.iter().cloned());
                words.join(" ")
            }
            kind => {
                let mut words = vec![
                    masked_query(self.url.as_deref().unwrap_or_default()),
                    format!("({})", kind.as_str()),
                ];
                words.extend(self.headers.keys().map(|key| format!("{key}: ****")));
                words.join(" ")
            }
        }
    }

    /// Checks the fields fit together; `name` is only for the message.
    pub fn validate(&self, name: &str) -> Result<()> {
        check_name(name)?;
        let blank = |text: &Option<String>| text.as_deref().is_none_or(|t| t.trim().is_empty());
        match self.kind() {
            TransportKind::Stdio => {
                if blank(&self.command) {
                    bail!("server '{name}' needs a command (or a url for a remote server)");
                }
                if self.url.is_some() || !self.headers.is_empty() || self.oauth.is_some() {
                    bail!(
                        "server '{name}' runs a command over stdio, so url, headers and oauth \
                         don't apply; remove them or set type = \"http\""
                    );
                }
            }
            kind => {
                if blank(&self.url) {
                    bail!(
                        "server '{name}' has type = \"{}\" but no url",
                        kind.as_str()
                    );
                }
                if self.command.is_some() || !self.args.is_empty() || !self.env.is_empty() {
                    bail!(
                        "server '{name}' is remote ({}), so command, args and env don't apply; \
                         use headers for credentials",
                        kind.as_str()
                    );
                }
            }
        }
        Ok(())
    }

    /// What to start, with `${VAR}` filled from `lookup` (gz's environment).
    pub fn expand(&self, lookup: &dyn Fn(&str) -> Option<String>) -> Result<Target> {
        let fill = |text: &str| expand_vars(text, lookup);
        Ok(match self.kind() {
            TransportKind::Stdio => Target::Stdio(Launch {
                program: fill(self.command.as_deref().unwrap_or_default())?,
                args: self.args.iter().map(|a| fill(a)).collect::<Result<_>>()?,
                env: fill_pairs(&self.env, &fill)?,
            }),
            kind => {
                let url = fill(self.url.as_deref().unwrap_or_default())?;
                let parsed = url::Url::parse(&url)
                    .with_context(|| format!("url '{url}' is not a valid URL"))?;
                if !matches!(parsed.scheme(), "http" | "https") {
                    bail!("url '{url}' must start with https:// (or http:// for a local server)");
                }
                let oauth = match &self.oauth {
                    Some(oauth) => OAuthSettings {
                        client_id: oauth.client_id.as_deref().map(fill).transpose()?,
                        client_secret: oauth.client_secret.as_deref().map(fill).transpose()?,
                        scopes: oauth.scopes.as_deref().map(fill).transpose()?,
                        callback_port: oauth.callback_port,
                    },
                    None => OAuthSettings::default(),
                };
                Target::Remote(Remote {
                    url,
                    sse: kind == TransportKind::Sse,
                    headers: fill_pairs(&self.headers, &fill)?,
                    oauth,
                })
            }
        })
    }
}

/// The URL with its query masked: some servers take an API key there.
fn masked_query(url: &str) -> String {
    match url.split_once('?') {
        Some((base, _)) => format!("{base}?****"),
        None => url.to_string(),
    }
}

fn fill_pairs(
    pairs: &BTreeMap<String, String>,
    fill: &dyn Fn(&str) -> Result<String>,
) -> Result<Vec<(String, String)>> {
    pairs
        .iter()
        .map(|(key, value)| Ok((key.clone(), fill(value)?)))
        .collect()
}

/// A server ready to connect to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Stdio(Launch),
    Remote(Remote),
}

/// A server's command line, ready to spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// A remote server, its variables filled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    pub url: String,
    /// The older HTTP+SSE transport rather than Streamable HTTP.
    pub sse: bool,
    pub headers: Vec<(String, String)>,
    pub oauth: OAuthSettings,
}

/// [`OAuthSpec`] with its variables filled.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OAuthSettings {
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub scopes: Option<String>,
    pub callback_port: Option<u16>,
}

/// Whether `name` may name a server: letters, digits, `_` and `-`.
pub fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= MAX_SERVER_NAME_CHARS
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

pub fn check_name(name: &str) -> Result<()> {
    if !is_valid_name(name) {
        bail!("server name '{name}' must be 1-{MAX_SERVER_NAME_CHARS} letters, digits, '_' or '-'");
    }
    Ok(())
}

/// Replaces each `${NAME}` with its value and each `${NAME:-default}` with
/// its value or, when unset or empty, the default; any other `$` stays.
pub fn expand_vars(text: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Result<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .with_context(|| format!("'{text}' opens ${{ without closing it"))?;
        let body = &after[..end];
        let (name, default) = match body.split_once(":-") {
            Some((name, default)) => (name, Some(default)),
            None => (body, None),
        };
        if !is_var_name(name) {
            bail!("'${{{body}}}' in '{text}' is not a variable name");
        }
        let value = match (lookup(name).filter(|v| !v.is_empty()), default) {
            (Some(value), _) => value,
            (None, Some(default)) => default.to_string(),
            (None, None) => lookup(name).with_context(|| {
                format!(
                    "${{{name}}} is not set in gz's environment; set it and start gz again \
                     (or write ${{{name}:-default}})"
                )
            })?,
        };
        out.push_str(&value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn is_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(name: &str) -> Option<String> {
        match name {
            "TOKEN" => Some("s3cret".into()),
            "HOME_DIR" => Some("/home/ana".into()),
            "EMPTY" => Some(String::new()),
            _ => None,
        }
    }

    fn stdio(arg: &str) -> ServerSpec {
        ServerSpec {
            command: Some("srv".into()),
            args: vec![arg.into()],
            ..ServerSpec::default()
        }
    }

    fn parse(text: &str) -> ServerSpec {
        let spec: ServerSpec = toml::from_str(text).unwrap();
        spec.validate("s").unwrap();
        spec
    }

    #[test]
    fn vars_are_filled_from_the_environment_and_other_dollars_stay() {
        let spec = parse(
            r#"
            command = "${HOME_DIR}/bin/server"
            args = ["--token", "${TOKEN}", "cost: $5"]
            env = { API_KEY = "Bearer ${TOKEN}" }
            "#,
        );
        let Target::Stdio(launch) = spec.expand(&lookup).unwrap() else {
            panic!("stdio expected")
        };
        assert_eq!(launch.program, "/home/ana/bin/server");
        assert_eq!(launch.args, ["--token", "s3cret", "cost: $5"]);
        assert_eq!(
            launch.env,
            [("API_KEY".to_string(), "Bearer s3cret".to_string())]
        );
    }

    #[test]
    fn a_default_fills_an_unset_or_empty_var() {
        let fill = |text: &str| expand_vars(text, &lookup).unwrap();
        assert_eq!(fill("${MISSING:-8080}"), "8080");
        assert_eq!(fill("${EMPTY:-x}"), "x");
        assert_eq!(fill("${TOKEN:-unused}"), "s3cret");
        assert_eq!(fill("a${MISSING:-}b"), "ab");
        assert_eq!(fill("${EMPTY}"), "", "a set but empty var is still set");
    }

    #[test]
    fn an_unset_or_malformed_var_says_which() {
        for (arg, why) in [
            ("${MISSING}", "${MISSING} is not set"),
            ("${TOKEN", "without closing it"),
            ("${1X}", "not a variable name"),
            ("${1X:-a}", "not a variable name"),
        ] {
            let err = stdio(arg).expand(&lookup).unwrap_err();
            assert!(format!("{err:#}").contains(why), "{arg}: {err:#}");
        }
    }

    #[test]
    fn a_remote_server_gets_its_url_and_headers_filled() {
        let spec = parse(
            r#"
            url = "https://${HOST:-mcp.example.com}/mcp"
            headers = { Authorization = "Bearer ${TOKEN}" }
            oauth = { client_id = "gz-${TOKEN}", scopes = "read" }
            "#,
        );
        assert_eq!(spec.kind(), TransportKind::Http);
        let Target::Remote(remote) = spec.expand(&lookup).unwrap() else {
            panic!("remote expected")
        };
        assert_eq!(remote.url, "https://mcp.example.com/mcp");
        assert!(!remote.sse);
        assert_eq!(
            remote.headers,
            [("Authorization".to_string(), "Bearer s3cret".to_string())]
        );
        assert_eq!(remote.oauth.client_id.as_deref(), Some("gz-s3cret"));
        let sse = parse("type = \"sse\"\nurl = \"http://127.0.0.1:9/sse\"");
        let Target::Remote(remote) = sse.expand(&lookup).unwrap() else {
            panic!("remote expected")
        };
        assert!(remote.sse);
        let json: ServerSpec =
            serde_json::from_str(r#"{"type": "streamable-http", "url": "https://x/mcp"}"#).unwrap();
        assert_eq!(json.kind(), TransportKind::Http);
    }

    #[test]
    fn the_display_masks_env_and_header_values_and_never_expands() {
        let spec = ServerSpec {
            command: Some("npx".into()),
            args: vec!["-y".into(), "@scope/server".into(), "${TOKEN}".into()],
            env: BTreeMap::from([("API_KEY".into(), "literal-secret".into())]),
            ..ServerSpec::default()
        };
        let shown = spec.display();
        assert_eq!(shown, "API_KEY=**** npx -y @scope/server ${TOKEN}");
        assert!(!shown.contains("literal-secret"));
        let remote = ServerSpec {
            url: Some("https://x/mcp".into()),
            headers: BTreeMap::from([("X-Key".into(), "literal-secret".into())]),
            ..ServerSpec::default()
        };
        assert_eq!(remote.display(), "https://x/mcp (http) X-Key: ****");
        let keyed = ServerSpec {
            url: Some("https://x/mcp?key=literal-secret".into()),
            ..ServerSpec::default()
        };
        assert_eq!(keyed.display(), "https://x/mcp?**** (http)");
    }

    #[test]
    fn mismatched_fields_name_the_problem() {
        for (text, why) in [
            ("command = \" \"", "needs a command"),
            ("args = []", "needs a command"),
            (
                "command = \"x\"\nurl = \"https://x\"\ntype = \"stdio\"",
                "url, headers and oauth don't apply",
            ),
            ("type = \"http\"", "no url"),
            (
                "url = \"https://x\"\ncommand = \"x\"",
                "command, args and env don't apply",
            ),
        ] {
            let spec: ServerSpec = toml::from_str(text).unwrap();
            let err = spec.validate("s").unwrap_err();
            assert!(format!("{err:#}").contains(why), "{text}: {err:#}");
        }
        let err = toml::from_str::<ServerSpec>("command = \"x\"\nargz = []").unwrap_err();
        assert!(err.to_string().contains("unknown field `argz`"), "{err}");
        let bad_url = parse("url = \"ftp://x\"").expand(&lookup).unwrap_err();
        assert!(format!("{bad_url:#}").contains("https://"), "{bad_url:#}");
        assert!(ServerSpec::default().validate("my server").is_err());
    }

    #[test]
    fn timeouts_and_output_limits_default_and_stay_in_bounds() {
        let mut spec = parse("command = \"x\"");
        assert_eq!(spec.timeout(), Duration::from_secs(60));
        assert_eq!(spec.startup_timeout(), Duration::from_secs(120));
        spec.timeout_seconds = Some(0);
        assert_eq!(spec.timeout(), Duration::from_secs(1));
        spec.timeout_seconds = Some(10_000);
        assert_eq!(spec.timeout(), Duration::from_secs(600));
        assert_eq!(spec.startup_timeout(), Duration::from_secs(600));
        spec.startup_timeout_seconds = Some(5);
        assert_eq!(spec.startup_timeout(), Duration::from_secs(5));

        assert_eq!(spec.output_limit(), None);
        spec.max_output_chars = Some(5_000);
        assert_eq!(spec.output_limit(), Some(5_000));
        spec.max_output_chars = Some(10);
        assert_eq!(spec.output_limit(), Some(MIN_OUTPUT_CHARS));
        spec.max_output_chars = Some(1_000_000);
        assert_eq!(spec.output_limit(), Some(1_000_000));
    }
}
