//! `/context`: what fills the model's context window. The server reports
//! one number, the conversation's size after the last model call (whole
//! prompt plus reply), with the window and the auto-compact threshold; it
//! does not split it. gz knows the text it sent itself (instructions,
//! memories, the skills index) and the conversation it showed, so those
//! parts are estimated from their characters, and the system prompt and
//! tool definitions are what remains of the reported total.
//!
//! Estimates use [`CHARS_PER_TOKEN`] characters per token (the usual
//! rough figure for English text and code) and are marked `~`.

use serde_json::Value;

use crate::cmd::stream::ContextUsage;
use crate::code_ui::items::Item;
use crate::code_ui::labels::MCP_VERB;
use crate::code_ui::panel::{Panel, Row};
use crate::code_ui::usage::short;

/// Characters per token for the estimated parts.
pub const CHARS_PER_TOKEN: usize = 4;

/// The parts gz can measure on its side, in characters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LocalParts {
    pub instructions: usize,
    pub instruction_entries: usize,
    pub memories: usize,
    pub memory_entries: usize,
    pub skills: usize,
    pub skill_entries: usize,
    /// The conversation as gz showed it: messages, answers, tool results
    /// (other than local MCP ones).
    pub messages: usize,
    /// Local MCP tool lists and call results in the conversation: the
    /// tools are listed on demand, so this is what they take up.
    pub mcp: usize,
    /// Local MCP servers configured.
    pub mcp_servers: usize,
}

impl LocalParts {
    /// Fills the conversation parts from the transcript: local MCP tool
    /// calls count as MCP tools, everything else as messages.
    pub fn with_transcript(mut self, items: &[Item]) -> Self {
        for item in items {
            let chars = item_chars(item);
            match item {
                Item::Tool(tool) if tool.label.verb == MCP_VERB => self.mcp += chars,
                _ => self.messages += chars,
            }
        }
        self
    }

    /// Fills the `clientContext` parts from the field as it would be sent.
    pub fn with_client_context(mut self, field: &Value) -> Self {
        let texts = |key: &str, parts: &[&str]| -> (usize, usize) {
            let entries = field[key].as_array().map(Vec::as_slice).unwrap_or_default();
            let chars = entries
                .iter()
                .map(|entry| {
                    parts
                        .iter()
                        .map(|part| entry[*part].as_str().map_or(0, |t| t.chars().count()))
                        .sum::<usize>()
                })
                .sum();
            (chars, entries.len())
        };
        (self.instructions, self.instruction_entries) = texts("instructions", &["path", "text"]);
        (self.memories, self.memory_entries) = texts("memories", &["id", "text"]);
        (self.skills, self.skill_entries) = texts("skills", &["name", "description"]);
        self
    }
}

/// Characters of the conversation one transcript item holds.
fn item_chars(item: &Item) -> usize {
    match item {
        Item::User(text) | Item::Assistant(text) | Item::AssistantMore(text) => {
            text.chars().count()
        }
        Item::UserWithContext { text, notes } => {
            text.chars().count() + notes.iter().map(|n| n.chars().count()).sum::<usize>()
        }
        Item::Tool(tool) => tool
            .output
            .as_deref()
            .or(tool.result.as_deref())
            .map_or(0, |o| o.chars().count()),
        _ => 0,
    }
}

fn tokens(chars: usize) -> u64 {
    chars.div_ceil(CHARS_PER_TOKEN) as u64
}

/// One row of the breakdown.
struct Part {
    label: &'static str,
    tokens: u64,
    estimated: bool,
    detail: String,
}

/// The `/context` panel.
pub fn panel(context: Option<&ContextUsage>, parts: &LocalParts) -> Panel {
    match context.and_then(|c| c.tokens.map(|used| (c, used))) {
        Some((context, used)) => measured(context, used, parts),
        None => unmeasured(context, parts),
    }
}

