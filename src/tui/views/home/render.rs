//! Drawing the home dashboard: four bordered tiles, two columns when there
//! is room, one otherwise.

use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Padding, Paragraph, Wrap},
    Frame,
};
use serde_json::json;

use super::tiles::{self, Budget, JobRun};
use super::{HomeView, Panel, Pick, Tile};
use crate::tui::format::{self, Format, TimeUnit};
use crate::tui::theme::{self, BadgeTone};
use crate::tui::ui::states;
use crate::tui::ui::widgets::fit;
use crate::tui::views::RenderCtx;

/// Items listed per group; the full screen is one Enter away.
pub const LIST_LIMIT: usize = 5;
/// Below this width the tiles stack in one column.
const TWO_COLUMN_MIN_WIDTH: u16 = 100;
const SPEND_HEIGHT: u16 = 6;
const NAME_WIDTH: usize = 26;

/// One line of a tile, and what Enter does on it (None: not selectable).
struct Entry {
    line: Line<'static>,
    pick: Option<Pick>,
}

impl Entry {
    fn text(line: Line<'static>) -> Self {
        Self { line, pick: None }
    }

    fn pick(line: Line<'static>, pick: Pick) -> Self {
        Self {
            line,
            pick: Some(pick),
        }
    }
}

pub fn render(home: &HomeView, frame: &mut Frame, area: Rect, ui: RenderCtx) {
    if area.width >= TWO_COLUMN_MIN_WIDTH {
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
                .areas(area);
        let [attention, spend] =
            Layout::vertical([Constraint::Min(6), Constraint::Length(SPEND_HEIGHT)]).areas(left);
        let [running, chats] =
            Layout::vertical([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(right);
        draw_panel(home, Panel::Attention, frame, attention, ui);
        draw_panel(home, Panel::Spend, frame, spend, ui);
        draw_panel(home, Panel::Running, frame, running, ui);
        draw_panel(home, Panel::Chats, frame, chats, ui);
        return;
    }
    let [attention, running, spend, chats] = Layout::vertical([
        Constraint::Min(5),
        Constraint::Min(4),
        Constraint::Length(SPEND_HEIGHT),
        Constraint::Min(4),
    ])
    .areas(area);
    draw_panel(home, Panel::Attention, frame, attention, ui);
    draw_panel(home, Panel::Running, frame, running, ui);
    draw_panel(home, Panel::Spend, frame, spend, ui);
    draw_panel(home, Panel::Chats, frame, chats, ui);
}

fn draw_panel(home: &HomeView, panel: Panel, frame: &mut Frame, area: Rect, ui: RenderCtx) {
    let focused = home.panel == panel;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::border(focused))
        .title(Span::styled(
            format!(" {} ", panel_title(panel, ui.now_ms)),
            if focused {
                theme::brand()
            } else {
                theme::title()
            },
        ))
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let width = inner.width as usize;
    let entries = match panel {
        Panel::Attention => attention(home, ui, width),
        Panel::Running => running(home, ui, width),
        Panel::Spend => spend(home, ui),
        Panel::Chats => chats(home, ui, width),
    };
    let selected = focused.then(|| home.selected_pick()).flatten();
    let lines: Vec<Line> = entries
        .into_iter()
        .map(|entry| match entry.pick {
            Some(pick) if Some(pick) == selected => entry.line.style(theme::selected_row()),
            _ => entry.line,
        })
        .collect();
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn panel_title(panel: Panel, now_ms: i64) -> String {
    match panel {
        Panel::Attention => "Needs attention".to_string(),
        Panel::Running => "Running now".to_string(),
        Panel::Spend => format!("AI spend · {} (UTC)", month_name(now_ms)),
        Panel::Chats => "Recent chats".to_string(),
    }
}

fn month_name(epoch_ms: i64) -> &'static str {
    const MONTHS: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    let iso = format::iso8601_utc(epoch_ms);
    let month: usize = iso[5..7].parse().unwrap_or(1);
    MONTHS[month.clamp(1, 12) - 1]
}

/// The loading line or the error line for a tile without data, if any.
fn placeholder<T>(tile: &Tile<T>, what: &str, ui: RenderCtx) -> Option<Entry> {
    if tile.data.is_some() {
        return None;
    }
    if tile.is_loading() {
        return Some(Entry::text(states::loading(what, ui.tick)));
    }
    let error = tile.error.as_deref()?;
    let style = if tile.denied {
        theme::muted()
    } else {
        theme::badge(BadgeTone::Bad)
    };
    Some(Entry::text(Line::from(Span::styled(
        format!("✗ {error}"),
        style,
    ))))
}

fn heading(icon: &str, text: String, tone: BadgeTone) -> Entry {
    Entry::text(Line::from(vec![
        Span::styled(format!("{icon} "), theme::badge(tone)),
        Span::styled(text, theme::badge(tone).add_modifier(Modifier::BOLD)),
    ]))
}

fn note(text: impl Into<String>) -> Entry {
    Entry::text(Line::from(Span::styled(text.into(), theme::muted())))
}

fn attention(home: &HomeView, ui: RenderCtx, width: usize) -> Vec<Entry> {
    let mut entries = Vec::new();
    match (&home.runs.data, placeholder(&home.runs, "failed runs", ui)) {
        (_, Some(entry)) => entries.push(entry),
        (Some(runs), None) if runs.failed.is_empty() => entries.push(heading(
            "✓",
            "No failed runs in the last 24 hours".to_string(),
            BadgeTone::Good,
        )),
        (Some(runs), None) => {
            let plus = if runs.truncated { "+" } else { "" };
            let count = runs.failed.len();
            let noun = if count == 1 { "run" } else { "runs" };
            entries.push(heading(
                "✗",
                format!("{count}{plus} failed job {noun} · last 24h"),
                BadgeTone::Bad,
            ));
            for (i, run) in runs.failed.iter().take(LIST_LIMIT).enumerate() {
                let when = run.ended_ms.or(run.started_ms);
                entries.push(Entry::pick(
                    run_line(run, relative(when, ui.now_ms), width),
                    Pick::FailedRun(i),
                ));
            }
            if count > LIST_LIMIT {
                entries.push(note(format!("   … and {} more", count - LIST_LIMIT)));
            }
        }
        (None, None) => {}
    }
    entries.push(Entry::text(Line::from("")));
    match (
        &home.approvals.data,
        placeholder(&home.approvals, "approval requests", ui),
    ) {
        (_, Some(entry)) => entries.push(entry),
        (Some(approvals), None) if approvals.is_empty() => entries.push(heading(
            "✓",
            "Nothing waiting for your review".to_string(),
            BadgeTone::Good,
        )),
        (Some(approvals), None) => {
            let count = tiles::approval_count(approvals.len());
            let noun = if approvals.len() == 1 {
                "agent version is"
            } else {
                "agent versions are"
            };
            entries.push(heading(
                "⚑",
                format!("{count} {noun} waiting for your review"),
                BadgeTone::Warn,
            ));
            for (i, approval) in approvals.iter().take(LIST_LIMIT).enumerate() {
                let version = approval
                    .version
                    .map(|v| format!("v{v}"))
                    .unwrap_or_default();
                let who = if approval.requested_by.is_empty() {
                    String::new()
                } else {
                    format!("by {}", approval.requested_by)
                };
                let when = relative(approval.requested_ms, ui.now_ms);
                entries.push(Entry::pick(
                    item_line(&approval.agent, &[&version, &who], &when, width),
                    Pick::Approval(i),
                ));
            }
            if approvals.len() > LIST_LIMIT {
                entries.push(note(format!(
                    "   … and {} more in Agents › Approval requests",
                    approvals.len() - LIST_LIMIT
                )));
            }
        }
        (None, None) => {}
    }
    entries
}

fn running(home: &HomeView, ui: RenderCtx, width: usize) -> Vec<Entry> {
    if let Some(entry) = placeholder(&home.runs, "running jobs", ui) {
        return vec![entry];
    }
    let Some(runs) = &home.runs.data else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    if runs.running.is_empty() {
        entries.push(note("Nothing running"));
    }
    for (i, run) in runs.running.iter().take(LIST_LIMIT).enumerate() {
        let elapsed = run
            .started_ms
            .map(|started| elapsed(ui.now_ms - started))
            .unwrap_or_else(|| "queued".to_string());
        let mut line = run_line(run, elapsed, width);
        line.spans[0] = Span::styled("● ", theme::badge(BadgeTone::Info));
        entries.push(Entry::pick(line, Pick::Running(i)));
    }
    if runs.running.len() > LIST_LIMIT {
        entries.push(note(format!(
            "  … and {} more",
            runs.running.len() - LIST_LIMIT
        )));
    }
    if !runs.stuck.is_empty() {
        let count = runs.stuck.len();
        let noun = if count == 1 { "run" } else { "runs" };
        entries.push(Entry::pick(
            Line::from(Span::styled(
                format!("⚠ {count} {noun} \"running\" over 24 h · may be stuck"),
                theme::badge(BadgeTone::Warn),
            )),
            Pick::Stuck,
        ));
    }
    entries
}

fn spend(home: &HomeView, ui: RenderCtx) -> Vec<Entry> {
    if let Some(entry) = placeholder(&home.spend, "AI spend", ui) {
        return vec![entry];
    }
    let Some(spend) = &home.spend.data else {
        return Vec::new();
    };
    let row = |label: &str, value: String, detail: String| {
        Line::from(vec![
            Span::styled(format!("{label:<13}"), theme::muted()),
            Span::styled(value, theme::title()),
            Span::styled(detail, theme::muted()),
        ])
    };
    let agents = match spend.agents {
        Some(agents) => format!("  incl. agents {}", tiles::usd(Some(agents))),
        None => String::new(),
    };
    vec![
        Entry::pick(
            row("You (chat)", tiles::usd(spend.mine), String::new()),
            Pick::MySpend,
        ),
        Entry::pick(
            row("Whole site", tiles::usd(spend.site), agents),
            Pick::SiteSpend,
        ),
        Entry::pick(budget_line(&spend.budget), Pick::Budget),
        note("Budget counts only spend tied to your budget policy."),
    ]
}

fn budget_line(budget: &Budget) -> Line<'static> {
    let window = |days: &Option<u64>| match days {
        Some(days) => format!(" · rolling {days} days"),
        None => String::new(),
    };
    let label = Span::styled(format!("{:<13}", "Budget"), theme::muted());
    match budget {
        Budget::NoLimit { window_days } => Line::from(vec![
            label,
            Span::raw("No budget limit set"),
            Span::styled(window(window_days), theme::muted()),
        ]),
        Budget::Limit {
            used_usd,
            limit_usd,
            window_days,
            exhausted,
        } => {
            let style = if *exhausted {
                theme::badge(BadgeTone::Bad)
            } else {
                Style::default()
            };
            let state = if *exhausted { " · used up" } else { "" };
            Line::from(vec![
                label,
                Span::styled(
                    format!(
                        "{} of {} used{state}",
                        tiles::usd(Some(*used_usd)),
                        tiles::usd(Some(*limit_usd))
                    ),
                    style,
                ),
                Span::styled(window(window_days), theme::muted()),
            ])
        }
        Budget::Unknown => Line::from(vec![label, Span::styled("unavailable", theme::muted())]),
    }
}

