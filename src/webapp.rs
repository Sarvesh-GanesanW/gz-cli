//! Reads the settings the Groundzero web app ships in its JavaScript bundle
//! (its Amplify `aws-exports`): the Cognito pool and the API gateway for each
//! backend. gz uses the exact same values, so no per-environment setup is
//! needed — dev, stage and prod all work from the site URL alone.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;

use crate::cognito::PoolConfig;

#[derive(Debug, Clone)]
pub struct WebConfig {
    pub pool: PoolConfig,
    /// gz service key → gateway base URL.
    pub gateways: HashMap<String, String>,
}

/// Web app API names that differ from gz's service keys.
fn service_key(api_name: &str) -> String {
    match api_name {
        "dataSource" => "data".to_string(),
        "etlProjects" => "etlprojects".to_string(),
        other => other.to_ascii_lowercase(),
    }
}

pub async fn discover(http: &reqwest::Client, web_url: &str) -> Result<WebConfig> {
    let base = web_url.trim_end_matches('/');
    let index = fetch_text(http, &format!("{base}/")).await?;
    let scripts = script_urls(&index, base);
    if scripts.is_empty() {
        bail!("no app bundle found at {base}");
    }
    for url in scripts {
        let bundle = fetch_text(http, &url).await?;
        if let Some(config) = parse_bundle(&bundle) {
            return Ok(config);
        }
    }
    bail!("sign-in settings not found in the web app at {base}")
}

fn script_urls(index_html: &str, base: &str) -> Vec<String> {
    index_html
        .split("src=\"")
        .skip(1)
        .filter_map(|s| s.split('"').next())
        .filter(|s| s.ends_with(".js") && s.contains("/static/js/"))
        .map(|s| {
            if s.starts_with("http") {
                s.to_string()
            } else {
                format!("{base}/{}", s.trim_start_matches('/'))
            }
        })
        .collect()
}

fn parse_bundle(bundle: &str) -> Option<WebConfig> {
    let pool = PoolConfig {
        pool_id: string_value(bundle, "aws_user_pools_id")?,
        client_id: string_value(bundle, "aws_user_pools_web_client_id")?,
    };
    Some(WebConfig {
        pool,
        gateways: gateways(bundle),
    })
}

async fn fetch_text(http: &reqwest::Client, url: &str) -> Result<String> {
    let response = http
        .get(url)
        .send()
        .await
        .with_context(|| format!("cannot reach {url}"))?;
    if !response.status().is_success() {
        bail!("GET {url} → HTTP {}", response.status());
    }
    response.text().await.context("cannot read response")
}

/// Finds `key: "value"` in the bundle: minified (`key:"value"`), JSON
/// (`"key":"value"`) or a development build (`key: 'value'`).
fn string_value(source: &str, key: &str) -> Option<String> {
    let pattern = format!(
        r#"(?:^|[^A-Za-z0-9_$])["']?{}["']?\s*:\s*(?:"([^"]*)"|'([^']*)')"#,
        regex::escape(key)
    );
    let found = regex::Regex::new(&pattern).ok()?.captures(source)?;
    let value = found.get(1).or_else(|| found.get(2))?.as_str();
    (!value.is_empty()).then(|| value.to_string())
}

/// Entries of `aws_cloud_logic_custom: [{name:"…", endpoint:"…"}, …]`.
fn gateways(bundle: &str) -> HashMap<String, String> {
    let Some(start) = bundle.find("aws_cloud_logic_custom") else {
        return HashMap::new();
    };
    let list = &bundle[start..];
    let list = &list[..list.find(']').unwrap_or(list.len())];
    list.split('{')
        .skip(1)
        .filter_map(|entry| {
            let name = string_value(entry, "name")?;
            let endpoint = string_value(entry, "endpoint")?;
            Some((
                service_key(&name),
                endpoint.trim_end_matches('/').to_string(),
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUNDLE: &str = r#"var e={aws_project_region:"ap-south-1",aws_user_pools_id:"ap-south-1_Ab1",aws_user_pools_web_client_id:"cid42",aws_cloud_logic_custom:[{name:"admin",endpoint:"https://a.example/stage",region:"ap-south-1"},{name:"dataSource",endpoint:"https://d.example/stage/",region:"ap-south-1"},{name:"etlProjects",endpoint:"https://p.example/stage",region:"ap-south-1"}],aws_appsync:{name:"x"}}"#;

    #[test]
    fn reads_pool_and_gateways_from_minified_bundle() {
        let config = parse_bundle(BUNDLE).unwrap();
        assert_eq!(config.pool.pool_id, "ap-south-1_Ab1");
        assert_eq!(config.pool.client_id, "cid42");
        assert_eq!(config.gateways.len(), 3);
        assert_eq!(config.gateways["admin"], "https://a.example/stage");
        assert_eq!(config.gateways["data"], "https://d.example/stage");
        assert_eq!(config.gateways["etlprojects"], "https://p.example/stage");
    }

    #[test]
    fn finds_app_scripts() {
        let html = r#"<script defer src="/static/js/main.abc.js"></script><script src="https://cdn.x/y.js"></script>"#;
        assert_eq!(
            script_urls(html, "https://site.test"),
            vec!["https://site.test/static/js/main.abc.js"]
        );
    }

    #[test]
    fn reads_a_development_build_with_spaces_and_single_quotes() {
        // What the React dev server serves: unminified, single quotes, and
        // later reads of the same key that must not be taken for its value.
        let bundle = r#"const awsmobile = {
  aws_project_region: 'ap-south-1',
  aws_user_pools_id: 'ap-south-1_Dev1',
  aws_user_pools_web_client_id: 'devclient7',
  aws_cloud_logic_custom: [{
    name: 'admin',
    endpoint: 'https://a.example/dev/',
    region: 'ap-south-1'
  }, {
    "name": "etlProjects",
    "endpoint": "https://p.example/dev"
  }]
};
if (config['aws_user_pools_id']) { use(config['aws_user_pools_web_client_id']); }"#;
        let config = parse_bundle(bundle).unwrap();
        assert_eq!(config.pool.pool_id, "ap-south-1_Dev1");
        assert_eq!(config.pool.client_id, "devclient7");
        assert_eq!(config.gateways["admin"], "https://a.example/dev");
        assert_eq!(config.gateways["etlprojects"], "https://p.example/dev");
    }

    #[test]
    fn a_key_inside_a_longer_name_is_not_matched() {
        let bundle = r#"x_aws_user_pools_id:"wrong",aws_user_pools_id:"right""#;
        assert_eq!(
            string_value(bundle, "aws_user_pools_id").as_deref(),
            Some("right")
        );
    }

    #[test]
    fn bundle_without_pool_is_rejected() {
        assert!(parse_bundle("var e={}").is_none());
    }
}
