//! Finished transcript items and how each renders to terminal lines. Items
//! are printed once into normal scrollback; only the live region redraws.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::code_ui::diff_view;
use crate::code_ui::labels::{self, DiffRow, Label};
use crate::code_ui::panel::{card_inner, framed};
use crate::tui::highlight::{self, Surface};
use crate::tui::{chat, markdown, theme};

/// Diff rows shown under an edit before collapsing to `… +N lines`.
const MAX_DIFF_ROWS: usize = 24;
const INDENT: &str = "  ";
/// How a notice that reports a result starts.
const RESULT_HOOK: &str = "⎿  ";
const TREE: &str = "  ⎿  ";
/// Where a tool's full output sits in the expanded transcript.
const OUTPUT_INDENT: &str = "     ";
pub const EXPAND_HINT: &str = " (ctrl+o to expand)";
/// The label verb of a `read_file` call, whose output is a numbered file.
const READ_VERB: &str = "Read";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Ok,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolItem {
    pub label: Label,
    pub status: ToolStatus,
    /// The `⎿` result line once finished.
    pub result: Option<String>,
    pub diff: Option<String>,
    /// Calls a script made, shown nested under it.
    pub children: Vec<String>,
    /// The full result the model received, for the expanded transcript.
    pub output: Option<String>,
}

impl ToolItem {
    /// True when the compact view hides detail the transcript can show.
    pub fn hides_detail(&self) -> bool {
        let long_output = self
            .output
            .as_deref()
            .is_some_and(|o| o.trim().lines().count() > 1);
        let long_diff = self
            .diff
            .as_deref()
            .is_some_and(|d| labels::diff_rows(d).len() > MAX_DIFF_ROWS);
        long_output || long_diff
    }
}

// Not `Eq`: subagent usage carries a float cost.
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    Banner {
        workspace: String,
        cwd: String,
        who: Option<String>,
        model: Option<String>,
        /// The permission mode the session starts in.
        mode: String,
    },
    User(String),
    /// A user turn with what its `@` mentions and images brought along,
    /// shown as `⎿` lines right under the message.
    UserWithContext {
        text: String,
        notes: Vec<String>,
    },
    Assistant(String),
    /// A later chunk of an answer whose start is already printed: same
    /// rendering, without the bullet.
    AssistantMore(String),
    Tool(ToolItem),
    /// Several read-only calls folded into one dim line (`Read 3 files`).
    Group(String),
    Notice(String),
    /// An error answering the user's own action (a slash command, a
    /// setting that couldn't be saved): screen-only, left out of `/export`.
    Error(String),
    /// The error a turn ended with (the run failed, the stream broke):
    /// shown like [`Item::Error`], and part of the conversation.
    TurnFailed(String),
    /// A slash command's output card (`/usage`, `/help`, `/last`, …).
    Panel(crate::code_ui::panel::Panel),
    /// The dim line after a turn: `✻ Worked for 24s · ↑ 3.1k ↓ 812 tokens`.
    Summary(String),
    /// A finished subagent: `● Task(name · profile)`, status and summary.
    Subagent(Box<crate::subagents::Subagent>),
    /// A permission decision, kept for the transcript only.
    Decision {
        title: String,
        choice: String,
        preview: Option<String>,
    },
    /// A shell command the user ran themselves (`!command`) and how it went.
    UserCommand {
        command: String,
        output: crate::code_ui::shell_item::ShellOutput,
    },
    /// What a background command printed so far (`/shells <id>`).
    ShellOutput(crate::code_ui::shell_item::ShellOutput),
    /// The model's reasoning summary: folded to one dim line on screen,
    /// in full in the transcript; never exported.
    Thinking {
        text: String,
        secs: u64,
        /// Still arriving (the live region), or finished.
        live: bool,
    },
}

