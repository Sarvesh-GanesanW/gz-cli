//! Groundzero platform API keys (`sk_<useCase>_<user>_<keyId>.<secret>`) for
//! automation and CI. Only a few backends accept them, so gz decides per
//! request whether to send the key, the signed-in user's token, or nothing.

use reqwest::Method;

/// Where the key came from; an explicit `GZ_API_KEY` wins over a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    Env,
    Profile,
}

/// Which credential a request carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Credential {
    User,
    ApiKey,
    /// Only a key is available and this route does not accept keys.
    Refuse,
}

/// Data-provider route prefixes the `dataSource` gateway leaves without a
/// Cognito authorizer (`/getdata*` has one, so keys stop at the gateway).
const DATA_KEY_PREFIXES: &[&str] = &["/getSchema", "/testConnection", "/storage", "/Odata"];

/// Agent builder reporting routes that accept keys (`reporting_request_context`).
const AGENT_KEY_PATHS: &[&str] = &[
    "/v1/agents/metrics",
    "/v1/agents/metrics/timeseries",
    "/v1/agents/metrics/models",
    "/v1/audit",
];

pub const KEYS_ONLY_HINT: &str =
    "API keys only work for ETL jobs, data connections and storage, and agent metrics";

/// Shape check only; the server does the real validation.
pub fn looks_like_key(text: &str) -> bool {
    let text = text.trim();
    text.starts_with("sk_")
        && text.contains('.')
        && !text.contains(char::is_whitespace)
        && text.len() <= 512
}

fn path_only(path: &str) -> &str {
    path.split('?').next().unwrap_or(path)
}

fn has_prefix(path: &str, prefix: &str) -> bool {
    path == prefix || path.starts_with(&format!("{prefix}/"))
}

/// Whether `service` accepts an API key on this route.
pub fn accepts(service: &str, method: &Method, path: &str) -> bool {
    let path = path_only(path);
    match service {
        "etl" => true,
        "data" => DATA_KEY_PREFIXES.iter().any(|p| has_prefix(path, p)),
        "agents" => *method == Method::GET && AGENT_KEY_PATHS.contains(&path),
        _ => false,
    }
}

/// The signed-in user wins unless the key was set explicitly for this run;
/// a key-only caller is refused where keys are not accepted.
pub fn choose(has_user: bool, key: Option<KeySource>, route_accepts: bool) -> Credential {
    match (key, has_user, route_accepts) {
        (Some(KeySource::Env), _, true) => Credential::ApiKey,
        (_, true, _) => Credential::User,
        (Some(_), false, true) => Credential::ApiKey,
        (Some(_), false, false) => Credential::Refuse,
        (None, false, _) => Credential::User,
    }
}

pub fn refusal(service: &str) -> String {
    format!(
        "this command needs a signed-in user ({service} doesn't accept API keys): run `gz auth login`. {KEYS_ONLY_HINT}."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_key_shape() {
        assert!(looks_like_key("sk_ci_ana_k1.secret"));
        assert!(!looks_like_key("eyJhbGciOi.payload.sig"));
        assert!(!looks_like_key("sk_no_secret_part"));
        assert!(!looks_like_key("sk_a b.c"));
    }

    #[test]
    fn only_key_capable_routes_accept_keys() {
        assert!(accepts("etl", &Method::POST, "/etl/job"));
        assert!(accepts("data", &Method::POST, "/storage/listFiles"));
        assert!(accepts("data", &Method::POST, "/getSchema"));
        assert!(!accepts("data", &Method::POST, "/getdata"));
        assert!(!accepts("data", &Method::POST, "/storageX"));
        assert!(accepts(
            "agents",
            &Method::GET,
            "/v1/agents/metrics?limit=5"
        ));
        assert!(!accepts("agents", &Method::GET, "/v1/agents"));
        assert!(!accepts("agents", &Method::POST, "/v1/audit"));
        for service in [
            "lakehouse",
            "chat",
            "files",
            "mlops",
            "etlprojects",
            "datasets",
        ] {
            assert!(!accepts(service, &Method::GET, "/anything"), "{service}");
        }
    }

    #[test]
    fn user_session_wins_unless_key_is_explicit() {
        assert_eq!(
            choose(true, Some(KeySource::Profile), true),
            Credential::User
        );
        assert_eq!(choose(true, Some(KeySource::Env), true), Credential::ApiKey);
        assert_eq!(choose(true, Some(KeySource::Env), false), Credential::User);
        assert_eq!(
            choose(false, Some(KeySource::Profile), true),
            Credential::ApiKey
        );
        assert_eq!(
            choose(false, Some(KeySource::Env), false),
            Credential::Refuse
        );
        assert_eq!(choose(false, None, false), Credential::User);
    }

    #[test]
    fn refusal_says_what_to_do() {
        let message = refusal("lakehouse");
        assert!(message.contains("gz auth login") && message.contains("lakehouse"));
    }
}
