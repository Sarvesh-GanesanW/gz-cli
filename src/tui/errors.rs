//! Failures in plain language. Every TUI error message is made here, so a
//! screen says "You don't have access to connections" instead of an HTTP
//! dump, and keeps the server's own message when it sent one.

use std::sync::LazyLock;

use regex::Regex;

/// `GET url → HTTP 403 Forbidden: message` or `→ status 403: message`, as
/// `http.rs` reports them.
static HTTP_STATUS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"→ (?:HTTP|status) (\d{3})[^:]*(?::\s*(.*))?$")
        .expect("the HTTP status pattern is a valid regex")
});
/// Providers that answer 200 with `{success: false, message}`.
static ENVELOPE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"→ (.+)$").expect("the envelope pattern is a valid regex"));

/// What went wrong, read from a transport error string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    Http {
        status: u16,
        message: Option<String>,
    },
    /// DNS, refused or reset connections.
    Network,
    Timeout,
    /// The server said no without a status (`{success: false}`).
    Refused(String),
    Other(String),
}

/// Who and what a message is about: "connections" and the "connections"
/// service, or "this connection" and "lakehouse".
#[derive(Debug, Clone, Copy)]
pub struct Subject<'a> {
    pub what: &'a str,
    pub service: &'a str,
}

pub fn parse(raw: &str) -> Failure {
    let first_line = raw.lines().next().unwrap_or(raw).trim();
    if let Some(caps) = HTTP_STATUS.captures(first_line) {
        let status = caps[1].parse().unwrap_or(0);
        let message = caps
            .get(2)
            .map(|m| m.as_str().trim().to_string())
            .filter(|m| !m.is_empty());
        return Failure::Http { status, message };
    }
    // Transport failures never carry the "→" that server answers do.
    if let Some(caps) = ENVELOPE.captures(first_line) {
        return Failure::Refused(caps[1].trim().to_string());
    }
    let lower = raw.to_lowercase();
    if lower.contains("timed out") || lower.contains("timeout") {
        return Failure::Timeout;
    }
    if [
        "dns error",
        "connect",
        "connection reset",
        "error sending request",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        return Failure::Network;
    }
    Failure::Other(first_line.to_string())
}

/// One readable line with a next step.
pub fn explain(raw: &str, subject: Subject) -> String {
    let Subject { what, service } = subject;
    match parse(raw) {
        Failure::Http {
            status: 401,
            message,
        } if is_permission_denial(service, message.as_deref()) => {
            with_server_message(format!("You don't have access to {what}."), message)
        }
        Failure::Http { status: 401, .. } => {
            "Your sign-in has expired. Run `gz auth login`, then press r.".to_string()
        }
        Failure::Http {
            status: 403,
            message,
        } => with_server_message(format!("You don't have access to {what}."), message),
        Failure::Http {
            status: 404,
            message,
        } => with_server_message(
            format!("{} was deleted or never existed.", capitalized(what)),
            message,
        ),
        Failure::Http { status, message } if status >= 500 => with_server_message(
            format!("The {service} service had an error. Press r to retry."),
            message,
        ),
        Failure::Http {
            message: Some(message),
            ..
        } => message,
        Failure::Http { status, .. } => format!("The {service} service refused this ({status})."),
        Failure::Network => {
            format!("Can't reach the {service} service. Check your network or VPN, then press r.")
        }
        Failure::Timeout => {
            format!("The {service} service took too long to answer. Press r to retry.")
        }
        Failure::Refused(message) | Failure::Other(message) => message,
    }
}

/// Services whose 401 means "not allowed", not an expired sign-in: the
/// agent builder answers `AccessDenied` with 401, and the MLOps provider
/// uses 401 for permission checks. gz refreshes tokens itself, so a real
/// expiry is rare there.
const SERVICES_DENYING_WITH_401: &[&str] = &["agents", "mlops"];

