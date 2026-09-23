use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

pub const DEFAULT_DOMAIN: &str = "groundzero.cloud";

/// EKS provider slug per service, mirroring the frontend's generateAdminEKSURL
/// call sites. `None` means no verified public hostname exists (Amplify
/// gateway services) and the URL must be set explicitly.
pub fn provider_for(service: &str) -> Option<&'static str> {
    match service {
        "lakehouse" => Some("lakehouseprovider"),
        "etl" => Some("etlprovider"),
        "files" => Some("filemanager"),
        "agents" => Some("gz-agent-builder-api"),
        "chat" | "auth" => Some("gz-chat"),
        "logs" => Some("logprovider"),
        _ => None,
    }
}

/// `https://{client}-admin-{provider}.{client}.api.{domain}`,
/// the exact formula the Groundzero frontend uses.
pub fn derive_base(client: &str, provider: &str, domain: &str) -> String {
    format!("https://{client}-admin-{provider}.{client}.api.{domain}")
}

/// Bundled Amplify gateway endpoints, copied from the web app's own
/// aws-exports for the dev environment. These are shared per environment
/// (tenancy rides on the auth headers plus `gz-site`), so no per-user
/// configuration is needed on dev domains. Anything else still needs an
/// explicit `gz configure --service` override.
pub fn gateway_for(domain: &str, service: &str) -> Option<&'static str> {
    if !domain.contains("dev") {
        return None;
    }
    match service {
        "admin" => Some("https://e75wtoiuak.execute-api.ap-south-1.amazonaws.com/dev"),
        "dashboards" => Some("https://s9y28z1gdd.execute-api.ap-south-1.amazonaws.com/dev"),
        "connections" => Some("https://mtu7da95hl.execute-api.ap-south-1.amazonaws.com/dev"),
        "data" => Some("https://l1tne6yeba.execute-api.ap-south-1.amazonaws.com/dev"),
        "datasets" => Some("https://l5fi22nwvg.execute-api.ap-south-1.amazonaws.com/dev"),
        "etlprojects" => Some("https://7lr4z0nm2c.execute-api.ap-south-1.amazonaws.com/dev"),
        "filters" => Some("https://vwo95ataq2.execute-api.ap-south-1.amazonaws.com/dev"),
        "mlops" => Some("https://70a6x016i9.execute-api.ap-south-1.amazonaws.com/dev"),
        "permissions" => Some("https://4dc0hpkw66.execute-api.ap-south-1.amazonaws.com/dev"),
        "rtes" => Some("https://nhunn639wk.execute-api.ap-south-1.amazonaws.com/dev"),
        "schedules" => Some("https://motd17rnh9.execute-api.ap-south-1.amazonaws.com/dev"),
        "visualizations" => Some("https://4sccup1vsf.execute-api.ap-south-1.amazonaws.com/dev"),
        "workspaces" => Some("https://rw571xu3h5.execute-api.ap-south-1.amazonaws.com/dev"),
        _ => None,
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Profile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub services: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_profile: Option<String>,
    #[serde(default)]
    pub profiles: HashMap<String, Profile>,
}

impl Config {
    pub fn active_name(&self, override_name: Option<&str>) -> String {
        if let Some(name) = override_name {
            return name.to_string();
        }
        if let Ok(env) = std::env::var("GZ_PROFILE") {
            if !env.trim().is_empty() {
                return env;
            }
        }
        self.current_profile
            .clone()
            .unwrap_or_else(|| "default".to_string())
    }

    pub fn profile(&self, name: &str) -> Profile {
        self.profiles.get(name).cloned().unwrap_or_default()
    }

    pub fn profile_mut(&mut self, name: &str) -> &mut Profile {
        self.profiles.entry(name.to_string()).or_default()
    }
}

pub fn config_path() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("cloud", "Groundzero", "gz")
        .context("cannot resolve user config directory")?;
    Ok(dirs.config_dir().join("config.toml"))
}

pub fn load_config() -> Result<Config> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(Config::default());
    }
    let text =
        fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
    if text.trim().is_empty() {
        return Ok(Config::default());
    }
    toml::from_str(&text).with_context(|| format!("cannot parse {}", path.display()))
}

