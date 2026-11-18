//! Time windows for commands that look back (`gz status --since`,
//! `gz usage compute --since`): `30m`, `6h`, `7d`, `2w`, or a UTC date or
//! time, and short spans like `2h 05m` for how long something ran.

use anyhow::{bail, Result};
use serde_json::Value;

use crate::tui::format;

const MINUTE_MS: i64 = 60 * 1000;
const HOUR_MS: i64 = 60 * MINUTE_MS;
const DAY_MS: i64 = 24 * HOUR_MS;
const WEEK_MS: i64 = 7 * DAY_MS;

/// The instant `text` names, in epoch milliseconds: an age back from
/// `now_ms` (`30m`, `6h`, `7d`, `2w`) or a UTC date or time
/// (`2026-11-01`, `2026-11-01T08:00`).
pub fn parse_since(text: &str, now_ms: i64) -> Result<i64> {
    let text = text.trim();
    if let Some(age) = parse_age(text) {
        return Ok(now_ms - age);
    }
    if let Some(at) = format::epoch_ms(&Value::String(text.to_string())) {
        if at > now_ms {
            bail!("--since {text} is in the future");
        }
        return Ok(at);
    }
    bail!(
        "--since takes an age like 30m, 6h, 7d or 2w, or a UTC date or time like 2026-11-01T08:00"
    )
}

fn parse_age(text: &str) -> Option<i64> {
    let unit = text.chars().last()?;
    let amount: i64 = text[..text.len() - unit.len_utf8()].parse().ok()?;
    let unit_ms = match unit {
        'm' => MINUTE_MS,
        'h' => HOUR_MS,
        'd' => DAY_MS,
        'w' => WEEK_MS,
        _ => return None,
    };
    (amount > 0).then_some(amount * unit_ms)
}

/// `45s`, `12m`, `2h 05m`, `3d 4h`.
pub fn span(ms: i64) -> String {
    let secs = ms.max(0) / 1000;
    let (days, hours, minutes) = (secs / 86_400, secs / 3_600 % 24, secs / 60 % 60);
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_763_460_000_000;

    #[test]
    fn ages_count_back_from_now() {
        assert_eq!(parse_since("30m", NOW).unwrap(), NOW - 30 * MINUTE_MS);
        assert_eq!(parse_since("6h", NOW).unwrap(), NOW - 6 * HOUR_MS);
        assert_eq!(parse_since("7d", NOW).unwrap(), NOW - 7 * DAY_MS);
        assert_eq!(parse_since("2w", NOW).unwrap(), NOW - 14 * DAY_MS);
    }

    #[test]
    fn dates_and_times_are_utc() {
        let at = parse_since("2025-11-01T08:00", NOW).unwrap();
        assert_eq!(format::iso8601_utc(at), "2025-11-01T08:00:00Z");
        assert!(parse_since("2025-11-01", NOW).is_ok());
    }

    #[test]
    fn refuses_nonsense_zero_and_the_future() {
        for bad in ["", "0d", "-3h", "5y", "yesterday", "2099-01-01"] {
            assert!(parse_since(bad, NOW).is_err(), "{bad}");
        }
    }

    #[test]
    fn spans_read_short() {
        assert_eq!(span(45_000), "45s");
        assert_eq!(span(12 * MINUTE_MS), "12m");
        assert_eq!(span(2 * HOUR_MS + 5 * MINUTE_MS), "2h 05m");
        assert_eq!(span(3 * DAY_MS + 4 * HOUR_MS), "3d 4h");
    }
}
