//! A live cost estimate while a turn runs. The server reports a turn's
//! cost only once it ends, but sends token counts after each step; gz
//! prices those with the dollars per token it saw for the same model on
//! earlier turns. The estimate is always shown as approximate and is
//! replaced by the server's figure when the turn ends.

use crate::cmd::stream::TokenCounts;

/// What each kind of token costs next to an input token, at the usual
/// provider ratios (output 5×, cache reads 0.1×, cache writes 1.25×). They
/// only spread one learned rate across the kinds; the rate itself comes
/// from real costs.
const OUTPUT_WEIGHT: f64 = 5.0;
const CACHE_READ_WEIGHT: f64 = 0.1;
const CACHE_WRITE_WEIGHT: f64 = 1.25;
/// How much a new turn moves the learned rate (the rest stays).
const NEW_TURN_SHARE: f64 = 0.5;

/// `tokens` in input-token equivalents.
pub fn weighted(tokens: &TokenCounts) -> f64 {
    tokens.input as f64
        + tokens.output as f64 * OUTPUT_WEIGHT
        + tokens.cache_read as f64 * CACHE_READ_WEIGHT
        + tokens.cache_write as f64 * CACHE_WRITE_WEIGHT
}

/// The rate after a turn that cost `usd` for `tokens`; `old` when the turn
/// says nothing (no tokens or no cost).
pub fn learn(old: Option<f64>, usd: f64, tokens: &TokenCounts) -> Option<f64> {
    let units = weighted(tokens);
    if units <= 0.0 || !usd.is_finite() || usd < 0.0 {
        return old;
    }
    let seen = usd / units;
    Some(match old {
        Some(old) => old * (1.0 - NEW_TURN_SHARE) + seen * NEW_TURN_SHARE,
        None => seen,
    })
}

/// What `tokens` cost at `rate`.
pub fn estimate(rate: f64, tokens: &TokenCounts) -> f64 {
    rate * weighted(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(input: u64, output: u64, cache_read: u64, cache_write: u64) -> TokenCounts {
        TokenCounts {
            input,
            output,
            cache_read,
            cache_write,
        }
    }

    #[test]
    fn a_learned_rate_prices_a_turn_of_the_same_shape_exactly() {
        let turn = tokens(1_000, 200, 10_000, 500);
        let rate = learn(None, 0.03, &turn).unwrap();
        assert!((estimate(rate, &turn) - 0.03).abs() < 1e-12);
        let double = tokens(2_000, 400, 20_000, 1_000);
        assert!((estimate(rate, &double) - 0.06).abs() < 1e-12);
    }

    #[test]
    fn later_turns_move_the_rate_halfway() {
        let turn = tokens(1_000, 0, 0, 0);
        let first = learn(None, 0.001, &turn).unwrap();
        let second = learn(Some(first), 0.003, &turn).unwrap();
        assert!((estimate(second, &turn) - 0.002).abs() < 1e-12);
    }

    #[test]
    fn turns_without_tokens_or_cost_teach_nothing() {
        assert_eq!(learn(None, 0.01, &TokenCounts::default()), None);
        assert_eq!(learn(Some(1e-6), f64::NAN, &tokens(5, 5, 0, 0)), Some(1e-6));
        assert_eq!(learn(Some(1e-6), -1.0, &tokens(5, 5, 0, 0)), Some(1e-6));
    }
}
