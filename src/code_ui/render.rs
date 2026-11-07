//! Draws the live region pinned under the scrollback: streaming answer,
//! running tools, spinner, then the input box (or a permission prompt) and
//! the mode line.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::assistant::tools::Access;
use crate::code_ui::items::{self, Item};
use crate::code_ui::state::{Picker, PickerRow, PromptView, UiState, SHORTCUTS};
use crate::code_ui::{transcript, usage};
use crate::tui::theme;

const VERBS: [&str; 5] = ["Thinking", "Working", "Reading", "Pondering", "Composing"];
const TIPS: [&str; 4] = [
    "Tip: shift+tab switches to accept-edits or plan (read-only) mode",
    "Tip: ask for a script when you need many files scanned at once",
    "Tip: type your next message now; it sends when this turn ends",
    "Tip: esc interrupts; your conversation stays",
];
/// Max rows of a script shown in its approval prompt.
const MAX_SCRIPT_ROWS: usize = 20;
/// Max rows of a server action's input shown in its approval prompt.
const MAX_ACTION_ROWS: usize = 12;
/// Input lines shown before the box scrolls.
const MAX_INPUT_ROWS: usize = 8;

/// Everything above the input box, as lines.
fn activity_lines(state: &UiState, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(item) = state.streaming_item() {
        let mut body = item.render(width);
        body.pop();
        lines.extend(body);
        lines.push(Line::from(""));
    }
    for tool in state.group.iter().chain(state.running.iter()) {
        let mut body = Item::Tool(tool.clone()).render(width);
        body.pop();
        lines.extend(body);
    }
    if let Some(since) = state.busy_since {
        let secs = since.elapsed().as_secs();
        let verb = VERBS[(secs / 4) as usize % VERBS.len()];
        let spark = theme::SPINNER[(state.tick as usize) % theme::SPINNER.len()];
        let mut stats = format!("{secs}s");
        if let Some(tokens) = usage::live_tokens(&state.usage.turn.tally) {
            stats.push_str(&format!(" · {tokens}"));
        }
        stats.push_str(" · esc to interrupt");
        lines.push(Line::from(vec![
            Span::styled(format!("{spark} {verb}… "), theme::brand()),
            Span::styled(format!("({stats})"), theme::muted()),
        ]));
        let tip = TIPS[state.turns % TIPS.len()];
        lines.push(Line::from(Span::styled(
            format!("  ⎿  {tip}"),
            theme::muted(),
        )));
    }
    if !lines.is_empty() {
        lines.push(Line::from(""));
    }
    lines
}

/// The rows the live region needs at `width`, capped at `max`.
pub fn live_height(state: &UiState, width: u16, max: u16) -> u16 {
    let width = width as usize;
    let activity = activity_lines(state, width).len();
    let below = below_height(state, width);
    ((activity + below) as u16).clamp(3, max.max(3))
}

/// Rows under the activity: a prompt, the picker, or the input box.
fn below_height(state: &UiState, width: usize) -> usize {
    if let Some(prompt) = &state.prompt {
        return prompt_lines(prompt, width).len();
    }
    if let Some(picker) = &state.picker {
        return picker_lines(picker, state, width).len();
    }
    input_rows(state) + 2 + footer_lines(state).len()
}

/// The model/effort picker as a rounded box.
pub fn picker_lines(picker: &Picker, state: &UiState, width: usize) -> Vec<Line<'static>> {
    let inner = width.saturating_sub(4).max(10);
    let current = state
        .model
        .clone()
        .or_else(|| state.catalog.as_ref()?.default_selection());
    let mut body = vec![
        Line::from(Span::styled(
            picker.title.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    for (i, row) in picker.rows.iter().enumerate() {
        let chosen = i == picker.selected;
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
                body.push(Line::from(Span::styled(title.clone(), theme::muted())));
                continue;
            }
            PickerRow::Model(model) => (
                model.label(),
                current
                    .as_ref()
                    .is_some_and(|c| c.model.key() == model.key()),
            ),
            PickerRow::Effort(effort) => (
                effort.clone(),
                // Only the current model's own effort counts as current.
                current.as_ref().is_some_and(|c| {
                    Some(c.model.key()) == picker.model.as_ref().map(|m| m.key())
                        && c.effort.as_ref() == Some(effort)
                }),
            ),
        };
        let mut spans = vec![
            marker,
            Span::styled(items::fit(&text, inner.saturating_sub(6)), style),
        ];
        if is_current {
            spans.push(Span::styled(" ✔", Style::default().fg(theme::OK)));
        }
        body.push(Line::from(spans));
    }
    body.push(Line::from(""));
    body.push(Line::from(Span::styled(
        "↑↓ move · enter select · esc cancel",
        theme::muted(),
    )));
    boxed(body, inner)
}

