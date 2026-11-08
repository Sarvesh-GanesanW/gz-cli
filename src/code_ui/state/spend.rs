//! Spend in `gz code`: every reported cost (main turns, inline and
//! background subagents) goes to the caps and the local ledger; the
//! server's rolling budget is refreshed after each turn, cached and off the
//! turn's path; a turn past a cap waits for the user's choice; and
//! `/budget` shows and sets the caps.

use std::time::{Duration, Instant};

use serde_json::Value;

use crate::assistant::AssistantEvent;
use crate::code_ui::items::Item;
use crate::code_ui::panel::{Panel, Row};
use crate::spend::budget::BudgetStatus;
use crate::spend::caps::{Alert, CapKind, CapUse, Caps};
use crate::spend::ledger::Ledger;
use crate::spend::meter::Meter;
use crate::spend::{self, usd, usd_limit, Level};

use super::{Effect, Picker, PickerRow, UiState};

/// A budget fetched this recently is reused instead of asked for again.
const BUDGET_FRESH_FOR: Duration = Duration::from_secs(20);
const BUDGET_USAGE: &str =
    "Use /budget, /budget session <usd>, /budget day <usd>, /budget session|day off, or /budget off";

/// The choices when a turn would start past a cap.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CapChoice {
    Raise(CapKind, f64),
    ContinueOnce,
    Cancel,
}

pub struct SpendState {
    pub meter: Meter,
    /// The server's budget as last fetched; `None` until it arrives.
    pub budget: Option<BudgetStatus>,
    /// Why the last refresh failed; the budget shown may be out of date.
    pub budget_error: Option<String>,
    budget_loading: bool,
    budget_fetched: Option<Instant>,
    budget_warned: bool,
    exhausted_noted: bool,
    /// Set after telling the user the budget is used up: the next send
    /// goes out anyway (the status may be stale).
    send_anyway: bool,
    ledger_error_shown: bool,
    /// The turn waiting for the user's choice at a cap.
    held: Option<Effect>,
}

impl SpendState {
    pub fn new(caps: Caps, ledger: Option<Ledger>) -> Self {
        Self::with_meter(Meter::new(caps, ledger))
    }

    pub fn with_meter(meter: Meter) -> Self {
        Self {
            meter,
            budget: None,
            budget_error: None,
            budget_loading: false,
            budget_fetched: None,
            budget_warned: false,
            exhausted_noted: false,
            send_anyway: false,
            ledger_error_shown: false,
            held: None,
        }
    }

    /// The highest level of the server budget and the caps.
    pub fn level(&self) -> Level {
        let server = self.budget.as_ref().map_or(Level::Ok, BudgetStatus::level);
        server.max(self.meter.guard.level())
    }

    /// `$1.24 this session · $38.10 of $50 left (30d)` for the mode line,
    /// plus any cap at 80% or more.
    pub fn status_text(&self, turns: usize) -> Option<String> {
        let guard = &self.meter.guard;
        let mut parts = Vec::new();
        if turns > 0 || guard.session_usd() > 0.0 {
            parts.push(format!("{} this session", usd(guard.session_usd())));
        }
        match &self.budget {
            Some(budget) if budget.exhausted => {
                parts.push(format!("AI budget used up ({}d)", budget.window_days))
            }
            Some(budget) if budget.limit_usd.is_some() => parts.push(budget.left_text()),
            _ => {}
        }
        for usage in CapKind::ALL
            .into_iter()
            .filter_map(|kind| guard.usage(kind))
        {
            match usage.level() {
                Level::Ok => {}
                Level::Warn => parts.push(format!(
                    "{} cap {:.0}%",
                    usage.kind.word(),
                    usage.fraction() * 100.0
                )),
                Level::Over => parts.push(format!("{} cap reached", usage.kind.word())),
            }
        }
        (!parts.is_empty()).then(|| parts.join(" · "))
    }
}

impl UiState {
    /// [`apply`](Self::apply), then a budget refresh once a turn ended.
    pub fn on_event(&mut self, event: AssistantEvent) -> Vec<Effect> {
        let ended = matches!(event, AssistantEvent::Done(_));
        let mut effects = self.apply(event);
        if ended {
            effects.extend(self.refresh_budget(false));
        }
        effects
    }

