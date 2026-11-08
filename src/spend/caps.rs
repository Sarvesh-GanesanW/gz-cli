//! The user's own spending caps for gz code (`/budget session|day <usd>`),
//! saved per profile: a warning at 80%, and at 100% the next turn waits
//! for the user to raise the cap, continue once, or cancel. Background
//! subagents don't start past a cap either; a "continue once" covers the
//! turn it lets through and the subagents that turn starts.

use serde::{Deserialize, Serialize};

use super::{usd, usd_limit, Level};

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Caps {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub day_usd: Option<f64>,
}

impl Caps {
    pub fn is_empty(&self) -> bool {
        self.session_usd.is_none() && self.day_usd.is_none()
    }

    pub fn get(&self, kind: CapKind) -> Option<f64> {
        match kind {
            CapKind::Session => self.session_usd,
            CapKind::Day => self.day_usd,
        }
    }

    /// `$5`, or `off` when that cap isn't set.
    pub fn shown(&self, kind: CapKind) -> String {
        self.get(kind).map_or_else(|| "off".to_string(), usd_limit)
    }

    pub fn with(mut self, kind: CapKind, usd: Option<f64>) -> Self {
        match kind {
            CapKind::Session => self.session_usd = usd,
            CapKind::Day => self.day_usd = usd,
        }
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapKind {
    Session,
    Day,
}

impl CapKind {
    pub const ALL: [CapKind; 2] = [CapKind::Session, CapKind::Day];

    pub fn word(self) -> &'static str {
        match self {
            CapKind::Session => "session",
            CapKind::Day => "day",
        }
    }
}

/// A cap and how much of it is spent.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CapUse {
    pub kind: CapKind,
    pub spent: f64,
    pub cap: f64,
}

impl CapUse {
    pub fn fraction(&self) -> f64 {
        self.spent / self.cap
    }

    pub fn level(&self) -> Level {
        Level::of(self.fraction())
    }

    /// The cap twice as high, at least a quarter above what's spent.
    pub fn raised(&self) -> f64 {
        (self.cap * 2.0).max(self.spent * 1.25)
    }

    /// `$4.10 of your $5 session cap`.
    pub fn describe(&self) -> String {
        format!(
            "{} of your {} {} cap",
            usd(self.spent),
            usd_limit(self.cap),
            self.kind.word()
        )
    }
}

/// A cap crossing gz tells the user about, once.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Alert {
    pub usage: CapUse,
    pub level: Level,
}

impl Alert {
    pub fn message(&self) -> String {
        match self.level {
            Level::Over => format!(
                "⚠ Your {} {} cap is reached ({} spent): the next turn waits for you to raise it, continue once, or cancel",
                usd_limit(self.usage.cap),
                self.usage.kind.word(),
                usd(self.usage.spent)
            ),
            _ => format!(
                "⚠ {:.0}% of your {} {} cap is used ({} of {}) · /budget to change it",
                self.usage.fraction() * 100.0,
                usd_limit(self.usage.cap),
                self.usage.kind.word(),
                usd(self.usage.spent),
                usd_limit(self.usage.cap)
            ),
        }
    }
}

/// A "continue once": granted for the turn about to start, then in use
/// while that turn runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Pass {
    #[default]
    None,
    Granted,
    InTurn,
}

/// Spend against the caps: this session's, and today's across sessions.
#[derive(Debug, Clone, PartialEq)]
pub struct Guard {
    caps: Caps,
    session_usd: f64,
    day: String,
    day_usd: f64,
    /// Levels already announced, per cap: the cap and (for the day cap)
    /// the day they were announced for.
    announced: Vec<(CapKind, Level, f64, String)>,
    pass: Pass,
}

impl Guard {
    /// `day_usd` is what the ledger holds for `day` already.
    pub fn new(caps: Caps, day: &str, day_usd: f64) -> Self {
        Self {
            caps,
            session_usd: 0.0,
            day: day.to_string(),
            day_usd,
            announced: Vec::new(),
            pass: Pass::None,
        }
    }

    pub fn caps(&self) -> Caps {
        self.caps
    }

    pub fn set_caps(&mut self, caps: Caps) {
        self.caps = caps;
    }

    pub fn session_usd(&self) -> f64 {
        self.session_usd
    }

    pub fn day_usd(&self) -> f64 {
        self.day_usd
    }

    /// Moves to `day` when the local date changed; `day_usd` is what the
    /// ledger already holds for it (other sessions may have spent).
    pub fn roll_to(&mut self, day: &str, day_usd: f64) {
        if day != self.day {
            self.day = day.to_string();
            self.day_usd = day_usd;
        }
    }

    /// Adds spend reported today (after [`roll_to`](Self::roll_to));
    /// `day_total` is the ledger's new total for today when it could be
    /// saved, which also counts other sessions. Returns the crossings not
    /// announced yet.
    pub fn spent(&mut self, usd: f64, day_total: Option<f64>) -> Vec<Alert> {
        self.session_usd += usd;
        self.day_usd = day_total.unwrap_or(self.day_usd + usd);
        self.new_alerts()
    }

    pub fn usage(&self, kind: CapKind) -> Option<CapUse> {
        let cap = self.caps.get(kind)?;
        let spent = match kind {
            CapKind::Session => self.session_usd,
            CapKind::Day => self.day_usd,
        };
        Some(CapUse { kind, spent, cap })
    }