pub fn save_config(config: &Config) -> Result<PathBuf> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).ok();
        }
    }
    let text = toml::to_string_pretty(config).context("cannot encode config")?;
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true).mode(0o600);
        let mut file = opts
            .open(&path)
            .with_context(|| format!("cannot write {}", path.display()))?;
        file.write_all(text.as_bytes())
            .with_context(|| format!("cannot write {}", path.display()))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("cannot secure {}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        fs::write(&path, text).with_context(|| format!("cannot write {}", path.display()))?;
    }
    Ok(path)
}

/// True for https URLs and http loopback (dev/test only).
pub fn is_secure_base(url: &str) -> bool {
    match url::Url::parse(url) {
        Ok(parsed) => {
            parsed.scheme() == "https"
                || (parsed.scheme() == "http"
                    && matches!(parsed.host_str(), Some("127.0.0.1" | "localhost" | "::1")))
        }
        Err(_) => false,
    }
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub profile_name: String,
    pub profile: Profile,
    pub flag_host: Option<String>,
    pub flag_token: Option<String>,
    pub token_source: &'static str,
}

impl Resolved {
    pub fn token(&self) -> Option<String> {
        if let Some(token) = &self.flag_token {
            if !token.trim().is_empty() {
                return Some(token.clone());
            }
        }
        if let Ok(env) = std::env::var("GZ_TOKEN") {
            if !env.trim().is_empty() {
                return Some(env);
            }
        }
        self.profile.token.clone()
    }

    pub fn token_expired(&self) -> bool {
        match self.profile.expires_at {
            Some(exp) => now_unix() >= exp,
            None => false,
        }
    }

    pub fn host(&self) -> Option<String> {
        if let Some(host) = &self.flag_host {
            if !host.trim().is_empty() {
                return Some(trim_trailing_slash(host));
            }
        }
        if let Ok(env) = std::env::var("GZ_HOST") {
            if !env.trim().is_empty() {
                return Some(trim_trailing_slash(&env));
            }
        }
        self.profile.host.as_ref().map(|h| trim_trailing_slash(h))
    }

    pub fn auth_host(&self) -> Option<String> {
        if let Ok(env) = std::env::var("GZ_AUTH_HOST") {
            if !env.trim().is_empty() {
                return Some(trim_trailing_slash(&env));
            }
        }
        if let Some(host) = &self.profile.auth_host {
            if !host.trim().is_empty() {
                return Some(trim_trailing_slash(host));
            }
        }
        self.base_for("auth").ok()
    }

    pub fn site(&self) -> Option<String> {
        if let Ok(env) = std::env::var("GZ_SITE") {
            if !env.trim().is_empty() {
                return Some(env);
            }
        }
        self.profile.site.clone()
    }

    pub fn client(&self) -> Option<String> {
        if let Ok(env) = std::env::var("GZ_CLIENT") {
            if !env.trim().is_empty() {
                return Some(env);
            }
        }
        self.profile.client.clone()
    }

    pub fn domain(&self) -> String {
        if let Ok(env) = std::env::var("GZ_DOMAIN") {
            if !env.trim().is_empty() {
                return env;
            }
        }
        self.profile
            .domain
            .clone()
            .unwrap_or_else(|| DEFAULT_DOMAIN.to_string())
    }

    pub fn base_for(&self, service: &str) -> Result<String> {
        if let Some(url) = self.profile.services.get(service) {
            if !url.trim().is_empty() {
                return Ok(trim_trailing_slash(url));
            }
        }
        if let Some(host) = self.host() {
            return Ok(host);
        }
        match (self.client(), provider_for(service)) {
            (Some(client), Some(provider)) => Ok(derive_base(&client, provider, &self.domain())),
            (_, None) => match gateway_for(&self.domain(), service) {
                Some(url) => Ok(url.to_string()),
                None => anyhow::bail!(
                    "service '{service}' has no URL configured. Set it once with `gz configure --service {service}=https://...`."
                ),
            },
            (None, _) => anyhow::bail!(
                "no client configured (profile '{profile}'). Run `gz configure --client <name> --site <site>`; service URLs derive automatically.",
                profile = self.profile_name
            ),
        }
    }
}

pub fn resolve(
    config: &Config,
    profile_flag: Option<&str>,
    host_flag: Option<String>,
    token_flag: Option<String>,
) -> Resolved {
    let name = config.active_name(profile_flag);
    let profile = config.profile(&name);
    let token_source = if token_flag.as_deref().is_some_and(|t| !t.trim().is_empty()) {
        "flag"
    } else if std::env::var("GZ_TOKEN").is_ok_and(|t| !t.trim().is_empty()) {
        "env"
    } else if profile.token.is_some() {
        "config"
    } else {
        "missing"
    };
    Resolved {
        profile_name: name,
        profile,
        flag_host: host_flag,
        flag_token: token_flag,
        token_source,
    }
}

