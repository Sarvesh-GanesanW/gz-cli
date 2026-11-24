//! Token, cost, and per-turn analytics for `gz code`: a live estimate while
//! an answer streams (the server reports real counts only once it's done),
//! the summary line after each turn with its full analytics behind Ctrl+O
//! and `/last`, and the session totals behind `/usage`.

use std::time::{Duration, Instant};

use unicode_width::UnicodeWidthStr;

use crate::cmd::stream::{ContextUsage, TokenCounts, Usage};
use crate::code_ui::panel::{Panel, Row};
use crate::subagents::{family_order, Status, Subagent};

/// Rough characters per token, for the live estimate only.
const CHARS_PER_TOKEN: usize = 4;
/// The footer warns once this share of the auto-compact threshold is left.
const CONTEXT_WARN_LEFT: f64 = 0.10;

/// Tokens and spend one turn (or the whole session) reported so far.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tally {
    /// Summed reported counts; `None` until a report arrives.
    pub tokens: Option<TokenCounts>,
    pub cost_usd: Option<f64>,
    pub reasoning_tokens: Option<u64>,
    /// Characters streamed since the last report, still only estimated.
    pub unreported_chars: usize,
    /// Tokens of the steps finished since the last full report: each run
    /// leg sends its counts as it ends, the cost only comes with the turn.
    pub steps: Option<TokenCounts>,
    /// The running turn's cost as estimated so far; `None` once the server
    /// reported the real one (or when no rate is known yet).
    pub estimated_usd: Option<f64>,
    /// How many run legs `steps` adds up.
    pub step_legs: usize,
    /// What gz priced for the steps before the last one of each turn. Every
    /// client-tools resume is billed as its own run, and only the last
    /// run's cost comes back, so these are always estimates.
    pub earlier_usd: Option<f64>,
    /// Earlier steps no rate was known for, so their cost is missing.
    pub unpriced_steps: usize,
}

impl Tally {
    pub fn record(&mut self, usage: &Usage) {
        // The report may count only the last step or the whole turn, so it
        // never lowers what the steps already added up to.
        let counts = self.turn_tokens(usage);
        self.steps = None;
        self.step_legs = 0;
        if let Some(counts) = &counts {
            self.tokens
                .get_or_insert_with(TokenCounts::default)
                .add(counts);
        }
        self.estimated_usd = None;
        if let Some(cost) = usage.cost_usd {
            *self.cost_usd.get_or_insert(0.0) += cost;
        }
        if let Some(reasoning) = usage.reasoning_tokens {
            *self.reasoning_tokens.get_or_insert(0) += reasoning;
        }
        self.unreported_chars = 0;
    }

    pub fn streamed(&mut self, chars: usize) {
        self.unreported_chars += chars;
    }

    /// The tokens of the whole run `report` closes: the report may count
    /// only its last leg or every leg, so it never lowers what the finished
    /// steps already add up to.
    pub fn turn_tokens(&self, report: &Usage) -> Option<TokenCounts> {
        match (report.tokens, self.steps) {
            (Some(reported), Some(steps)) => Some(at_least(reported, steps)),
            (reported, steps) => reported.or(steps),
        }
    }

    /// One step's counts: they cover the text streamed so far.
    pub fn step(&mut self, counts: &TokenCounts) {
        self.steps
            .get_or_insert_with(TokenCounts::default)
            .add(counts);
        self.step_legs += 1;
        self.unreported_chars = 0;
    }

    /// The legs before the last one of the run `report` closes, and their
    /// tokens: `report` counts only its own (last) leg.
    pub fn earlier_steps(&self, report: &Usage) -> Option<(usize, TokenCounts)> {
        let steps = self.steps?;
        if self.step_legs < 2 {
            return None;
        }
        let last = report.tokens.unwrap_or_default();
        Some((self.step_legs - 1, minus(steps, last)))
    }

    /// Earlier steps priced at `usd` (an estimate).
    pub fn priced_earlier(&mut self, usd: f64) {
        *self.earlier_usd.get_or_insert(0.0) += usd;
    }

    /// What pricing a run's earlier steps came to.
    pub fn add_earlier(&mut self, earlier: Earlier) {
        match earlier {
            Earlier::None => {}
            Earlier::Priced(usd) => self.priced_earlier(usd),
            Earlier::Unpriced(steps) => self.unpriced_steps += steps,
        }
    }

    /// Everything spent: the reported cost plus the earlier steps priced
    /// here, and whether any of it is estimated; `None` before any cost.
    pub fn total_cost(&self) -> Option<(f64, bool)> {
        let reported = self.cost_usd?;
        Some((
            reported + self.earlier_usd.unwrap_or(0.0),
            self.earlier_usd.is_some(),
        ))
    }

    /// Steps never followed by a full report (a cancelled or failed turn)
    /// still count.
    pub fn settle_steps(&mut self) {
        self.step_legs = 0;
        if let Some(steps) = self.steps.take() {
            self.tokens
                .get_or_insert_with(TokenCounts::default)
                .add(&steps);
        }
    }

