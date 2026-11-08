//! Token, cost, and per-turn analytics for `gz code`: a live estimate while
//! an answer streams (the server reports real counts only once it's done),
//! the summary line after each turn with its full analytics behind Ctrl+O
//! and `/last`, and the session totals behind `/usage`.

use std::time::{Duration, Instant};

use crate::cmd::stream::{ContextUsage, TokenCounts, Usage};
use crate::code_ui::panel::{Panel, Row};
use crate::subagents::{Status, Subagent};

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
}

impl Tally {
    pub fn record(&mut self, usage: &Usage) {
        if let Some(counts) = &usage.tokens {
            self.tokens
                .get_or_insert_with(TokenCounts::default)
                .add(counts);
        }
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

    /// Output tokens: reported ones plus an estimate for unreported text.
    /// The flag is true when any part is estimated.
    pub fn output_tokens(&self) -> (u64, bool) {
        let reported = self.tokens.map(|t| t.output).unwrap_or(0);
        let estimate = self.unreported_chars.div_ceil(CHARS_PER_TOKEN) as u64;
        (reported + estimate, estimate > 0 || self.tokens.is_none())
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
        self.unreported_chars += other.unreported_chars;
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
    /// Wall time, set when the turn ends.
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
        }
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
    pub session_time: Duration,
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
            turns: 0,
        }
    }
}

impl UsageBook {
    pub fn start_turn(&mut self, model: Option<String>) {
        self.turn = TurnStats::start(model);
    }

    /// Closes the turn: stamps its wall time and folds it into the session.
    pub fn end_turn(&mut self) -> &TurnStats {
        self.turn.wall = Some(self.turn.started.elapsed());
        self.session.merge(&self.turn.tally);
        self.session_tools.add(&self.turn.tools);
        self.session_subagents += self.turn.subagents.len();
        self.session_time += self.turn.elapsed();
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
                    "{} of {} tokens · {headroom}",
                    short(tokens),
                    short(context.window)
                ),
            )
        }
        _ => Row::field(
            "used",
            format!(
                "not reported by this model · {} token window · auto-compacts at {threshold}",
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

/// `↓ ~1.2k tokens` for the spinner.
pub fn live_tokens(tally: &Tally) -> Option<String> {
    let (output, estimated) = tally.output_tokens();
    if output == 0 {
        return None;
    }
    let approx = if estimated { "~" } else { "" };
    Some(format!("↓ {approx}{} tokens", compact(output)))
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

/// The dim line after a turn: `✻ Worked for 24s · ↑ 3.1k ↓ 812 tokens`.
pub fn turn_summary(stats: &TurnStats) -> String {
    let tally = &stats.tally;
    let mut line = format!("✻ Worked for {}", duration(stats.elapsed()));
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
    if let Some(cost) = tally.cost_usd {
        line.push_str(&format!(" · ${cost:.4}"));
    }
    if stats.tools.calls() > 0 {
        line.push_str(&format!(" · {}", plural(stats.tools.calls(), "tool")));
    }
    if !stats.subagents.is_empty() {
        line.push_str(&format!(" · {}", plural(stats.subagents.len(), "subagent")));
    }
    line
}

/// One row per subagent: `Check logs (data_engineer)  12s · ↑ 1.2k ↓ 300 · $0.0021 · done`.
fn subagent_rows(agents: &[Subagent]) -> Vec<Row> {
    agents
        .iter()
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
            Row::entry(&format!("{}{profile}", agent.name), parts.join(" · "))
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
    if let Some(cost) = tally.cost_usd {
        rows.push(Row::field("cost", format!("${cost:.4}")));
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
        .aside(format!(
            "{} · {}",
            plural(book.turns, "turn"),
            duration(book.session_time)
        ))
        .section(Some("Context"), context)
        .section(Some("Tokens"), token_rows(&book.session))
        .section(None, work)
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
        assert_eq!(live_tokens(&tally).as_deref(), Some("↓ 812 tokens"));
        tally.streamed(40);
        assert_eq!(tally.output_tokens(), (822, true));
    }

    #[test]
    fn summarises_a_turn_like_a_coding_cli() {
        let mut stats = finished(24);
        stats.record(&usage(1_000, 812, 2_100, Some(0.0123)));
        stats.tools.local = 2;
        stats.tools.server = 1;
        assert_eq!(
            turn_summary(&stats),
            "✻ Worked for 24s · ↑ 3.1k ↓ 812 tokens · $0.0123 · 3 tools"
        );
        let mut unreported = finished(65);
        unreported.tally.streamed(400);
        assert_eq!(
            turn_summary(&unreported),
            "✻ Worked for 1m 5s · ↓ ~100 tokens (estimated)"
        );
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
            "41% · 82.1k of 200k tokens · auto-compacts at 170k (87.9k to go)",
            "48 earlier messages resent in full (up to 48) · 12 older ones summarized",
        ] {
            assert!(report.contains(part), "missing {part:?} in\n{report}");
        }
        book.context = Some(context(None));
        let unreported = super::report(&book).plain_lines().join("\n");
        assert!(
            unreported
                .contains("not reported by this model · 200k token window · auto-compacts at 170k"),
            "{unreported}"
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
