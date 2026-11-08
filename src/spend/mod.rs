//! What `gz code` spends and what it may still spend, shown where people
//! work: the server's rolling budget (`GET /chat/budget-status`), the
//! user's own caps per session and per day, and a small local ledger of
//! gz code's spend per day and model.
//!
//! Groundzero prices every model call on the server; gz only adds up the
//! `costUsd` each run reports in its `response_metrics` event.

pub mod budget;
pub mod caps;
pub mod day;
pub mod ledger;
pub mod meter;

/// Share of a limit at which gz warns.
pub const WARN_AT: f64 = 0.8;

/// How close spend is to a limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Ok,
    /// At or past [`WARN_AT`].
    Warn,
    /// At or past the limit.
    Over,
}

impl Level {
    pub fn of(fraction: f64) -> Self {
        if fraction >= 1.0 {
            Level::Over
        } else if fraction >= WARN_AT {
            Level::Warn
        } else {
            Level::Ok
        }
    }
}

/// `$1.24`; amounts above zero but under a cent show as `<$0.01`.
pub fn usd(amount: f64) -> String {
    if amount > 0.0 && amount < 0.01 {
        return "<$0.01".to_string();
    }
    format!("${amount:.2}")
}

/// A limit: `$50` for whole dollars, `$12.50` otherwise.
pub fn usd_limit(amount: f64) -> String {
    if (amount - amount.round()).abs() < 0.005 {
        format!("${:.0}", amount.round())
    } else {
        format!("${amount:.2}")
    }
}

/// Reads a dollar amount the user typed: `5`, `5.50`, `$5`.
pub fn parse_usd(text: &str) -> Result<f64, String> {
    let amount: f64 = text
        .trim()
        .trim_start_matches('$')
        .parse()
        .map_err(|_| format!("'{text}' isn't a dollar amount; use e.g. 5 or 12.50"))?;
    if !amount.is_finite() || amount <= 0.0 {
        return Err("a cap must be more than $0".to_string());
    }
    Ok(amount)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_turn_at_eighty_percent_and_at_the_limit() {
        assert_eq!(Level::of(0.79), Level::Ok);
        assert_eq!(Level::of(0.8), Level::Warn);
        assert_eq!(Level::of(1.0), Level::Over);
    }

    #[test]
    fn money_reads_like_a_bill() {
        assert_eq!(usd(1.2449), "$1.24");
        assert_eq!(usd(0.004), "<$0.01");
        assert_eq!(usd(0.0), "$0.00");
        assert_eq!(usd_limit(50.0), "$50");
        assert_eq!(usd_limit(12.5), "$12.50");
    }

    #[test]
    fn caps_must_be_positive_dollar_amounts() {
        assert_eq!(parse_usd("$5"), Ok(5.0));
        assert_eq!(parse_usd("12.50"), Ok(12.5));
        assert!(parse_usd("0").is_err());
        assert!(parse_usd("ten").unwrap_err().contains("dollar amount"));
    }
}
