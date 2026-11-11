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
        _ => None,
    }
}

/// `https://{client}-admin-{provider}.{client}.api.{domain}`,
/// the exact formula the Groundzero frontend uses.
pub fn derive_base(client: &str, provider: &str, domain: &str) -> String {
    format!("https://{client}-admin-{provider}.{client}.api.{domain}")
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Profile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_host: Option<String>,
    /// The web app's address when it isn't `https://{site}.{client}.{domain}`
    /// (a local dev server); sign-in settings and `Origin` come from it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
    /// Per-service URL overrides set with `gz configure --service`.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub services: HashMap<String, String>,
    /// API gateways discovered from the web app at login.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub gateways: HashMap<String, String>,
    /// Cognito ID token, sent as `Authorization` to every service.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Cognito access token, sent only to the AdminQueries gateway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// Cognito user pool and web app client, discovered from the web app at login.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cognito_pool_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cognito_client_id: Option<String>,
    /// `gz code`'s last chosen model (a mode value or `conn:PROVIDER:name`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_model: Option<String>,
    /// `code_model`'s display name, so the welcome box can show it before
    /// the model list loads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_model_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_effort: Option<String>,
    /// `gz code /subagents off` remembered (absent means on).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_subagents: Option<bool>,
    /// `gz code`'s auto-compact threshold in tokens (`/autocompact`,
    /// `--auto-compact`); absent means the server's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_auto_compact: Option<u64>,
    /// The user's choice for models that cost more past a prompt size:
    /// true uses the full window at the long-context price, false keeps
    /// conversations on the standard price. Absent until `gz code` has asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_long_context: Option<bool>,
    /// `gz code`'s subagent settings (`/subagents model|effort|context|max|parallel`);
    /// absent means the server's defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_subagent_settings: Option<crate::assistant::subagent_settings::SubagentSettings>,
    /// Folders outside the start folder that `gz code` may switch to
    /// without asking ("always allow this folder").
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub code_allowed_dirs: Vec<PathBuf>,
    /// `gz code /memory off` remembered (absent means on).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_memory: Option<bool>,
    /// `gz code`'s own spending caps (`/budget session|day <usd>`);
    /// absent means no caps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_spend_caps: Option<crate::spend::caps::Caps>,
    /// Platform API key for automation (`gz auth login --api-key`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// `gz sql`'s last warehouse and compute size (names).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sql_warehouse: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sql_compute: Option<String>,
    /// A `gz sql` session left running (`--keep-session` or an interrupted
    /// exit), reused next time if the server says it is still up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sql_session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sql_session_warehouse: Option<String>,
    /// Settings this build does not know (written by a newer gz), kept so
    /// saving the config never drops them.
    #[serde(flatten)]
    pub unknown: toml::Table,
}

impl Profile {
    pub fn pool(&self) -> Option<crate::cognito::PoolConfig> {
        Some(crate::cognito::PoolConfig {
            pool_id: self.cognito_pool_id.clone()?,
            client_id: self.cognito_client_id.clone()?,
        })
    }

    pub fn clear_session(&mut self) {
        self.token = None;
        self.expires_at = None;
        self.refresh_token = None;
        self.access_token = None;
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_profile: Option<String>,
    #[serde(default)]
    pub profiles: HashMap<String, Profile>,
    #[serde(flatten)]
    pub unknown: toml::Table,
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

/// `$XDG_CONFIG_HOME/gz/config.toml`, else `~/.config/gz/config.toml` on
/// Unix (macOS included, as documented), and the roaming AppData dir on Windows.
pub fn config_path() -> Result<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(xdg).join("gz").join("config.toml"));
    }
    if cfg!(windows) {
        return platform_config_path();
    }
    let home = directories::BaseDirs::new()
        .context("cannot resolve home directory")?
        .home_dir()
        .to_path_buf();
    Ok(home.join(".config").join("gz").join("config.toml"))
}

fn platform_config_path() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("cloud", "Groundzero", "gz")
        .context("cannot resolve user config directory")?;
    Ok(dirs.config_dir().join("config.toml"))
}

/// Where gz <= 0.2.1 kept its config on macOS, read once so upgrades keep
/// their sign-in. The next save writes to `config_path()`.
fn legacy_config_path() -> Option<PathBuf> {
    if !cfg!(target_os = "macos") || std::env::var_os("XDG_CONFIG_HOME").is_some() {
        return None;
    }
    platform_config_path().ok().filter(|p| p.exists())
}