    /// Everything counted so far, steps included; `None` before any count.
    pub fn counted(&self) -> Option<TokenCounts> {
        match (self.tokens, self.steps) {
            (Some(mut tokens), Some(steps)) => {
                tokens.add(&steps);
                Some(tokens)
            }
            (tokens, steps) => tokens.or(steps),
        }
    }

    /// [`Self::counted`] plus an estimate for text not yet counted: what
    /// the live cost estimate prices.
    pub fn live_counts(&self) -> TokenCounts {
        let mut counts = self.counted().unwrap_or_default();
        counts.output += self.unreported_chars.div_ceil(CHARS_PER_TOKEN) as u64;
        counts
    }

    /// Output tokens: counted ones plus an estimate for uncounted text.
    /// The flag is true when any part is estimated.
    pub fn output_tokens(&self) -> (u64, bool) {
        let counted = self.counted();
        let reported = counted.map(|t| t.output).unwrap_or(0);
        let estimate = self.unreported_chars.div_ceil(CHARS_PER_TOKEN) as u64;
        (reported + estimate, estimate > 0 || counted.is_none())
    }

    fn merge(&mut self, other: &Tally) {
        if let Some(counts) = &other.tokens {
            self.tokens
                .get_or_insert_with(TokenCounts::default)
                .add(counts);
        }
        if let Some(cost) = other.cost_usd {
            *self.cost_usd.get_or_insert(0.0) += cost;
        }
        if let Some(reasoning) = other.reasoning_tokens {
            *self.reasoning_tokens.get_or_insert(0) += reasoning;
        }
        if let Some(usd) = other.earlier_usd {
            self.priced_earlier(usd);
        }
        self.unpriced_steps += other.unpriced_steps;
        self.unreported_chars += other.unreported_chars;
    }
}

/// Each count of `all` less `part`'s.
fn minus(all: TokenCounts, part: TokenCounts) -> TokenCounts {
    TokenCounts {
        input: all.input.saturating_sub(part.input),
        output: all.output.saturating_sub(part.output),
        cache_read: all.cache_read.saturating_sub(part.cache_read),
        cache_write: all.cache_write.saturating_sub(part.cache_write),
    }
}

/// `$0.0300`, or `~$0.0500` when earlier steps were priced here, plus a
/// note when some could not be priced.
fn cost_text(tally: &Tally) -> Option<String> {
    let (usd, estimated) = tally.total_cost()?;
    let approx = if estimated { "~" } else { "" };
    let mut text = format!("{approx}${usd:.4}");
    if tally.unpriced_steps > 0 {
        text.push_str(&format!(
            " + {} not priced",
            plural(tally.unpriced_steps, "earlier step")
        ));
    }
    Some(text)
}

/// [`cost_text`] with where each part came from, for the analytics panels.
fn cost_detail(tally: &Tally) -> Option<String> {
    let mut text = cost_text(tally)?;
    if let (Some(reported), Some(earlier)) = (tally.cost_usd, tally.earlier_usd) {
        text.push_str(&format!(
            " · ${reported:.4} reported · ~${earlier:.4} estimated for earlier steps"
        ));
    }
    Some(text)
}

/// The steps of a run before its last, once its cost arrived.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Earlier {
    /// There were none, or they cost nothing.
    None,
    /// Priced at the model's rate (an estimate), in dollars.
    Priced(f64),
    /// This many could not be priced: no rate known for the model.
    Unpriced(usize),
}

/// Each count the larger of the two.
fn at_least(reported: TokenCounts, steps: TokenCounts) -> TokenCounts {
    TokenCounts {
        input: reported.input.max(steps.input),
        output: reported.output.max(steps.output),
        cache_read: reported.cache_read.max(steps.cache_read),
        cache_write: reported.cache_write.max(steps.cache_write),
    }
}

/// Tool activity in a turn or session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ToolCounts {
    /// Tools gz ran on this machine.
    pub local: usize,
    /// Tools the server ran itself.
    pub server: usize,
    /// `gz.*` helper calls made from inside scripts.
    pub helper: usize,
    /// Bytes of local tool output sent to the model.
    pub context_bytes: usize,
}

impl ToolCounts {
    fn add(&mut self, other: &ToolCounts) {
        self.local += other.local;
        self.server += other.server;
        self.helper += other.helper;
        self.context_bytes += other.context_bytes;
    }

    fn calls(&self) -> usize {
        self.local + self.server
    }
}

/// Everything measured about one question and its answer.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnStats {
    /// Model and effort the turn was sent with, as shown in the mode line.
    pub model: Option<String>,
    pub started: Instant,
    pub wall: Option<Duration>,
    /// Time until the first answer text arrived, measured here.
    pub first_text: Option<Duration>,
    /// The server's own measurements, from the last report.
    pub server_first_token_ms: Option<u64>,
    pub tokens_per_second: Option<f64>,
    pub tally: Tally,
    pub tools: ToolCounts,
    /// Subagents the turn ran, as they finished (usage from the run's
    /// metrics once those arrive).
    pub subagents: Vec<Subagent>,
    /// Time a permission prompt or question waited on the user; not work.
    pub waited: Duration,
    waiting_since: Option<Instant>,
}