fn measured(context: &ContextUsage, used: u64, parts: &LocalParts) -> Panel {
    let window = context.window;
    let local = local_parts(parts);
    let local_total: u64 = local.iter().map(|p| p.tokens).sum();
    // The transcript can hold more than the model saw (summarized history),
    // so the conversation never takes more than what the total leaves.
    let left = used.saturating_sub(local_total);
    let mcp = tokens(parts.mcp).min(left);
    let messages = tokens(parts.messages).min(left - mcp);
    let system = left - mcp - messages;
    let buffer = window.saturating_sub(context.auto_compact_at);
    let free = window.saturating_sub(used + buffer);
    let mut rows = vec![Row::meter(
        "used",
        used as f64 / window as f64,
        format!(
            "{} of {} tokens, as the server reported after the last reply",
            short(used),
            short(window)
        ),
    )];
    let all = std::iter::once(Part {
        label: "system & tools",
        tokens: system,
        estimated: true,
        detail: "the chat's own prompt and tool definitions: the rest of the total".into(),
    })
    .chain(local)
    .chain([
        mcp_part(parts, mcp),
        Part {
            label: "messages",
            tokens: messages,
            estimated: true,
            detail: history_detail(context),
        },
        Part {
            label: "free",
            tokens: free,
            estimated: false,
            detail: "before the chat starts summarizing older messages".into(),
        },
        Part {
            label: "compact buffer",
            tokens: buffer,
            estimated: false,
            detail: format!(
                "past {} the chat summarizes",
                short(context.auto_compact_at)
            ),
        },
    ]);
    rows.extend(all.map(|part| part_row(&part, window)));
    Panel::new("Context")
        .aside(format!("{} window", short(window)))
        .section(None, rows)
        .footer(footer())
}

fn unmeasured(context: Option<&ContextUsage>, parts: &LocalParts) -> Panel {
    let mut rows: Vec<Row> = local_parts(parts)
        .into_iter()
        .chain([mcp_part(parts, tokens(parts.mcp))])
        .map(|part| part_row(&part, 0))
        .collect();
    rows.push(Row::field(
        "messages",
        format!("~{} tokens shown so far", short(tokens(parts.messages))),
    ));
    let note = match context {
        Some(c) => format!(
            "This model reports no token counts · {} token window · auto-compacts at {}",
            short(c.window),
            short(c.auto_compact_at)
        ),
        None => "Send a message to measure the rest: the server reports the total after each reply"
            .to_string(),
    };
    rows.push(Row::note(note));
    Panel::new("Context")
        .aside("not measured yet")
        .section(None, rows)
        .footer(footer())
}

fn local_parts(parts: &LocalParts) -> Vec<Part> {
    let count =
        |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    vec![
        Part {
            label: "instructions",
            tokens: tokens(parts.instructions),
            estimated: true,
            detail: format!(
                "{} incl. the environment snapshot · /instructions",
                count(parts.instruction_entries, "entry", "entries")
            ),
        },
        Part {
            label: "memories",
            tokens: tokens(parts.memories),
            estimated: true,
            detail: format!(
                "{} · /memory",
                count(parts.memory_entries, "memory", "memories")
            ),
        },
        Part {
            label: "skills",
            tokens: tokens(parts.skills),
            estimated: true,
            detail: format!(
                "names and descriptions of {}",
                count(parts.skill_entries, "skill", "skills")
            ),
        },
    ]
}

/// Local MCP tools: listed on demand, so what they take is their lists
/// and results in the conversation (`tokens`).
fn mcp_part(parts: &LocalParts, tokens: u64) -> Part {
    let servers = match parts.mcp_servers {
        0 => "no local MCP servers".to_string(),
        1 => "1 local server".to_string(),
        n => format!("{n} local servers"),
    };
    let detail = match (parts.mcp_servers, tokens) {
        (0, 0) => format!("{servers} · /mcp"),
        _ => format!("{servers} · tool lists and results asked for so far"),
    };
    Part {
        label: "MCP tools",
        tokens,
        estimated: true,
        detail,
    }
}

fn history_detail(context: &ContextUsage) -> String {
    match context.history {
        Some(history) if history.summarized > 0 => format!(
            "{} resent in full, {} older ones summarized",
            history.replayed, history.summarized
        ),
        Some(history) => format!("{} earlier messages resent in full", history.replayed),
        None => "this conversation's messages and tool results".into(),
    }
}

/// `~12.3k  6% · detail`; no percent without a window.
fn part_row(part: &Part, window: u64) -> Row {
    let approx = if part.estimated { "~" } else { "" };
    let share = match window {
        0 => String::new(),
        w => format!(" · {}%", (part.tokens as f64 * 100.0 / w as f64).round()),
    };
    Row::field(
        part.label,
        format!("{approx}{}{share} · {}", short(part.tokens), part.detail),
    )
}

