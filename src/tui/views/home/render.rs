//! Drawing the home dashboard: four bordered tiles, two columns when there
//! is room, one otherwise.

use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::Modifier,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Padding, Paragraph},
    Frame,
};
use serde_json::json;

use super::tiles::{self, JobRun};
use super::{HomeView, List, Panel, Pick, Tile, LISTS};
use crate::spend::budget::BudgetStatus;
use crate::spend::ledger::Summary;
use crate::tui::format::{self, Format, TimeUnit};
use crate::tui::theme::{self, BadgeTone};
use crate::tui::ui::states;
use crate::tui::ui::widgets::{self, fit};
use crate::tui::views::RenderCtx;

/// Items listed per group before the first layout decides how many fit.
pub const LIST_LIMIT: usize = 5;
/// Items every list shows before spare rows are handed out: none, so a
/// list starts as its one-line summary and small terminals still fit
/// every tile whole.
const MIN_LISTED: usize = 0;
/// Items each list shows, by [`List::index`].
pub type Listed = [usize; LISTS.len()];
/// Below this width the tiles stack in one column.
const TWO_COLUMN_MIN_WIDTH: u16 = 100;
/// The top and bottom border of a tile.
const BORDER_ROWS: u16 = 2;
/// Space between a tile's side border and its text.
const PADDING: u16 = 1;
/// Cells of the budget meter.
const BUDGET_CELLS: usize = 10;
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

/// Lays the tiles out and draws them. Returns how many items each list
/// shows, so selection reaches exactly what is on screen.
pub fn render(home: &HomeView, frame: &mut Frame, area: Rect, ui: RenderCtx) -> Listed {
    let mut listed = Listed::default();
    let columns: Vec<(Rect, &[Panel])> = if area.width >= TWO_COLUMN_MIN_WIDTH {
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
                .areas(area);
        vec![
            (left, &[Panel::Attention, Panel::Spend]),
            (right, &[Panel::Running, Panel::Chats]),
        ]
    } else {
        vec![(
            area,
            &[Panel::Attention, Panel::Running, Panel::Spend, Panel::Chats],
        )]
    };
    for (column, panels) in columns {
        let (limits, tiles) = plan_column(home, panels, column, ui);
        let mut y = column.y;
        for (panel, entries) in tiles {
            let room = column.bottom().saturating_sub(y);
            // A frame needs a line inside it to say anything.
            if room <= BORDER_ROWS {
                break;
            }
            let height = tile_height(&entries).min(room);
            let area = Rect::new(column.x, y, column.width, height);
            draw_panel(home, panel, entries, frame, area, ui);
            for list in panel.lists() {
                listed[list.index()] = limits[list.index()];
            }
            y += height;
        }
    }
    listed
}

fn tile_height(entries: &[Entry]) -> u16 {
    entries.len() as u16 + BORDER_ROWS
}

/// Sizes a column's tiles to their content. Every list starts as its
/// summary; each spare row then goes to the list with the most items still
/// hidden, so a long list grows while a short one keeps its few lines.
fn plan_column(
    home: &HomeView,
    panels: &[Panel],
    column: Rect,
    ui: RenderCtx,
) -> (Listed, Vec<(Panel, Vec<Entry>)>) {
    let width = column.width.saturating_sub(BORDER_ROWS + 2 * PADDING) as usize;
    let build = |panel: Panel, limits: &Listed| {
        wrapped(panel_entries(home, panel, ui, width, limits), width)
    };
    let mut limits: Listed = [MIN_LISTED; LISTS.len()];
    let mut tiles: Vec<Vec<Entry>> = panels.iter().map(|p| build(*p, &limits)).collect();
    loop {
        let used: u16 = tiles.iter().map(|t| tile_height(t)).sum();
        let grown = panels
            .iter()
            .enumerate()
            .flat_map(|(tile, panel)| panel.lists().iter().map(move |list| (tile, *list)))
            .filter(|(_, list)| home.hidden(*list, limits[list.index()]) > 0)
            .filter_map(|(tile, list)| {
                let mut trial = limits;
                trial[list.index()] += 1;
                let bigger = build(panels[tile], &trial);
                let fits = used - tile_height(&tiles[tile]) + tile_height(&bigger) <= column.height;
                fits.then_some((tile, list, trial, bigger))
            })
            // Ties go to the list higher up.
            .max_by_key(|(_, list, _, _)| {
                let hidden = home.hidden(*list, limits[list.index()]);
                (hidden, std::cmp::Reverse(list.index()))
            });
        match grown {
            Some((tile, _, trial, bigger)) => {
                limits = trial;
                tiles[tile] = bigger;
            }
            None => return (limits, panels.iter().copied().zip(tiles).collect()),
        }
    }
}

fn panel_entries(
    home: &HomeView,
    panel: Panel,
    ui: RenderCtx,
    width: usize,
    limits: &Listed,
) -> Vec<Entry> {
    let limit = |list: List| limits[list.index()];
    match panel {
        Panel::Attention => attention(home, ui, width, limit(List::Failed), limit(List::Approvals)),
        Panel::Running => running(home, ui, width, limit(List::Running)),
        Panel::Spend => spend(home, ui),
        Panel::Chats => chats(home, ui, width, limit(List::Chats)),
    }
}

/// Splits lines wider than the tile into several, so a tile's height is
/// exactly its line count. Items are already cut to fit; this is for
/// notes and error messages.
fn wrapped(entries: Vec<Entry>, width: usize) -> Vec<Entry> {
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        if entry.line.width() <= width || width == 0 {
            out.push(entry);
            continue;
        }
        let style = entry.line.spans.last().map(|s| s.style).unwrap_or_default();
        let text: String = entry
            .line
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        out.extend(
            format::wrap(&text, width)
                .into_iter()
                .map(|part| Entry::text(Line::from(Span::styled(part, style)))),
        );
    }
    out
}