    /// A run reported `usd` spent on `model`.
    pub(super) fn spend_reported(&mut self, usd: f64, model: Option<String>) {
        let recorded = self.spend.meter.record(usd, model.as_deref());
        if let Some(why) = recorded.ledger_error {
            if !std::mem::replace(&mut self.spend.ledger_error_shown, true) {
                self.record(Item::Error(format!(
                    "couldn't save today's spend; day caps count this session only: {why}"
                )));
            }
        }
        for alert in recorded.alerts {
            self.cap_alert(alert);
        }
    }

    fn cap_alert(&mut self, alert: Alert) {
        self.close_text();
        self.close_group();
        self.record(Item::Notice(alert.message()));
    }

    /// The model a main turn runs on, without its effort, for the ledger.
    pub(super) fn model_name(&self) -> Option<String> {
        self.model
            .clone()
            .or_else(|| self.catalog.as_ref()?.default_selection())
            .map(|s| s.model.label())
    }

    /// Asks for the server budget unless a fresh one is at hand or a
    /// request is out; `force` skips the freshness check.
    pub fn refresh_budget(&mut self, force: bool) -> Option<Effect> {
        if self.spend.budget_loading {
            return None;
        }
        let fresh = self
            .spend
            .budget_fetched
            .is_some_and(|at| at.elapsed() < BUDGET_FRESH_FOR);
        if fresh && !force {
            return None;
        }
        self.spend.budget_loading = true;
        Some(Effect::FetchBudget)
    }

    /// The server's answer to a budget refresh. A failed refresh keeps the
    /// last known budget and says nothing in the transcript (`/budget`
    /// shows why).
    pub fn budget_arrived(&mut self, answer: Result<Value, String>) {
        self.spend.budget_loading = false;
        self.spend.budget_fetched = Some(Instant::now());
        let status = answer.and_then(|value| {
            BudgetStatus::from_json(&value)
                .ok_or_else(|| "the budget status reply had no windowDays".to_string())
        });
        let budget = match status {
            Ok(budget) => budget,
            Err(why) => {
                self.spend.budget_error = Some(why);
                return;
            }
        };
        self.spend.budget_error = None;
        self.announce_budget(&budget);
        self.spend.budget = Some(budget);
    }

    /// Warns once past 80%, and says once when the budget is used up; both
    /// may come again after it frees up.
    fn announce_budget(&mut self, budget: &BudgetStatus) {
        let level = budget.level();
        if budget.exhausted {
            if !std::mem::replace(&mut self.spend.exhausted_noted, true) {
                self.record(Item::Error(budget.exhausted_message()));
            }
            return;
        }
        self.spend.exhausted_noted = false;
        self.spend.send_anyway = false;
        match level {
            Level::Ok => self.spend.budget_warned = false,
            Level::Warn | Level::Over => {
                if !std::mem::replace(&mut self.spend.budget_warned, true) {
                    self.record(Item::Notice(budget.warning_message()));
                }
            }
        }
    }

    /// Lets a send through, or holds it: a used-up server budget is named
    /// first (the next send goes anyway), and past a cap the turn waits
    /// for the user's choice. Everything else passes untouched.
    pub fn gate(&mut self, effect: Effect) -> Vec<Effect> {
        if !matches!(effect, Effect::Send(_) | Effect::SendReports(_)) {
            return vec![effect];
        }
        if let Effect::Send(text) = &effect {
            let exhausted = self.spend.budget.as_ref().filter(|b| b.exhausted).cloned();
            if let Some(budget) = exhausted {
                if !std::mem::replace(&mut self.spend.send_anyway, true) {
                    self.record(Item::Error(format!(
                        "{} Not sent; press enter again to send anyway.",
                        budget.exhausted_message()
                    )));
                    self.give_back(text);
                    return self.refresh_budget(true).into_iter().collect();
                }
            }
        }
        if self.spend.meter.guard.may_start() {
            return vec![effect];
        }
        self.spend.held = Some(effect);
        self.offer_cap_choice();
        Vec::new()
    }

    /// Whether background subagents may start now.
    pub(super) fn may_start_runs(&self) -> bool {
        self.spend.meter.guard.may_start()
    }

