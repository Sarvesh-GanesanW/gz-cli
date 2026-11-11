//! `--max-scan` and the profile's `sql_max_scan`: `gz sql` refuses a query
//! whose estimated scan (see [`super::estimate`]) is over the limit, before
//! anything runs, and says how to run it anyway.

use anyhow::{bail, Context, Result};

use crate::http::ApiClient;
use crate::sql::estimate::{self, format_size, parse_size, ScanEstimate};

/// The word that turns the limit off for one command.
pub const OFF: &str = "off";

/// The limit a command runs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanLimit {
    /// No limit: nothing is estimated.
    Off,
    /// At most this many bytes, from `--max-scan` or the profile.
    Bytes { bytes: u64, from_flag: bool },
}

impl ScanLimit {
    /// `--max-scan` wins over the profile's `sql_max_scan`; neither means
    /// no limit.
    pub fn resolve(flag: Option<&str>, saved: Option<&str>) -> Result<Self> {
        match (flag, saved) {
            (Some(text), _) => Self::parse(text, true).context("--max-scan"),
            (None, Some(text)) => Self::parse(text, false)
                .context("the profile's sql_max_scan; change it with `gz sql max-scan`"),
            (None, None) => Ok(ScanLimit::Off),
        }
    }

    pub fn parse(text: &str, from_flag: bool) -> Result<Self> {
        if text.trim().eq_ignore_ascii_case(OFF) {
            return Ok(ScanLimit::Off);
        }
        Ok(ScanLimit::Bytes {
            bytes: parse_size(text)?,
            from_flag,
        })
    }

    /// Why `estimate` may not run under this limit, or `None` when it may.
    pub fn refusal(&self, estimate: &ScanEstimate) -> Option<String> {
        let ScanLimit::Bytes { bytes, from_flag } = *self else {
            return None;
        };
        if estimate.read.bytes <= bytes {
            return None;
        }
        let source = if from_flag {
            "--max-scan"
        } else {
            "sql_max_scan in this profile"
        };
        Some(format!(
            "This query reads {}, over the {} limit ({source}). Nothing ran. Narrow it \
             (a filter on a partition column, fewer tables), or run it anyway with \
             --max-scan {} or --max-scan {OFF}.",
            estimate.amount(),
            format_size(bytes),
            override_size(estimate.read.bytes),
        ))
    }
}

/// The smallest whole number of GB (or MB below one GB) that lets `bytes`
/// through, as `--max-scan` takes it.
pub fn override_size(bytes: u64) -> String {
    const MB: u64 = 1 << 20;
    const GB: u64 = 1 << 30;
    if bytes < GB {
        format!("{}MB", bytes.div_ceil(MB).max(1))
    } else {
        format!("{}GB", bytes.div_ceil(GB))
    }
}

/// Estimates `sql` and refuses it when it is over `limit`. A query that
/// cannot be estimated is refused too: the limit was asked for, so it is
/// not skipped silently.
pub async fn enforce(
    http: &ApiClient,
    warehouse: &str,
    sql: &str,
    limit: ScanLimit,
) -> Result<Option<ScanEstimate>> {
    if limit == ScanLimit::Off {
        return Ok(None);
    }
    let estimate = estimate::estimate(http, warehouse, sql)
        .await
        .with_context(|| {
            format!("cannot estimate the scan to check the limit; run with --max-scan {OFF} to skip the check")
        })?;
    if let Some(reason) = limit.refusal(&estimate) {
        let detail = estimate.lines()[1..].join("\n");
        bail!("{reason}\n{detail}");
    }
    Ok(Some(estimate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::estimate::Totals;
    use crate::table_history::TableName;

    fn reading(bytes: u64) -> ScanEstimate {
        let table = TableName {
            database: "DemoDB".into(),
            table: "inventory".into(),
        };
        ScanEstimate::upper_bound(
            vec![(
                table,
                Totals {
                    bytes,
                    files: 1,
                    rows: 1,
                },
            )],
            Vec::new(),
        )
    }

    #[test]
    fn the_flag_wins_over_the_profile_and_off_turns_it_off() {
        assert_eq!(
            ScanLimit::resolve(Some("2GB"), Some("1GB")).unwrap(),
            ScanLimit::Bytes {
                bytes: 2 << 30,
                from_flag: true
            }
        );
        assert_eq!(
            ScanLimit::resolve(Some("off"), Some("1GB")).unwrap(),
            ScanLimit::Off
        );
        assert_eq!(ScanLimit::resolve(None, None).unwrap(), ScanLimit::Off);
        assert!(ScanLimit::resolve(None, Some("lots")).is_err());
    }

    #[test]
    fn a_query_over_the_limit_is_refused_with_the_override() {
        let limit = ScanLimit::resolve(None, Some("1GB")).unwrap();
        let reason = limit.refusal(&reading(9_530_959_833)).unwrap();
        assert!(
            reason.contains(
                "reads up to 8.9 GB, over the 1.0 GB limit (sql_max_scan in this profile)"
            ),
            "{reason}"
        );
        assert!(
            reason.contains("--max-scan 9GB or --max-scan off"),
            "{reason}"
        );
        assert_eq!(limit.refusal(&reading(1 << 30)), None);
    }

    #[test]
    fn the_override_rounds_up_to_whole_units() {
        assert_eq!(override_size(25_116), "1MB");
        assert_eq!(override_size(1_224_791_258), "2GB");
        assert_eq!(override_size(1 << 30), "1GB");
    }
}
