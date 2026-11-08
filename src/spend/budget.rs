//! The caller's server-side budget: `GET chat /chat/budget-status`
//! (gz-chat-eks api/routes/budgets.py, from `fn_r_chat_budget_status` in
//! groundzero-site-manager-api site-db/chat-functions.mjs). An admin sets it
//! per user or per group; it is a rolling window and the server refuses
//! runs once it is used up. `/chat/metrics` carries the same object as its
//! `budget` field.

use serde_json::Value;

use super::{usd, usd_limit, Level};

/// Who the budget belongs to (`policySource`, `policySubjectName`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Policy {
    User(String),
    Group(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct BudgetStatus {
    /// `None`: no budget set.
    pub limit_usd: Option<f64>,
    /// Finalized spend in the window.
    pub used_usd: f64,
    /// Runs in flight hold this much until they finish.
    pub reserved_usd: f64,
    /// What can still be spent (after reservations); `None` without a limit.
    pub left_usd: Option<f64>,
    pub exhausted: bool,
    pub window_days: u64,
    pub policy: Option<Policy>,
}

/// Money arrives as decimal strings (`"38.10"`) or numbers.
fn money(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

impl BudgetStatus {
    /// `None` when `value` isn't a budget status.
    pub fn from_json(value: &Value) -> Option<Self> {
        let window_days = value["windowDays"].as_u64()?;
        let limit_usd = value["limitCents"].as_f64().map(|cents| cents / 100.0);
        let used_usd = money(&value["usedUsd"])
            .or_else(|| value["usedCents"].as_f64().map(|c| c / 100.0))
            .unwrap_or(0.0);
        let left_usd = limit_usd.map(|limit| {
            money(&value["availableUsd"])
                .or_else(|| value["remainingCents"].as_f64().map(|c| c / 100.0))
                .unwrap_or((limit - used_usd).max(0.0))
        });
        let name = value["policySubjectName"].as_str().unwrap_or_default();
        let policy = match value["policySource"].as_str() {
            Some("user") => Some(Policy::User(name.to_string())),
            Some("group") => Some(Policy::Group(name.to_string())),
            _ => None,
        };
        Some(Self {
            limit_usd,
            used_usd,
            reserved_usd: money(&value["activeReservedUsd"]).unwrap_or(0.0),
            left_usd,
            exhausted: value["exhausted"].as_bool() == Some(true),
            window_days,
            policy,
        })
    }

    /// Share of the limit spent or held; `None` without a limit.
    pub fn used_fraction(&self) -> Option<f64> {
        let limit = self.limit_usd?;
        if limit <= 0.0 {
            return Some(1.0);
        }
        Some((1.0 - self.left_usd.unwrap_or(0.0) / limit).clamp(0.0, 1.0))
    }

    pub fn level(&self) -> Level {
        if self.exhausted {
            return Level::Over;
        }
        self.used_fraction().map_or(Level::Ok, Level::of)
    }

    /// `$38.10 of $50 left (30d)`, or `no budget limit (30d)`.
    pub fn left_text(&self) -> String {
        let window = format!("({}d)", self.window_days);
        match (self.limit_usd, self.left_usd) {
            (Some(limit), Some(left)) => {
                format!("{} of {} left {window}", usd(left), usd_limit(limit))
            }
            _ => format!("no budget limit {window}"),
        }
    }

    /// `set for you (alice)`, `set for group data-team`.
    pub fn set_by(&self) -> String {
        match &self.policy {
            Some(Policy::User(name)) if !name.is_empty() => format!("set for you ({name})"),
            Some(Policy::Group(name)) if !name.is_empty() => format!("set for group {name}"),
            Some(Policy::User(_)) => "set for you".to_string(),
            Some(Policy::Group(_)) => "set for your group".to_string(),
            None => "set by your admin".to_string(),
        }
    }

    /// What to tell the user before they send while the budget is used up.
    pub fn exhausted_message(&self) -> String {
        let limit = self.limit_usd.map(usd_limit).unwrap_or_default();
        format!(
            "Your {}-day AI budget ({limit}, {}) is used up, so Groundzero will refuse new turns. \
             It frees up as older spend leaves the window; ask your Groundzero admin to raise it.",
            self.window_days,
            self.set_by()
        )
    }

    /// The warning shown once spend crosses 80% of the limit.
    pub fn warning_message(&self) -> String {
        let percent = self.used_fraction().unwrap_or(0.0) * 100.0;
        format!(
            "⚠ {percent:.0}% of your {}-day AI budget is used: {} ({})",
            self.window_days,
            self.left_text(),
            self.set_by()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The shape `fn_r_chat_budget_status` returns, as the web app's tests
    /// mock it (ground-zero ChatMessage.test.js).
    fn status(available: &str, exhausted: bool, source: &str) -> BudgetStatus {
        BudgetStatus::from_json(&json!({
            "site": "acme", "policySource": source, "policySubjectName": "data-team",
            "limitCents": 5000, "usedCents": 1190, "remainingCents": 3810,
            "usedUsd": "11.900000000000000000", "activeReservedUsd": "0.000000000000000000",
            "availableUsd": available, "exhausted": exhausted, "currency": "USD", "windowDays": 30
        }))
        .unwrap()
    }

    #[test]
    fn shows_what_is_left_of_the_limit() {
        let budget = status("38.100000000000000000", false, "group");
        assert_eq!(budget.left_text(), "$38.10 of $50 left (30d)");
        assert_eq!(budget.level(), Level::Ok);
        assert_eq!(budget.set_by(), "set for group data-team");
    }

    #[test]
    fn reservations_count_toward_the_warning() {
        let budget = status("9.000000000000000000", false, "user");
        assert_eq!(budget.level(), Level::Warn);
        assert!(budget
            .warning_message()
            .starts_with("⚠ 82% of your 30-day AI budget"));
        assert!(budget.warning_message().contains("set for you (data-team)"));
    }

    #[test]
    fn exhausted_says_who_sets_it() {
        let budget = status("0", true, "group");
        assert_eq!(budget.level(), Level::Over);
        let message = budget.exhausted_message();
        assert!(message.contains("30-day AI budget ($50, set for group data-team) is used up"));
        assert!(message.contains("refuse"));
    }

    #[test]
    fn no_limit_is_not_a_warning() {
        let budget = BudgetStatus::from_json(&json!({
            "limitCents": null, "remainingCents": null, "usedCents": 0, "usedUsd": "0",
            "exhausted": false, "windowDays": 30, "policySource": null
        }))
        .unwrap();
        assert_eq!(budget.left_text(), "no budget limit (30d)");
        assert_eq!(budget.level(), Level::Ok);
        assert_eq!(BudgetStatus::from_json(&json!({"detail": "nope"})), None);
    }
}
