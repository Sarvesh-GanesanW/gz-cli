//! Facts about the signed-in user that screens gate on. Today: whether the
//! user is a site admin, so admin-only actions are offered only to admins,
//! as the web app does (several of those routes have no permission check
//! of their own, so hiding them is the only guard).

use std::sync::atomic::{AtomicBool, Ordering};

use base64::Engine;
use serde_json::Value;

use crate::config::Resolved;

/// Set once at start-up from the sign-in tokens; read while drawing.
static ADMIN: AtomicBool = AtomicBool::new(false);

pub fn set_admin(admin: bool) {
    ADMIN.store(admin, Ordering::Relaxed);
}

pub fn is_admin() -> bool {
    ADMIN.load(Ordering::Relaxed)
}

/// The web's rule (`auth/helper/isAdmin.js`): the access token's
/// `cognito:groups` holds a group named like the client, ignoring case.
/// The ID token carries the same claim, so it is the fallback.
pub fn admin_in(resolved: &Resolved) -> bool {
    let Some(client) = resolved.client() else {
        return false;
    };
    [&resolved.profile.access_token, &resolved.profile.token]
        .into_iter()
        .flatten()
        .any(|token| {
            token_groups(token)
                .iter()
                .any(|group| group.eq_ignore_ascii_case(&client))
        })
}

/// The `cognito:groups` claim of a JWT, without verifying it: the server
/// checks the token; this only decides what to offer.
fn token_groups(token: &str) -> Vec<String> {
    let Some(payload) = token.split('.').nth(1) else {
        return Vec::new();
    };
    let Ok(bytes) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload) else {
        return Vec::new();
    };
    let Ok(claims) = serde_json::from_slice::<Value>(&bytes) else {
        return Vec::new();
    };
    claims["cognito:groups"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|g| g.as_str().map(str::to_string))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn jwt(claims: Value) -> String {
        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        format!(
            "{}.{}.sig",
            engine.encode(br#"{"alg":"none"}"#),
            engine.encode(claims.to_string())
        )
    }

    fn resolved_with(token: String) -> Resolved {
        let config = crate::config::Config::default();
        let mut resolved = crate::config::resolve(&config, Some("t"), None, None);
        resolved.profile.client = Some("Acme".into());
        resolved.profile.access_token = Some(token);
        resolved
    }

    #[test]
    fn admins_are_in_the_group_named_after_the_client() {
        let admin = resolved_with(jwt(json!({"cognito:groups": ["developers", "acme"]})));
        assert!(admin_in(&admin));
        let member = resolved_with(jwt(json!({"cognito:groups": ["developers"]})));
        assert!(!admin_in(&member));
    }

    #[test]
    fn unreadable_tokens_are_not_admin() {
        assert!(!admin_in(&resolved_with("not-a-jwt".into())));
        assert!(!admin_in(&resolved_with(jwt(json!({"sub": "x"})))));
    }
}
