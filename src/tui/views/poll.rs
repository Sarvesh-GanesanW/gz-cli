//! Refetching on an interval while a view is open. A poller lives inside
//! the view (or grid) it refreshes, so popping the view stops it.

use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct Poller {
    every: Duration,
    last: Option<Instant>,
    paused: bool,
}

impl Poller {
    pub fn every(every: Duration) -> Self {
        Self {
            every,
            last: None,
            paused: false,
        }
    }

    /// Whether a refetch is due at `now`; when it is, the interval restarts.
    /// The first call after creation only starts the clock, since the view
    /// fetched once when it opened.
    pub fn due(&mut self, now: Instant) -> bool {
        if self.paused {
            return false;
        }
        match self.last {
            None => {
                self.last = Some(now);
                false
            }
            Some(last) if now.duration_since(last) >= self.every => {
                self.last = Some(now);
                true
            }
            Some(_) => false,
        }
    }

    /// Restarts the interval, e.g. after a manual reload.
    pub fn restart(&mut self, now: Instant) {
        self.last = Some(now);
    }

    /// Stops polling (a run finished) until `resume`.
    #[allow(
        dead_code,
        reason = "spec vocabulary for the screens being converted next"
    )]
    pub fn pause(&mut self) {
        self.paused = true;
    }

    #[allow(
        dead_code,
        reason = "spec vocabulary for the screens being converted next"
    )]
    pub fn resume(&mut self) {
        self.paused = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fires_once_per_interval() {
        let start = Instant::now();
        let mut poller = Poller::every(Duration::from_secs(5));
        assert!(!poller.due(start));
        assert!(!poller.due(start + Duration::from_secs(4)));
        assert!(poller.due(start + Duration::from_secs(5)));
        assert!(!poller.due(start + Duration::from_secs(6)));
        assert!(poller.due(start + Duration::from_secs(10)));
    }

    #[test]
    fn paused_pollers_stay_quiet_until_resumed() {
        let start = Instant::now();
        let mut poller = Poller::every(Duration::from_secs(1));
        poller.due(start);
        poller.pause();
        assert!(!poller.due(start + Duration::from_secs(5)));
        poller.resume();
        assert!(poller.due(start + Duration::from_secs(6)));
    }

    #[test]
    fn restarting_pushes_the_next_fetch_back() {
        let start = Instant::now();
        let mut poller = Poller::every(Duration::from_secs(5));
        poller.due(start);
        poller.restart(start + Duration::from_secs(4));
        assert!(!poller.due(start + Duration::from_secs(6)));
        assert!(poller.due(start + Duration::from_secs(9)));
    }
}