/// Words a permission check puts in its 401 message (Lambda and ETL access
/// checks say "You do not have … permissions on …").
const DENIAL_WORDS: &[&str] = &[
    "permission",
    "accessdenied",
    "access denied",
    "not authorized",
    "do not have",
];

/// Whether a 401 is a permission check rather than an expired sign-in.
fn is_permission_denial(service: &str, message: Option<&str>) -> bool {
    let says_denied = message.is_some_and(|message| {
        let lower = message.to_lowercase();
        DENIAL_WORDS.iter().any(|word| lower.contains(word))
    });
    says_denied || SERVICES_DENYING_WITH_401.contains(&service)
}

fn with_server_message(friendly: String, message: Option<String>) -> String {
    match message {
        Some(message) => format!("{friendly} ({message})"),
        None => friendly,
    }
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONNECTIONS: Subject = Subject {
        what: "connections",
        service: "connections",
    };

    #[test]
    fn reads_status_and_server_message() {
        assert_eq!(
            parse("GET https://x/connection → HTTP 403 Forbidden: You do not have \"List\" permissions"),
            Failure::Http {
                status: 403,
                message: Some("You do not have \"List\" permissions".into())
            }
        );
        assert_eq!(
            parse("POST u → status 404: missing"),
            Failure::Http {
                status: 404,
                message: Some("missing".into())
            }
        );
    }

    #[test]
    fn statuses_become_plain_sentences() {
        let expired = explain("GET u → HTTP 401 Unauthorized: Unauthorized", CONNECTIONS);
        assert!(expired.contains("gz auth login"));
        let denied = explain(
            "GET u → HTTP 403 Forbidden: no List permission",
            CONNECTIONS,
        );
        assert_eq!(
            denied,
            "You don't have access to connections. (no List permission)"
        );
        let gone = explain("GET u → HTTP 404 Not Found: ", CONNECTIONS);
        assert_eq!(gone, "Connections was deleted or never existed.");
        let broken = explain(
            "GET u → HTTP 502 Bad Gateway: Internal server error",
            CONNECTIONS,
        );
        assert!(broken.starts_with("The connections service had an error."));
        assert!(broken.ends_with("(Internal server error)"));
    }

    #[test]
    fn permission_401s_are_not_sign_in_expiry() {
        let agents = Subject {
            what: "this agent",
            service: "agents",
        };
        let denied = explain(
            r#"GET u → HTTP 401 Unauthorized: You do not have "All" or "Read" permission on agent."#,
            agents,
        );
        assert!(denied.starts_with("You don't have access to this agent."));
        assert!(!denied.contains("gz auth login"));
        let mlops = Subject {
            what: "models",
            service: "mlops",
        };
        assert_eq!(
            explain("GET u → HTTP 401 Unauthorized", mlops),
            "You don't have access to models."
        );
        let lambda = explain(
            "GET u → HTTP 401 Unauthorized: You do not have List permissions on dataset",
            CONNECTIONS,
        );
        assert!(lambda.starts_with("You don't have access"));
        let expired = explain("GET u → HTTP 401 Unauthorized: Unauthorized", CONNECTIONS);
        assert!(
            expired.contains("gz auth login"),
            "plain 401s still mean sign in"
        );
    }

    #[test]
    fn other_client_errors_show_the_server_message() {
        let msg = explain(
            "POST u → HTTP 400 Bad Request: siteName is required",
            CONNECTIONS,
        );
        assert_eq!(msg, "siteName is required");
    }

    #[test]
    fn network_and_timeouts_name_the_service() {
        let offline = explain(
            "request failed: GET https://x: error sending request: dns error",
            CONNECTIONS,
        );
        assert!(offline.starts_with("Can't reach the connections service"));
        let slow = explain("request failed: GET u: operation timed out", CONNECTIONS);
        assert!(slow.contains("took too long"));
    }

    #[test]
    fn envelope_refusals_keep_their_message() {
        let msg = explain("POST https://x/storage → Connection not found", CONNECTIONS);
        assert_eq!(msg, "Connection not found");
    }
}