impl Item {
    /// Lines for this item at `width` columns, followed by a blank spacer.
    pub fn render(&self, width: usize) -> Vec<Line<'static>> {
        self.render_as(width, false)
    }

    /// Everything, uncut: full tool outputs and diffs (the Ctrl+O view).
    pub fn render_expanded(&self, width: usize) -> Vec<Line<'static>> {
        self.render_as(width, true)
    }

    fn render_as(&self, width: usize, expanded: bool) -> Vec<Line<'static>> {
        let width = width.max(20);
        let mut lines = match self {
            Item::Banner {
                workspace,
                cwd,
                who,
                model,
                mode,
            } => banner(
                workspace,
                cwd,
                who.as_deref(),
                model.as_deref(),
                mode,
                width,
            ),
            Item::User(text) => user_rows(text, width),
            Item::UserWithContext { text, notes } => {
                let mut rows = user_rows(text, width);
                rows.extend(notes.iter().map(|note| {
                    Line::from(vec![
                        Span::styled(TREE, theme::muted()),
                        Span::styled(fit(note, width.saturating_sub(TREE.len())), theme::muted()),
                    ])
                }));
                rows
            }
            Item::Assistant(text) => bulleted(
                Some(Style::default()),
                markdown::render(text, width.saturating_sub(INDENT.len())),
            ),
            Item::AssistantMore(text) => bulleted(
                None,
                markdown::render(text, width.saturating_sub(INDENT.len())),
            ),
            Item::Tool(tool) => tool_lines(tool, width, expanded),
            Item::Subagent(agent) => {
                crate::subagents::render::finished_lines(agent, width, expanded)
            }
            Item::Group(text) => vec![Line::from(vec![
                Span::raw(INDENT),
                Span::styled(text.clone(), theme::muted()),
                Span::styled(EXPAND_HINT, theme::muted()),
            ])],
            Item::Summary(text) => vec![Line::from(Span::styled(
                format!("{INDENT}{text}"),
                theme::muted(),
            ))],
            Item::Decision {
                title,
                choice,
                preview,
            } => decision_lines(title, choice, preview.as_deref(), width),
            Item::Notice(text) => wrapped(Span::raw(INDENT), text, width, theme::muted()),
            Item::Error(text) | Item::TurnFailed(text) => {
                let style = Style::default().fg(theme::ERROR);
                wrapped(Span::styled("● ", style), text, width, style)
            }
            Item::Panel(panel) => panel.render(width),
            Item::UserCommand { command, output } => {
                let mut rows = crate::code_ui::shell_item::command_rows(command, width);
                rows.extend(crate::code_ui::shell_item::output_lines(output, width));
                rows
            }
            Item::ShellOutput(output) => crate::code_ui::shell_item::output_lines(output, width),
            Item::Thinking { text, secs, live } if expanded => {
                crate::code_ui::thinking::expanded(text, *secs, *live, width)
            }
            Item::Thinking { text, secs, live } => {
                crate::code_ui::thinking::folded(text, *secs, *live)
            }
        };
        lines.push(Line::from(""));
        lines
    }
}

/// `● ` on the first line (when `bullet` is given), two-space hanging
/// indent after. Blank lines stay empty so a chunked answer prints exactly
/// like the whole one.
fn bulleted(bullet: Option<Style>, body: Vec<Line<'static>>) -> Vec<Line<'static>> {
    body.into_iter()
        .enumerate()
        .map(|(i, line)| {
            if line.width() == 0 {
                return Line::from("");
            }
            let lead = match (i, bullet) {
                (0, Some(style)) => Span::styled("● ", style),
                _ => Span::raw(INDENT),
            };
            let mut spans = vec![lead];
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect()
}

/// `label: value` with a dim label and a normal-weight value, so the value
/// stays readable on dark themes; empty when there is no value.
fn labeled(label: &str, value: Option<&str>, hint: Option<&str>) -> Vec<Span<'static>> {
    let Some(value) = value else {
        return Vec::new();
    };
    let mut spans = vec![
        Span::styled(format!("{label}: "), theme::muted()),
        Span::raw(value.to_string()),
    ];
    if let Some(hint) = hint {
        spans.push(Span::styled(hint.to_string(), theme::muted()));
    }
    spans
}

/// What the welcome box says when no model was chosen yet: the chat
/// server picks one.
const DEFAULT_MODEL: &str = "server default";

fn banner(
    workspace: &str,
    cwd: &str,
    who: Option<&str>,
    model: Option<&str>,
    mode: &str,
    width: usize,
) -> Vec<Line<'static>> {
    let model = model.unwrap_or(DEFAULT_MODEL);
    let inner = card_inner(width);
    let mut rows: Vec<Vec<Span<'static>>> = vec![
        vec![
            Span::styled("✻ ", theme::brand()),
            Span::styled(
                "Welcome to gz code",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  ·  {workspace}"), theme::muted()),
        ],
        vec![],
        labeled("model", Some(model), Some("  (/models to change)")),
        labeled("folder", Some(cwd), None),
        labeled("mode", Some(mode), Some("  (shift+tab to change)")),
    ];
    if who.is_some() {
        rows.push(labeled("signed in", who, None));
    }
    rows.push(vec![]);
    rows.push(vec![Span::styled(
        "/help for commands · ? for shortcuts · esc to interrupt",
        theme::muted(),
    )]);
    let border = theme::border(true);
    let mut lines = vec![Line::from(Span::styled(
        format!("╭{}╮", "─".repeat(inner + 2)),
        border,
    ))];
    lines.extend(rows.into_iter().map(|spans| framed(spans, inner, border)));
    lines.push(Line::from(Span::styled(
        format!("╰{}╯", "─".repeat(inner + 2)),
        border,
    )));
    lines
}

/// The prompt echo: `› text` on a full-width subtle bar.
fn user_rows(text: &str, width: usize) -> Vec<Line<'static>> {
    let body = width.saturating_sub(2);
    let mut lines = Vec::new();
    // Indented, so code the user pasted keeps its shape.
    for (i, piece) in crate::tui::chat::wrap_indented(text, body)
        .into_iter()
        .enumerate()
    {
        let lead = if i == 0 { "› " } else { "  " };
        let row = format!("{lead}{piece}");
        let pad = width.saturating_sub(row.width());
        lines.push(Line::from(Span::styled(
            format!("{row}{}", " ".repeat(pad)),
            theme::user_row(),
        )));
    }
    lines
}

