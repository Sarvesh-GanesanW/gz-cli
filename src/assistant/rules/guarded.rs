//! gz's own project settings: the `.gz` folder (permission rules, hooks,
//! MCP servers, instruction rules) and `.mcp.json`. A project's deny and
//! ask rules count without trust, so an edit that removed one would undo
//! the user's protection quietly; every edit there asks, whatever a mode,
//! grant or allow rule says.

use std::path::{Component, Path};

use super::files::PROJECT_DIR;
use super::pattern::{Call, EDIT_TOOLS};
use super::{Kind, RuleVerdict, Source};

/// The MCP servers file a project may keep at its top.
const MCP_FILE: &str = ".mcp.json";

/// What the prompt names as the rule.
const RULE: &str = "edit(.gz/**, .mcp.json)";

/// gz's ask for an edit of its project settings, if `call` is one.
pub fn verdict(call: &Call) -> Option<RuleVerdict> {
    let path = call.path?;
    let edits = EDIT_TOOLS.contains(&call.tool);
    (edits && holds_settings(path)).then(|| RuleVerdict {
        kind: Kind::Ask,
        source: Source::BuiltIn,
        rule: RULE.to_string(),
        unclear: false,
    })
}

/// Whether `path` is in a `.gz` folder or is a `.mcp.json` file.
pub fn holds_settings(path: &Path) -> bool {
    let in_gz_folder = path
        .parent()
        .is_some_and(|folder| folder.components().any(is_gz_folder));
    in_gz_folder || path.file_name().is_some_and(|name| name == MCP_FILE)
}

fn is_gz_folder(part: Component) -> bool {
    part.as_os_str() == PROJECT_DIR
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(path: &str) -> Option<RuleVerdict> {
        verdict(&Call {
            tool: "write_file",
            path: Some(Path::new(path)),
            command: None,
            host: None,
            local_tool: None,
        })
    }

    #[test]
    fn edits_of_gz_settings_ask_and_other_edits_do_not() {
        for path in [
            "/p/.gz/permissions.toml",
            "/p/.gz/permissions.local.toml",
            "/p/.gz/hooks.toml",
            "/p/.gz/mcp.json",
            "/p/.gz/rules/style.md",
            "/p/.mcp.json",
        ] {
            let verdict = edit(path).unwrap_or_else(|| panic!("{path} should ask"));
            assert_eq!(verdict.kind, Kind::Ask);
            assert!(verdict.ask_note().contains("gz itself"));
        }
        for path in ["/p/src/main.rs", "/p/GZ.md", "/p/notes.gz", "/p/x.gz/a"] {
            assert_eq!(edit(path), None, "{path}");
        }
    }

    #[test]
    fn reads_of_gz_settings_need_no_ask() {
        let read = Call {
            tool: "read_file",
            path: Some(Path::new("/p/.gz/permissions.toml")),
            command: None,
            host: None,
            local_tool: None,
        };
        assert_eq!(verdict(&read), None);
    }
}
