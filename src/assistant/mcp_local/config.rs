//! `mcp.toml`: the MCP servers the user runs on this machine for `gz code`.
//!
//! ```toml
//! [servers.files]
//! command = "npx"
//! args = ["-y", "@scope/server"]
//! env = { KEY = "${KEY}" }
//! timeout_seconds = 60
//! ```
//!
//! `${NAME}` in `command`, `args` and `env` values is filled from gz's own
//! environment when the server starts, so the file itself needs no secrets.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// Seconds one request may take when the server sets none.
const DEFAULT_TIMEOUT_SECS: u64 = 60;
const MAX_TIMEOUT_SECS: u64 = 600;
/// The chat server applies the same rule to the names the model sends
/// (gz-chat-eks agent/client_tools.py `LocalMcpServerName`).
const MAX_SERVER_NAME_CHARS: usize = 64;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    servers: BTreeMap<String, ServerSpec>,
}

/// One `[servers.<name>]` table, as written (before `${VAR}` expansion).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerSpec {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Seconds each request may take (1-600; default 60). Starting the
    /// server has its own, longer allowance.
    pub timeout_seconds: Option<u64>,
}

impl ServerSpec {
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(
            self.timeout_seconds
                .unwrap_or(DEFAULT_TIMEOUT_SECS)
                .clamp(1, MAX_TIMEOUT_SECS),
        )
    }

    /// The command line as the user wrote it, with every env value masked:
    /// safe to show, since `${VAR}` references are shown unexpanded.
    pub fn display(&self) -> String {
        let mut words: Vec<String> = self.env.keys().map(|key| format!("{key}=****")).collect();
        words.push(self.command.clone());
        words.extend(self.args.iter().cloned());
        words.join(" ")
    }

    /// What to run, with `${VAR}` filled from `lookup` (gz's environment).
    pub fn expand(&self, lookup: &dyn Fn(&str) -> Option<String>) -> Result<Launch> {
        let fill = |text: &str| expand_vars(text, lookup);
        Ok(Launch {
            program: fill(&self.command)?,
            args: self.args.iter().map(|a| fill(a)).collect::<Result<_>>()?,
            env: self
                .env
                .iter()
                .map(|(key, value)| Ok((key.clone(), fill(value)?)))
                .collect::<Result<_>>()?,
        })
    }
}

/// A server's command line, ready to spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// Every configured server, by name. A missing file is no servers.
pub fn load(path: &Path) -> Result<BTreeMap<String, ServerSpec>> {
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let text =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    parse(&text).with_context(|| format!("{} is not valid", path.display()))
}

fn parse(text: &str) -> Result<BTreeMap<String, ServerSpec>> {
    let file: File = toml::from_str(text)?;
    for (name, spec) in &file.servers {
        check_name(name)?;
        if spec.command.trim().is_empty() {
            bail!("server '{name}' has an empty command");
        }
    }
    Ok(file.servers)
}

/// Whether `name` may name a server: letters, digits, `_` and `-`.
pub fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= MAX_SERVER_NAME_CHARS
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn check_name(name: &str) -> Result<()> {
    if !is_valid_name(name) {
        bail!(
            "server name '{name}' must be 1-{MAX_SERVER_NAME_CHARS} letters, digits, '_' or '-' \
             (rename [servers.{name}])"
        );
    }
    Ok(())
}

/// Replaces each `${NAME}` with its value; any other `$` stays as is.
fn expand_vars(text: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Result<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .with_context(|| format!("'{text}' opens ${{ without closing it"))?;
        let name = &after[..end];
        if !is_var_name(name) {
            bail!("'${{{name}}}' in '{text}' is not a variable name");
        }
        let value = lookup(name).with_context(|| {
            format!("${{{name}}} is not set in gz's environment; set it and start gz again")
        })?;
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
            _ => None,
        }
    }

    #[test]
    fn vars_are_filled_from_the_environment_and_other_dollars_stay() {
        let spec = parse(
            r#"
            [servers.files]
            command = "${HOME_DIR}/bin/server"
            args = ["--token", "${TOKEN}", "cost: $5"]
            env = { API_KEY = "Bearer ${TOKEN}" }
            "#,
        )
        .unwrap()
        .remove("files")
        .unwrap();
        let launch = spec.expand(&lookup).unwrap();
        assert_eq!(launch.program, "/home/ana/bin/server");
        assert_eq!(launch.args, ["--token", "s3cret", "cost: $5"]);
        assert_eq!(
            launch.env,
            [("API_KEY".to_string(), "Bearer s3cret".to_string())]
        );
    }

    #[test]
    fn an_unset_or_malformed_var_says_which() {
        let spec = |arg: &str| ServerSpec {
            command: "srv".into(),
            args: vec![arg.into()],
            env: BTreeMap::new(),
            timeout_seconds: None,
        };
        for (arg, why) in [
            ("${MISSING}", "${MISSING} is not set"),
            ("${TOKEN", "without closing it"),
            ("${1X}", "not a variable name"),
        ] {
            let err = spec(arg).expand(&lookup).unwrap_err();
            assert!(format!("{err:#}").contains(why), "{arg}: {err:#}");
        }
    }

    #[test]
    fn the_display_masks_env_values_and_never_expands() {
        let spec = ServerSpec {
            command: "npx".into(),
            args: vec!["-y".into(), "@scope/server".into(), "${TOKEN}".into()],
            env: BTreeMap::from([("API_KEY".into(), "literal-secret".into())]),
            timeout_seconds: None,
        };
        let shown = spec.display();
        assert_eq!(shown, "API_KEY=**** npx -y @scope/server ${TOKEN}");
        assert!(!shown.contains("literal-secret"));
    }

    #[test]
    fn bad_files_name_the_problem() {
        for (text, why) in [
            (
                "[servers.\"my server\"]\ncommand = \"x\"",
                "server name 'my server'",
            ),
            ("[servers.a]\ncommand = \" \"", "empty command"),
            (
                "[servers.a]\ncommand = \"x\"\nargz = []",
                "unknown field `argz`",
            ),
            ("[servers.a]\nargs = []", "missing field `command`"),
            ("[servers.a", "expected"),
        ] {
            let err = parse(text).unwrap_err();
            assert!(format!("{err:#}").contains(why), "{text}: {err:#}");
        }
    }

    #[test]
    fn timeouts_default_and_stay_in_bounds() {
        let mut spec = parse("[servers.a]\ncommand = \"x\"")
            .unwrap()
            .remove("a")
            .unwrap();
        assert_eq!(spec.timeout(), Duration::from_secs(60));
        spec.timeout_seconds = Some(0);
        assert_eq!(spec.timeout(), Duration::from_secs(1));
        spec.timeout_seconds = Some(10_000);
        assert_eq!(spec.timeout(), Duration::from_secs(600));
    }
}