    /// Opens the cap choice, unless it is already open.
    pub(super) fn offer_cap_choice(&mut self) {
        let Some(usage) = self.spend.meter.guard.reached() else {
            return;
        };
        let is_open = self
            .picker
            .as_ref()
            .is_some_and(|p| matches!(p.rows.first(), Some(PickerRow::Cap(_))));
        if is_open {
            return;
        }
        let waiting = match (&self.spend.held, self.queued.len()) {
            (Some(_), 0) => "your message waits".to_string(),
            (Some(_), n) => format!("your message and {n} subagent(s) wait"),
            (None, n) => format!("{n} subagent(s) wait"),
        };
        self.picker = Some(Picker {
            title: format!("{} is spent · {waiting}", capitalized(&usage.describe())),
            rows: vec![
                PickerRow::Cap(CapChoice::Raise(usage.kind, usage.raised())),
                PickerRow::Cap(CapChoice::ContinueOnce),
                PickerRow::Cap(CapChoice::Cancel),
            ],
            selected: 0,
            model: None,
        });
    }

    /// Carries out the user's choice at a cap.
    pub(super) fn choose_at_cap(&mut self, choice: CapChoice) -> Vec<Effect> {
        self.picker = None;
        let held = self.spend.held.take();
        let mut effects = Vec::new();
        match choice {
            CapChoice::Raise(kind, to) => {
                let caps = self.spend.meter.guard.caps().with(kind, Some(to));
                self.spend.meter.guard.set_caps(caps);
                self.record(Item::Notice(format!(
                    "{} cap raised to {}",
                    capitalized(kind.word()),
                    usd_limit(to)
                )));
                effects.push(Effect::SetCaps(caps));
                effects.extend(held);
                effects.extend(self.start_queued());
            }
            CapChoice::ContinueOnce => {
                self.spend.meter.guard.continue_once();
                self.record(Item::Notice("Continuing once past your cap".into()));
                effects.extend(self.start_queued());
                match held {
                    Some(effect) => effects.push(effect),
                    // Only subagents waited; the pass ends with them.
                    None => self.spend.meter.guard.revoke_pass(),
                }
            }
            CapChoice::Cancel => {
                let dropped = self.drop_queued();
                let mut note = "Not sent: you're past your cap".to_string();
                if dropped > 0 {
                    note.push_str(&format!(" · {dropped} queued subagent(s) won't start"));
                }
                note.push_str(" · /budget shows and changes your caps");
                self.record(Item::Notice(note));
                match held {
                    Some(Effect::Send(text)) => self.give_back(&text),
                    Some(Effect::SendReports(reports)) => self.reports.push_front(reports),
                    _ => {}
                }
            }
        }
        effects
    }

    /// A message that wasn't sent goes back into the input when that is
    /// empty; it is in the history (↑) either way.
    fn give_back(&mut self, text: &str) {
        if self.input_text().trim().is_empty() {
            self.set_input(text);
        }
    }

    /// `/budget`, `/budget session|day <usd>|off`, `/budget off`.
    pub(super) fn budget_command(&mut self, command: &str) -> Vec<Effect> {
        let words: Vec<&str> = command.split_whitespace().skip(1).collect();
        let caps = self.spend.meter.guard.caps();
        let changed = match words.as_slice() {
            [] => {
                self.record(Item::Panel(self.budget_panel()));
                return self.refresh_budget(false).into_iter().collect();
            }
            ["off"] => Caps::default(),
            [kind, value] => {
                let kind = match *kind {
                    "session" => CapKind::Session,
                    "day" => CapKind::Day,
                    _ => return self.budget_usage(),
                };
                let amount = match *value {
                    "off" => None,
                    value => match spend::parse_usd(value) {
                        Ok(amount) => Some(amount),
                        Err(why) => {
                            self.record(Item::Error(format!("{why}. {BUDGET_USAGE}")));
                            return Vec::new();
                        }
                    },
                };
                caps.with(kind, amount)
            }
            _ => return self.budget_usage(),
        };
        self.spend.meter.guard.set_caps(changed);
        self.record(Item::Notice(caps_summary(&changed)));
        vec![Effect::SetCaps(changed)]
    }

    fn budget_usage(&mut self) -> Vec<Effect> {
        self.record(Item::Error(BUDGET_USAGE.into()));
        Vec::new()
    }

