//! When a failed request is tried again, and after how long.
//!
//! A request is retried only when sending it twice cannot do anything twice:
//! - any request whose connection never opened (nothing reached the server);
//! - GET and HEAD, on a dropped connection, a 5xx other than 504, or a 429;
//! - a chat POST carrying an `X-Request-ID`, the same ways: the chat server
//!   answers a repeated id with the run the first request started instead
//!   of starting another (gz-chat api/routes/chat/run_start.py:219-246,
//!   chat_client_tools.py:114-139, chat_approvals.py, chat_steering.py).
//!   The retry reuses the request as built, so the id is the same.
//!
//! A timeout is never retried, and neither is a 504: both mean the server
//! may still be working on the request, so a retry would repeat slow work
//! and multiply the wait by the number of tries.

use std::time::Duration;

/// The service whose POSTs dedupe by [`REQUEST_ID`].
pub const DEDUPING_SERVICE: &str = "chat";
pub const REQUEST_ID: &str = "X-Request-ID";
/// Tries per request, the first included.
pub const MAX_ATTEMPTS: u32 = 4;
const FIRST_DELAY: Duration = Duration::from_millis(500);
const MAX_DELAY: Duration = Duration::from_secs(4);
/// The longest `Retry-After` gz waits out; a longer one fails at once and
/// the error says how long the server asked for.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(20);

/// How a try failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// The connection never opened.
    Connect,
    /// It broke after the request may have been sent.
    Transport,
    /// No answer came within the client timeout.
    Timeout,
    /// The server answered with this status and, maybe, `Retry-After`
    /// in seconds.
    Status(u16, Option<u64>),
}

/// How long to wait before the next try after `failures` failed tries
/// (the last one being `failure`), or `None` to give up. `repeatable`
/// says whether the request may safely reach the server twice.
pub fn delay(failures: u32, failure: Failure, repeatable: bool) -> Option<Duration> {
    if failures >= MAX_ATTEMPTS {
        return None;
    }
    let backoff = FIRST_DELAY
        .saturating_mul(1 << (failures.saturating_sub(1)).min(8))
        .min(MAX_DELAY);
    match failure {
        Failure::Connect => Some(backoff),
        Failure::Transport if repeatable => Some(backoff),
        Failure::Status(status, retry_after) if repeatable && is_transient(status) => {
            match retry_after.map(Duration::from_secs) {
                Some(asked) if asked > MAX_RETRY_AFTER => None,
                Some(asked) => Some(asked.max(backoff)),
                None => Some(backoff),
            }
        }
        Failure::Transport | Failure::Timeout | Failure::Status(..) => None,
    }
}

const GATEWAY_TIMEOUT: u16 = 504;

/// Statuses that say "not now" rather than "no". A gateway timeout is left
/// out: the work behind it may still be running.
fn is_transient(status: u16) -> bool {
    status == 429 || ((500..600).contains(&status) && status != GATEWAY_TIMEOUT)
}

/// `Retry-After` as whole seconds (the HTTP-date form is not used by the
/// Groundzero services).
pub fn retry_after_secs(value: Option<&str>) -> Option<u64> {
    value?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_connection_that_never_opened_is_always_retried() {
        assert_eq!(
            delay(1, Failure::Connect, false),
            Some(Duration::from_millis(500))
        );
        assert_eq!(
            delay(2, Failure::Connect, false),
            Some(Duration::from_secs(1))
        );
        assert_eq!(delay(MAX_ATTEMPTS, Failure::Connect, false), None);
    }

    #[test]
    fn a_request_that_may_have_arrived_is_retried_only_when_repeatable() {
        assert_eq!(delay(1, Failure::Transport, false), None);
        assert!(delay(1, Failure::Transport, true).is_some());
        assert_eq!(delay(1, Failure::Status(503, None), false), None);
        assert!(delay(1, Failure::Status(503, None), true).is_some());
    }

    #[test]
    fn a_timeout_is_never_retried() {
        assert_eq!(delay(1, Failure::Timeout, true), None);
        assert_eq!(delay(1, Failure::Timeout, false), None);
    }

    #[test]
    fn a_gateway_timeout_is_never_retried() {
        assert_eq!(delay(1, Failure::Status(504, None), true), None);
    }

    #[test]
    fn only_not_now_statuses_are_retried() {
        assert!(delay(1, Failure::Status(429, None), true).is_some());
        assert!(delay(1, Failure::Status(500, None), true).is_some());
        assert_eq!(delay(1, Failure::Status(404, None), true), None);
        assert_eq!(delay(1, Failure::Status(409, None), true), None);
    }

    #[test]
    fn retry_after_is_honoured_up_to_a_limit() {
        assert_eq!(
            delay(1, Failure::Status(429, Some(3)), true),
            Some(Duration::from_secs(3))
        );
        assert_eq!(delay(1, Failure::Status(429, Some(600)), true), None);
        assert_eq!(retry_after_secs(Some(" 7 ")), Some(7));
        assert_eq!(retry_after_secs(Some("Wed, 21 Oct 2026")), None);
    }

    #[test]
    fn the_backoff_doubles_up_to_a_cap() {
        let waits: Vec<_> = (1..MAX_ATTEMPTS)
            .map(|n| delay(n, Failure::Connect, true).unwrap())
            .collect();
        assert_eq!(waits, [500, 1000, 2000].map(Duration::from_millis).to_vec());
    }
}