fn chats(home: &HomeView, ui: RenderCtx, width: usize) -> Vec<Entry> {
    if let Some(entry) = placeholder(&home.chats, "conversations", ui) {
        return vec![entry];
    }
    let Some(chats) = &home.chats.data else {
        return Vec::new();
    };
    if chats.is_empty() {
        return vec![note("No conversations yet · press n to start one")];
    }
    let mut entries: Vec<Entry> = chats
        .iter()
        .enumerate()
        .map(|(i, chat)| {
            let title = fit(&chat.title, width.saturating_sub(2));
            Entry::pick(
                Line::from(vec![Span::styled("› ", theme::muted()), Span::raw(title)]),
                Pick::Chat(i),
            )
        })
        .collect();
    entries.push(note("Newest first · n new chat"));
    entries
}

/// "  ChallanReceiptBulk   Invoice-Extractor   2h ago".
fn run_line(run: &JobRun, right: String, width: usize) -> Line<'static> {
    item_line(&run.job, &[&run.project], &right, width)
}

/// A list item: the name, dim details, and a right-hand value, cut to fit.
fn item_line(name: &str, details: &[&str], right: &str, width: usize) -> Line<'static> {
    let details: Vec<&str> = details.iter().copied().filter(|d| !d.is_empty()).collect();
    let details = details.join(" · ");
    let name_width = NAME_WIDTH.min(width.saturating_sub(right.chars().count() + 4));
    let name = format!("{:<name_width$}", fit(name, name_width));
    let room = width.saturating_sub(name_width + right.chars().count() + 5);
    let details = format!("{:<room$}", fit(&details, room));
    Line::from(vec![
        Span::raw("  "),
        Span::raw(name),
        Span::raw(" "),
        Span::styled(details, theme::muted()),
        Span::raw(" "),
        Span::styled(right.to_string(), theme::muted()),
    ])
}

fn relative(epoch_ms: Option<i64>, now_ms: i64) -> String {
    epoch_ms.map_or_else(String::new, |t| format::relative_time(t, now_ms))
}

fn elapsed(ms: i64) -> String {
    format::show(
        Some(&json!(ms.max(0))),
        Format::Duration(TimeUnit::Millis),
        0,
    )
    .text
}