    /// The `/budget` panel: this session, today and this month on this
    /// machine, the caps, and the server budget.
    fn budget_panel(&self) -> Panel {
        let guard = &self.spend.meter.guard;
        let mut spent = vec![Row::field("this session", usd(guard.session_usd()))];
        match self.spend.meter.summary() {
            Some(Ok(summary)) => {
                spent.push(Row::field("today", usd(summary.today_usd)));
                spent.push(Row::field("this month", usd(summary.month_usd)));
                for (model, amount) in summary.month_models.iter().take(4) {
                    spent.push(Row::entry(model, usd(*amount)));
                }
            }
            Some(Err(why)) => spent.push(Row::note(format!("today's spend is unavailable: {why}"))),
            None => spent.push(Row::field("today", usd(guard.day_usd()))),
        }
        let caps = CapKind::ALL
            .into_iter()
            .map(|kind| match guard.usage(kind) {
                Some(usage) => cap_row(&usage),
                None => Row::field(&format!("{} cap", kind.word()), "off"),
            })
            .collect();
        Panel::new("Spend & caps")
            .aside("gz code")
            .section(Some("Spent"), spent)
            .section(Some("Your caps"), caps)
            .section(Some("Groundzero budget"), self.server_budget_rows())
            .footer("/budget session <usd> · /budget day <usd> · /budget off")
    }

    fn server_budget_rows(&self) -> Vec<Row> {
        let mut rows = match &self.spend.budget {
            Some(budget) => crate::cmd::usage::budget_rows(budget),
            None if self.spend.budget_loading => vec![Row::note("loading…")],
            None => vec![Row::note("not loaded yet")],
        };
        if let Some(why) = &self.spend.budget_error {
            rows.push(Row::note(format!("couldn't refresh: {why}")));
        }
        rows
    }
}

fn cap_row(usage: &CapUse) -> Row {
    Row::meter(
        &format!("{} cap", usage.kind.word()),
        usage.fraction(),
        format!("{} of {}", usd(usage.spent), usd_limit(usage.cap)),
    )
}

