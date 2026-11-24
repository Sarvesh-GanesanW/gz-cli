//! Spend in `gz code`: every reported cost (main turns, inline and
//! background subagents) goes to the caps and the local ledger; the
//! server's rolling budget is refreshed after each turn, cached and off the
//! turn's path; a turn past a cap waits for the user's choice; and
//! `/budget` shows and sets the caps.

use std::time::{Duration, Instant};

use serde_json::Value;

use crate::assistant::AssistantEvent;
use crate::cmd::stream::{TokenCounts, Usage};
use crate::code_ui::items::Item;
use crate::code_ui::panel::{Panel, Row};
use crate::code_ui::usage::{Earlier, Tally};
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
    /// The running turn's estimated cost, until the server reports it.
    pub live_usd: f64,
    /// Live-estimate rates by model, as learned or read from the ledger;
    /// `None` caches a model the ledger has no rate for.
    rates: std::collections::HashMap<String, Option<f64>>,
    /// The part of this session's spend gz priced itself (earlier steps of
    /// multi-step turns); the session total is shown as approximate.
    pub estimated_usd: f64,
    /// Each running background subagent's steps since its last report.
    background_steps: std::collections::HashMap<String, Tally>,
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
            live_usd: 0.0,
            rates: Default::default(),
            estimated_usd: 0.0,
            background_steps: Default::default(),
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
        if self.live_usd > 0.0 || self.estimated_usd > 0.0 {
            let total = usd(guard.session_usd() + self.live_usd);
            // `<$0.01` already says it is rough; `~<$0.01` reads oddly.
            let approx = if total.starts_with('<') { "" } else { "~" };
            parts.push(format!("{approx}{total} this session"));
        } else if turns > 0 || guard.session_usd() > 0.0 {
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

    /// Prices what the running turn counted so far at the rate known for
    /// its model; nothing until a rate is known.
    pub(super) fn estimate_live_cost(&mut self) {
        let model = self.model_name();
        let Some(rate) = self.rate_for(model.as_deref()) else {
            return;
        };
        let tally = &mut self.usage.turn.tally;
        let estimate = spend::rates::estimate(rate, &tally.live_counts());
        tally.estimated_usd = Some(estimate);
        self.spend.live_usd = estimate;
    }

    /// The rate for `model`, read from the ledger once per session.
    fn rate_for(&mut self, model: Option<&str>) -> Option<f64> {
        let key = model.unwrap_or_default().to_string();
        if let Some(rate) = self.spend.rates.get(&key) {
            return *rate;
        }
        let rate = self.spend.meter.rate(model);
        self.spend.rates.insert(key, rate);
        rate
    }

    /// A main run ended with `report`. Its cost covers only its own leg:
    /// every client-tools resume is a separate run on the server, and a
    /// paused run's cost is never sent. So the rate is learned from the
    /// last leg alone, and the earlier legs are priced at that rate and
    /// counted (as estimates) in the turn, the caps and the ledger.
    pub(super) fn run_reported(&mut self, report: &Usage) {
        let model = self.model_name();
        let earlier = self.usage.turn.tally.earlier_steps(report);
        self.usage.turn.record(report);
        let priced = self.charge_run(report, model, earlier);
        self.usage.turn.tally.add_earlier(priced);
        self.spend.live_usd = 0.0;
    }

    /// One step of background subagent `id` finished.
    pub(super) fn background_step(&mut self, id: &str, tokens: &TokenCounts) {
        let steps = self.spend.background_steps.entry(id.to_string());
        steps.or_default().step(tokens);
    }

    /// A background subagent's run ended: priced like a main run, and
    /// counted in the session (not the turn: it is its own run).
    pub(super) fn background_reported(&mut self, id: &str, report: &Usage, model: Option<String>) {
        let mut tally = self.spend.background_steps.remove(id).unwrap_or_default();
        let earlier = tally.earlier_steps(report);
        tally.record(report);
        tally.add_earlier(self.charge_run(report, model, earlier));
        self.usage.add_to_session(&tally);
    }

    /// Charges a finished run's cost to the caps and ledger, learns the
    /// model's rate from its last leg, and prices the `earlier` legs.
    fn charge_run(
        &mut self,
        report: &Usage,
        model: Option<String>,
        earlier: Option<(usize, TokenCounts)>,
    ) -> Earlier {
        let Some(cost) = report.cost_usd else {
            return Earlier::None;
        };
        if let Some(tokens) = &report.tokens {
            self.learn_rate(model.as_deref(), cost, tokens);
        }
        self.spend_reported(cost, model.clone());
        let Some((legs, tokens)) = earlier else {
            return Earlier::None;
        };
        let Some(rate) = self.rate_for(model.as_deref()) else {
            return Earlier::Unpriced(legs);
        };
        let usd = spend::rates::estimate(rate, &tokens);
        if usd <= 0.0 {
            return Earlier::None;
        }
        self.spend.estimated_usd += usd;
        self.spend_reported(usd, model);
        Earlier::Priced(usd)
    }

    /// Moves `model`'s rate toward what `tokens` cost (`usd`).
    fn learn_rate(&mut self, model: Option<&str>, usd: f64, tokens: &TokenCounts) {
        let known = self.rate_for(model);
        let Some(rate) = spend::rates::learn(known, usd, tokens) else {
            return;
        };
        self.spend
            .rates
            .insert(model.unwrap_or_default().to_string(), Some(rate));
        self.spend.meter.keep_rate(model, rate);
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
    /// first (the next send goes anyway), a conversation about to cost more
    /// waits for the user's price choice, and past a cap the turn waits for
    /// theirs too. Everything else passes untouched.
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
        let Some(effect) = self.hold_for_price(effect) else {
            return Vec::new();
        };
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
    pub(super) fn give_back(&mut self, text: &str) {
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
    fn a_tiny_live_estimate_reads_as_under_a_cent_without_a_tilde() {
        let mut s = state();
        s.spend.live_usd = 0.002;
        assert_eq!(
            s.spend.status_text(s.turns).as_deref(),
            Some("<$0.01 this session")
        );
        s.spend.live_usd = 0.25;
        assert_eq!(
            s.spend.status_text(s.turns).as_deref(),
            Some("~$0.25 this session")
        );
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
    fn a_background_subagents_earlier_steps_are_priced_too() {
        let mut s = state();
        s.spend = SpendState::new(Caps::default(), Some(temp_ledger("bg-steps", "p")));
        let for_bg = |event| AssistantEvent::ForSubagent {
            id: "sa_1".into(),
            event: Box::new(event),
        };
        s.apply(for_bg(step(10_000, 400)));
        s.apply(for_bg(step(10_000, 400)));
        s.apply(for_bg(leg_report(10_000, 400, 0.03)));
        assert_eq!(s.usage.session.total_cost(), Some((0.06, true)));
        assert_eq!(
            s.usage.session.tokens.map(|t| t.input),
            Some(20_000),
            "both steps' tokens count"
        );
        assert_eq!(
            s.spend.status_text(s.turns).as_deref(),
            Some("~$0.06 this session")
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

    fn step(input: u64, output: u64) -> AssistantEvent {
        AssistantEvent::StepUsage(crate::cmd::stream::TokenCounts {
            input,
            output,
            ..Default::default()
        })
    }

    /// A run's final report: its own (last) leg's tokens and cost.
    fn leg_report(input: u64, output: u64, usd: f64) -> AssistantEvent {
        AssistantEvent::Usage(Usage {
            tokens: Some(crate::cmd::stream::TokenCounts {
                input,
                output,
                ..Default::default()
            }),
            cost_usd: Some(usd),
            ..Usage::default()
        })
    }

    #[test]
    fn cost_shows_live_once_a_rate_is_known() {
        let ledger = temp_ledger("live-cost", "p");
        let mut s = state();
        s.spend = SpendState::new(Caps::default(), Some(ledger.clone()));
        s.start_turn("first");
        s.apply(step(10_000, 400));
        assert_eq!(
            crate::code_ui::usage::live_tokens(&s.usage.turn.tally).as_deref(),
            Some("↑ 10.0k ↓ 400 tokens"),
            "tokens show at once, cost needs a rate first"
        );
        // Two legs; the server reports only the last one's cost.
        s.apply(step(10_000, 400));
        s.apply(leg_report(10_000, 400, 0.03));
        assert_eq!(
            s.usage.turn.tally.total_cost(),
            Some((0.06, true)),
            "the earlier leg is priced at the last leg's rate"
        );
        assert!(
            crate::code_ui::usage::turn_summary(&s.usage.turn).contains(" · ~$0.0600"),
            "{}",
            crate::code_ui::usage::turn_summary(&s.usage.turn)
        );
        s.apply(AssistantEvent::Done(TurnEnd::Completed));
        assert_eq!(
            s.spend.status_text(s.turns).as_deref(),
            Some("~$0.06 this session")
        );
        assert!(
            (s.spend.meter.guard.session_usd() - 0.06).abs() < 1e-9,
            "caps count the estimated leg too"
        );

        s.start_turn("second");
        s.apply(step(10_000, 400));
        assert_eq!(
            s.spend.status_text(s.turns).as_deref(),
            Some("~$0.09 this session"),
            "one leg priced at the rate of one leg"
        );
        assert_eq!(
            crate::code_ui::usage::live_tokens(&s.usage.turn.tally).as_deref(),
            Some("↑ 10.0k ↓ 400 tokens · ~$0.0300")
        );
        s.apply(AssistantEvent::Text("x".repeat(4_000)));
        assert!(s.spend.live_usd > 0.03, "streamed text counts too");
        s.apply(leg_report(10_000, 400, 0.02));
        assert_eq!(s.usage.turn.tally.total_cost(), Some((0.02, false)));
        assert_eq!(
            s.spend.status_text(s.turns).as_deref(),
            Some("~$0.08 this session")
        );
        assert_eq!(s.usage.turn.tally.estimated_usd, None);

        // A new session starts with the rate it learned.
        let mut next = state();
        next.spend = SpendState::new(Caps::default(), Some(ledger));
        next.start_turn("third");
        next.apply(step(10_000, 400));
        assert!(next.spend.live_usd > 0.0);
        next.apply(AssistantEvent::Done(TurnEnd::Cancelled));
        assert_eq!(
            next.spend.live_usd, 0.0,
            "an estimate never outlives its turn"
        );
    }

    #[test]
    fn earlier_legs_without_a_rate_are_named_not_priced() {
        let mut s = state();
        s.spend = SpendState::new(Caps::default(), Some(temp_ledger("unpriced", "p")));
        s.start_turn("first");
        s.apply(step(10_000, 400));
        s.apply(step(0, 0));
        s.apply(cost(0.03));
        let summary = crate::code_ui::usage::turn_summary(&s.usage.turn);
        assert!(
            summary.contains(" · $0.0300 + 1 earlier step not priced"),
            "{summary}"
        );
        s.apply(AssistantEvent::Done(TurnEnd::Completed));
        assert_eq!(
            s.spend.status_text(s.turns).as_deref(),
            Some("$0.03 this session")
        );
    }

    #[test]
    fn the_ledger_is_read_once_for_a_model_without_a_rate() {
        let ledger = temp_ledger("rate-cache", "p");
        let mut s = state();
        s.spend = SpendState::new(Caps::default(), Some(ledger.clone()));
        s.start_turn("first");
        s.apply(AssistantEvent::Text("a".into()));
        // A rate written behind the session's back is not seen: the miss
        // was cached instead of re-reading the file on every delta.
        ledger.set_rate(s.model_name().as_deref(), 1e-6).unwrap();
        s.apply(AssistantEvent::Text("b".repeat(400)));
        assert_eq!(s.spend.live_usd, 0.0);
    }

    #[test]
    fn an_open_prompt_stops_the_turns_work_clock() {
        use crate::assistant::permissions::PermissionRequest;
        use crate::assistant::tools::Access;
        let mut s = state();
        s.start_turn("edit it");
        s.on_tick();
        assert!(s.usage.turn.waiting().is_zero(), "no prompt yet");
        let (tx, _rx) = tokio::sync::oneshot::channel();
        s.open_prompt(crate::code_ui::approver::Ask {
            request: PermissionRequest {
                access: Access::Write,
                summary: "edit_file a.py".into(),
                preview: None,
                chat_scope: "all edits in this chat".into(),
                always_scope: None,
                action: None,
                asked_by: None,
                target: None,
                note: None,
            },
            reply: tx,
            background: None,
        });
        s.on_tick();
        std::thread::sleep(Duration::from_millis(30));
        assert!(s.usage.turn.waiting() >= Duration::from_millis(30));
        s.prompt = None;
        s.on_tick();
        let waited = s.usage.turn.waited;
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(s.usage.turn.waiting(), waited, "the clock runs again");
    }

    #[test]
    fn a_background_subagents_prompt_leaves_the_main_clock_running() {
        use crate::assistant::permissions::PermissionRequest;
        use crate::assistant::tools::Access;
        let mut s = state();
        s.start_turn("edit it");
        let (tx, _rx) = tokio::sync::oneshot::channel();
        s.open_prompt(crate::code_ui::approver::Ask {
            request: PermissionRequest {
                access: Access::Write,
                summary: "edit_file a.py".into(),
                preview: None,
                chat_scope: "all edits in this chat".into(),
                always_scope: None,
                action: None,
                asked_by: None,
                target: None,
                note: None,
            },
            reply: tx,
            background: Some("bg-1".into()),
        });
        assert!(s.prompt.is_some());
        s.on_tick();
        std::thread::sleep(Duration::from_millis(20));
        assert!(s.usage.turn.waiting().is_zero());
    }
}