fn input_rows(state: &UiState) -> usize {
    state.input.lines().len().clamp(1, MAX_INPUT_ROWS)
}

const COMMAND_MENU_HINT: &str = "↑↓ select · tab complete · enter run · esc dismiss";
const MENTION_MENU_HINT: &str = "↑↓ select · tab/enter insert · esc dismiss";

/// A popup under the input (slash commands, `@` files): one row per match,
/// the highlighted one marked like the pickers, then a key hint.
fn menu_lines(menu: &[(&str, &str)], selected: usize, hint: &str) -> Vec<Line<'static>> {
    let width = menu.iter().map(|(name, _)| name.len()).max().unwrap_or(0) + 3;
    let mut lines: Vec<Line<'static>> = menu
        .iter()
        .enumerate()
        .map(|(i, (name, what))| {
            let chosen = i == selected;
            let name_style = if chosen {
                Style::default()
                    .fg(theme::ACCENT)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Line::from(vec![
                Span::styled(
                    if chosen { "  ❯ " } else { "    " },
                    Style::default().fg(theme::ACCENT),
                ),
                Span::styled(format!("{name:<width$}"), name_style),
                Span::styled(what.to_string(), theme::muted()),
            ])
        })
        .collect();
    lines.push(Line::from(Span::styled(
        format!("    {hint}"),
        theme::muted(),
    )));
    lines
}

fn footer_lines(state: &UiState) -> Vec<Line<'static>> {
    let menu = state.command_menu();
    if !menu.is_empty() {
        let selected = state.menu_selected.min(menu.len() - 1);
        return menu_lines(&menu, selected, COMMAND_MENU_HINT);
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
        return menu_lines(&rows, selected, MENTION_MENU_HINT);
    }
    if state.show_shortcuts {
        return SHORTCUTS
            .iter()
            .map(|s| Line::from(Span::styled(format!("  {s}"), theme::muted())))
            .collect();
    }
    let mut spans = vec![Span::raw("  ")];
    if state.exit_armed() {
        spans.push(Span::styled(
            "Press Ctrl+C again to exit",
            Style::default().fg(theme::WARN),
        ));
    } else if let Some(banner) = state.mode().banner() {
        spans.push(Span::styled(banner, Style::default().fg(theme::ACCENT_ALT)));
        spans.push(Span::styled(" (shift+tab to cycle)", theme::muted()));
    } else {
        spans.push(Span::styled("? for shortcuts", theme::muted()));
    }
    if let Some(model) = state.model_label() {
        spans.push(Span::styled(format!("  ·  {model}"), theme::muted()));
    }
    if !state.queue.is_empty() {
        spans.push(Span::styled(
            format!("  ·  {} queued", state.queue.len()),
            theme::muted(),
        ));
    }
    vec![Line::from(spans)]
}

