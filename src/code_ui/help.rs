//! `/help`: every slash command, grouped, and the key bindings, on a card
//! as wide as the terminal.

use crate::code_ui::panel::{Panel, Row};
use crate::code_ui::state::{COMMANDS, SHORTCUTS};

/// Command groups in the order shown; a command in no group (a newer one)
/// is listed under "More".
const GROUPS: &[(&str, &[&str])] = &[
    (
        "Conversation",
        &[
            "/new",
            "/clear",
            "/resume",
            "/compact",
            "/autocompact",
            "/rewind",
            "/rename",
            "/fork",
            "/export",
            "/exit",
        ],
    ),
    (
        "Model and permissions",
        &[
            "/models",
            "/effort",
            "/mode",
            "/auto",
            "/allow",
            "/permissions",
            "/sandbox",
        ],
    ),
    (
        "Folders and context",
        &[
            "/cd",
            "/add-dir",
            "/dirs",
            "/context",
            "/init",
            "/memory",
            "/instructions",
            "/skills",
            "/mcp",
            "/hooks",
        ],
    ),
    ("Work in progress", &["/subagents", "/shells", "/diff"]),
    ("Review", &["/review", "/security-review"]),
    ("Usage and cost", &["/usage", "/last", "/budget"]),
    (
        "Screen",
        &[
            "/help",
            "/theme",
            "/notifications",
            "/statusline",
            "/terminal-setup",
        ],
    ),
];

const OTHER_GROUP: &str = "More";

/// Heading of the section that lists the user's own command files.
const CUSTOM_GROUP: &str = "Your commands (.gz/commands, commands/ in the gz config folder)";

/// The `/help` card; `custom` lists the user's own commands (and notes on
/// command files that went unused).
pub fn panel(custom: Vec<Row>) -> Panel {
    let mut panel = Panel::new("Help").aside("gz code").full_width();
    for (heading, names) in GROUPS {
        let rows = COMMANDS
            .iter()
            .filter(|(name, _)| names.contains(name))
            .map(|(name, what)| Row::entry(name, *what))
            .collect();
        panel = panel.section(Some(heading), rows);
    }
    let grouped = |name: &str| GROUPS.iter().any(|(_, names)| names.contains(&name));
    let rest = COMMANDS
        .iter()
        .filter(|(name, _)| !grouped(name))
        .map(|(name, what)| Row::entry(name, *what))
        .collect();
    panel = panel
        .section(Some(OTHER_GROUP), rest)
        .section(Some(CUSTOM_GROUP), custom);
    // The shortcut list points at /help, which is this card.
    let keys = SHORTCUTS
        .iter()
        .filter(|keys| !keys.starts_with("/help"))
        .map(|keys| Row::note(*keys))
        .collect();
    panel
        .section(Some("Keys"), keys)
        .footer("@ attaches files · ! runs a shell command · esc interrupts")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code_ui::panel::Section;
    use unicode_width::UnicodeWidthStr;

    fn text(lines: &[ratatui::text::Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn every_command_is_listed_once() {
        let panel = panel(Vec::new());
        let listed: Vec<&str> = panel
            .sections
            .iter()
            .flat_map(|s: &Section| &s.rows)
            .filter_map(|row| match row {
                Row::Entry { key, .. } => Some(key.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(listed.len(), COMMANDS.len());
        for (name, _) in COMMANDS {
            assert!(listed.contains(name), "{name} is missing from /help");
        }
    }

    #[test]
    fn the_card_spans_the_terminal_and_never_overflows() {
        for width in [40, 80, 140, 200] {
            let lines = text(&panel(Vec::new()).render(width));
            assert!(lines.iter().all(|l| l.width() <= width), "width {width}");
            assert_eq!(lines[0].width(), width, "width {width}");
        }
    }

    #[test]
    fn a_wide_terminal_keeps_each_command_on_one_line() {
        let lines = text(&panel(Vec::new()).render(160));
        let joined = lines.join("\n");
        for heading in ["Conversation", "Keys", "Screen"] {
            assert!(joined.contains(heading), "{heading}");
        }
        let resume = lines.iter().position(|l| l.contains("/resume")).unwrap();
        assert!(
            lines[resume + 1].contains("/compact"),
            "{}",
            lines[resume + 1]
        );
    }
}