fn decision_lines(
    title: &str,
    choice: &str,
    preview: Option<&str>,
    width: usize,
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(vec![
        Span::styled("◆ ", theme::muted()),
        Span::styled(
            title.to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" · {choice}"), theme::muted()),
    ])];
    lines.extend(output_lines(preview.unwrap_or_default(), width));
    lines
}

/// A tool's full output, dim and indented under its call.
fn output_lines(output: &str, width: usize) -> Vec<Line<'static>> {
    let room = width.saturating_sub(OUTPUT_INDENT.len());
    output
        .trim_end()
        .lines()
        .map(|line| {
            Line::from(Span::styled(
                format!("{OUTPUT_INDENT}{}", fit(line, room)),
                theme::muted(),
            ))
        })
        .collect()
}

/// A tool's full output for the transcript, dim and indented under its
/// call, long lines wrapped (not cut) so every character can be read.
fn full_output_lines(output: &str, width: usize) -> Vec<Line<'static>> {
    let room = width.saturating_sub(OUTPUT_INDENT.len()).max(1);
    output
        .trim_end()
        .lines()
        .flat_map(|line| hard_wrap(line, room))
        .map(|piece| {
            Line::from(Span::styled(
                format!("{OUTPUT_INDENT}{piece}"),
                theme::muted(),
            ))
        })
        .collect()
}

/// `line` broken into pieces at most `width` columns wide, keeping every
/// character (spaces and indentation included); tabs become four spaces.
fn hard_wrap(line: &str, width: usize) -> Vec<String> {
    let flat = line.replace('\t', "    ");
    let mut pieces = Vec::new();
    let mut piece = String::new();
    let mut used = 0;
    for ch in flat.chars() {
        let cols = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + cols > width && !piece.is_empty() {
            pieces.push(std::mem::take(&mut piece));
            used = 0;
        }
        piece.push(ch);
        used += cols;
    }
    pieces.push(piece);
    pieces
}

/// A file read's numbered rows (`     12\tcode`), the line numbers dim and
/// the code highlighted. `None` when the output isn't in that shape (an
/// error, an empty file), so it is shown the usual way.
fn file_lines(
    output: &str,
    syntax: &'static syntect::parsing::SyntaxReference,
    width: usize,
) -> Option<Vec<Line<'static>>> {
    let rows: Vec<(&str, Option<&str>)> = output
        .trim_end()
        .lines()
        .map(|row| match row.split_once('\t') {
            Some((number, code)) if number.trim().parse::<usize>().is_ok() => (number, Some(code)),
            _ => (row, None),
        })
        .collect();
    if !rows.iter().any(|(_, code)| code.is_some()) {
        return None;
    }
    let code: String = rows
        .iter()
        .filter_map(|(_, code)| code.map(|c| format!("{c}\n")))
        .collect();
    let mut tokens = highlight::highlight(&code, syntax).into_iter();
    let room = width.saturating_sub(OUTPUT_INDENT.len());
    let lines = rows
        .into_iter()
        .map(|(number, code)| {
            let Some(code) = code else {
                // Notes such as `… 40 more lines; read again with offset 61`.
                return Line::from(Span::styled(
                    format!("{OUTPUT_INDENT}{}", fit(number, room)),
                    theme::muted(),
                ));
            };
            let lead = format!("{number}  ");
            let text = diff_view::styled_text(
                code,
                tokens.next().as_deref(),
                &[],
                Style::default(),
                Style::default(),
                Surface::Plain,
            );
            let mut spans = vec![
                Span::raw(OUTPUT_INDENT),
                Span::styled(lead.clone(), theme::muted()),
            ];
            spans.extend(spans_of(fit_runs(
                text,
                room.saturating_sub(lead.width()),
                theme::muted(),
            )));
            Line::from(spans)
        })
        .collect();
    Some(lines)
}