    /// The first cap fully spent, if any.
    pub fn reached(&self) -> Option<CapUse> {
        CapKind::ALL
            .into_iter()
            .filter_map(|kind| self.usage(kind))
            .find(|usage| usage.level() == Level::Over)
    }

    /// The highest level any cap is at.
    pub fn level(&self) -> Level {
        CapKind::ALL
            .into_iter()
            .filter_map(|kind| self.usage(kind))
            .map(|usage| usage.level())
            .max()
            .unwrap_or(Level::Ok)
    }

    /// Whether a new run may start: under every cap, or let through once.
    pub fn may_start(&self) -> bool {
        self.pass != Pass::None || self.reached().is_none()
    }

    /// The user chose "continue once" for the turn about to start.
    pub fn continue_once(&mut self) {
        self.pass = Pass::Granted;
    }

    /// Drops a "continue once" no turn used.
    pub fn revoke_pass(&mut self) {
        self.pass = Pass::None;
    }

    pub fn turn_started(&mut self) {
        if self.pass == Pass::Granted {
            self.pass = Pass::InTurn;
        }
    }

    pub fn turn_ended(&mut self) {
        if self.pass == Pass::InTurn {
            self.pass = Pass::None;
        }
    }

    fn new_alerts(&mut self) -> Vec<Alert> {
        let mut alerts = Vec::new();
        let uses: Vec<CapUse> = CapKind::ALL
            .into_iter()
            .filter_map(|kind| self.usage(kind))
            .collect();
        for usage in uses {
            let level = usage.level();
            if level == Level::Ok {
                continue;
            }
            let day = match usage.kind {
                CapKind::Day => self.day.clone(),
                CapKind::Session => String::new(),
            };
            let key = (usage.kind, level, usage.cap, day);
            if !self.announced.contains(&key) {
                self.announced.push(key);
                alerts.push(Alert { usage, level });
            }
        }
        alerts
    }
}

/// A one-shot `gz code` refused because a cap is spent; the process exits
/// with [`CapReached::EXIT_CODE`].
#[derive(Debug)]
pub struct CapReached(pub CapUse);

impl CapReached {
    pub const EXIT_CODE: i32 = 3;
}

impl std::fmt::Display for CapReached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "not sent: {} is spent; raise it in `gz code` with /budget {} <usd>, or remove it with /budget off",
            self.0.describe(),
            self.0.kind.word()
        )
    }
}

impl std::error::Error for CapReached {}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard(session: Option<f64>, day: Option<f64>) -> Guard {
        Guard::new(
            Caps {
                session_usd: session,
                day_usd: day,
            },
            "2026-11-08",
            0.0,
        )
    }

    #[test]
    fn warns_once_at_eighty_percent_then_once_at_the_cap() {
        let mut guard = guard(Some(5.0), None);
        assert!(guard.spent(3.0, None).is_empty());
        let warned = guard.spent(1.2, None);
        assert_eq!(warned.len(), 1);
        assert_eq!(warned[0].level, Level::Warn);
        assert!(
            warned[0]
                .message()
                .starts_with("⚠ 84% of your $5 session cap is used"),
            "{}",
            warned[0].message()
        );
        assert!(guard.spent(0.1, None).is_empty(), "the warning shows once");
        assert!(guard.may_start());
        let over = guard.spent(0.7, None);
        assert_eq!(over[0].level, Level::Over);
        assert!(over[0].message().contains("the next turn waits"));
        assert_eq!(guard.reached().unwrap().kind, CapKind::Session);
        assert!(!guard.may_start());
    }

    #[test]
    fn continue_once_lets_one_turn_and_its_subagents_through() {
        let mut guard = guard(None, Some(1.0));
        guard.spent(1.0, None);
        assert!(!guard.may_start());
        guard.continue_once();
        guard.turn_started();
        assert!(guard.may_start(), "subagents of that turn may start");
        guard.spent(0.3, None);
        guard.turn_ended();
        assert!(!guard.may_start(), "the next turn asks again");
    }

    #[test]
    fn raising_the_cap_lets_turns_through_and_warns_again_later() {
        let mut guard = guard(Some(2.0), None);
        guard.spent(2.0, None);
        let usage = guard.reached().unwrap();
        assert_eq!(usage.raised(), 4.0);
        guard.set_caps(guard.caps().with(CapKind::Session, Some(usage.raised())));
        assert!(guard.may_start());
        let warned = guard.spent(1.3, None);
        assert_eq!(warned[0].level, Level::Warn, "80% of the new cap is news");
    }

    #[test]
    fn the_day_cap_counts_other_sessions_and_starts_over_tomorrow() {
        let mut guard = Guard::new(
            Caps {
                session_usd: None,
                day_usd: Some(10.0),
            },
            "2026-11-08",
            9.5,
        );
        assert_eq!(guard.spent(0.6, None)[0].level, Level::Over);
        assert!(!guard.may_start());
        guard.roll_to("2026-11-09", 0.0);
        assert!(guard.may_start());
        assert_eq!(guard.day_usd(), 0.0);
        assert!((guard.session_usd() - 0.6).abs() < 1e-9);
        assert_eq!(
            guard.spent(8.5, None)[0].level,
            Level::Warn,
            "a new day warns anew"
        );
    }
}