pub fn load_config() -> Result<Config> {
    let mut path = config_path()?;
    if !path.exists() {
        match legacy_config_path() {
            Some(legacy) => path = legacy,
            None => return Ok(Config::default()),
        }
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
/// `localhost`, its subdomains (always loopback, RFC 6761) and loopback IPs.
fn is_loopback_host(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1" | "[::1]")
        || host.ends_with(".localhost")
}

pub fn is_secure_base(url: &str) -> bool {
    match url::Url::parse(url) {
        Ok(parsed) => {
            parsed.scheme() == "https"
                || (parsed.scheme() == "http" && parsed.host_str().is_some_and(is_loopback_host))
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

    /// The API key to use, if any: `GZ_API_KEY` first, then the profile's.
    pub fn api_key(&self) -> Option<(String, crate::apikey::KeySource)> {
        use crate::apikey::KeySource;
        if let Ok(env) = std::env::var("GZ_API_KEY") {
            if !env.trim().is_empty() {
                return Some((env.trim().to_string(), KeySource::Env));
            }
        }
        self.profile
            .api_key
            .clone()
            .map(|key| (key, KeySource::Profile))
    }

    pub fn token_expired(&self) -> bool {
        self.token_source == "config"
            && self.profile.expires_at.is_some_and(|exp| now_unix() >= exp)
    }

    /// Whether gz renews the saved token by itself once it expires: a
    /// signed-in profile with a refresh token and its user pool.
    pub fn can_renew(&self) -> bool {
        self.token_source == "config"
            && self.profile.refresh_token.is_some()
            && self.profile.pool().is_some()
    }

    /// Signed in: a token gz may use, still valid or renewed on the next
    /// request. A refresh that then fails surfaces on that request.
    pub fn is_signed_in(&self) -> bool {
        self.token().is_some() && (!self.token_expired() || self.can_renew())
    }

    /// The web app's own address: `GZ_WEB_URL` or the profile's `web_url`
    /// (a local dev server), else `https://{site}.{client}.{domain}`. Several
    /// providers read the tenant site from the `Origin` header in this form.
    pub fn web_url(&self) -> Option<String> {
        let configured = std::env::var("GZ_WEB_URL")
            .ok()
            .filter(|url| !url.trim().is_empty())
            .or_else(|| {
                self.profile
                    .web_url
                    .clone()
                    .filter(|url| !url.trim().is_empty())
            });
        if let Some(url) = configured {
            return Some(trim_trailing_slash(url.trim()));
        }
        Some(format!(
            "https://{}.{}.{}",
            self.site()?,
            self.client()?,
            self.domain()
        ))
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
        if let Some(provider) = provider_for(service) {
            let client = self.client().with_context(|| {
                format!(
                    "no client configured (profile '{}'). Run `gz configure`.",
                    self.profile_name
                )
            })?;
            return Ok(derive_base(&client, provider, &self.domain()));
        }
        match self.profile.gateways.get(service) {
            Some(url) => Ok(url.clone()),
            None if self.profile.gateways.is_empty() => anyhow::bail!(
                "service '{service}' URL comes from the web app; run `gz auth login` to load it"
            ),
            None => anyhow::bail!(
                "service '{service}' isn't deployed on this site; set it with `gz configure --service {service}=https://...`"
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
    fn an_expired_token_is_still_signed_in_while_gz_can_renew_it() {
        let mut config = Config::default();
        let profile = config.profile_mut("p");
        profile.token = Some("id-token".into());
        profile.expires_at = Some(now_unix() - 60);
        let expired_only = resolve(&config, Some("p"), None, None);
        assert!(expired_only.token_expired());
        assert!(!expired_only.is_signed_in(), "nothing to renew it with");

        let profile = config.profile_mut("p");
        profile.refresh_token = Some("refresh".into());
        profile.cognito_pool_id = Some("us-east-1_pool".into());
        profile.cognito_client_id = Some("client".into());
        let renewable = resolve(&config, Some("p"), None, None);
        assert!(renewable.token_expired() && renewable.can_renew());
        assert!(renewable.is_signed_in());

        config.profile_mut("p").expires_at = Some(now_unix() + 3600);
        assert!(resolve(&config, Some("p"), None, None).is_signed_in());
        assert!(!resolve(&Config::default(), Some("p"), None, None).is_signed_in());
    }

    #[test]
    fn joins_base_and_path() {
        assert_eq!(join_url("https://h.test/", "/a/b"), "https://h.test/a/b");
        assert_eq!(join_url("https://h.test", "a"), "https://h.test/a");
    }

    #[test]
    fn gateway_services_resolve_without_client() {
        let mut config = Config::default();
        assert!(resolve(&config, Some("default"), None, None)
            .base_for("datasets")
            .unwrap_err()
            .to_string()
            .contains("gz auth login"));
        let profile = config.profile_mut("default");
        profile
            .gateways
            .insert("datasets".to_string(), "https://gw.test/stage".to_string());
        let resolved = resolve(&config, Some("default"), None, None);
        assert_eq!(
            resolved.base_for("datasets").unwrap(),
            "https://gw.test/stage"
        );
        assert!(resolved
            .base_for("rtes")
            .unwrap_err()
            .to_string()
            .contains("isn't deployed"));
    }

    #[test]
    fn saving_keeps_settings_from_newer_versions() {
        let text = "future_top = 1\n\n[profiles.p]\nsite = \"acme\"\nfuture_field = \"kept\"\n";
        let config: Config = toml::from_str(text).unwrap();
        assert_eq!(config.profile("p").site.as_deref(), Some("acme"));
        let saved = toml::to_string_pretty(&config).unwrap();
        assert!(saved.contains("future_field = \"kept\""), "{saved}");
        assert!(saved.contains("future_top = 1"), "{saved}");
    }

    #[test]
    fn subagent_settings_save_as_a_table_and_read_back() {
        use crate::assistant::subagent_settings::SubagentSettings;
        let text = "[profiles.p]\nsite = \"acme\"\nfuture_field = \"kept\"\n";
        let mut config: Config = toml::from_str(text).unwrap();
        let settings = SubagentSettings {
            model: Some("haiku55".into()),
            max_concurrent: Some(3),
            ..SubagentSettings::default()
        };
        config.profile_mut("p").code_subagent_settings = Some(settings.clone());
        let saved = toml::to_string_pretty(&config).unwrap();
        let read: Config = toml::from_str(&saved).unwrap();
        assert_eq!(
            read.profile("p").code_subagent_settings,
            Some(settings),
            "{saved}"
        );
        assert_eq!(
            read.profile("p").unknown["future_field"].as_str(),
            Some("kept")
        );
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
        assert_eq!(provider_for("logs"), None);
        assert_eq!(provider_for("data"), None);
        assert_eq!(provider_for("mlops"), None);
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
        config
            .profile_mut("p")
            .gateways
            .insert("data".to_string(), "https://gw.test/stage".to_string());
        let resolved = resolve(&config, Some("p"), None, None);
        assert_eq!(resolved.base_for("data").unwrap(), "https://gw.test/stage");
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
