//! `/mcp`: the user's local MCP servers (`mcp.toml`) with their state and
//! tool counts; `/mcp restart <name>` starts one fresh.

use crate::assistant::mcp_local::LocalMcp;
use crate::code_ui::items::Item;
use crate::code_ui::panel::{Panel, Row};

use super::{Effect, UiState};

const USAGE: &str = "Use /mcp to list your local MCP servers, or /mcp restart <name>";
const SAMPLE: &str = "[servers.files]  command = \"npx\"  args = [\"-y\", \"@scope/server\"]";

/// What `/mcp` asks the driver to do; servers are started off the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpCommand {
    List,
    Restart(String),
}

impl UiState {
    pub(super) fn mcp_command(&mut self, command: &str) -> Vec<Effect> {
        let mut words = command.split_whitespace().skip(1);
        let parsed = match (words.next(), words.next(), words.next()) {
            (None, ..) => McpCommand::List,
            (Some("restart"), Some(name), None) => McpCommand::Restart(name.to_string()),
            _ => {
                self.record(Item::Error(USAGE.into()));
                return Vec::new();
            }
        };
        if self.local_mcp.is_none() {
            self.record(Item::Error(
                "Local MCP servers aren't available in this session".into(),
            ));
            return Vec::new();
        }
        vec![Effect::Mcp(parsed)]
    }
}

/// Runs a `/mcp` command; returns what to print.
pub async fn run(servers: &LocalMcp, command: McpCommand) -> Vec<Item> {
    let mut items = Vec::new();
    if let McpCommand::Restart(name) = &command {
        items.push(match servers.restart(name).await {
            Ok(1) => Item::Notice(format!("✓ Restarted {name} · 1 tool")),
            Ok(tools) => Item::Notice(format!("✓ Restarted {name} · {tools} tools")),
            Err(err) => Item::Error(format!("{err:#}")),
        });
    }
    items.push(Item::Panel(panel(servers).await));
    items
}

async fn panel(servers: &LocalMcp) -> Panel {
    let config = servers.config_path().display().to_string();
    let mut panel = Panel::new("MCP servers").aside(config.clone());
    if let Some(why) = servers.config_error() {
        return panel
            .section(None, vec![Row::note(format!("Not loaded: {why}"))])
            .footer("Fix the file and start gz again");
    }
    let statuses = servers.statuses().await;
    if statuses.is_empty() {
        return panel
            .section(
                None,
                vec![
                    Row::note(format!("None yet. Add one to {config}, e.g.")),
                    Row::note(SAMPLE),
                ],
            )
            .footer("gz starts a server the first time the model uses it");
    }
    let rows = statuses
        .iter()
        .flat_map(|status| {
            [
                Row::entry(&status.name, status.phase.label()),
                Row::note(format!("⎿ {}", status.command)),
            ]
        })
        .collect();
    panel = panel.section(None, rows);
    panel.footer("/mcp restart <name> starts one fresh · each server's stderr is in mcp-logs/")
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
        assert!(s.slash("/mcp stop files").is_empty());
        assert!(
            matches!(s.flush.last(), Some(Item::Error(e)) if e.contains("/mcp restart <name>"))
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
}