fn draw_panel(
    home: &HomeView,
    panel: Panel,
    entries: Vec<Entry>,
    frame: &mut Frame,
    area: Rect,
    ui: RenderCtx,
) {
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
        .padding(Padding::horizontal(PADDING));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let selected = focused.then(|| home.selected_pick()).flatten();
    let lines: Vec<Line> = entries
        .into_iter()
        .map(|entry| match entry.pick {
            Some(pick) if Some(pick) == selected => entry.line.style(theme::selected_row()),
            _ => entry.line,
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
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

fn attention(
    home: &HomeView,
    ui: RenderCtx,
    width: usize,
    failed_limit: usize,
    approvals_limit: usize,
) -> Vec<Entry> {
    if home.nothing_needs_attention() {
        return vec![heading(
            "✓",
            // Short enough for one line in the narrowest two-column tile.
            "No failed runs in 24h · nothing to review".to_string(),
            BadgeTone::Good,
        )];
    }
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
            for (i, run) in runs.failed.iter().take(failed_limit).enumerate() {
                let when = run.ended_ms.or(run.started_ms);
                entries.push(Entry::pick(
                    run_line(run, relative(when, ui.now_ms), width),
                    Pick::FailedRun(i),
                ));
            }
            // With nothing listed the heading's count says it all.
            if count > failed_limit && failed_limit > 0 {
                entries.push(note(format!("   … and {} more", count - failed_limit)));
            }
        }
        (None, None) => {}
    }
    // Two one-line headings read fine together; a list needs air below it.
    if entries.len() > 1 {
        entries.push(Entry::text(Line::from("")));
    }
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
            for (i, approval) in approvals.iter().take(approvals_limit).enumerate() {
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
            if approvals.len() > approvals_limit && approvals_limit > 0 {
                entries.push(note(format!(
                    "   … and {} more in Agents › Approval requests",
                    approvals.len() - approvals_limit
                )));
            }
        }
        (None, None) => {}
    }
    entries
}

fn running(home: &HomeView, ui: RenderCtx, width: usize, limit: usize) -> Vec<Entry> {
    if let Some(entry) = placeholder(&home.runs, "running jobs", ui) {
        return vec![entry];
    }
    let Some(runs) = &home.runs.data else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    match runs.running.len() {
        0 => entries.push(note("Nothing running")),
        1 if limit == 0 => entries.push(note("1 job running")),
        count if limit == 0 => entries.push(note(format!("{count} jobs running"))),
        _ => {}
    }
    for (i, run) in runs.running.iter().take(limit).enumerate() {
        let elapsed = run
            .started_ms
            .map(|started| elapsed(ui.now_ms - started))
            .unwrap_or_else(|| "queued".to_string());
        let mut line = run_line(run, elapsed, width);
        line.spans[0] = Span::styled("● ", theme::badge(BadgeTone::Info));
        entries.push(Entry::pick(line, Pick::Running(i)));
    }
    if runs.running.len() > limit && limit > 0 {
        entries.push(note(format!("  … and {} more", runs.running.len() - limit)));
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
    let mut entries = vec![
        Entry::pick(
            row("You (chat)", tiles::usd(spend.mine), String::new()),
            Pick::MySpend,
        ),
        Entry::pick(
            row("Whole site", tiles::usd(spend.site), agents),
            Pick::SiteSpend,
        ),
        Entry::pick(budget_line(spend.budget.as_ref()), Pick::Budget),
    ];
    if let Some(summary) = &home.local {
        entries.push(Entry::text(local_line(summary)));
    }
    entries.push(note("Budget counts only spend tied to your budget policy."));
    entries
}

fn budget_line(budget: Option<&BudgetStatus>) -> Line<'static> {
    let mut spans = vec![Span::styled(format!("{:<13}", "Budget"), theme::muted())];
    match budget {
        Some(budget) => spans.extend(widgets::budget_meter(budget, BUDGET_CELLS)),
        None => spans.push(Span::styled("unavailable", theme::muted())),
    }
    Line::from(spans)
}

/// gz code's own spend on this machine, from its local ledger.
fn local_line(summary: &Summary) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{:<13}", "gz code"), theme::muted()),
        Span::raw(widgets::local_spend_text(summary, 0)),
    ])
}

fn chats(home: &HomeView, ui: RenderCtx, width: usize, limit: usize) -> Vec<Entry> {
    if let Some(entry) = placeholder(&home.chats, "conversations", ui) {
        return vec![entry];
    }
    let Some(chats) = &home.chats.data else {
        return Vec::new();
    };
    if chats.is_empty() {
        return vec![note("No conversations yet · press n to start one")];
    }
    if limit == 0 {
        let noun = if chats.len() == 1 { "chat" } else { "chats" };
        return vec![note(format!("{} recent {noun} · n new chat", chats.len()))];
    }
    let mut entries: Vec<Entry> = chats
        .iter()
        .take(limit)
        .enumerate()
        .map(|(i, chat)| {
            let title = fit(&chat.title, width.saturating_sub(2));
            Entry::pick(
                Line::from(vec![Span::styled("› ", theme::muted()), Span::raw(title)]),
                Pick::Chat(i),
            )
        })
        .collect();
    let footer = match chats.len().saturating_sub(limit) {
        0 => "Newest first · n new chat".to_string(),
        more => format!("… and {more} more · newest first · n new chat"),
    };
    entries.push(note(footer));
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