fn tool_lines(tool: &ToolItem, width: usize, expanded: bool) -> Vec<Line<'static>> {
    let bullet = match tool.status {
        ToolStatus::Running => theme::muted(),
        ToolStatus::Ok => Style::default().fg(theme::OK),
        ToolStatus::Failed => Style::default().fg(theme::ERROR),
    };
    let mut head = vec![
        Span::styled("● ", bullet),
        Span::styled(
            tool.label.verb.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ];
    if !tool.label.target.is_empty() {
        let room = width.saturating_sub(tool.label.verb.width() + 4);
        head.push(Span::raw(format!("({})", fit(&tool.label.target, room))));
    }
    let mut lines = vec![Line::from(head)];
    for child in &tool.children {
        lines.push(tree_line(child, width, theme::muted()));
    }
    if let Some(result) = &tool.result {
        let style = if tool.status == ToolStatus::Failed {
            Style::default().fg(theme::ERROR)
        } else {
            theme::muted()
        };
        let mut line = tree_line(result, width, style);
        if !expanded && tool.hides_detail() {
            line.spans.push(Span::styled(EXPAND_HINT, theme::muted()));
        }
        lines.push(line);
    }
    if expanded {
        if let Some(output) = tool.output.as_deref().filter(|o| !o.trim().is_empty()) {
            let file_read = (tool.label.verb == READ_VERB)
                .then(|| highlight::syntax_for_path(&tool.label.target))
                .flatten()
                .and_then(|syntax| file_lines(output, syntax, width));
            lines.extend(file_read.unwrap_or_else(|| full_output_lines(output, width)));
        }
    }
    if let Some(diff) = &tool.diff {
        let cap = (!expanded).then_some(MAX_DIFF_ROWS);
        lines.extend(diff_rows_capped(diff, width, cap));
    }
    lines
}

fn tree_line(text: &str, width: usize, style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(TREE, theme::muted()),
        Span::styled(fit(text, width.saturating_sub(TREE.width())), style),
    ])
}

/// Numbered diff rows; additions and removals on full-width colored bars.
pub fn diff_block(diff: &str, width: usize) -> Vec<Line<'static>> {
    diff_rows_capped(diff, width, Some(MAX_DIFF_ROWS))
}

