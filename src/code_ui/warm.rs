//! Keeps the chat server ready while the user types: a renewed ID token and
//! an open connection, so pressing Enter sends at once instead of first
//! paying a token refresh and a TLS handshake. Load balancers close idle
//! connections, so the warm-up is repeated while the user types, at most
//! once per [`REWARM_AFTER`].

use std::time::{Duration, Instant};

use crate::http::ApiClient;

/// Shorter than common load balancer idle timeouts (60 s), so a connection
/// warmed while typing is still open at Enter.
const REWARM_AFTER: Duration = Duration::from_secs(20);
const CHAT: &str = "chat";

#[derive(Default)]
pub struct Warmer {
    last: Option<Instant>,
}

impl Warmer {
    /// Warms the chat server in the background unless that was done
    /// recently.
    pub fn poke(&mut self, http: &ApiClient) {
        if !self.is_due(Instant::now()) {
            return;
        }
        self.last = Some(Instant::now());
        let http = http.clone();
        tokio::spawn(async move { http.warm_up(CHAT).await });
    }

    fn is_due(&self, now: Instant) -> bool {
        self.last
            .is_none_or(|last| now.saturating_duration_since(last) >= REWARM_AFTER)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warms_at_first_then_at_most_every_twenty_seconds() {
        let start = Instant::now();
        let mut warmer = Warmer::default();
        assert!(warmer.is_due(start));
        warmer.last = Some(start);
        assert!(!warmer.is_due(start + Duration::from_secs(5)));
        assert!(warmer.is_due(start + REWARM_AFTER));
    }
}
