use anyhow::{Context, Result};
use reqwest::Method;
use serde_json::Value;
use std::collections::BTreeMap;

use crate::cli::{BodyArgs, Command};
use crate::http::ApiClient;
use crate::output::{self, OutputFormat};

pub mod admin;
pub mod agents;
pub mod approvals;
pub mod apps;
pub mod auth;
pub mod code;
pub mod code_report;
pub mod code_unattended;
pub mod data_branch;
pub mod datafs;
pub mod devloop;
pub mod explain;
pub mod flow_run;
pub mod flows;
pub mod impact;
pub mod job_fix;
pub mod jobs;
pub mod lakehouse;
pub mod mcp;
pub mod mlops;
pub mod model_access;
pub mod ops;
pub mod search;
pub mod skills;
pub mod stream;
pub mod sub_agents;
pub mod table_history;
pub mod update;
pub mod usage;

pub struct Runtime {
    pub http: ApiClient,
    pub format: OutputFormat,
    pub limit: Option<usize>,
    pub quiet: bool,
    /// The user's own `-q`; `quiet` is also forced on for commands that
    /// draw their own progress.
    pub asked_quiet: bool,
}

impl Runtime {
    pub async fn show(&self, value: Value) -> Result<()> {
        output::print_value(&value, self.format, self.limit)
    }

    /// Like [`Self::show`], with the table narrowed to the useful columns.
    pub fn show_columns(&self, value: &Value, columns: &[&str]) -> Result<()> {
        output::print_with_columns(value, self.format, self.limit, columns)
    }

    pub async fn call(
        &self,
        service: &str,
        method: Method,
        path: &str,
        body: &BodyArgs,
        extra: Option<Value>,
    ) -> Result<()> {
        let value = self.fetch(service, method, path, body, extra).await?;
        self.show(value).await
    }

    /// Like [`Self::call`], returning the reply instead of printing it.
    pub async fn fetch(
        &self,
        service: &str,
        method: Method,
        path: &str,
        body: &BodyArgs,
        extra: Option<Value>,
    ) -> Result<Value> {
        let mut payload = merge_body(body.data.as_deref(), extra)?;
        let mut query = parse_query(&body.query)?;
        // GET handlers read the query string only, so --data fields go there.
        if method == Method::GET {
            if let Some(fields) = payload.take() {
                query.extend(query_pairs(&fields)?);
            }
        }
        self.http
            .request_json(service, method, path, &query, payload)
            .await
    }

    /// GET with optional query pairs, printed.
    pub async fn get(&self, service: &str, path: &str, query: &[(String, String)]) -> Result<()> {
        let value = self
            .http
            .request_json(service, Method::GET, path, query, None)
            .await?;
        self.show(value).await
    }

    /// Request with a JSON body built from flags, printed.
    pub async fn send(&self, service: &str, method: Method, path: &str, body: Value) -> Result<()> {
        let value = self
            .http
            .request_json(service, method, path, &[], Some(body))
            .await?;
        self.show(value).await
    }

    /// Request without a body (DELETE, bodiless POST), printed.
    pub async fn send_empty(&self, service: &str, method: Method, path: &str) -> Result<()> {
        let value = self
            .http
            .request_json(service, method, path, &[], None)
            .await?;
        self.show(value).await
    }
}