/// [`diff_block`] showing at most `cap` rows (`None`: all of them). Rows
/// are highlighted by the file's language, and inside a changed line pair
/// the words that changed stand out from the rest of the line.
fn diff_rows_capped(diff: &str, width: usize, cap: Option<usize>) -> Vec<Line<'static>> {
    let rows = labels::diff_rows(diff);
    let cap = cap.unwrap_or(usize::MAX);
    let number_width = rows
        .iter()
        .filter_map(|r| match r {
            DiffRow::Context { line, .. }
            | DiffRow::Added { line, .. }
            | DiffRow::Removed { line, .. } => Some(line.to_string().len()),
            DiffRow::Gap => None,
        })
        .max()
        .unwrap_or(1);
    let shown = &rows[..rows.len().min(cap)];
    let tokens = match diff_view::diff_path(diff).and_then(highlight::syntax_for_path) {
        Some(syntax) => diff_view::row_tokens(shown, syntax),
        None => vec![None; shown.len()],
    };
    let changed = diff_view::changed_words(shown);
    let indent = "      ";
    let room = width.saturating_sub(indent.len() + number_width + 3);
    let mut lines: Vec<Line<'static>> = shown
        .iter()
        .zip(tokens)
        .zip(changed)
        .map(|((row, tokens), changed)| {
            let body = |text: &str, base: Style, emphasis: Style, surface: Surface| {
                let runs = diff_view::styled_text(
                    text,
                    tokens.as_deref(),
                    &changed,
                    base,
                    emphasis,
                    surface,
                );
                fit_runs(runs, room, base)
            };
            match row {
                DiffRow::Gap => Line::from(Span::styled(format!("{indent}…"), theme::muted())),
                DiffRow::Context { line, text } => {
                    let mut spans = vec![
                        Span::raw(indent),
                        Span::styled(format!("{line:>number_width$}   "), theme::muted()),
                    ];
                    let plain = Style::default();
                    spans.extend(spans_of(body(text, plain, plain, Surface::Plain)));
                    Line::from(spans)
                }
                DiffRow::Added { line, text } => bar(
                    indent,
                    format!("{line:>number_width$} + "),
                    body(
                        text,
                        theme::diff_added(),
                        theme::diff_added_word(),
                        Surface::Bar,
                    ),
                    width,
                    theme::diff_added(),
                ),
                DiffRow::Removed { line, text } => bar(
                    indent,
                    format!("{line:>number_width$} - "),
                    body(
                        text,
                        theme::diff_removed(),
                        theme::diff_removed_word(),
                        Surface::Bar,
                    ),
                    width,
                    theme::diff_removed(),
                ),
            }
        })
        .collect();
    if rows.len() > cap {
        lines.push(Line::from(Span::styled(
            format!("{indent}… +{} lines (ctrl+o to expand)", rows.len() - cap),
            theme::muted(),
        )));
    }
    lines
}

fn spans_of(runs: Vec<(String, Style)>) -> Vec<Span<'static>> {
    runs.into_iter().map(|(t, s)| Span::styled(t, s)).collect()
}

/// Styled runs cut to `width` columns, ending in `…` (in `ellipsis` style)
/// when something was cut: the run version of [`fit`].
fn fit_runs(runs: Vec<(String, Style)>, width: usize, ellipsis: Style) -> Vec<(String, Style)> {
    let total: usize = runs.iter().map(|(t, _)| t.width()).sum();
    if total <= width {
        return runs;
    }
    let mut out: Vec<(String, Style)> = Vec::new();
    let mut used = 0;
    'runs: for (text, style) in runs {
        let mut kept = String::new();
        for ch in text.chars() {
            let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if used + w + 1 > width {
                if !kept.is_empty() {
                    out.push((kept, style));
                }
                break 'runs;
            }
            kept.push(ch);
            used += w;
        }
        out.push((kept, style));
    }
    out.push(("…".to_string(), ellipsis));
    out
}

/// `lead` then `text`, wrapped to `width` with the later lines under the
/// first one's text. A line cut at the terminal's edge loses its end, and
/// a notice ends with what the user needs: why, and what to do next.
fn wrapped(lead: Span<'static>, text: &str, width: usize, style: Style) -> Vec<Line<'static>> {
    // A result line's hook (`⎿  `) belongs to its first line only.
    let hook = if text.starts_with(RESULT_HOOK) {
        RESULT_HOOK
    } else {
        ""
    };
    let hang = lead.content.width() + hook.width();
    chat::wrap(&text[hook.len()..], width.saturating_sub(hang))
        .into_iter()
        .enumerate()
        .map(|(i, piece)| {
            if i == 0 {
                Line::from(vec![
                    lead.clone(),
                    Span::styled(format!("{hook}{piece}"), style),
                ])
            } else {
                Line::from(vec![
                    Span::raw(" ".repeat(hang)),
                    Span::styled(piece, style),
                ])
            }
        })
        .collect()
}

/// A full-width coloured row: `lead` (line number and sign) and the
/// styled text on the bar, padded to the edge in the bar's style.
fn bar(
    indent: &str,
    lead: String,
    text: Vec<(String, Style)>,
    width: usize,
    style: Style,
) -> Line<'static> {
    let used: usize = lead.width() + text.iter().map(|(t, _)| t.width()).sum::<usize>();
    let pad = width.saturating_sub(indent.len() + used);
    let mut spans = vec![Span::raw(indent.to_string()), Span::styled(lead, style)];
    spans.extend(spans_of(text));
    spans.push(Span::styled(" ".repeat(pad), style));
    Line::from(spans)
}