impl TurnStats {
    pub fn start(model: Option<String>) -> Self {
        Self {
            model,
            started: Instant::now(),
            wall: None,
            first_text: None,
            server_first_token_ms: None,
            tokens_per_second: None,
            tally: Tally::default(),
            tools: ToolCounts::default(),
            subagents: Vec::new(),
            waited: Duration::ZERO,
            waiting_since: None,
        }
    }

    /// Marks whether the turn is waiting on the user right now (a prompt
    /// or question is open); safe to call repeatedly.
    pub fn set_waiting(&mut self, waiting: bool) {
        self.set_waiting_at(waiting, Instant::now());
    }

    /// [`Self::set_waiting`] as of `now`.
    pub fn set_waiting_at(&mut self, waiting: bool, now: Instant) {
        match (waiting, self.waiting_since) {
            (true, None) => self.waiting_since = Some(now),
            (false, Some(since)) => {
                self.waited += now.saturating_duration_since(since);
                self.waiting_since = None;
            }
            _ => {}
        }
    }

    /// Time waiting on the user so far, an open wait included.
    pub fn waiting(&self) -> Duration {
        self.waiting_at(Instant::now())
    }

    /// [`Self::waiting`] as of `now`.
    pub fn waiting_at(&self, now: Instant) -> Duration {
        self.waited
            + self
                .waiting_since
                .map_or(Duration::ZERO, |since| now.saturating_duration_since(since))
    }

    /// Wall time minus the time spent waiting on the user.
    pub fn worked(&self) -> Duration {
        self.elapsed().saturating_sub(self.waiting())
    }

    pub fn streamed(&mut self, chars: usize) {
        self.first_text
            .get_or_insert_with(|| self.started.elapsed());
        self.tally.streamed(chars);
    }

    pub fn record(&mut self, usage: &Usage) {
        self.tally.record(usage);
        for (id, share) in &usage.subagents {
            if let Some(agent) = self.subagents.iter_mut().find(|a| a.id == *id) {
                agent.usage = *share;
            }
        }
        if usage.time_to_first_token_ms.is_some() {
            self.server_first_token_ms = usage.time_to_first_token_ms;
        }
        if usage.output_tokens_per_second.is_some() {
            self.tokens_per_second = usage.output_tokens_per_second;
        }
    }

    fn elapsed(&self) -> Duration {
        self.wall.unwrap_or_else(|| self.started.elapsed())
    }
}

/// The current turn, the one before it, and the session totals.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageBook {
    pub turn: TurnStats,
    pub last: Option<TurnStats>,
    /// The conversation's context fill as of its last finished run.
    pub context: Option<ContextUsage>,
    pub session: Tally,
    pub session_tools: ToolCounts,
    pub session_subagents: usize,
    /// Time turns spent working, waits on the user left out.
    pub session_time: Duration,
    /// Time turns waited on the user's answers.
    pub session_waited: Duration,
    /// When the session started, for how long it has been open.
    pub opened: Instant,
    pub turns: usize,
}

impl Default for UsageBook {
    fn default() -> Self {
        Self {
            turn: TurnStats::start(None),
            last: None,
            context: None,
            session: Tally::default(),
            session_tools: ToolCounts::default(),
            session_subagents: 0,
            session_time: Duration::ZERO,
            session_waited: Duration::ZERO,
            opened: Instant::now(),
            turns: 0,
        }
    }
}

impl UsageBook {
    /// The context fill a run reported. A run that reported no count (an
    /// interrupted one) keeps the last known one, so the fill and the
    /// countdown to auto-compact stay on screen.
    pub fn set_context(&mut self, mut context: ContextUsage) {
        if context.tokens.is_none() {
            context.tokens = self.context.as_ref().and_then(|old| old.tokens);
        }
        self.context = Some(context);
    }

    /// Counts a run that is not part of the turn (a background subagent's)
    /// in the session.
    pub fn add_to_session(&mut self, tally: &Tally) {
        self.session.merge(tally);
    }

    pub fn start_turn(&mut self, model: Option<String>) {
        self.turn = TurnStats::start(model);
    }

    /// Closes the turn: stamps its wall time and folds it into the session.
    pub fn end_turn(&mut self) -> &TurnStats {
        self.turn.set_waiting(false);
        self.turn.wall = Some(self.turn.started.elapsed());
        self.turn.tally.settle_steps();
        self.turn.tally.estimated_usd = None;
        self.session.merge(&self.turn.tally);
        self.session_tools.add(&self.turn.tools);
        self.session_subagents += self.turn.subagents.len();
        self.session_time += self.turn.worked();
        self.session_waited += self.turn.waited;
        self.turns += 1;
        self.last = Some(self.turn.clone());
        &self.turn
    }
}

