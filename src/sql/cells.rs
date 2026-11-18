//! Date and timestamp cells as the lakehouse pages them: HTTP dates
//! (`Wed, 18 Nov 2026 11:07:43 GMT`, Flask's JSON form for both) or ISO
//! text. Read into days and microseconds for Parquet, and rewritten as
//! ISO 8601 for CSV and JSON Lines files.

pub const MICROS_PER_DAY: i64 = 86_400_000_000;

/// A column whose cells are dates or timestamps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Temporal {
    Date,
    /// `utc` for Spark's `timestamp`; `timestamp_ntz` has no zone.
    Timestamp {
        utc: bool,
    },
}

impl Temporal {
    /// The kind for a type the session names, if it is a date or time.
    pub fn of(named: &str) -> Option<Self> {
        match named {
            "date" => Some(Self::Date),
            "timestamp" => Some(Self::Timestamp { utc: true }),
            "timestamp_ntz" => Some(Self::Timestamp { utc: false }),
            _ => None,
        }
    }

    /// `2026-11-18`, `2026-11-18T11:07:43Z`, or `2026-11-18T11:07:43.5`
    /// without a zone; `None` when `text` is no date.
    pub fn iso(self, text: &str) -> Option<String> {
        let (days, micros) = parse_instant(text)?;
        let (year, month, day) = civil_from_days(days);
        let date = format!("{year:04}-{month:02}-{day:02}");
        let Self::Timestamp { utc } = self else {
            return Some(date);
        };
        let seconds = micros / 1_000_000;
        let fraction = micros % 1_000_000;
        let mut text = format!(
            "{date}T{:02}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        );
        if fraction > 0 {
            let digits = format!("{fraction:06}");
            text.push('.');
            text.push_str(digits.trim_end_matches('0'));
        }
        if utc {
            text.push('Z');
        }
        Some(text)
    }
}

/// Days since 1970-01-01 and microseconds into that day, from an HTTP date
/// (`Wed, 18 Nov 2026 11:07:43 GMT`) or ISO text (`2026-11-18`,
/// `2026-11-18T11:07:43.5Z`, `2026-11-18 11:07:43`).
pub fn parse_instant(text: &str) -> Option<(i64, i64)> {
    let text = text.trim();
    if text.contains(',') {
        return parse_http_date(text);
    }
    let (date, time) = match text.find(['T', ' ']) {
        Some(at) => (&text[..at], Some(&text[at + 1..])),
        None => (text, None),
    };
    let mut parts = date.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: u32 = parts.next()?.parse().ok()?;
    let day: u32 = parts.next()?.parse().ok()?;
    let micros = match time {
        Some(time) => time_of_day(time.trim_end_matches('Z').split('+').next()?)?,
        None => 0,
    };
    Some((days_from_civil(year, month, day)?, micros))
}

fn parse_http_date(text: &str) -> Option<(i64, i64)> {
    let mut words = text.split_once(',')?.1.split_whitespace();
    let day: u32 = words.next()?.parse().ok()?;
    let month = month_number(words.next()?)?;
    let year: i64 = words.next()?.parse().ok()?;
    let micros = time_of_day(words.next()?)?;
    Some((days_from_civil(year, month, day)?, micros))
}

fn month_number(name: &str) -> Option<u32> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    MONTHS
        .iter()
        .position(|m| m.eq_ignore_ascii_case(name))
        .map(|at| at as u32 + 1)
}

/// `11:07:43.25` → microseconds since midnight.
fn time_of_day(text: &str) -> Option<i64> {
    let mut parts = text.split(':');
    let hours: i64 = parts.next()?.parse().ok()?;
    let minutes: i64 = parts.next()?.parse().ok()?;
    let seconds = parts.next().unwrap_or("0");
    let (whole, fraction) = seconds.split_once('.').unwrap_or((seconds, ""));
    let whole: i64 = whole.parse().ok()?;
    let fraction: String = fraction.chars().chain("000000".chars()).take(6).collect();
    let fraction: i64 = fraction.parse().ok()?;
    if hours > 23 || minutes > 59 || whole > 60 {
        return None;
    }
    Some(((hours * 60 + minutes) * 60 + whole) * 1_000_000 + fraction)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard
/// Hinnant's `days_from_civil`).
fn days_from_civil(year: i64, month: u32, day: u32) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let shifted_month = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Some(era * 146_097 + day_of_era - 719_468)
}

/// The date `days` after 1970-01-01 (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_http_and_iso_dates() {
        assert_eq!(parse_instant("Thu, 01 Jan 1970 00:00:00 GMT"), Some((0, 0)));
        assert_eq!(
            parse_instant("Wed, 18 Nov 2026 11:07:43 GMT"),
            Some((20_775, (11 * 3600 + 7 * 60 + 43) * 1_000_000))
        );
        assert_eq!(parse_instant("2026-11-18"), Some((20_775, 0)));
        assert_eq!(
            parse_instant("2024-02-29T00:00:01.5Z"),
            Some((19_782, 1_500_000))
        );
        assert_eq!(
            parse_instant("1969-12-31 23:59:59"),
            Some((-1, 86_399_000_000))
        );
        assert_eq!(parse_instant("soon"), None);
    }

    #[test]
    fn rewrites_cells_as_iso() {
        let http = "Wed, 18 Nov 2026 11:07:43 GMT";
        assert_eq!(Temporal::Date.iso(http).as_deref(), Some("2026-11-18"));
        assert_eq!(
            Temporal::Timestamp { utc: true }.iso(http).as_deref(),
            Some("2026-11-18T11:07:43Z")
        );
        assert_eq!(
            Temporal::Timestamp { utc: false }
                .iso("2024-02-29 00:00:01.250")
                .as_deref(),
            Some("2024-02-29T00:00:01.25")
        );
        assert_eq!(
            Temporal::Date.iso("1969-12-31").as_deref(),
            Some("1969-12-31")
        );
        assert_eq!(Temporal::Date.iso("not a date"), None);
        assert_eq!(
            Temporal::of("timestamp_ntz"),
            Some(Temporal::Timestamp { utc: false })
        );
        assert_eq!(Temporal::of("string"), None);
    }

    #[test]
    fn days_round_trip() {
        for days in [-719_468, -1, 0, 19_782, 20_775, 2_932_896] {
            let (year, month, day) = civil_from_days(days);
            assert_eq!(days_from_civil(year, month, day), Some(days));
        }
    }
}