/// Cuts `text` to `width` columns with an ellipsis; tabs become spaces.
pub fn fit(text: &str, width: usize) -> String {
    let flat = text.replace('\t', "    ");
    if flat.width() <= width {
        return flat;
    }
    let mut out = String::new();
    for ch in flat.chars() {
        if out.width() + 2 > width {
            break;
        }
        out.push(ch);
    }
    format!("{out}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_notice_wraps_so_its_reason_and_next_step_stay_readable() {
        let notice = Item::Notice(
            "auto mode blocked: run_command find / -type f -print 2>/dev/null | sort > \
             /private/tmp/all-files-inventory.txt · Writes outside the folder you opened. · \
             /allow lets it run"
                .into(),
        );
        // Every item ends with a blank line; the text is what comes before.
        let shown = |item: &Item, width: usize| -> Vec<String> {
            let mut lines = text(&item.render(width));
            assert_eq!(lines.pop().as_deref(), Some(""));
            lines
        };
        let lines = shown(&notice, 60);
        assert!(lines.len() > 1, "{lines:#?}");
        assert!(lines.iter().all(|line| line.width() <= 60), "{lines:#?}");
        let joined: Vec<&str> = lines.iter().map(|line| line.trim()).collect();
        assert!(
            joined
                .join(" ")
                .ends_with("Writes outside the folder you opened. · /allow lets it run"),
            "nothing is cut: {lines:#?}"
        );
        assert!(
            lines[1..].iter().all(|line| line.starts_with("  ")),
            "later lines sit under the first: {lines:#?}"
        );

        let result = shown(&Item::Notice(format!("⎿  {}", "word ".repeat(30))), 40);
        assert!(result[0].starts_with("  ⎿  word"));
        assert!(
            result[1].starts_with("     word"),
            "under the text, past the hook: {result:#?}"
        );

        let error = shown(&Item::Error("cannot do it ".repeat(12)), 40);
        assert!(error.len() > 1 && error.iter().all(|line| line.width() <= 40));
        assert!(error[0].starts_with("● ") && error[1].starts_with("  cannot"));

        assert_eq!(shown(&Item::Notice("short".into()), 40), ["  short"]);
    }

    #[test]
    fn banner_values_are_not_dimmed() {
        let row = labeled("site", Some("preview.stage"), None);
        assert_eq!(row[0].style, theme::muted());
        assert_eq!(row[1].style, Style::default());
        assert!(labeled("model", None, None).is_empty());
    }
    use crate::code_ui::labels::label;

    fn text(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn tool_call_shows_title_and_tree_result() {
        let item = Item::Tool(ToolItem {
            label: label("read_file", "read_file src/math.rs"),
            status: ToolStatus::Ok,
            result: Some("Read 7 lines".into()),
            diff: None,
            children: vec![],
            output: None,
        });
        let lines = item.render(80);
        let out = text(&lines);
        assert_eq!(out[0], "● Read(src/math.rs)");
        assert_eq!(out[1], "  ⎿  Read 7 lines");
        assert_eq!(out[2], "", "items end with a spacer");
        assert_eq!(lines[0].spans[0].style.fg, Some(theme::OK));
        assert!(lines[0].spans[1]
            .style
            .add_modifier
            .contains(Modifier::BOLD));
    }

    #[test]
    fn expanded_output_wraps_long_lines_instead_of_cutting_them() {
        let message = "Access denied to collection finance-very-long-collection-name-here";
        let item = Item::Tool(ToolItem {
            label: label("Search documents", "Search documents finance"),
            status: ToolStatus::Failed,
            result: Some(format!("Error: {message}")),
            diff: None,
            children: vec![],
            output: Some(format!("{message}\n\tindented")),
        });
        let out = text(&item.render_expanded(40));
        let body: Vec<&String> = out
            .iter()
            .filter(|l| l.starts_with(OUTPUT_INDENT))
            .collect();
        assert!(body.len() > 2, "{out:?}");
        let joined: String = body[..body.len() - 1]
            .iter()
            .map(|l| &l[OUTPUT_INDENT.len()..])
            .collect();
        assert_eq!(joined, message, "every character is kept");
        assert_eq!(body.last().unwrap().trim_start(), "indented");
        assert!(out.iter().all(|l| l.width() <= 40), "{out:?}");
        assert!(body.iter().all(|l| !l.contains('…')), "nothing is cut");
    }

    #[test]
    fn script_children_nest_under_the_call() {
        let item = Item::Tool(ToolItem {
            label: label("run_script", "run_script python (3 lines)"),
            status: ToolStatus::Running,
            result: None,
            diff: None,
            children: vec!["read_file src/a.rs".into()],
            output: None,
        });
        let out = text(&item.render(80));
        assert_eq!(out[0], "● Script(python · 3 lines)");
        assert_eq!(out[1], "  ⎿  read_file src/a.rs");
    }

    #[test]
    fn diff_rows_are_numbered_and_colored_full_width() {
        let diff = "--- a/x\n+++ b/x\n@@ -18,3 +18,3 @@\n keep\n-old\n+new\n";
        let lines = diff_block(diff, 40);
        let out = text(&lines);
        assert_eq!(out[0], "      18   keep");
        assert!(out[1].starts_with("      19 - old"));
        assert_eq!(out[1].width(), 40, "removed row spans the full width");
        assert_eq!(lines[1].spans[1].style, theme::diff_removed());
        assert_eq!(lines[2].spans[1].style, theme::diff_added());
    }

    #[test]
    fn diff_rows_are_highlighted_by_the_files_language() {
        let diff = "--- a/src/x.rs\n+++ b/src/x.rs\n@@ -1,2 +1,2 @@\n fn keep() {}\n-let n = 1;\n+let n = 2;\n";
        let lines = diff_block(diff, 60);
        let span = |row: usize, text: &str| {
            lines[row]
                .spans
                .iter()
                .find(|s| s.content.contains(text))
                .unwrap_or_else(|| panic!("{text} in row {row}: {:?}", lines[row]))
                .clone()
        };
        assert_eq!(span(0, "fn").style.fg, Some(theme::ACCENT_ALT));
        let keyword = span(2, "let").style;
        assert_eq!(keyword.fg, Some(ratatui::style::Color::LightMagenta));
        assert_eq!(keyword.bg, theme::diff_added().bg, "colours sit on the bar");
        assert_eq!(
            text(&lines)[2].width(),
            60,
            "added row still spans the width"
        );
    }

    #[test]
    fn changed_words_stand_out_inside_an_edited_line() {
        let diff = "--- a/notes.txt\n+++ b/notes.txt\n@@ -1 +1 @@\n-the quick brown fox\n+the quick red fox\n";
        let lines = diff_block(diff, 60);
        let style_of = |row: usize, word: &str| {
            lines[row]
                .spans
                .iter()
                .find(|s| s.content.contains(word))
                .map(|s| s.style)
                .unwrap()
        };
        assert_eq!(style_of(0, "brown"), theme::diff_removed_word());
        assert_eq!(style_of(0, "quick"), theme::diff_removed());
        assert_eq!(style_of(1, "red"), theme::diff_added_word());
        assert_eq!(style_of(1, "fox"), theme::diff_added());
    }

    #[test]
    fn long_diff_rows_are_cut_with_an_ellipsis_at_every_width() {
        let diff = format!(
            "--- a/x.py\n+++ b/x.py\n@@ -1 +1 @@\n-x = \"{}\"\n+x = \"{}\"\n",
            "a".repeat(200),
            "b".repeat(200)
        );
        for width in [40, 80, 120] {
            let out = text(&diff_block(&diff, width));
            for line in &out {
                assert!(line.width() <= width, "{width}: {line}");
            }
            assert!(out[0].contains('…'), "{out:#?}");
        }
    }

    #[test]
    fn long_diffs_collapse() {
        let body: String = (0..40).map(|i| format!("+line {i}\n")).collect();
        let diff = format!("--- /dev/null\n+++ b/x\n@@ -0,0 +1,40 @@\n{body}");
        let out = text(&diff_block(&diff, 60));
        assert_eq!(out.last().unwrap(), "      … +16 lines (ctrl+o to expand)");
    }

    #[test]
    fn user_rows_span_the_width_on_a_bar() {
        let lines = Item::User("hello there".into()).render(30);
        assert_eq!(text(&lines)[0].width(), 30);
        assert!(text(&lines)[0].starts_with("› hello there"));
        assert_eq!(lines[0].spans[0].style, theme::user_row());
    }

    #[test]
    fn user_rows_keep_indentation() {
        let lines = Item::User("def f():\n    return 1".into()).render(40);
        assert!(
            text(&lines)[1].starts_with("      return 1"),
            "{:?}",
            text(&lines)
        );
    }

    #[test]
    fn assistant_text_is_markdown_with_a_bullet() {
        let out = text(
            &Item::Assistant("Hi **there**\n\n| a | b |\n|---|---|\n| 1 | 2 |".into()).render(60),
        );
        assert_eq!(out[0], "● Hi there");
        assert!(out.iter().any(|l| l.starts_with("  ┌")), "{out:#?}");
    }

    #[test]
    fn the_welcome_box_names_model_folder_mode_and_who() {
        let item = Item::Banner {
            workspace: "demo".into(),
            cwd: "/w/demo".into(),
            who: Some("acme · sagatest".into()),
            model: None,
            mode: "plan (read-only)".into(),
        };
        let out = text(&item.render(80)).join("\n");
        assert!(out.contains("model: server default"), "{out}");
        assert!(out.contains("folder: /w/demo"), "{out}");
        assert!(out.contains("mode: plan (read-only)"), "{out}");
        assert!(out.contains("signed in: acme · sagatest"), "{out}");
        assert!(!out.contains("http"), "{out}");
        let unsigned = Item::Banner {
            workspace: "demo".into(),
            cwd: "/w".into(),
            who: None,
            model: None,
            mode: "auto".into(),
        };
        let rows = text(&unsigned.render(60));
        let blank = format!("│{}│", " ".repeat(58));
        let blanks = rows.iter().filter(|row| **row == blank).count();
        assert_eq!(blanks, 2, "no empty row for a missing value: {rows:#?}");
    }

    #[test]
    fn banner_fits_narrow_terminals() {
        let item = Item::Banner {
            workspace: "demo".into(),
            cwd: "/a/very/long/path/that/keeps/going/and/going/and/going".into(),
            who: Some("preview · sagatest".into()),
            model: Some("Claude Opus 5 · high".into()),
            mode: "ask before edits and commands".into(),
        };
        for width in [40, 80, 120] {
            for line in text(&item.render(width)) {
                assert!(line.width() <= width, "{line}");
            }
        }
    }

    #[test]
    fn collapsed_results_hint_and_the_transcript_expands_them() {
        let item = Item::Tool(ToolItem {
            label: label("read_file", "read_file src/a.rs"),
            status: ToolStatus::Ok,
            result: Some("Read 3 lines".into()),
            diff: None,
            children: vec![],
            output: Some("one\ntwo\nthree".into()),
        });
        let compact = text(&item.render(80));
        assert_eq!(compact[1], "  ⎿  Read 3 lines (ctrl+o to expand)");
        let expanded = text(&item.render_expanded(80));
        assert_eq!(&expanded[2..5], &["     one", "     two", "     three"]);
    }

    #[test]
    fn expanded_file_reads_are_highlighted_by_extension() {
        let item = Item::Tool(ToolItem {
            label: label("read_file", "read_file src/a.rs"),
            status: ToolStatus::Ok,
            result: Some("Read 2 lines".into()),
            diff: None,
            children: vec![],
            output: Some(
                "     1\tfn main() {}\n     2\t// done\n… 3 more lines; read again with offset 3\n"
                    .into(),
            ),
        });
        let lines = item.render_expanded(60);
        let out = text(&lines);
        assert_eq!(out[2], "          1  fn main() {}");
        assert_eq!(out[3], "          2  // done");
        assert_eq!(out[4], "     … 3 more lines; read again with offset 3");
        let style_of = |row: usize, needle: &str| {
            lines[row]
                .spans
                .iter()
                .find(|s| s.content.contains(needle))
                .map(|s| s.style)
                .unwrap()
        };
        assert_eq!(style_of(2, "fn").fg, Some(theme::ACCENT_ALT));
        assert_eq!(style_of(3, "done").fg, Some(theme::MUTED));
        for width in [40, 80] {
            assert!(text(&item.render_expanded(width))
                .iter()
                .all(|l| l.width() <= width));
        }
    }

    #[test]
    fn reads_of_unknown_file_types_stay_dim() {
        let item = Item::Tool(ToolItem {
            label: label("read_file", "read_file notes.unknownext"),
            status: ToolStatus::Ok,
            result: Some("Read 1 line".into()),
            diff: None,
            children: vec![],
            output: Some("     1\thello\n".into()),
        });
        let lines = item.render_expanded(60);
        assert_eq!(text(&lines)[2], "          1\thello".replace('\t', "    "));
        assert_eq!(lines[2].spans[0].style, theme::muted());
    }

    #[test]
    fn continuation_chunks_have_no_bullet() {
        let out = text(&Item::AssistantMore("more text".into()).render(40));
        assert_eq!(out[0], "  more text");
    }
}
