use anyhow::{Context, Result};
use reqwest::Method;
use serde_json::Value;
use std::collections::BTreeMap;

use crate::cli::{BodyArgs, Command};
use crate::http::ApiClient;
use crate::output::{self, OutputFormat};

pub mod agents;
pub mod apps;
pub mod auth;
pub mod datafs;
pub mod jobs;
pub mod lakehouse;
pub mod mlops;
pub mod ops;

pub struct Runtime {
    pub http: ApiClient,
    pub format: OutputFormat,
    pub limit: Option<usize>,
    pub quiet: bool,
}

impl Runtime {
    pub async fn show(&self, value: Value) -> Result<()> {
        output::print_value(&value, self.format, self.limit)
    }

    pub async fn call(
        &self,
        service: &str,
        method: Method,
        path: &str,
        body: &BodyArgs,
        extra: Option<Value>,
    ) -> Result<()> {
        let payload = merge_body(body.data.as_deref(), extra)?;
        let query = parse_query(&body.query)?;
        let value = self
            .http
            .request_json(service, method, path, &query, payload)
            .await?;
        self.show(value).await
    }
}

pub async fn dispatch(rt: &Runtime, command: &Command) -> Result<()> {
    match command {
        Command::Doctor => ops::doctor(rt).await,
        Command::Configure(args) => ops::configure(args).await,
        Command::Profile(args) => ops::profile(&args.action, rt).await,
        Command::Auth(args) => auth::run(rt, &args.action).await,
        Command::Warehouse(args) => lakehouse::warehouse(rt, &args.action).await,
        Command::Sql(args) => lakehouse::sql(rt, &args.action).await,
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
