//! Adds each reported cost to the caps and the local ledger, moving to a
//! new day when the local date changes. Shared by the interactive UI and
//! the plain `gz code` modes.

use super::caps::{Alert, Caps, Guard};
use super::day;
use super::ledger::{Ledger, Summary};

/// Today's local date; tests pass their own.
pub type Today = Box<dyn Fn() -> String + Send>;

pub struct Meter {
    pub guard: Guard,
    ledger: Option<Ledger>,
    today: Today,
}

/// What recording one cost turned up.
#[derive(Debug, Default, PartialEq)]
pub struct Recorded {
    pub alerts: Vec<Alert>,
    /// Why the ledger couldn't be saved (day caps then count this session only).
    pub ledger_error: Option<String>,
}

impl Meter {
    pub fn new(caps: Caps, ledger: Option<Ledger>) -> Self {
        Self::with_clock(caps, ledger, Box::new(day::today))
    }

    pub fn with_clock(caps: Caps, ledger: Option<Ledger>, today: Today) -> Self {
        let day = today();
        let spent_today = ledger
            .as_ref()
            .and_then(|l| l.day_total(&day).ok())
            .unwrap_or(0.0);
        Self {
            guard: Guard::new(caps, &day, spent_today),
            ledger,
            today,
        }
    }

    /// Moves the caps to today if the date changed since the last cost.
    pub fn catch_up(&mut self) {
        let today = (self.today)();
        let spent_today = self
            .ledger
            .as_ref()
            .and_then(|l| l.day_total(&today).ok())
            .unwrap_or(0.0);
        self.guard.roll_to(&today, spent_today);
    }

    /// Adds `usd` spent on `model` now.
    pub fn record(&mut self, usd: f64, model: Option<&str>) -> Recorded {
        self.catch_up();
        let today = (self.today)();
        let saved = self
            .ledger
            .as_ref()
            .map(|ledger| ledger.record(&today, model, usd));
        let (day_total, ledger_error) = match saved {
            Some(Ok(total)) => (Some(total), None),
            Some(Err(err)) => (None, Some(format!("{err:#}"))),
            None => (None, None),
        };
        Recorded {
            alerts: self.guard.spent(usd, day_total),
            ledger_error,
        }
    }

    /// Today and this month from the ledger; `None` without one.
    pub fn summary(&self) -> Option<Result<Summary, String>> {
        let ledger = self.ledger.as_ref()?;
        Some(
            ledger
                .summary(&(self.today)())
                .map_err(|err| format!("{err:#}")),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spend::ledger::temp_ledger;
    use crate::spend::Level;
    use std::sync::{Arc, Mutex};

    fn clock(day: &Arc<Mutex<String>>) -> Today {
        let day = day.clone();
        Box::new(move || day.lock().unwrap().clone())
    }

    #[test]
    fn day_caps_count_earlier_sessions_and_reset_at_midnight() {
        let ledger = temp_ledger("meter-rollover", "p");
        ledger.record("2026-11-08", None, 4.0).unwrap();
        let day = Arc::new(Mutex::new("2026-11-08".to_string()));
        let caps = Caps {
            session_usd: None,
            day_usd: Some(5.0),
        };
        let mut meter = Meter::with_clock(caps, Some(ledger.clone()), clock(&day));
        let recorded = meter.record(0.5, Some("Claude Opus 5"));
        assert_eq!(
            recorded.alerts[0].level,
            Level::Warn,
            "4.5 of 5 counts the earlier session"
        );
        meter.record(0.6, None);
        assert!(!meter.guard.may_start());

        *day.lock().unwrap() = "2026-11-09".to_string();
        meter.catch_up();
        assert!(meter.guard.may_start(), "a new day starts under the cap");
        meter.record(0.2, Some("Claude Opus 5"));
        assert!((ledger.day_total("2026-11-08").unwrap() - 5.1).abs() < 1e-9);
        assert!((ledger.day_total("2026-11-09").unwrap() - 0.2).abs() < 1e-9);
        assert!((meter.guard.session_usd() - 1.3).abs() < 1e-9);
    }

    #[test]
    fn spend_made_elsewhere_today_counts_at_the_next_cost() {
        let ledger = temp_ledger("meter-elsewhere", "p");
        let day = Arc::new(Mutex::new("2026-11-08".to_string()));
        let caps = Caps {
            session_usd: None,
            day_usd: Some(1.0),
        };
        let mut meter = Meter::with_clock(caps, Some(ledger.clone()), clock(&day));
        ledger.record("2026-11-08", None, 0.95).unwrap();
        meter.record(0.1, None);
        assert!(!meter.guard.may_start());
    }

    #[test]
    fn an_unwritable_ledger_still_counts_the_session() {
        let ledger = temp_ledger("meter-broken", "p");
        std::fs::create_dir_all(ledger_dir(&ledger)).unwrap();
        std::fs::write(ledger_dir(&ledger).join("code_spend.json"), "{oops").unwrap();
        let caps = Caps {
            session_usd: Some(1.0),
            day_usd: None,
        };
        let mut meter = Meter::with_clock(caps, Some(ledger), Box::new(|| "2026-11-08".into()));
        let recorded = meter.record(1.0, None);
        assert!(recorded.ledger_error.unwrap().contains("cannot parse"));
        assert!(!meter.guard.may_start());
    }

    fn ledger_dir(ledger: &Ledger) -> std::path::PathBuf {
        ledger.path().parent().unwrap().to_path_buf()
    }
}