/// `1234` → `1.2k`, `1_500_000` → `1.5M`.
pub fn compact(n: u64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1_000_000.0),
        n if n >= 1_000 => format!("{:.1}k", n as f64 / 1_000.0),
        n => n.to_string(),
    }
}

/// [`compact`] without a trailing `.0`: `200k`, `1M`, `82.1k`.
pub fn short(n: u64) -> String {
    let text = compact(n);
    match text.strip_suffix(|c: char| c.is_ascii_alphabetic()) {
        Some(number) if number.ends_with(".0") => {
            format!("{}{}", &number[..number.len() - 2], &text[number.len()..])
        }
        _ => text,
    }
}

/// The Context section of `/usage`: a fill meter against the window, how
/// far it is from auto-compact, and how much history was resent.
fn context_rows(context: &ContextUsage) -> Vec<Row> {
    let threshold = short(context.auto_compact_at);
    let mut rows = vec![match (context.tokens, context.used()) {
        (Some(tokens), Some(used)) => {
            let headroom = match context.left_before_compact() {
                Some(left) if left > 0 => {
                    format!("auto-compacts at {threshold} ({} to go)", short(left))
                }
                _ => format!("past {threshold}, older messages get summarized"),
            };
            Row::meter(
                "used",
                used,
                format!(
                    "{} of {} model budget · {headroom}",
                    short(tokens),
                    short(context.window)
                ),
            )
        }
        _ => Row::field(
            "used",
            format!(
                "not reported by this model · {} model budget · auto-compacts at {threshold}",
                short(context.window)
            ),
        ),
    }];
    if let Some(history) = context.history {
        let mut text = format!(
            "{} earlier messages resent in full (up to {})",
            history.replayed, history.limit
        );
        if history.summarized > 0 {
            text.push_str(&format!(" · {} older ones summarized", history.summarized));
        }
        rows.push(Row::field("history", text));
    }
    rows
}

/// The footer's countdown to auto-compact, like a coding CLI's: `120k until
/// auto-compact`, highlighted once little is left.
pub fn context_note(context: &ContextUsage) -> Option<(String, bool)> {
    let left = context.left_before_compact()?;
    let warn = (left as f64) <= context.auto_compact_at as f64 * CONTEXT_WARN_LEFT;
    Some((format!("{} until auto-compact", short(left)), warn))
}

/// `↑ 12.4k ↓ ~1.2k tokens · ~$0.0310` for the spinner: counts as each
/// step reports them, and the estimated cost once a rate is known.
pub fn live_tokens(tally: &Tally) -> Option<String> {
    let (tokens, cost) = live_parts(tally);
    let tokens = tokens?;
    Some(match cost {
        Some(cost) => format!("{tokens} · {cost}"),
        None => tokens,
    })
}

/// The token counts and the estimated cost of [`live_tokens`], apart.
fn live_parts(tally: &Tally) -> (Option<String>, Option<String>) {
    let (output, estimated) = tally.output_tokens();
    let prompt = tally.counted().map_or(0, |t| t.prompt());
    if output == 0 && prompt == 0 {
        return (None, None);
    }
    let approx = if estimated { "~" } else { "" };
    let tokens = match prompt {
        0 => format!("↓ {approx}{} tokens", compact(output)),
        _ => format!("↑ {} ↓ {approx}{} tokens", compact(prompt), compact(output)),
    };
    let cost = tally
        .estimated_usd
        .filter(|usd| *usd > 0.0)
        .map(|usd| format!("~${usd:.4}"));
    (Some(tokens), cost)
}

/// The spinner's `(12s · ↑ 10.0k ↓ 400 tokens · ~$0.0300 · esc to
/// interrupt)`, kept within `room` columns: the cost goes first, then the
/// counts; the time and how to interrupt always stay.
pub fn spinner_stats(secs: u64, tally: &Tally, room: usize) -> String {
    let (tokens, cost) = live_parts(tally);
    let shapes = [[tokens.clone(), cost], [tokens, None], [None, None]];
    let mut text = String::new();
    for parts in shapes {
        let mut stats = vec![format!("{secs}s")];
        stats.extend(parts.into_iter().flatten());
        stats.push("esc to interrupt".to_string());
        text = format!("({})", stats.join(" · "));
        if text.width() <= room {
            break;
        }
    }
    text
}

/// `850ms`, `24s`, `1m 5s`.
pub fn duration(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    match secs {
        0 => format!("{}ms", elapsed.as_millis()),
        s if s < 60 => format!("{s}s"),
        s => format!("{}m {}s", s / 60, s % 60),
    }
}

fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