fn footer() -> String {
    format!(
        "~ estimated at {CHARS_PER_TOKEN} characters per token from what gz sent · \
         /compact frees space · /autocompact sets when"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::stream::HistoryReplay;
    use serde_json::json;

    fn usage(tokens: Option<u64>) -> ContextUsage {
        ContextUsage {
            tokens,
            window: 200_000,
            auto_compact_at: 160_000,
            history: Some(HistoryReplay {
                replayed: 12,
                summarized: 0,
                limit: 50,
            }),
        }
    }

    fn text(panel: &Panel) -> String {
        panel.plain_lines().join("\n")
    }

    /// Whether a line holds `label` and then `value`.
    fn has(lines: &str, label: &str, value: &str) -> bool {
        lines
            .lines()
            .any(|l| l.trim_start().starts_with(label) && l.contains(value))
    }

    #[test]
    fn the_client_context_parts_are_counted_from_the_field() {
        let field = json!({
            "instructions": [{"source": "user", "path": "abcd", "text": "x".repeat(396)}],
            "memories": [{"id": "m_1", "text": "Uses tabs"}, {"id": "p_1", "text": "make"}],
            "skills": [{"name": "pdf", "description": "Read PDFs"}],
        });
        let parts = LocalParts::default().with_client_context(&field);
        assert_eq!((parts.instructions, parts.instruction_entries), (400, 1));
        assert_eq!((parts.memories, parts.memory_entries), (19, 2));
        assert_eq!((parts.skills, parts.skill_entries), (12, 1));
    }

    #[test]
    fn a_measured_window_splits_the_reported_total() {
        let parts = LocalParts {
            instructions: 8_000,
            instruction_entries: 3,
            memories: 400,
            memory_entries: 4,
            messages: 40_000,
            ..LocalParts::default()
        };
        let lines = text(&panel(Some(&usage(Some(30_000))), &parts));
        for (label, value) in [
            ("used", "30k of 200k tokens"),
            ("system & tools", "~17.9k · 9%"),
            ("instructions", "~2k · 1% · 3 entries"),
            ("memories", "~100 · 0% · 4 memories"),
            ("messages", "~10k · 5% · 12 earlier messages"),
            ("free", "130k · 65%"),
            ("compact buffer", "40k · 20%"),
            ("MCP tools", "~0 · 0% · no local MCP servers"),
        ] {
            assert!(
                has(&lines, label, value),
                "{label} {value:?} missing:\n{lines}"
            );
        }
    }

    #[test]
    fn local_mcp_calls_count_as_mcp_tools_not_messages() {
        use crate::code_ui::items::{ToolItem, ToolStatus};
        use crate::code_ui::labels::label;
        let tool = |name: &str, summary: &str, output: &str| {
            Item::Tool(ToolItem {
                label: label(name, summary),
                status: ToolStatus::Ok,
                result: None,
                diff: None,
                children: Vec::new(),
                output: Some(output.to_string()),
            })
        };
        let items = [
            Item::User("hi".repeat(10)),
            tool(
                "local_mcp_tools",
                "local_mcp_tools docs",
                &"t".repeat(4_000),
            ),
            tool("read_file", "read_file src/a.rs", &"r".repeat(400)),
        ];
        let parts = LocalParts {
            mcp_servers: 1,
            ..LocalParts::default()
        }
        .with_transcript(&items);
        assert_eq!((parts.mcp, parts.messages), (4_000, 420));
        let lines = text(&panel(Some(&usage(Some(30_000))), &parts));
        assert!(
            has(&lines, "MCP tools", "~1k · 1% · 1 local server"),
            "{lines}"
        );
        assert!(has(&lines, "messages", "~105 "), "{lines}");
    }

    #[test]
    fn messages_never_take_more_than_the_total_leaves() {
        let parts = LocalParts {
            instructions: 4_000,
            messages: 4_000_000,
            ..LocalParts::default()
        };
        let lines = text(&panel(Some(&usage(Some(5_000))), &parts));
        assert!(has(&lines, "system & tools", "~0 "), "{lines}");
        assert!(has(&lines, "messages", "~4k"), "{lines}");
    }

    #[test]
    fn before_the_first_reply_only_local_parts_are_shown() {
        let parts = LocalParts {
            instructions: 400,
            instruction_entries: 1,
            mcp_servers: 2,
            ..LocalParts::default()
        };
        let lines = text(&panel(None, &parts));
        assert!(lines.contains("not measured yet"));
        assert!(has(&lines, "instructions", "~100 · 1 entry "), "{lines}");
        assert!(lines.contains("2 local servers"), "{lines}");
        assert!(lines.contains("Send a message to measure"));
        let silent = text(&panel(Some(&usage(None)), &parts));
        assert!(silent.contains("reports no token counts"), "{silent}");
    }

    #[test]
    fn the_panel_fits_narrow_terminals() {
        let parts = LocalParts {
            instructions: 8_000,
            mcp_servers: 3,
            messages: 9_000,
            ..LocalParts::default()
        };
        for context in [Some(usage(Some(30_000))), None] {
            let panel = panel(context.as_ref(), &parts);
            for width in [40, 80, 200] {
                for line in panel.render(width) {
                    assert!(line.width() <= width, "{width}: {line:?}");
                }
            }
        }
    }
}