/// The permission prompt as a rounded box.
pub fn prompt_lines(prompt: &PromptView, width: usize) -> Vec<Line<'static>> {
    let inner = width.saturating_sub(4).max(10);
    let request = &prompt.request;
    let target = request
        .summary
        .split_once(' ')
        .map(|(_, t)| t.to_string())
        .unwrap_or_else(|| request.summary.clone());
    let (title, question) = match request.access {
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
        Access::ServerAction => (
            "",
            format!("{}?", request.summary.trim_end_matches(['?', '.'])),
        ),
    };
    let title = match (request.access, &request.action) {
        (Access::ServerAction, Some(action)) => humanize(action),
        (Access::ServerAction, None) => "Approve action".to_string(),
        _ => title.to_string(),
    };
    let mut body: Vec<Line<'static>> = vec![
        Line::from(Span::styled(
            title,
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    body.extend(preview_lines(
        request.access,
        request.preview.as_deref(),
        &request.summary,
        inner,
    ));
    body.push(Line::from(""));
    body.push(Line::from(question));
    for (i, option) in prompt.options().iter().enumerate() {
        let chosen = i == prompt.selected;
        let marker = if chosen { "❯ " } else { "  " };
        let style = if chosen {
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        body.push(Line::from(vec![
            Span::styled(marker, Style::default().fg(theme::ACCENT)),
            Span::styled(
                format!("{}. {}", i + 1, items::fit(option, inner.saturating_sub(5))),
                style,
            ),
        ]));
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
fn action_preview_lines(preview: &str, width: usize) -> Vec<Line<'static>> {
    let total = preview.lines().count();
    let mut lines: Vec<Line<'static>> = preview
        .lines()
        .take(MAX_ACTION_ROWS)
        .map(|l| Line::from(Span::styled(items::fit(l, width), theme::muted())))
        .collect();
    if total > MAX_ACTION_ROWS {
        lines.push(Line::from(Span::styled(
            format!("… +{} lines", total - MAX_ACTION_ROWS),
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
) -> Vec<Line<'static>> {
    if access == Access::ServerAction {
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
    match access {
        Access::Write => items::diff_block(preview, width)
            .into_iter()
            .map(|line| {
                // The box adds its own margin; drop the scrollback indent.
                let mut spans = line.spans;
                if let Some(first) = spans.first_mut() {
                    first.content = first.content.trim_start().to_string().into();
                }
                Line::from(spans)
            })
            .collect(),
        Access::Command => vec![Line::from(vec![
            Span::styled("$ ", theme::muted()),
            Span::styled(
                items::fit(preview, width.saturating_sub(2)),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ])],
        Access::Script => {
            let total = preview.lines().count();
            let mut lines: Vec<Line<'static>> = preview
                .lines()
                .take(MAX_SCRIPT_ROWS)
                .enumerate()
                .map(|(i, code)| {
                    Line::from(vec![
                        Span::styled(format!("{:>3} │ ", i + 1), theme::muted()),
                        Span::raw(items::fit(code, width.saturating_sub(6))),
                    ])
                })
                .collect();
            if total > MAX_SCRIPT_ROWS {
                lines.push(Line::from(Span::styled(
                    format!("    … +{} lines", total - MAX_SCRIPT_ROWS),
                    theme::muted(),
                )));
            }
            lines
        }
        _ => preview
            .lines()
            .take(MAX_SCRIPT_ROWS)
            .map(|l| Line::from(Span::styled(items::fit(l, width), theme::muted())))
            .collect(),
    }
}

/// Wraps lines in a rounded border `inner` columns wide.
fn boxed(body: Vec<Line<'static>>, inner: usize) -> Vec<Line<'static>> {
    let border = theme::border(true);
    let mut out = vec![Line::from(Span::styled(
        format!("╭{}╮", "─".repeat(inner + 2)),
        border,
    ))];
    for line in body {
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
    let mut activity = activity_lines(state, width);
    let below_height = below_height(state, width) as u16;
    // When space is short, keep the newest activity lines.
    let room = area.height.saturating_sub(below_height) as usize;
    if activity.len() > room {
        activity.drain(..activity.len() - room);
    }
    let top = Rect::new(area.x, area.y, area.width, activity.len() as u16);
    frame.render_widget(Paragraph::new(activity), top);
    let mut y = top.bottom();
    let boxed_lines = match (&state.prompt, &state.picker) {
        (Some(prompt), _) => Some(prompt_lines(prompt, width)),
        (None, Some(picker)) => Some(picker_lines(picker, state, width)),
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
            .map(|i| {
                Line::from(Span::styled(
                    if i == 0 { "› " } else { "  " },
                    theme::brand(),
                ))
            })
            .collect();
        frame.render_widget(Paragraph::new(prefix), Rect::new(area.x, y, 2, rows));
        let editor = Rect::new(area.x + 2, y, area.width.saturating_sub(2), rows);
        frame.render_widget(&state.input, editor);
        y += rows;
    }
    if y < area.bottom() {
        frame.render_widget(rule(), Rect::new(area.x, y, area.width, 1));
        y += 1;
    }
    let footer = footer_lines(state);
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
    fn edit_prompt_shows_diff_question_and_options() {
        let mut s = state();
        let (tx, _rx) = tokio::sync::oneshot::channel();
        s.open_prompt(Ask {
            request: PermissionRequest {
                access: Access::Write,
                summary: "edit_file src/math.rs".into(),
                preview: Some("--- a/src/math.rs\n+++ b/src/math.rs\n@@ -1,2 +1,2 @@\n fn a() {}\n-fn b() {}\n+fn c() {}\n".into()),
                session_scope: "all edits this session".into(),
            action: None,
            },
            reply: tx,
        });
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
            assert!(
                out.contains("3. No, and tell gz what to do differently (esc)"),
                "{out}"
            );
        }
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
                session_scope: "this action in this conversation".into(),
                action: Some("submit_job".into()),
            },
            reply: tx,
        });
        let out = screen(&mut s, 100);
        assert!(out.contains("Submit job"), "{out}");
        assert!(out.contains("\"jobId\": 1006"), "{out}");
        assert!(out.contains("Run the nightly ETL job now?"), "{out}");
        assert!(
            out.contains("2. Yes, and don't ask again for this action in this conversation"),
            "{out}"
        );
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
}