/// The dim line after a turn: `✻ Worked for 24s · ↑ 3.1k ↓ 812 tokens`,
/// ending with the model that answered, so a model change is seen to take.
pub fn turn_summary(stats: &TurnStats) -> String {
    let tally = &stats.tally;
    let mut line = format!("✻ Worked for {}", duration(stats.worked()));
    match tally.tokens {
        Some(tokens) => {
            let (output, estimated) = tally.output_tokens();
            let approx = if estimated { "~" } else { "" };
            line.push_str(&format!(
                " · ↑ {} ↓ {approx}{} tokens",
                compact(tokens.prompt()),
                compact(output)
            ));
        }
        None => {
            if let Some(tokens) = live_tokens(tally) {
                line.push_str(&format!(" · {tokens} (estimated)"));
            }
        }
    }
    if let Some(cost) = cost_text(tally) {
        line.push_str(&format!(" · {cost}"));
    }
    if stats.tools.calls() > 0 {
        line.push_str(&format!(" · {}", plural(stats.tools.calls(), "tool")));
    }
    if !stats.subagents.is_empty() {
        line.push_str(&format!(" · {}", plural(stats.subagents.len(), "subagent")));
    }
    if let Some(model) = &stats.model {
        line.push_str(&format!(" · {model}"));
    }
    line
}

/// One row per subagent: `Check logs (data_engineer)  12s · ↑ 1.2k ↓ 300 · $0.0021 · done`.
fn subagent_rows(agents: &[Subagent]) -> Vec<Row> {
    // Children finish first; list each under the subagent that started it.
    let agents: Vec<&Subagent> = agents.iter().collect();
    family_order(&agents)
        .into_iter()
        .map(|i| agents[i])
        .map(|agent| {
            let profile = agent
                .profile
                .as_deref()
                .map(|p| format!(" ({p})"))
                .unwrap_or_default();
            let mut parts: Vec<String> = agent.duration.map(duration).into_iter().collect();
            let usage = agent.usage;
            if usage.input_tokens + usage.output_tokens > 0 {
                parts.push(format!(
                    "↑ {} ↓ {}",
                    compact(usage.input_tokens),
                    compact(usage.output_tokens)
                ));
            }
            if let Some(cost) = usage.cost_usd {
                parts.push(format!("${cost:.4}"));
            }
            parts.push(agent.status.word().to_string());
            let pad = "  ".repeat(agent.depth);
            Row::entry(&format!("{pad}{}{profile}", agent.name), parts.join(" · "))
        })
        .collect()
}

/// `Subagents · 2 · 1 failed`.
fn subagent_heading(agents: &[Subagent]) -> String {
    let failed = agents.iter().filter(|a| a.status == Status::Failed).count();
    let mut heading = format!("Subagents · {}", agents.len());
    if failed > 0 {
        heading.push_str(&format!(" · {failed} failed"));
    }
    heading
}

/// The full analytics of one turn (Ctrl+O transcript and `/last`).
pub fn turn_details(stats: &TurnStats) -> Panel {
    let tally = &stats.tally;
    let mut timing = vec![Row::field("wall time", duration(stats.elapsed()))];
    if !stats.waiting().is_zero() {
        timing.push(Row::field(
            "waiting for you",
            format!(
                "{} · worked {}",
                duration(stats.waiting()),
                duration(stats.worked())
            ),
        ));
    }
    let first = match (stats.server_first_token_ms, stats.first_text) {
        (Some(ms), _) => Some(duration(Duration::from_millis(ms))),
        (None, Some(local)) => Some(format!("{} (measured here)", duration(local))),
        (None, None) => None,
    };
    if let Some(first) = first {
        timing.push(Row::field("first token", first));
    }
    if let Some(speed) = stats.tokens_per_second {
        timing.push(Row::field("speed", format!("{speed:.0} tokens/s")));
    }
    let mut panel = Panel::new("Turn analytics");
    if let Some(model) = &stats.model {
        panel = panel.aside(model.clone());
    }
    panel
        .section(None, timing)
        .section(Some("Tokens"), token_rows(tally))
        .section(None, work_rows(tally, &stats.tools))
        .section(
            Some(&subagent_heading(&stats.subagents)),
            subagent_rows(&stats.subagents),
        )
}

/// Cost, tools, and context sent, shared by the turn and session panels.
fn work_rows(tally: &Tally, tools: &ToolCounts) -> Vec<Row> {
    let mut rows = Vec::new();
    if let Some(cost) = cost_detail(tally) {
        rows.push(Row::field("cost", cost));
    }
    rows.push(Row::field(
        "tools",
        format!(
            "{} local · {} server · {} script helper calls",
            tools.local, tools.server, tools.helper
        ),
    ));
    rows.push(Row::field("context sent", bytes(tools.context_bytes)));
    rows
}

fn token_rows(tally: &Tally) -> Vec<Row> {
    let Some(t) = tally.tokens else {
        let (output, _) = tally.output_tokens();
        return vec![Row::field(
            "output",
            format!("~{} (estimated; not reported)", group(output)),
        )];
    };
    let mut counts = vec![
        ("input", t.input),
        ("cache read", t.cache_read),
        ("cache write", t.cache_write),
        ("output", t.output),
    ];
    if let Some(reasoning) = tally.reasoning_tokens {
        counts.push(("reasoning", reasoning));
    }
    counts.push(("total", t.prompt() + t.output));
    right_aligned(&counts, group)
}