/// Random RFC 4122 version-4 UUID, e.g. a new chat's `conversationId`.
pub fn new_uuid() -> String {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Percent-encodes one URL path segment, as the web app's `encodeURIComponent`.
pub fn seg(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Flattens a JSON object of scalars into query pairs.
pub fn query_pairs(fields: &Value) -> Result<Vec<(String, String)>> {
    let Value::Object(map) = fields else {
        anyhow::bail!("--data for a GET request must be a JSON object of query fields");
    };
    map.iter()
        .map(|(key, value)| match value {
            Value::String(s) => Ok((key.clone(), s.clone())),
            Value::Number(_) | Value::Bool(_) => Ok((key.clone(), value.to_string())),
            _ => anyhow::bail!("query field '{key}' must be a string, number, or boolean"),
        })
        .collect()
}

/// Asks a yes/no question before a change, unless `yes` (a `--yes` flag) was
/// given. Without a terminal to ask on, it refuses instead of guessing.
pub fn confirm(yes: bool, question: &str) -> Result<()> {
    use std::io::IsTerminal;
    if yes {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        anyhow::bail!("{question} — pass --yes to confirm when not running interactively");
    }
    let answer = prompt_line(&format!("{question} [y/N] "))?;
    if matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes") {
        Ok(())
    } else {
        anyhow::bail!("cancelled")
    }
}

/// Prints `label` and reads one trimmed line from stdin.
pub fn prompt_line(label: &str) -> Result<String> {
    use std::io::{BufRead, Write};
    eprint!("{label}");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .context("cannot read input")?;
    Ok(line.trim().to_string())
}

pub async fn dispatch(rt: &Runtime, command: &Command) -> Result<()> {
    match command {
        Command::Doctor { offline } => ops::doctor(rt, *offline).await,
        Command::Configure(args) => {
            ops::configure(args, Some(&rt.http.resolved().profile_name)).await
        }
        Command::Profile(args) => ops::profile(&args.action, rt).await,
        Command::Auth(args) => auth::run(rt, &args.action).await,
        Command::Warehouse(args) => lakehouse::warehouse(rt, &args.action).await,
        Command::Sql(args) => match &args.action {
            None => {
                let max_scan = args.scan.max_scan.as_deref();
                crate::sql::run(rt, &args.shell, crate::sql::Source::Auto, max_scan).await
            }
            Some(action) => lakehouse::sql(rt, action, &args.scan).await,
        },
        Command::Table(args) => lakehouse::table(rt, &args.action).await,
        Command::Backup(args) => lakehouse::backup(rt, &args.action).await,
        Command::Govern(args) => lakehouse::govern(rt, &args.action).await,
        Command::Tags(args) => lakehouse::tags(rt, &args.action).await,
        Command::Fs(args) => datafs::fs(rt, &args.action).await,
        Command::Data(args) => datafs::data(rt, &args.action).await,
        Command::Jobs(args) => jobs::run(rt, &args.action).await,
        Command::Notebooks(args) => mlops::notebooks(rt, &args.action).await,
        Command::Models(args) => mlops::models(rt, &args.action).await,
        Command::Agents(args) => agents::agents(rt, &args.action).await,
        Command::Chat(args) => agents::chat(rt, &args.action).await,
        Command::Logs(args) => ops::logs(rt, &args.action).await,
        Command::Rte(args) => ops::rte(rt, &args.action).await,
        Command::Sites(args) => datafs::sites(rt, &args.action).await,
        Command::Request(args) => ops::raw_request(rt, args).await,
        Command::Projects(args) => apps::projects(rt, &args.action).await,
        Command::Designer(args) => apps::designer(rt, &args.action).await,
        Command::Workspaces(args) => apps::workspaces(rt, &args.action).await,
        Command::Schedules(args) => apps::schedules(rt, &args.action).await,
        Command::Permissions(args) => apps::permissions(rt, &args.action).await,
        Command::Connections(args) => apps::connections(rt, &args.action).await,
        Command::Tui => crate::tui::run(rt).await,
        Command::Completion(args) => ops::completion(&args.shell),
        Command::Update(args) => update::run(args.version.as_deref(), args.check).await,
        Command::Code(args) => code::run(rt, args).await,
        Command::Mcp(args) => mcp::run(rt, &args.action).await,
        Command::Admin(args) => admin::run(rt, &args.action).await,
        Command::Search(args) => search::run(rt, args).await,
        Command::Run(args) => devloop::run(rt, args).await,
        Command::Sync(args) => devloop::sync(rt, args).await,
        Command::Usage(args) => match args.action {
            None => usage::run(rt).await,
            Some(crate::cli::UsageAction::Budget) => usage::budget(rt).await,
        },
        Command::Flows(args) => flows::run(rt, &args.action).await,
        Command::Approvals(args) => approvals::run(rt, &args.action).await,
        Command::Datasets(args) => apps::designer(rt, &args.designer_action()).await,
        Command::Skills(args) => skills::run(rt, &args.action).await,
        Command::Plugins(args) => skills::plugins(rt, &args.action).await,
        Command::Fix(args) => job_fix::run(rt, args).await,
        Command::Diff(args) => table_history::diff(rt, args).await,
        Command::Bisect(args) => table_history::bisect(rt, args).await,
        Command::Explain(args) => explain::run(rt, args).await,
        Command::Impact(args) => impact::run(rt, args).await,
    }
}

pub fn read_data_arg(raw: Option<&str>) -> Result<Option<Value>> {
    let Some(text) = raw else {
        return Ok(None);
    };
    let text = text.trim();
    if text == "@-" {
        use std::io::Read;
        let mut content = String::new();
        std::io::stdin()
            .read_to_string(&mut content)
            .context("cannot read --data from stdin")?;
        return serde_json::from_str(&content)
            .context("stdin is not valid JSON")
            .map(Some);
    }
    if let Some(path) = text.strip_prefix('@') {
        let content =
            std::fs::read_to_string(path).with_context(|| format!("cannot read {path}"))?;
        return serde_json::from_str(&content)
            .with_context(|| format!("{path} is not valid JSON"))
            .map(Some);
    }
    serde_json::from_str(text)
        .context("--data must be JSON or @file.json")
        .map(Some)
}

pub fn merge_body(raw: Option<&str>, extra: Option<Value>) -> Result<Option<Value>> {
    let from_flag = read_data_arg(raw)?;
    match (from_flag, extra) {
        (None, None) => Ok(None),
        (Some(value), None) => Ok(Some(value)),
        (None, Some(value)) => Ok(Some(value)),
        (Some(Value::Object(mut a)), Some(Value::Object(b))) => {
            for (key, val) in b {
                a.insert(key, val);
            }
            Ok(Some(Value::Object(a)))
        }
        (Some(_), Some(_)) => {
            anyhow::bail!("--data must be a JSON object when combined with command flags")
        }
    }
}

pub fn object_body(pairs: Vec<(&str, Value)>) -> Value {
    let map: BTreeMap<String, Value> = pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    Value::Object(map.into_iter().collect())
}

pub fn parse_query(items: &[String]) -> Result<Vec<(String, String)>> {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let (key, value) = item
            .split_once('=')
            .with_context(|| format!("--query must be K=V, got '{item}'"))?;
        out.push((key.to_string(), value.to_string()));
    }
    Ok(out)
}

