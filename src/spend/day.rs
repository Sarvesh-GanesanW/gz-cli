//! The user's local calendar day, which day caps and the ledger count by.
//! On Unix the offset comes from the system's time zone; elsewhere gz has
//! no zone database and counts in UTC.

use crate::tui::format::civil_from_days;

const SECS_PER_DAY: i64 = 86_400;

/// Today, as `2026-11-08`.
pub fn today() -> String {
    on(crate::tui::format::system_now_ms() / 1000)
}

/// The local date at `epoch_secs`, as `2026-11-08`.
pub fn on(epoch_secs: i64) -> String {
    let local = epoch_secs + utc_offset_secs(epoch_secs);
    let (year, month, day) = civil_from_days(local.div_euclid(SECS_PER_DAY));
    format!("{year:04}-{month:02}-{day:02}")
}

/// `2026-11` for `2026-11-08`.
pub fn month_of(day: &str) -> &str {
    day.get(..7).unwrap_or(day)
}

#[cfg(unix)]
fn utc_offset_secs(epoch_secs: i64) -> i64 {
    let time = epoch_secs as libc::time_t;
    // SAFETY: `localtime_r` only writes the `tm` we own; a zeroed `tm` is a
    // valid value for it to fill.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&time, &mut tm).is_null() {
            return 0;
        }
        tm.tm_gmtoff as i64
    }
}

#[cfg(not(unix))]
fn utc_offset_secs(_epoch_secs: i64) -> i64 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_and_months_are_iso_dates() {
        let day = on(1_794_139_200); // 2026-11-08T12:00:00Z
        assert_eq!(day.len(), 10);
        assert!(day.starts_with("2026-11-0"), "{day}");
        assert_eq!(month_of("2026-11-08"), "2026-11");
    }
}
