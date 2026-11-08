//! gz code's own spend per profile, local day and model, in
//! `code_spend.json` next to the gz config (owner-only). Each turn's
//! reported cost is added as it arrives, background subagents included, so
//! day caps hold across sessions and the TUI can show today and this month.
//! The file holds dates, model names and dollar amounts, nothing else.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::day;

const FILE: &str = "code_spend.json";
/// Older days are dropped past this, so the file stays small.
const KEEP_DAYS: usize = 400;
/// The model name used when the run's model isn't known.
pub const UNKNOWN_MODEL: &str = "default model";

/// One day's spend.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaySpend {
    pub total_usd: f64,
    #[serde(default)]
    pub models: BTreeMap<String, f64>,
}

impl DaySpend {
    fn add(&mut self, model: &str, usd: f64) {
        self.total_usd += usd;
        *self.models.entry(model.to_string()).or_insert(0.0) += usd;
    }
}

/// Every profile's days, by `2026-11-08`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Book {
    #[serde(default)]
    profiles: BTreeMap<String, BTreeMap<String, DaySpend>>,
}

/// Today and this month, for one profile.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Summary {
    pub today_usd: f64,
    pub month_usd: f64,
    /// This month per model, most spent first.
    pub month_models: Vec<(String, f64)>,
}

/// One profile's ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ledger {
    path: PathBuf,
    profile: String,
}

impl Ledger {
    /// The ledger in the gz config folder (honors `XDG_CONFIG_HOME`).
    pub fn open(profile: &str) -> Result<Self> {
        let config = crate::config::config_path()?;
        let dir = config.parent().context("config path has no folder")?;
        Ok(Self::at(dir.join(FILE), profile))
    }

    pub fn at(path: PathBuf, profile: &str) -> Self {
        Self {
            path,
            profile: profile.to_string(),
        }
    }

    #[cfg(test)]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Adds `usd` spent on `model` on `day`; returns the day's new total.
    pub fn record(&self, day: &str, model: Option<&str>, usd: f64) -> Result<f64> {
        let mut book = self.load()?;
        let days = book.profiles.entry(self.profile.clone()).or_default();
        let spend = days.entry(day.to_string()).or_default();
        spend.add(model.unwrap_or(UNKNOWN_MODEL), usd);
        let total = spend.total_usd;
        while days.len() > KEEP_DAYS {
            days.pop_first();
        }
        let text = serde_json::to_string_pretty(&book).context("cannot encode the ledger")?;
        crate::assistant::private_file::write_private(&self.path, &text)?;
        Ok(total)
    }

    /// What was spent on `day`.
    pub fn day_total(&self, day: &str) -> Result<f64> {
        Ok(self.days()?.get(day).map_or(0.0, |spend| spend.total_usd))
    }

    /// `day` and its month.
    pub fn summary(&self, day: &str) -> Result<Summary> {
        let days = self.days()?;
        let month = day::month_of(day);
        let mut models: BTreeMap<String, f64> = BTreeMap::new();
        let mut month_usd = 0.0;
        for spend in days
            .iter()
            .filter(|(date, _)| day::month_of(date) == month)
            .map(|(_, spend)| spend)
        {
            month_usd += spend.total_usd;
            for (model, usd) in &spend.models {
                *models.entry(model.clone()).or_insert(0.0) += usd;
            }
        }
        let mut month_models: Vec<(String, f64)> = models.into_iter().collect();
        month_models.sort_by(|a, b| b.1.total_cmp(&a.1));
        Ok(Summary {
            today_usd: days.get(day).map_or(0.0, |spend| spend.total_usd),
            month_usd,
            month_models,
        })
    }

    fn days(&self) -> Result<BTreeMap<String, DaySpend>> {
        Ok(self
            .load()?
            .profiles
            .remove(&self.profile)
            .unwrap_or_default())
    }

    /// A missing file is an empty ledger; a damaged one is an error, so it
    /// is never overwritten with less than it held.
    fn load(&self) -> Result<Book> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Book::default()),
            Err(err) => {
                return Err(err).with_context(|| format!("cannot read {}", self.path.display()))
            }
        };
        serde_json::from_str(&text).with_context(|| {
            format!(
                "cannot parse {}; fix or delete it to start counting again",
                self.path.display()
            )
        })
    }
}

#[cfg(test)]
pub(crate) fn temp_ledger(name: &str, profile: &str) -> Ledger {
    let dir = std::env::temp_dir().join(format!("gz-ledger-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    Ledger::at(dir.join(FILE), profile)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turns_add_up_per_day_and_model() {
        let ledger = temp_ledger("turns", "p");
        ledger
            .record("2026-11-08", Some("Claude Opus 5"), 0.40)
            .unwrap();
        ledger
            .record("2026-11-08", Some("Claude Haiku 5.5"), 0.05)
            .unwrap();
        let total = ledger
            .record("2026-11-08", Some("Claude Opus 5"), 0.10)
            .unwrap();
        assert!((total - 0.55).abs() < 1e-9);
        let summary = ledger.summary("2026-11-08").unwrap();
        assert!((summary.today_usd - 0.55).abs() < 1e-9);
        assert_eq!(summary.month_models[0].0, "Claude Opus 5");
        assert!((summary.month_models[0].1 - 0.50).abs() < 1e-9);
    }

    #[test]
    fn a_new_day_starts_at_zero_and_the_month_keeps_counting() {
        let ledger = temp_ledger("rollover", "p");
        ledger.record("2026-10-31", None, 3.0).unwrap();
        ledger
            .record("2026-11-07", Some("Claude Opus 5"), 1.0)
            .unwrap();
        ledger
            .record("2026-11-08", Some("Claude Opus 5"), 0.25)
            .unwrap();
        assert!((ledger.day_total("2026-11-08").unwrap() - 0.25).abs() < 1e-9);
        let summary = ledger.summary("2026-11-08").unwrap();
        assert!(
            (summary.month_usd - 1.25).abs() < 1e-9,
            "October is not November"
        );
        assert!(ledger.summary("2026-11-09").unwrap().today_usd == 0.0);
    }

    #[test]
    fn profiles_keep_their_own_spend() {
        let work = temp_ledger("profiles", "work");
        let other = Ledger::at(work.path.clone(), "other");
        work.record("2026-11-08", None, 2.0).unwrap();
        other.record("2026-11-08", None, 0.5).unwrap();
        assert_eq!(work.day_total("2026-11-08").unwrap(), 2.0);
        assert_eq!(other.day_total("2026-11-08").unwrap(), 0.5);
    }

    #[test]
    fn a_damaged_file_is_reported_and_left_alone() {
        let ledger = temp_ledger("damaged", "p");
        std::fs::create_dir_all(ledger.path.parent().unwrap()).unwrap();
        std::fs::write(&ledger.path, "{not json").unwrap();
        let err = ledger.record("2026-11-08", None, 1.0).unwrap_err();
        assert!(format!("{err:#}").contains("fix or delete it"), "{err:#}");
        assert_eq!(std::fs::read_to_string(&ledger.path).unwrap(), "{not json");
    }
}