pub fn read_text_arg(raw: &str) -> Result<String> {
    if let Some(path) = raw.strip_prefix('@') {
        return std::fs::read_to_string(path).with_context(|| format!("cannot read {path}"));
    }
    Ok(raw.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn makes_v4_uuids() {
        let id = new_uuid();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
        assert!(matches!(&id[19..20], "8" | "9" | "a" | "b"));
        assert_ne!(id, new_uuid());
    }

    #[test]
    fn encodes_path_segments() {
        assert_eq!(seg("sales db/2024?#"), "sales%20db%2F2024%3F%23");
        assert_eq!(seg("a-b_c.d~"), "a-b_c.d~");
    }

    #[test]
    fn turns_get_data_into_query() {
        let q = query_pairs(&json!({"limit": 5, "name": "x", "on": true})).unwrap();
        assert_eq!(q.len(), 3);
        assert!(q.contains(&("limit".to_string(), "5".to_string())));
        assert!(query_pairs(&json!({"nested": {"a": 1}})).is_err());
        assert!(query_pairs(&json!([1])).is_err());
    }

    #[test]
    fn parses_query_pairs() {
        let out = parse_query(&["a=1".to_string(), "b=x=y".to_string()]).unwrap();
        assert_eq!(
            out,
            vec![
                ("a".to_string(), "1".to_string()),
                ("b".to_string(), "x=y".to_string())
            ]
        );
        assert!(parse_query(&["nope".to_string()]).is_err());
    }

    #[test]
    fn reads_data_from_file() {
        let path = std::env::temp_dir().join(format!("gz-data-{}.json", std::process::id()));
        std::fs::write(&path, r#"{"a": 1}"#).unwrap();
        let arg = format!("@{}", path.display());
        let value = read_data_arg(Some(&arg)).unwrap().unwrap();
        assert_eq!(value, json!({"a": 1}));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn merges_flag_body_over_inline() {
        let merged = merge_body(Some(r#"{"a": 1}"#), Some(json!({"b": 2})))
            .unwrap()
            .unwrap();
        assert_eq!(merged, json!({"a": 1, "b": 2}));
        assert!(merge_body(Some("[1]"), Some(json!({"b": 2}))).is_err());
        assert!(merge_body(None, None).unwrap().is_none());
    }
}
