//! Times in a table's history. The server sends UTC ISO times
//! (`2026-09-16T10:19:43.733000+00:00`); a user names a moment as a UTC
//! date or time. Both are compared as `YYYY-MM-DDTHH:MM:SS` text, which
//! sorts in time order.

const SORTABLE_LEN: usize = "2026-09-16T10:19:43".len();

/// The seconds-precision prefix of a server time.
pub fn sortable(server_time: &str) -> &str {
    server_time.get(..SORTABLE_LEN).unwrap_or(server_time)
}

/// `2026-09-16 10:19 UTC`, or the text itself when it is not a time.
pub fn display(server_time: &str) -> String {
    match server_time.get(..SORTABLE_LEN) {
        Some(prefix) if parse(prefix).is_some() => {
            format!("{} {} UTC", &prefix[..10], &prefix[11..16])
        }
        _ => server_time.to_string(),
    }
}

/// A user's UTC date or time as sortable text: `2026-09-16` (its first
/// moment), `2026-09-16T08:00`, `2026-09-16 08:00:30`, each with an optional
/// trailing `Z`. `None` when the text is not one of these.
pub fn parse(text: &str) -> Option<String> {
    let text = text.strip_suffix(['Z', 'z']).unwrap_or(text);
    let (date, time) = match text.split_once(['T', ' ']) {
        Some((date, time)) => (date, time),
        None => (text, "00:00:00"),
    };
    let [year, month, day] = numbers::<3>(date, '-', &[4, 2, 2])?;
    let time = if time.len() == 5 {
        format!("{time}:00")
    } else {
        time.to_string()
    };
    let [hour, minute, second] = numbers::<3>(&time, ':', &[2, 2, 2])?;
    let valid = (1..=12).contains(&month)
        && (1..=31).contains(&day)
        && hour < 24
        && minute < 60
        && second < 60;
    valid.then(|| format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}"))
}

/// `N` numbers split by `separator`, each with the given digit count.
fn numbers<const N: usize>(text: &str, separator: char, digits: &[usize; N]) -> Option<[u32; N]> {
    let parts: Vec<&str> = text.split(separator).collect();
    if parts.len() != N {
        return None;
    }
    let mut out = [0u32; N];
    for (slot, (part, width)) in out.iter_mut().zip(parts.iter().zip(digits)) {
        if part.len() != *width || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        *slot = part.parse().ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_date_or_time_becomes_sortable_utc_text() {
        assert_eq!(parse("2026-09-16").as_deref(), Some("2026-09-16T00:00:00"));
        assert_eq!(
            parse("2026-09-16T08:05").as_deref(),
            Some("2026-09-16T08:05:00")
        );
        assert_eq!(
            parse("2026-09-16 08:05:09Z").as_deref(),
            Some("2026-09-16T08:05:09")
        );
        for wrong in [
            "",
            "main",
            "2026-13-01",
            "2026-09-16T25:00",
            "26-09-16",
            "2026-9-16",
            "1525656351876290757",
        ] {
            assert_eq!(parse(wrong), None, "{wrong}");
        }
    }

    #[test]
    fn a_server_time_is_shown_to_the_minute() {
        assert_eq!(
            display("2026-09-16T10:19:43.733000+00:00"),
            "2026-09-16 10:19 UTC"
        );
        assert_eq!(display(""), "");
        assert!(sortable("2026-09-16T10:19:43.733000+00:00") <= "2026-09-16T10:19:43");
    }
}