pub fn trim_trailing_slash(url: &str) -> String {
    url.trim_end_matches('/').to_string()
}

pub fn join_url(base: &str, path: &str) -> String {
    format!(
        "{}/{}",
        trim_trailing_slash(base),
        path.trim_start_matches('/')
    )
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn redact_token(token: &str) -> String {
    let tail: String = token
        .chars()
        .rev()
        .take(4)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_base_and_path() {
        assert_eq!(join_url("https://h.test/", "/a/b"), "https://h.test/a/b");
        assert_eq!(join_url("https://h.test", "a"), "https://h.test/a");
    }

    #[test]
    fn gateway_services_resolve_without_client() {
        let mut config = Config::default();
        let profile = config.profile_mut("default");
        profile.domain = Some("groundzerodev.cloud".to_string());
        let resolved = resolve(&config, Some("default"), None, None);
        assert_eq!(
            resolved.base_for("datasets").unwrap(),
            "https://l5fi22nwvg.execute-api.ap-south-1.amazonaws.com/dev"
        );
        assert!(resolved.base_for("rtes").unwrap().contains("execute-api"));
    }

    #[test]
    fn gateway_map_covers_dev_only() {
        let dev = gateway_for("groundzerodev.cloud", "datasets").unwrap();
        assert!(dev.starts_with("https://"));
        assert!(dev.ends_with("/dev"));
        assert!(gateway_for("groundzero.cloud", "datasets").is_none());
        assert!(gateway_for("groundzerodev.cloud", "nope").is_none());
    }

    #[test]
    fn redacts_to_last_four() {
        assert_eq!(redact_token("secret-token-1234"), "…1234");
    }

    #[test]
    fn missing_profile_is_default() {
        let config = Config::default();
        assert!(config.profile("nope").host.is_none());
        assert_eq!(config.active_name(Some("x")), "x");
    }

    #[test]
    fn derives_frontend_style_urls() {
        assert_eq!(
            derive_base("dev", "etlprovider", "groundzero.cloud"),
            "https://dev-admin-etlprovider.dev.api.groundzero.cloud"
        );
        assert_eq!(provider_for("etl"), Some("etlprovider"));
        assert_eq!(provider_for("lakehouse"), Some("lakehouseprovider"));
        assert_eq!(provider_for("files"), Some("filemanager"));
        assert_eq!(provider_for("agents"), Some("gz-agent-builder-api"));
        assert_eq!(provider_for("chat"), Some("gz-chat"));
        assert_eq!(provider_for("auth"), Some("gz-chat"));
        assert_eq!(provider_for("logs"), Some("logprovider"));
        assert_eq!(provider_for("data"), None);
        assert_eq!(provider_for("mlops"), None);
        assert_eq!(provider_for("iceberg"), None);
    }

    #[test]
    fn base_url_prefers_override_then_host_then_derived() {
        let mut config = Config::default();
        let entry = config.profile_mut("p");
        entry.client = Some("dev".to_string());
        entry
            .services
            .insert("etl".to_string(), "https://custom.test/".to_string());
        let resolved = resolve(&config, Some("p"), None, None);
        assert_eq!(resolved.base_for("etl").unwrap(), "https://custom.test");
        assert_eq!(
            resolved.base_for("lakehouse").unwrap(),
            "https://dev-admin-lakehouseprovider.dev.api.groundzero.cloud"
        );
        assert!(resolved.base_for("data").is_err());
        let with_host = resolve(
            &config,
            Some("p"),
            Some("https://stub.test".to_string()),
            None,
        );
        assert_eq!(with_host.base_for("data").unwrap(), "https://stub.test");
        let bare = resolve(&Config::default(), Some("nope"), None, None);
        assert!(bare.base_for("etl").is_err());
    }

    #[test]
    fn secure_base_allows_https_and_loopback_only() {
        assert!(is_secure_base("https://api.example.com"));
        assert!(is_secure_base("https://api.example.com:8443/x"));
        assert!(is_secure_base("http://127.0.0.1:18080"));
        assert!(is_secure_base("http://localhost:18080"));
        assert!(!is_secure_base("http://api.example.com"));
        assert!(!is_secure_base("http://10.0.0.5"));
        assert!(!is_secure_base("not a url"));
    }
}
