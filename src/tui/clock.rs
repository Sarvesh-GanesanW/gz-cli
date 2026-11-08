//! The time the app acts on, passed in rather than read inside, so polling
//! and relative times can be tested with a fake clock.

use std::time::Instant;

use crate::tui::format;

#[derive(Debug, Clone, Copy)]
pub struct Now {
    /// Monotonic time, for intervals.
    pub instant: Instant,
    /// Wall-clock milliseconds since the epoch, for "3h ago".
    pub epoch_ms: i64,
}

impl Now {
    pub fn system() -> Self {
        Self {
            instant: Instant::now(),
            epoch_ms: format::system_now_ms(),
        }
    }

    /// This moment plus `by` (tests step the clock with it).
    #[cfg(test)]
    pub fn advanced(self, by: std::time::Duration) -> Self {
        Self {
            instant: self.instant + by,
            epoch_ms: self.epoch_ms + by.as_millis() as i64,
        }
    }
}
