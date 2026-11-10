//! Finished transcript items and how each renders to terminal lines. Items
//! are printed once into normal scrollback; only the live region redraws.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::code_ui::labels::{self, DiffRow, Label};
use crate::code_ui::panel::{card_inner, framed};
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
    Error(String),
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
            } => banner(workspace, cwd, who.as_deref(), model.as_deref(), width),
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
            Item::Error(text) => {
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

fn banner(
    workspace: &str,
    cwd: &str,
    who: Option<&str>,
    model: Option<&str>,
    width: usize,
) -> Vec<Line<'static>> {
    let inner = card_inner(width);
    let rows: Vec<Vec<Span<'static>>> = vec![
        vec![
            Span::styled("✻ ", theme::brand()),
            Span::styled(
                "Welcome to gz code",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  ·  {workspace}"), theme::muted()),
        ],
        vec![],
        labeled("cwd", Some(cwd), None),
        labeled("site", who, None),
        labeled("model", model, Some("  (/models to change)")),
        vec![],
        vec![Span::styled(
            "/help for commands · ? for shortcuts · esc to interrupt",
            theme::muted(),
        )],
    ];
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
    for (i, piece) in crate::tui::chat::wrap(text, body).into_iter().enumerate() {
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
            lines.extend(output_lines(output, width));
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

/// [`diff_block`] showing at most `cap` rows (`None`: all of them).
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
    let indent = "      ";
    let room = width.saturating_sub(indent.len() + number_width + 3);
    let mut lines: Vec<Line<'static>> = rows
        .iter()
        .take(cap)
        .map(|row| match row {
            DiffRow::Gap => Line::from(Span::styled(format!("{indent}…"), theme::muted())),
            DiffRow::Context { line, text } => Line::from(vec![
                Span::raw(indent),
                Span::styled(format!("{line:>number_width$}   "), theme::muted()),
                Span::raw(fit(text, room)),
            ]),
            DiffRow::Added { line, text } => bar(
                indent,
                format!("{line:>number_width$} + {}", fit(text, room)),
                width,
                theme::diff_added(),
            ),
            DiffRow::Removed { line, text } => bar(
                indent,
                format!("{line:>number_width$} - {}", fit(text, room)),
                width,
                theme::diff_removed(),
            ),
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

fn bar(indent: &str, body: String, width: usize, style: Style) -> Line<'static> {
    let pad = width.saturating_sub(indent.len() + body.width());
    Line::from(vec![
        Span::raw(indent.to_string()),
        Span::styled(format!("{body}{}", " ".repeat(pad)), style),
    ])
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
    fn assistant_text_is_markdown_with_a_bullet() {
        let out = text(
            &Item::Assistant("Hi **there**\n\n| a | b |\n|---|---|\n| 1 | 2 |".into()).render(60),
        );
        assert_eq!(out[0], "● Hi there");
        assert!(out.iter().any(|l| l.starts_with("  ┌")), "{out:#?}");
    }

    #[test]
    fn banner_fits_narrow_terminals() {
        let item = Item::Banner {
            workspace: "demo".into(),
            cwd: "/a/very/long/path/that/keeps/going/and/going/and/going".into(),
            who: Some("preview.stage.groundzerodev.cloud · sagatest".into()),
            model: Some("Claude Opus 5 · high".into()),
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
    fn continuation_chunks_have_no_bullet() {
        let out = text(&Item::AssistantMore("more text".into()).render(40));
        assert_eq!(out[0], "  more text");
    }
}
