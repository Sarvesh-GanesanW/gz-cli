//! Redaction for `gz bug-report`: settings go into a file people attach to
//! public issues, so anything that could be a credential or identify the
//! user is replaced, judged by its key name and by the shape of its value.

use std::path::Path;

pub const REDACTED: &str = "[redacted]";
pub const REDACTED_PERSONAL: &str = "[redacted: personal]";

/// Whole words in a key that mark its value as a credential (`api_key`,
/// `signing_key`, `db_pwd`).
const SECRET_WORDS: &[&str] = &[
    "key", "keys", "pwd", "pass", "pin", "otp", "sig", "salt", "nonce", "cookie", "cookies",
];
/// Fragments that mark a credential however the key is spelled
/// (`refreshToken`, `client_secret`, `x-api-key`, `PRIVATEKEY`).
const SECRET_FRAGMENTS: &[&str] = &[
    "token",
    "secret",
    "password",
    "passwd",
    "passphrase",
    "credential",
    "apikey",
    "privatekey",
    "accesskey",
    "authorization",
    "signature",
    "bearer",
    "jwt",
];
/// Fragments that mark the user's identity.
const PERSONAL_FRAGMENTS: &[&str] = &["username", "email", "phone"];
/// Opaque strings at least this long, with letters and digits and no
/// spaces or path separators, are treated as credentials whatever the key.
const OPAQUE_MIN_CHARS: usize = 32;

