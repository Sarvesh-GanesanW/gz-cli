//! Spotting credentials in free text, so gz never stores one where it would
//! be sent to the model on every turn (memories, for now).

use regex::Regex;
use std::sync::OnceLock;

/// Well-known token shapes, plus `password = …` style assignments.
const PATTERNS: &[&str] = &[
    // AWS access key ids.
    r"\b(AKIA|ASIA)[0-9A-Z]{16}\b",
    // GitHub tokens.
    r"\b(ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{30,}\b",
    r"\bgithub_pat_[A-Za-z0-9_]{40,}\b",
    // Slack tokens.
    r"\bxox[abprs]-[A-Za-z0-9-]{10,}",
    // OpenAI, Anthropic and similar `sk-` keys; Stripe live keys.
    r"\bsk-[A-Za-z0-9_-]{20,}",
    r"\b(sk|rk)_live_[A-Za-z0-9]{16,}",
    // Google API keys.
    r"\bAIza[0-9A-Za-z_-]{35}\b",
    // JSON web tokens.
    r"\beyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}",
    // PEM private keys.
    r"-----BEGIN [A-Z ]*PRIVATE KEY-----",
    // Credentials in URLs.
    r"[a-z][a-z0-9+.-]*://[^\s/:@]+:[^\s/@]+@",
    // `password: hunter2`, `api_key=…`, `token = "…"`.
    r#"(?i)\b(password|passwd|secret|api[_-]?key|access[_-]?key|token|private[_-]?key)\b\s*[:=]\s*["']?[^\s"']{6,}"#,
];

/// Shortest unbroken run of key-like characters treated as a secret even
/// without a known prefix.
const OPAQUE_MIN_CHARS: usize = 32;

fn patterns() -> &'static [Regex] {
    static COMPILED: OnceLock<Vec<Regex>> = OnceLock::new();
    COMPILED.get_or_init(|| {
        PATTERNS
            .iter()
            .map(|p| Regex::new(p).expect("the secret patterns are valid regexes"))
            .collect()
    })
}

/// Whether `text` seems to hold a key, token or password.
pub fn looks_like_secret(text: &str) -> bool {
    patterns().iter().any(|p| p.is_match(text)) || text.split_whitespace().any(is_opaque_token)
}

/// A long run mixing letters and digits with no spaces, like a random key
/// (`3f9aQ…`); plain words, paths and URLs don't count.
fn is_opaque_token(word: &str) -> bool {
    let word = word.trim_matches(|c: char| "\"'`.,;()[]{}<>".contains(c));
    if word.chars().count() < OPAQUE_MIN_CHARS || word.contains('/') || word.contains('.') {
        return false;
    }
    let key_like = word
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+' | '='));
    let has_digit = word.chars().any(|c| c.is_ascii_digit());
    let has_letter = word.chars().any(|c| c.is_ascii_alphabetic());
    key_like && has_digit && has_letter
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spots_common_credentials() {
        for text in [
            "my key is AKIAIOSFODNN7EXAMPLE",
            "use ghp_abcdefghijklmnopqrstuvwxyz0123456789",
            "sk-ant-api03-abcdefghijklmnopqrstuvwxyz",
            "token=eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N",
            "-----BEGIN OPENSSH PRIVATE KEY-----",
            "db is postgres://admin:hunter22@db.internal/app",
            "password: correct-horse",
            "API_KEY = 'abcdef123456'",
            "the build secret is 9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
        ] {
            assert!(looks_like_secret(text), "{text}");
        }
    }

    #[test]
    fn leaves_ordinary_preferences_alone() {
        for text in [
            "Uses snake_case for Python",
            "Prefers pnpm over npm; run tests with `make test`",
            "The token budget for subagents is 200k",
            "Docs live at https://docs.example.com/guides/getting-started",
            "Repo is at ~/code/gz-cli/src/assistant/local_context.rs",
        ] {
            assert!(!looks_like_secret(text), "{text}");
        }
    }
}