/// `Caps: session $5 · day off`.
fn caps_summary(caps: &Caps) -> String {
    format!(
        "Caps: session {} · day {}",
        caps.shown(CapKind::Session),
        caps.shown(CapKind::Day)
    )
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::stream::Usage;
    use crate::code_ui::state::test_support::*;
    use crate::spend::ledger::temp_ledger;
    use crossterm::event::KeyCode;
    use serde_json::json;

    use crate::assistant::TurnEnd;

    fn capped(session: Option<f64>, day: Option<f64>) -> UiState {
        let mut s = state();
        s.spend = SpendState::new(
            Caps {
                session_usd: session,
                day_usd: day,
            },
            None,
        );
        s
    }

    fn cost(usd: f64) -> AssistantEvent {
        AssistantEvent::Usage(Usage {
            cost_usd: Some(usd),
            ..Usage::default()
        })
    }

    fn turn(s: &mut UiState, prompt: &str, usd: f64) -> Vec<Effect> {
        s.start_turn(prompt);
        s.apply(cost(usd));
        s.apply(AssistantEvent::Done(TurnEnd::Completed))
    }

    fn notices(s: &UiState) -> Vec<String> {
        s.transcript
            .iter()
            .filter_map(|item| match item {
                Item::Notice(text) | Item::Error(text) => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    fn budget(available: &str, exhausted: bool) -> Value {
        json!({"policySource": "group", "policySubjectName": "data-team", "limitCents": 5000,
               "usedCents": 4100, "remainingCents": 900, "usedUsd": "41.0",
               "activeReservedUsd": "0", "availableUsd": available, "exhausted": exhausted,
               "windowDays": 30, "currency": "USD"})
    }

    #[test]
    fn session_spend_adds_main_turns_and_background_subagents() {
        let mut s = state();
        turn(&mut s, "one", 0.5);
        s.apply(AssistantEvent::ForSubagent {
            id: "sa_1".into(),
            event: Box::new(cost(0.25)),
        });
        turn(&mut s, "two", 0.49);
        assert_eq!(
            s.spend.status_text(s.turns).as_deref(),
            Some("$1.24 this session")
        );
        s.budget_arrived(Ok(budget("38.10", false)));
        assert_eq!(
            s.spend.status_text(s.turns).as_deref(),
            Some("$1.24 this session · $38.10 of $50 left (30d)")
        );
    }

    #[test]
    fn spend_is_written_to_the_ledger_per_model() {
        let ledger = temp_ledger("ui-ledger", "p");
        let mut s = with_catalog();
        s.spend = SpendState::new(Caps::default(), Some(ledger.clone()));
        turn(&mut s, "one", 0.30);
        let today = crate::spend::day::today();
        let summary = ledger.summary(&today).unwrap();
        assert_eq!(summary.month_models[0].0, "Claude Sonnet 5.5");
        assert!((summary.today_usd - 0.30).abs() < 1e-9);
    }

    #[test]
    fn a_turn_past_the_cap_waits_and_raising_sends_it() {
        let mut s = capped(Some(1.0), None);
        turn(&mut s, "one", 0.85);
        assert!(notices(&s)
            .iter()
            .any(|n| n.starts_with("⚠ 85% of your $1 session cap")));
        assert_eq!(
            s.spend.status_text(s.turns).as_deref(),
            Some("$0.85 this session · session cap 85%")
        );
        turn(&mut s, "two", 0.20);
        assert!(s.gate(Effect::Send("three".into())).is_empty(), "held");
        let picker = s.picker.clone().expect("the choice opens");
        assert!(
            picker
                .title
                .starts_with("$1.05 of your $1 session cap is spent"),
            "{}",
            picker.title
        );
        assert_eq!(
            picker.rows[0],
            PickerRow::Cap(CapChoice::Raise(CapKind::Session, 2.0))
        );
        let effects = s.on_key(key(KeyCode::Enter));
        assert_eq!(
            effects,
            vec![
                Effect::SetCaps(Caps {
                    session_usd: Some(2.0),
                    day_usd: None
                }),
                Effect::Send("three".into())
            ]
        );
        assert_eq!(
            s.gate(Effect::Send("three".into())),
            vec![Effect::Send("three".into())]
        );
    }

    #[test]
    fn continue_once_sends_one_turn_then_asks_again() {
        let mut s = capped(None, Some(1.0));
        turn(&mut s, "one", 1.0);
        s.gate(Effect::Send("two".into()));
        s.on_key(key(KeyCode::Down));
        let effects = s.on_key(key(KeyCode::Enter));
        assert_eq!(effects, vec![Effect::Send("two".into())]);
        assert_eq!(
            s.gate(Effect::Send("two".into())),
            vec![Effect::Send("two".into())]
        );
        s.start_turn("two");
        assert!(s.may_start_runs(), "the turn's subagents may start");
        s.apply(AssistantEvent::Done(TurnEnd::Completed));
        assert!(
            s.gate(Effect::Send("three".into())).is_empty(),
            "the next turn asks again"
        );
    }

    #[test]
    fn cancel_gives_the_message_back_and_drops_waiting_subagents() {
        let mut s = capped(Some(0.5), None);
        turn(&mut s, "one", 0.6);
        s.gate(Effect::Send("try again".into()));
        s.on_key(key(KeyCode::Esc));
        assert_eq!(s.input_text(), "try again");
        assert!(s.picker.is_none());
        assert!(notices(&s)
            .last()
            .unwrap()
            .starts_with("Not sent: you're past your cap"));
    }

    #[test]
    fn background_subagents_wait_past_the_cap() {
        use crate::subagents::SubagentRequest;
        let mut s = capped(Some(0.5), None);
        turn(&mut s, "one", 0.6);
        let effects = s.apply(AssistantEvent::SubagentRequested(SubagentRequest {
            id: "sa_9".into(),
            name: "Scan logs".into(),
            profile: "analyst".into(),
            prompt: "scan".into(),
            model: None,
            reasoning_effort: None,
            context_tokens: None,
        }));
        assert!(effects.is_empty(), "nothing starts past the cap");
        assert_eq!(s.queued.len(), 1);
        assert!(s
            .picker
            .as_ref()
            .unwrap()
            .title
            .contains("1 subagent(s) wait"));
        s.on_key(key(KeyCode::Down));
        let effects = s.on_key(key(KeyCode::Enter));
        assert!(
            matches!(effects.as_slice(), [Effect::StartSubagent(_)]),
            "{effects:?}"
        );
        assert!(
            !s.may_start_runs(),
            "the pass ended with the subagents it let through"
        );
    }

    #[test]
    fn a_used_up_server_budget_is_named_before_sending() {
        let mut s = state();
        s.budget_arrived(Ok(budget("0", true)));
        assert!(
            notices(&s)[0].contains("30-day AI budget ($50, set for group data-team) is used up")
        );
        assert_eq!(
            s.spend.status_text(0).as_deref(),
            Some("AI budget used up (30d)")
        );
        assert_eq!(s.spend.level(), Level::Over);
        let effects = s.gate(Effect::Send("hello".into()));
        assert_eq!(
            effects,
            vec![Effect::FetchBudget],
            "not sent; the budget is checked again"
        );
        assert_eq!(s.input_text(), "hello");
        assert!(notices(&s)
            .last()
            .unwrap()
            .ends_with("press enter again to send anyway."));
        s.budget_arrived(Ok(budget("0", true)));
        assert_eq!(
            s.gate(Effect::Send("hello".into())),
            vec![Effect::Send("hello".into())]
        );
    }

    #[test]
    fn the_budget_warns_once_at_eighty_percent() {
        let mut s = state();
        s.budget_arrived(Ok(budget("9.00", false)));
        s.budget_arrived(Ok(budget("8.00", false)));
        let warnings: Vec<String> = notices(&s)
            .into_iter()
            .filter(|n| n.contains("% of your 30-day AI budget"))
            .collect();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(s.spend.level(), Level::Warn);
    }

    #[test]
    fn a_failed_refresh_keeps_the_last_budget_quietly() {
        let mut s = state();
        assert_eq!(s.refresh_budget(false), Some(Effect::FetchBudget));
        assert_eq!(s.refresh_budget(false), None, "one request at a time");
        s.budget_arrived(Ok(budget("38.10", false)));
        assert_eq!(s.refresh_budget(false), None, "fresh enough");
        assert_eq!(s.refresh_budget(true), Some(Effect::FetchBudget));
        s.budget_arrived(Err("GET /chat/budget-status → HTTP 503".into()));
        assert!(notices(&s).is_empty(), "no transcript noise");
        assert_eq!(s.spend.budget.as_ref().unwrap().left_usd, Some(38.10));
        let panel = s.budget_panel().plain_lines().join("\n");
        assert!(
            panel.contains("couldn't refresh: GET /chat/budget-status → HTTP 503"),
            "{panel}"
        );
    }

    #[test]
    fn each_turn_end_refreshes_the_budget() {
        let mut s = state();
        s.start_turn("one");
        assert!(s.on_event(cost(0.1)).is_empty());
        let effects = s.on_event(AssistantEvent::Done(TurnEnd::Completed));
        assert_eq!(effects.first(), Some(&Effect::FetchBudget), "{effects:?}");
    }

    #[test]
    fn budget_commands_set_and_clear_caps() {
        let mut s = state();
        assert_eq!(
            s.budget_command("/budget session $5"),
            vec![Effect::SetCaps(Caps {
                session_usd: Some(5.0),
                day_usd: None
            })]
        );
        assert_eq!(
            s.budget_command("/budget day 12.50"),
            vec![Effect::SetCaps(Caps {
                session_usd: Some(5.0),
                day_usd: Some(12.5)
            })]
        );
        assert_eq!(notices(&s).last().unwrap(), "Caps: session $5 · day $12.50");
        assert_eq!(
            s.budget_command("/budget session off"),
            vec![Effect::SetCaps(Caps {
                session_usd: None,
                day_usd: Some(12.5)
            })]
        );
        assert_eq!(
            s.budget_command("/budget off"),
            vec![Effect::SetCaps(Caps::default())]
        );
        assert!(s.budget_command("/budget week 5").is_empty());
        assert!(s.budget_command("/budget day 0").is_empty());
        assert!(notices(&s).last().unwrap().contains("more than $0"));
    }

    #[test]
    fn the_budget_panel_shows_spend_caps_and_the_server_budget() {
        let ledger = temp_ledger("ui-panel", "p");
        let mut s = state();
        s.spend = SpendState::new(
            Caps {
                session_usd: Some(2.0),
                day_usd: None,
            },
            Some(ledger),
        );
        s.model = with_catalog()
            .catalog
            .unwrap()
            .resolve("opus", Some("high"));
        turn(&mut s, "one", 1.0);
        s.budget_arrived(Ok(budget("9.00", false)));
        let panel = s.budget_panel().plain_lines().join("\n");
        for part in [
            "this session  $1.00",
            "today         $1.00",
            "Claude Opus 5",
            "session cap",
            " 50% · $1.00 of $2",
            "day cap       off",
            " 82% · $9.00 of $50 left (30d)",
            "set for group data-team",
        ] {
            assert!(panel.contains(part), "missing {part:?} in\n{panel}");
        }
    }
}
