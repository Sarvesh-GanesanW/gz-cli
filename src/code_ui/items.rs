//! Finished transcript items and how each renders to terminal lines. Items
//! are printed once into normal scrollback; only the live region redraws.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::code_ui::labels::{self, DiffRow, Label};
use crate::tui::{markdown, theme};

/// Diff rows shown under an edit before collapsing to `… +N lines`.
const MAX_DIFF_ROWS: usize = 24;
const INDENT: &str = "  ";
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    Banner {
        workspace: String,
        cwd: String,
        who: Option<String>,
        model: Option<String>,
    },
    User(String),
    Assistant(String),
    /// A later chunk of an answer whose start is already printed: same
    /// rendering, without the bullet.
    AssistantMore(String),
    Tool(ToolItem),
    /// Several read-only calls folded into one dim line (`Read 3 files`).
    Group(String),
    Notice(String),
    Error(String),
    /// Plain dim text block (help, shortcuts).
    Info(Vec<String>),
    /// The dim line after a turn: `✻ Worked for 24s · ↑ 3.1k ↓ 812 tokens`.
    Summary(String),
    /// A permission decision, kept for the transcript only.
    Decision {
        title: String,
        choice: String,
        preview: Option<String>,
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
            } => banner(workspace, cwd, who.as_deref(), model.as_deref(), width),
            Item::User(text) => user_rows(text, width),
            Item::Assistant(text) => bulleted(
                Some(Style::default()),
                markdown::render(text, width.saturating_sub(INDENT.len())),
            ),
            Item::AssistantMore(text) => bulleted(
                None,
                markdown::render(text, width.saturating_sub(INDENT.len())),
            ),
            Item::Tool(tool) => tool_lines(tool, width, expanded),
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
            Item::Notice(text) => vec![Line::from(Span::styled(
                format!("{INDENT}{text}"),
                theme::muted(),
            ))],
            Item::Error(text) => vec![Line::from(vec![
                Span::styled("● ", Style::default().fg(theme::ERROR)),
                Span::styled(text.clone(), Style::default().fg(theme::ERROR)),
            ])],
            Item::Info(rows) => rows
                .iter()
                .map(|r| Line::from(Span::styled(format!("{INDENT}{r}"), theme::muted())))
                .collect(),
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

fn banner(
    workspace: &str,
    cwd: &str,
    who: Option<&str>,
    model: Option<&str>,
    width: usize,
) -> Vec<Line<'static>> {
    let inner = width.saturating_sub(4).min(76);
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
        vec![Span::styled(format!("cwd: {cwd}"), theme::muted())],
        vec![Span::styled(
            who.map(|w| format!("site: {w}")).unwrap_or_default(),
            theme::muted(),
        )],
        vec![Span::styled(
            model
                .map(|m| format!("model: {m}  (/models to change)"))
                .unwrap_or_default(),
            theme::muted(),
        )],
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
    for spans in rows {
        let used: usize = spans.iter().map(|s| s.content.width()).sum();
        let mut row = vec![Span::styled("│ ", border)];
        let fits = used <= inner;
        if fits {
            row.extend(spans);
        } else {
            let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
            row.push(Span::styled(fit(&text, inner), theme::muted()));
        }
        row.push(Span::raw(" ".repeat(inner.saturating_sub(used.min(inner)))));
        row.push(Span::styled(" │", border));
        lines.push(Line::from(row));
    }
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