/// Rows whose numbers share a right edge, so digits line up.
pub fn right_aligned(counts: &[(&str, u64)], format: fn(u64) -> String) -> Vec<Row> {
    let shown: Vec<String> = counts.iter().map(|(_, n)| format(*n)).collect();
    let width = shown.iter().map(String::len).max().unwrap_or(0);
    counts
        .iter()
        .zip(shown)
        .map(|((label, _), value)| Row::field(label, format!("{value:>width$}")))
        .collect()
}

/// The `/usage` report for this session.
pub fn report(book: &UsageBook) -> Panel {
    let mut work = work_rows(&book.session, &book.session_tools);
    if book.session_subagents > 0 {
        work.push(Row::field("subagents", book.session_subagents.to_string()));
    }
    if book.turns == 0 {
        return Panel::new("This session").section(
            None,
            vec![Row::note(
                "No turns yet · tokens, cost, and tools show here after your first question",
            )],
        );
    }
    let context = book.context.as_ref().map(context_rows).unwrap_or_default();
    Panel::new("This session")
        .aside(plural(book.turns, "turn"))
        .section(Some("Context"), context)
        .section(Some("Tokens"), token_rows(&book.session))
        .section(None, work)
        .section(None, vec![Row::field("time", session_time(book))])
}

/// `4m 2s working · 3m 4s waiting for you · open 10m 6s`.
fn session_time(book: &UsageBook) -> String {
    let mut parts = vec![format!("{} working", duration(book.session_time))];
    if !book.session_waited.is_zero() {
        parts.push(format!("{} waiting for you", duration(book.session_waited)));
    }
    parts.push(format!("open {}", duration(book.opened.elapsed())));
    parts.join(" · ")
}

