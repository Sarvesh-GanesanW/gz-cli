//! `/mcp`: the user's MCP servers from every scope, with their state and
//! tools; `/mcp reconnect <name>` connects one afresh, `/mcp approve` and
//! `/mcp decline` answer for a project server.

use crate::assistant::mcp_local::{LocalMcp, Phase, ServerStatus};
use crate::code_ui::items::Item;
use crate::code_ui::panel::{Panel, Row};

use super::{Effect, UiState};

const USAGE: &str = "Use /mcp to list your MCP servers, or /mcp reconnect|approve|decline <name>";
const SAMPLE: &str = "[servers.files]  command = \"npx\"  args = [\"-y\", \"@scope/server\"]";

/// What `/mcp` asks the driver to do; servers are started off the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpCommand {
    List,
    Restart(String),
    Approve(String),
    Decline(String),
}

impl UiState {
    pub(super) fn mcp_command(&mut self, command: &str) -> Vec<Effect> {
        let mut words = command.split_whitespace().skip(1);
        let parsed = match (words.next(), words.next(), words.next()) {
            (None, ..) => McpCommand::List,
            (Some("restart" | "reconnect"), Some(name), None) => {
                McpCommand::Restart(name.to_string())
            }
            (Some("approve"), Some(name), None) => McpCommand::Approve(name.to_string()),
            (Some("decline"), Some(name), None) => McpCommand::Decline(name.to_string()),
            _ => {
                self.record(Item::Error(USAGE.into()));
                return Vec::new();
            }
        };
        if self.local_mcp.is_none() {
            self.record(Item::Error(
                "MCP servers aren't available in this session".into(),
            ));
            return Vec::new();
        }
        if let McpCommand::Restart(name) = &parsed {
            // Connecting can take up to the startup timeout; say it began.
            self.record(Item::Notice(format!("Reconnecting {name}…")));
        }
        vec![Effect::Mcp(parsed)]
    }
}

/// Runs a `/mcp` command; returns what to print.
pub async fn run(servers: &LocalMcp, command: McpCommand) -> Vec<Item> {
    let mut items = Vec::new();
    match &command {
        McpCommand::List => {}
        McpCommand::Restart(name) => items.push(match servers.restart(name).await {
            Ok(1) => Item::Notice(format!("✓ Reconnected {name} · 1 tool")),
            Ok(tools) => Item::Notice(format!("✓ Reconnected {name} · {tools} tools")),
            Err(err) => Item::Error(format!("{err:#}")),
        }),
        McpCommand::Approve(name) => items.push(match servers.approve(name, true).await {
            Ok(()) => Item::Notice(format!(
                "✓ Approved {name}; it connects on first use. A changed entry asks again"
            )),
            Err(err) => Item::Error(format!("{err:#}")),
        }),
        McpCommand::Decline(name) => items.push(match servers.approve(name, false).await {
            Ok(()) => Item::Notice(format!("{name} stays off; /mcp approve {name} turns it on")),
            Err(err) => Item::Error(format!("{err:#}")),
        }),
    }
    items.push(Item::Panel(panel(servers)));
    items
}

fn panel(servers: &LocalMcp) -> Panel {
    let config = servers.config_path().display().to_string();
    let mut panel = Panel::new("MCP servers").aside(config.clone());
    let problems: Vec<Row> = servers
        .problems()
        .iter()
        .map(|why| Row::note(format!("Not loaded: {why}")))
        .collect();
    let statuses = servers.statuses();
    if statuses.is_empty() {
        return panel
            .section(None, problems)
            .section(
                None,
                vec![
                    Row::note(format!(
                        "None yet. Add one with `gz mcp add`, or to {config}, e.g."
                    )),
                    Row::note(SAMPLE),
                ],
            )
            .footer("gz connects to a server the first time the model uses it");
    }
    let rows = statuses
        .iter()
        .flat_map(|status| server_rows(servers, status))
        .collect();
    panel = panel
        .section(None, rows)
        .section(Some("Problems"), problems);
    panel.footer(
        "/mcp reconnect <name> · /mcp approve|decline <name> for project servers · \
         gz mcp login <name> signs in · stderr of local servers is in mcp-logs/",
    )
}