/// Redacts one TOML value found under `key`, recursing into tables and
/// arrays. `home` is shown as `~` so paths don't name the user.
pub fn redact_value(key: &str, value: &toml::Value, home: Option<&Path>) -> toml::Value {
    if is_personal_key(key) {
        return toml::Value::String(REDACTED_PERSONAL.to_string());
    }
    if is_secret_key(key) {
        return toml::Value::String(REDACTED.to_string());
    }
    match value {
        toml::Value::String(text) => toml::Value::String(redact_text(text, home)),
        toml::Value::Table(table) => toml::Value::Table(redact_table(table, home)),
        toml::Value::Array(items) => toml::Value::Array(
            items
                .iter()
                .map(|item| redact_value(key, item, home))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// [`redact_value`] for every entry of a table.
pub fn redact_table(table: &toml::Table, home: Option<&Path>) -> toml::Table {
    table
        .iter()
        .map(|(key, value)| (key.clone(), redact_value(key, value, home)))
        .collect()
}

/// A string value whose key looked harmless: still redacted when its shape
/// says credential; URLs lose query strings and user info; the home folder
/// becomes `~`.
pub fn redact_text(text: &str, home: Option<&Path>) -> String {
    if looks_like_credential(text) {
        return REDACTED.to_string();
    }
    let carries_extras = text.contains(['?', '#', '@']);
    if text.contains("://") && carries_extras {
        return crate::error_log::strip_url(text);
    }
    match home.map(|home| home.to_string_lossy().to_string()) {
        Some(home) if !home.is_empty() && text.starts_with(&home) => {
            format!("~{}", &text[home.len()..])
        }
        _ => text.to_string(),
    }
}

pub fn is_secret_key(key: &str) -> bool {
    let words = key_words(key);
    let joined = words.concat();
    words
        .iter()
        .any(|word| SECRET_WORDS.contains(&word.as_str()))
        || SECRET_FRAGMENTS
            .iter()
            .any(|fragment| joined.contains(fragment))
}

fn is_personal_key(key: &str) -> bool {
    let joined = key_words(key).concat();
    PERSONAL_FRAGMENTS
        .iter()
        .any(|fragment| joined.contains(fragment))
}

/// `refreshToken`, `refresh-token`, `REFRESH_TOKEN` → `["refresh", "token"]`.
fn key_words(key: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut previous_lower = false;
    for c in key.chars() {
        if !c.is_alphanumeric() {
            push_word(&mut words, &mut current);
            previous_lower = false;
            continue;
        }
        if c.is_uppercase() && previous_lower {
            push_word(&mut words, &mut current);
        }
        previous_lower = c.is_lowercase() || c.is_ascii_digit();
        current.extend(c.to_lowercase());
    }
    push_word(&mut words, &mut current);
    words
}

fn push_word(words: &mut Vec<String>, current: &mut String) {
    if !current.is_empty() {
        words.push(std::mem::take(current));
    }
}

fn looks_like_credential(text: &str) -> bool {
    let text = text.trim();
    is_jwt(text)
        || crate::apikey::looks_like_key(text)
        || text.to_ascii_lowercase().starts_with("bearer ")
        || text.to_ascii_lowercase().starts_with("basic ")
        || is_opaque(text)
}

fn is_jwt(text: &str) -> bool {
    text.starts_with("eyJ") && text.matches('.').count() == 2 && !text.contains(' ')
}

/// A long run of token characters with letters and digits, that is not a
/// UUID, a path or a URL.
fn is_opaque(text: &str) -> bool {
    let token_chars = text
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '_' | '-' | '.'));
    text.len() >= OPAQUE_MIN_CHARS
        && token_chars
        && !text.starts_with('/')
        && !text.contains("//")
        && text.chars().any(|c| c.is_ascii_digit())
        && text.chars().any(|c| c.is_ascii_alphabetic())
        && !is_uuid(text)
}

fn is_uuid(text: &str) -> bool {
    let groups: Vec<&str> = text.split('-').collect();
    groups.iter().map(|g| g.len()).eq([8, 4, 4, 4, 12])
        && groups
            .iter()
            .all(|g| g.chars().all(|c| c.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redacted(toml_text: &str) -> toml::Table {
        let table: toml::Table = toml::from_str(toml_text).unwrap();
        redact_table(&table, Some(Path::new("/Users/ana")))
    }

    fn text_of(table: &toml::Table, key: &str) -> String {
        table[key].as_str().unwrap().to_string()
    }

    #[test]
    fn known_credentials_are_redacted() {
        let out = redacted(
            r#"
            token = "id-token-value"
            refresh_token = "r"
            access_token = "a"
            api_key = "sk_ci_ana_k1.secret"
            expires_at = 1700000000
            "#,
        );
        for key in ["token", "refresh_token", "access_token", "api_key"] {
            assert_eq!(text_of(&out, key), REDACTED, "{key}");
        }
        assert_eq!(out["expires_at"].as_integer(), Some(1_700_000_000));
    }

    #[test]
    fn unknown_keys_that_look_secret_are_redacted_in_any_spelling() {
        let out = redacted(
            r#"
            clientSecret = "x"
            "x-api-key" = "x"
            PRIVATEKEY = "x"
            db_pwd = "x"
            signing_key = "x"
            aws_secret_access_key = "x"
            Authorization = "x"
            webhook_signature = "x"
            mfa_otp = "x"
            sessionCookie = "x"
            my_password_hint = "x"
            "#,
        );
        for (key, value) in &out {
            assert_eq!(value.as_str(), Some(REDACTED), "{key}");
        }
    }

    #[test]
    fn ordinary_settings_are_kept() {
        let out = redacted(
            r#"
            host = "https://acme-admin-lakehouseprovider.acme.api.groundzero.cloud"
            auth_host = "https://auth.example.com"
            client = "acme"
            site = "main"
            domain = "groundzero.cloud"
            cognito_pool_id = "us-east-1_AbCdEf123"
            keyboard_layout = "us"
            monkey = "banana"
            code_effort = "high"
            sql_session = "0b6f3c1e-7d2a-4f7b-9a0e-3c5d1e2f4a6b"
            "#,
        );
        assert_eq!(text_of(&out, "client"), "acme");
        assert_eq!(text_of(&out, "auth_host"), "https://auth.example.com");
        assert_eq!(text_of(&out, "keyboard_layout"), "us");
        assert_eq!(text_of(&out, "monkey"), "banana");
        assert_eq!(text_of(&out, "cognito_pool_id"), "us-east-1_AbCdEf123");
        assert_eq!(
            text_of(&out, "sql_session"),
            "0b6f3c1e-7d2a-4f7b-9a0e-3c5d1e2f4a6b"
        );
    }

    #[test]
    fn nested_tables_and_arrays_are_redacted_too() {
        let out = redacted(
            r#"
            [future_feature]
            name = "kept"
            [future_feature.auth]
            client_secret = "x"
            region = "eu"
            [[future_feature.connections]]
            label = "warehouse"
            password = "x"
            [credentials]
            user = "x"
            region = "x"
            "#,
        );
        let feature = out["future_feature"].as_table().unwrap();
        assert_eq!(feature["name"].as_str(), Some("kept"));
        let auth = feature["auth"].as_table().unwrap();
        assert_eq!(auth["client_secret"].as_str(), Some(REDACTED));
        assert_eq!(auth["region"].as_str(), Some("eu"));
        let connection = feature["connections"].as_array().unwrap()[0]
            .as_table()
            .unwrap();
        assert_eq!(connection["label"].as_str(), Some("warehouse"));
        assert_eq!(connection["password"].as_str(), Some(REDACTED));
        assert_eq!(
            out["credentials"].as_str(),
            Some(REDACTED),
            "a whole table under a secret key goes"
        );
    }

    #[test]
    fn values_shaped_like_credentials_are_redacted_under_any_key() {
        let out = redacted(
            r#"
            note = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJhbmEifQ.c2lnbmF0dXJl"
            ci = "sk_ci_ana_k1.abcdef"
            header = "Bearer abc"
            blob = "AKIAxxxxxxxxxxxxxxxx9f8e7d6c5b4a3a2b1c"
            "#,
        );
        for (key, value) in &out {
            assert_eq!(value.as_str(), Some(REDACTED), "{key}");
        }
    }

    #[test]
    fn urls_lose_query_strings_and_user_info() {
        let out = redacted(
            r#"
            [services]
            files = "https://bob:hunter2@files.example.com/api?sig=abc"
            "#,
        );
        assert_eq!(
            out["services"]["files"].as_str(),
            Some("https://files.example.com/api")
        );
    }

    #[test]
    fn the_user_is_not_named() {
        let out = redacted(
            r#"
            username = "ana@example.com"
            contact_email = "ana@example.com"
            code_allowed_dirs = ["/Users/ana/code/app", "/srv/data"]
            "#,
        );
        assert_eq!(text_of(&out, "username"), REDACTED_PERSONAL);
        assert_eq!(text_of(&out, "contact_email"), REDACTED_PERSONAL);
        let dirs: Vec<&str> = out["code_allowed_dirs"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(toml::Value::as_str)
            .collect();
        assert_eq!(dirs, ["~/code/app", "/srv/data"]);
    }

    #[test]
    fn key_words_split_every_spelling() {
        assert_eq!(key_words("refreshToken"), ["refresh", "token"]);
        assert_eq!(key_words("x-api-key"), ["x", "api", "key"]);
        assert_eq!(key_words("AWS_SECRET"), ["aws", "secret"]);
    }
}
