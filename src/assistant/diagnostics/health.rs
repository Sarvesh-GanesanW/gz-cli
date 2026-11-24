//! Checkers that keep timing out. Every run is time-boxed, but a checker
//! that cannot finish (a cold build cache, a hung tool) would still cost up
//! to two timeouts on every edit. After one timeout gz stops running it
//! before edits (the after-edit run alone still reports problems), and
//! after three in a row (one edit can time out twice: before and after) it
//! stops running it for the rest of the session.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use super::checkers::Run;

/// Timeouts in a row that turn a checker off for the session.
pub const STRIKES_OFF: u32 = 3;

/// A checker in one folder: the same tool may hang in one project only.
type Key = (String, PathBuf);

#[derive(Default)]
pub struct Health {
    strikes: Mutex<HashMap<Key, u32>>,
}

/// What a finished run did to its checker's standing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Fine,
    /// Timed out; still run after edits, but no longer before them.
    Slow,
    /// Timed out [`STRIKES_OFF`] times in a row; off for the session.
    TurnedOff,
}

impl Health {
    fn strikes(&self, run: &Run) -> u32 {
        let strikes = self.strikes.lock().unwrap_or_else(|e| e.into_inner());
        strikes.get(&key(run)).copied().unwrap_or(0)
    }

    /// Whether `run` timed out too often to run again this session.
    pub fn is_off(&self, run: &Run) -> bool {
        self.strikes(run) >= STRIKES_OFF
    }

    /// Whether `run` timed out lately, so it is not run before edits.
    pub fn is_slow(&self, run: &Run) -> bool {
        self.strikes(run) > 0
    }

    /// Notes how a run of `run` ended.
    pub fn record(&self, run: &Run, timed_out: bool) -> Verdict {
        let mut strikes = self.strikes.lock().unwrap_or_else(|e| e.into_inner());
        if !timed_out {
            strikes.remove(&key(run));
            return Verdict::Fine;
        }
        let count = strikes.entry(key(run)).or_default();
        *count += 1;
        if *count >= STRIKES_OFF {
            Verdict::TurnedOff
        } else {
            Verdict::Slow
        }
    }
}

fn key(run: &Run) -> Key {
    (run.name.clone(), run.cwd.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn run(name: &str, cwd: &str) -> Run {
        Run {
            name: name.into(),
            command: String::new(),
            cwd: cwd.into(),
            timeout: Duration::from_secs(1),
            compare_before: true,
        }
    }

    #[test]
    fn timeouts_in_a_row_back_off_then_turn_off_and_a_finish_resets() {
        let health = Health::default();
        let vet = run("go vet", "/w/api");
        assert!(!health.is_slow(&vet));
        assert_eq!(health.record(&vet, true), Verdict::Slow);
        assert!(health.is_slow(&vet) && !health.is_off(&vet));
        assert_eq!(health.record(&vet, false), Verdict::Fine);
        assert!(!health.is_slow(&vet), "a finished run clears the strikes");
        for _ in 1..STRIKES_OFF {
            assert_eq!(health.record(&vet, true), Verdict::Slow);
        }
        assert_eq!(health.record(&vet, true), Verdict::TurnedOff);
        assert!(health.is_off(&vet));
        assert!(
            !health.is_slow(&run("go vet", "/w/web")),
            "another folder keeps its own standing"
        );
    }
}