fn server_rows(servers: &LocalMcp, status: &ServerStatus) -> Vec<Row> {
    let mut rows = vec![
        Row::entry(&status.name, status.phase.label()),
        Row::note(format!(
            "⎿ {} · {} · {}",
            status.scope.as_str(),
            status.transport,
            status.command
        )),
    ];
    let name = &status.name;
    match &status.phase {
        Phase::NeedsLogin(_) => rows.push(Row::note(format!(
            "⎿ run `gz mcp login {name}` in a terminal, then /mcp reconnect {name}"
        ))),
        Phase::NeedsApproval => {
            if let Ok(entry) = servers.entry(name) {
                rows.push(Row::note(format!("⎿ from {}", entry.source.display())));
                rows.push(Row::note(format!(
                    "⎿ {}",
                    serde_json::to_string(&entry.spec).unwrap_or_default()
                )));
            }
            rows.push(Row::note(format!(
                "⎿ runs only once approved: /mcp approve {name} or /mcp decline {name}"
            )));
        }
        _ => {}
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code_ui::state::test_support::state;

    #[test]
    fn mcp_lists_or_restarts_and_explains_its_usage() {
        let mut s = state();
        let dir = std::env::temp_dir().join(format!("gz-ui-mcp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        s.local_mcp = Some(LocalMcp::from_file(&dir.join("mcp.toml")));
        assert_eq!(s.slash("/mcp"), vec![Effect::Mcp(McpCommand::List)]);
        assert_eq!(
            s.slash("/mcp restart files"),
            vec![Effect::Mcp(McpCommand::Restart("files".into()))]
        );
        assert!(
            matches!(s.flush.last(), Some(Item::Notice(n)) if n == "Reconnecting files…"),
            "a restart says it began: {:?}",
            s.flush.last()
        );
        for (line, command) in [
            ("/mcp reconnect files", McpCommand::Restart("files".into())),
            (
                "/mcp approve tracker",
                McpCommand::Approve("tracker".into()),
            ),
            (
                "/mcp decline tracker",
                McpCommand::Decline("tracker".into()),
            ),
        ] {
            assert_eq!(s.slash(line), vec![Effect::Mcp(command)], "{line}");
        }
        assert!(s.slash("/mcp stop files").is_empty());
        assert!(
            matches!(s.flush.last(), Some(Item::Error(e)) if e.contains("/mcp reconnect|approve|decline <name>"))
        );
        assert!(super::super::commands::COMMANDS
            .iter()
            .any(|(name, _)| *name == "/mcp"));
    }

    #[tokio::test]
    async fn the_panel_shows_each_servers_state_and_masked_command() {
        let dir = std::env::temp_dir().join(format!("gz-ui-mcp-panel-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("mcp.toml");
        std::fs::write(
            &config,
            "[servers.files]\ncommand = \"npx\"\nargs = [\"-y\", \"@scope/server\"]\nenv = { TOKEN = \"literal\" }\n",
        )
        .unwrap();
        let items = run(&LocalMcp::from_file(&config), McpCommand::List).await;
        let Some(Item::Panel(panel)) = items.last() else {
            panic!("{items:?}")
        };
        let text = panel.plain_lines().join("\n");
        assert!(
            text.contains("files") && text.contains("not started"),
            "{text}"
        );
        assert!(text.contains("TOKEN=**** npx -y @scope/server"), "{text}");
        assert!(!text.contains("literal"), "{text}");

        std::fs::write(&config, "[servers.files]\n").unwrap();
        let items = run(&LocalMcp::from_file(&config), McpCommand::List).await;
        let Some(Item::Panel(panel)) = items.last() else {
            panic!("{items:?}")
        };
        let text = panel.plain_lines().join(" ");
        assert!(
            text.contains("Not loaded") && text.contains("mcp.toml"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn a_project_server_is_shown_for_review_until_approved() {
        use crate::assistant::mcp_local::scopes::Places;
        let dir = std::env::temp_dir().join(format!("gz-ui-mcp-approve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("project")).unwrap();
        std::fs::write(
            dir.join("project/.mcp.json"),
            r#"{"mcpServers": {"tracker": {"command": "tracker-mcp", "args": ["--stdio"]}}}"#,
        )
        .unwrap();
        let places = Places::at(dir.join("config"), Some(&dir.join("project")));
        let servers = LocalMcp::from_places(places.clone());
        assert_eq!(servers.pending_approval(), ["tracker"]);
        let items = run(&servers, McpCommand::List).await;
        let Some(Item::Panel(panel)) = items.last() else {
            panic!("{items:?}")
        };
        let text = panel.plain_lines().join("\n");
        for part in [
            "needs approval",
            "project · stdio · tracker-mcp --stdio",
            ".mcp.json",
            "/mcp approve tracker",
        ] {
            assert!(text.contains(part), "{part}: {text}");
        }
        for width in [40, 80] {
            for line in panel.render(width) {
                assert!(line.width() <= width, "{width}: {line:?}");
            }
        }
        let items = run(&servers, McpCommand::Approve("tracker".into())).await;
        assert!(
            matches!(&items[0], Item::Notice(n) if n.contains("Approved tracker")),
            "{items:?}"
        );
        assert!(servers.pending_approval().is_empty());
        assert!(
            LocalMcp::from_places(places).pending_approval().is_empty(),
            "the approval is remembered"
        );
    }
}