/// `1234567` → `1,234,567`.
pub fn group(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// `950 B`, `12.3 KB`, `4.1 MB`.
fn bytes(n: usize) -> String {
    match n {
        n if n >= 1_048_576 => format!("{:.1} MB", n as f64 / 1_048_576.0),
        n if n >= 1024 => format!("{:.1} KB", n as f64 / 1024.0),
        n => format!("{n} B"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u64, output: u64, cache_read: u64, cost: Option<f64>) -> Usage {
        Usage {
            tokens: Some(TokenCounts {
                input,
                output,
                cache_read,
                cache_write: 0,
            }),
            cost_usd: cost,
            ..Usage::default()
        }
    }

    fn finished(seconds: u64) -> TurnStats {
        let mut stats = TurnStats::start(Some("Claude Opus 5 · high".into()));
        stats.wall = Some(Duration::from_secs(seconds));
        stats
    }

    #[test]
    fn estimates_until_a_report_then_uses_real_counts() {
        let mut tally = Tally::default();
        tally.streamed(4_801);
        assert_eq!(live_tokens(&tally).as_deref(), Some("↓ ~1.2k tokens"));
        tally.record(&usage(1_000, 812, 2_100, None));
        assert_eq!(live_tokens(&tally).as_deref(), Some("↑ 3.1k ↓ 812 tokens"));
        tally.streamed(40);
        assert_eq!(tally.output_tokens(), (822, true));
    }

    fn counts(input: u64, output: u64, cache_read: u64) -> TokenCounts {
        TokenCounts {
            input,
            output,
            cache_read,
            cache_write: 0,
        }
    }

    #[test]
    fn step_counts_show_live_and_the_final_report_never_lowers_them() {
        let mut tally = Tally::default();
        tally.streamed(400);
        tally.step(&counts(1_000, 120, 9_000));
        assert_eq!(tally.unreported_chars, 0, "the step counted that text");
        assert_eq!(live_tokens(&tally).as_deref(), Some("↑ 10.0k ↓ 120 tokens"));
        tally.step(&counts(500, 30, 11_000));
        tally.estimated_usd = Some(0.0123);
        assert_eq!(
            live_tokens(&tally).as_deref(),
            Some("↑ 21.5k ↓ 150 tokens · ~$0.0123")
        );
        // A report of only the last step: the steps' sum stands.
        tally.record(&usage(500, 30, 11_000, Some(0.02)));
        assert_eq!(tally.tokens, Some(counts(1_500, 150, 20_000)));
        assert_eq!(tally.estimated_usd, None);
        assert_eq!(tally.cost_usd, Some(0.02));
        // A report of the whole turn, larger than the steps: it stands.
        let mut whole = Tally::default();
        whole.step(&counts(100, 10, 0));
        whole.record(&usage(900, 50, 4_000, None));
        assert_eq!(whole.tokens, Some(counts(900, 50, 4_000)));
    }

    #[test]
    fn earlier_steps_are_the_legs_before_the_reported_one() {
        let mut tally = Tally::default();
        let last = usage(1_000, 100, 9_000, Some(0.03));
        assert_eq!(tally.earlier_steps(&last), None, "no steps");
        tally.step(&counts(1_000, 100, 9_000));
        assert_eq!(tally.earlier_steps(&last), None, "one leg is the last");
        tally.step(&counts(3_000, 200, 9_500));
        tally.step(&counts(1_000, 100, 9_000));
        assert_eq!(
            tally.earlier_steps(&last),
            Some((2, counts(4_000, 300, 18_500)))
        );
        tally.record(&last);
        assert_eq!(tally.step_legs, 0);
    }

    #[test]
    fn priced_earlier_steps_show_as_an_estimate_with_their_parts() {
        let mut book = UsageBook::default();
        book.start_turn(None);
        book.turn.tally.record(&usage(1_000, 100, 0, Some(0.03)));
        book.turn.tally.priced_earlier(0.02);
        let details = turn_details(&book.turn).plain_lines().join("\n");
        assert!(
            details.contains("~$0.0500 · $0.0300 reported · ~$0.0200 estimated for earlier steps"),
            "{details}"
        );
        book.end_turn();
        assert_eq!(book.session.total_cost(), Some((0.05, true)));
        let report = report(&book).plain_lines().join("\n");
        assert!(report.contains("~$0.0500"), "{report}");
    }

    #[test]
    fn a_turn_that_ends_without_a_report_keeps_its_steps() {
        let mut book = UsageBook::default();
        book.start_turn(None);
        book.turn.tally.step(&counts(2_000, 80, 0));
        book.turn.tally.estimated_usd = Some(0.01);
        let ended = book.end_turn();
        assert_eq!(ended.tally.tokens, Some(counts(2_000, 80, 0)));
        assert_eq!(ended.tally.estimated_usd, None);
        assert_eq!(book.session.tokens, Some(counts(2_000, 80, 0)));
    }

    #[test]
    fn summarises_a_turn_like_a_coding_cli() {
        let mut stats = finished(24);
        stats.record(&usage(1_000, 812, 2_100, Some(0.0123)));
        stats.tools.local = 2;
        stats.tools.server = 1;
        assert_eq!(
            turn_summary(&stats),
            "✻ Worked for 24s · ↑ 3.1k ↓ 812 tokens · $0.0123 · 3 tools · Claude Opus 5 · high",
            "the model that answered closes the line"
        );
        let mut unreported = finished(65);
        unreported.tally.streamed(400);
        unreported.model = None;
        assert_eq!(
            turn_summary(&unreported),
            "✻ Worked for 1m 5s · ↓ ~100 tokens (estimated)"
        );
    }

    #[test]
    fn details_list_a_child_under_the_subagent_that_started_it() {
        use crate::subagents::{Board, SubagentEvent};
        let now = std::time::Instant::now();
        let mut board = Board::default();
        let mut stats = finished(30);
        for (id, name, parent) in [
            ("sa_1", "Audit orders", None),
            ("sa_2", "Check nulls", Some("sa_1")),
        ] {
            let started =
                serde_json::json!({"subagentId": id, "name": name, "parentSubagentId": parent});
            board.apply(
                &SubagentEvent::parse("subagent_started", &started).unwrap(),
                now,
            );
        }
        // The child reports back before its parent can finish.
        for id in ["sa_2", "sa_1"] {
            let finished = serde_json::json!({"subagentId": id, "status": "completed"});
            let done = board.apply(
                &SubagentEvent::parse("subagent_finished", &finished).unwrap(),
                now,
            );
            stats.subagents.extend(done);
        }
        let lines = turn_details(&stats).plain_lines();
        let row = |name: &str| lines.iter().position(|l| l.contains(name)).unwrap();
        let (parent, child) = (row("Audit orders"), row("Check nulls"));
        assert_eq!(child, parent + 1, "{lines:#?}");
        let indent = |line: &str| line.len() - line.trim_start().len();
        assert_eq!(indent(&lines[child]), indent(&lines[parent]) + 2);
    }

    #[test]
    fn details_cover_model_timing_tokens_cost_and_tools() {
        let mut stats = finished(12);
        stats.record(&Usage {
            time_to_first_token_ms: Some(850),
            output_tokens_per_second: Some(61.7),
            reasoning_tokens: Some(40),
            ..usage(1_200, 300, 5_000, Some(0.02))
        });
        stats.tools = ToolCounts {
            local: 3,
            server: 1,
            helper: 12,
            context_bytes: 12_600,
        };
        let text = turn_details(&stats).plain_lines().join("\n");
        for part in [
            "Turn analytics · Claude Opus 5 · high",
            "wall time     12s",
            "first token   850ms",
            "speed         62 tokens/s",
            "cache read    5,000",
            "reasoning        40",
            "total         6,500",
            "cost          $0.0200",
            "3 local · 1 server · 12 script helper calls",
            "context sent  12.3 KB",
        ] {
            assert!(text.contains(part), "missing {part:?} in\n{text}");
        }
    }

    #[test]
    fn session_totals_add_up_across_turns() {
        let mut book = UsageBook::default();
        book.start_turn(None);
        book.turn.record(&usage(100, 20, 0, Some(0.01)));
        book.turn.tools.local = 2;
        book.end_turn();
        book.start_turn(None);
        book.turn.record(&usage(50, 5, 1_000, Some(0.02)));
        book.end_turn();
        let tokens = book.session.tokens.unwrap();
        assert_eq!(
            (tokens.input, tokens.output, tokens.cache_read),
            (150, 25, 1_000)
        );
        let report = report(&book).plain_lines().join("\n");
        assert!(report.contains("2 turns"), "{report}");
        assert!(report.contains("1,175"), "{report}");
        assert!(report.contains("$0.0300"), "{report}");
        assert!(report.contains("2 local"), "{report}");
        assert!(book.last.is_some());
    }

    #[test]
    fn a_fresh_session_says_so_instead_of_zeros() {
        let report = report(&UsageBook::default()).plain_lines().join("\n");
        assert!(report.contains("No turns yet"), "{report}");
        assert!(!report.contains("~0"), "{report}");
    }

    fn context(tokens: Option<u64>) -> ContextUsage {
        ContextUsage {
            tokens,
            window: 200_000,
            auto_compact_at: 170_000,
            history: Some(crate::cmd::stream::HistoryReplay {
                replayed: 48,
                summarized: 12,
                limit: 48,
            }),
        }
    }

    #[test]
    fn usage_shows_context_fill_and_headroom() {
        let mut book = UsageBook::default();
        book.start_turn(None);
        book.end_turn();
        book.context = Some(context(Some(82_100)));
        let report = report(&book).plain_lines().join("\n");
        for part in [
            "  Context",
            "41% · 82.1k of 200k model budget · auto-compacts at 170k (87.9k to go)",
            "48 earlier messages resent in full (up to 48) · 12 older ones summarized",
        ] {
            assert!(report.contains(part), "missing {part:?} in\n{report}");
        }
        book.context = Some(context(None));
        let unreported = super::report(&book).plain_lines().join("\n");
        assert!(
            unreported
                .contains("not reported by this model · 200k model budget · auto-compacts at 170k"),
            "{unreported}"
        );
    }

    #[test]
    fn an_interrupted_run_keeps_the_last_known_fill() {
        let mut book = UsageBook::default();
        book.set_context(context(Some(82_100)));
        book.set_context(context(None));
        assert_eq!(book.context.as_ref().unwrap().tokens, Some(82_100));
        assert!(context_note(book.context.as_ref().unwrap()).is_some());
        book.set_context(context(Some(90_000)));
        assert_eq!(book.context.as_ref().unwrap().tokens, Some(90_000));
    }

    #[test]
    fn waiting_on_the_user_is_not_work() {
        let mut book = UsageBook::default();
        book.start_turn(None);
        book.turn.started -= Duration::from_secs(50);
        book.turn.set_waiting(true);
        book.turn.waiting_since = Some(Instant::now() - Duration::from_secs(30));
        book.turn.set_waiting(true);
        book.turn.set_waiting(false);
        let ended = book.end_turn().clone();
        assert!(ended.waited >= Duration::from_secs(30));
        assert!(
            ended.worked() <= Duration::from_secs(21),
            "{:?}",
            ended.worked()
        );
        assert!(
            turn_summary(&ended).starts_with("✻ Worked for 20s"),
            "{}",
            turn_summary(&ended)
        );
        let details = turn_details(&ended).plain_lines().join("\n");
        assert!(details.contains("waiting for you"), "{details}");
        assert!(details.contains("30s · worked 20s"), "{details}");
        book.opened -= Duration::from_secs(600);
        let report = report(&book).plain_lines().join("\n");
        assert!(report.contains("This session · 1 turn"), "{report}");
        assert!(
            report.contains("20s working · 30s waiting for you · open 10m"),
            "{report}"
        );
    }

    #[test]
    fn footer_counts_down_to_auto_compact() {
        assert_eq!(
            context_note(&context(Some(82_100))),
            Some(("87.9k until auto-compact".to_string(), false))
        );
        assert_eq!(
            context_note(&context(Some(166_000))),
            Some(("4k until auto-compact".to_string(), true))
        );
        assert_eq!(
            context_note(&context(Some(190_000))),
            Some(("0 until auto-compact".to_string(), true))
        );
        assert_eq!(context_note(&context(None)), None);
    }

    #[test]
    fn formats_counts() {
        assert_eq!(compact(999), "999");
        assert_eq!(compact(1_250), "1.2k");
        assert_eq!(compact(2_500_000), "2.5M");
        assert_eq!(group(1_234_567), "1,234,567");
        assert_eq!(short(200_000), "200k");
        assert_eq!(short(82_100), "82.1k");
        assert_eq!(short(1_000_000), "1M");
        assert_eq!(short(999), "999");
        assert_eq!(bytes(950), "950 B");
        assert_eq!(duration(Duration::from_millis(850)), "850ms");
    }
}
