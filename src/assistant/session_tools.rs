//! Claude Code's tools that the session answers itself instead of through
//! the sandboxed catalog in [`crate::assistant::tools`]: reading and
//! stopping background commands, asking the user, and plan mode.
//! `todo_write` runs on the chat server; gz only offers it and shows the
//! list it streams.

use serde_json::Value;

use crate::assistant::questions::{self, Query};
use crate::assistant::tools::{ToolName, ToolOutput};
use crate::assistant::{Approver, AssistantEvent};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionTool {
    CommandOutput,
    StopCommand,
    AskUserQuestion,
    EnterPlanMode,
    ExitPlanMode,
    TodoWrite,
}

impl SessionTool {
    pub const ALL: [SessionTool; 6] = [
        SessionTool::CommandOutput,
        SessionTool::StopCommand,
        SessionTool::AskUserQuestion,
        SessionTool::EnterPlanMode,
        SessionTool::ExitPlanMode,
        SessionTool::TodoWrite,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            SessionTool::CommandOutput => "command_output",
            SessionTool::StopCommand => "stop_command",
            SessionTool::AskUserQuestion => "ask_user_question",
            SessionTool::EnterPlanMode => "enter_plan_mode",
            SessionTool::ExitPlanMode => "exit_plan_mode",
            SessionTool::TodoWrite => "todo_write",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.as_str() == name)
    }

    /// Tools that talk to the user, which background subagents don't get.
    fn is_interactive(self) -> bool {
        matches!(
            self,
            SessionTool::AskUserQuestion
                | SessionTool::EnterPlanMode
                | SessionTool::ExitPlanMode
                | SessionTool::TodoWrite
        )
    }

    fn is_background_command(self) -> bool {
        matches!(self, SessionTool::CommandOutput | SessionTool::StopCommand)
    }
}

/// What a session can run besides the original catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Abilities {
    /// A front end shows questions, plans and the todo list.
    pub interactive: bool,
    /// A front end hears when background commands end.
    pub background_commands: bool,
}

/// The newer tools to offer: those the server lists (`clientTools` in its
/// capabilities) and this session can run, in the server's catalog order.
pub fn extra_tools(server: &[String], abilities: Abilities) -> Vec<&'static str> {
    let runs = |name: &str| {
        if ToolName::EXTRA.iter().any(|t| t.as_str() == name) {
            return true;
        }
        SessionTool::parse(name).is_some_and(|tool| {
            (abilities.interactive || !tool.is_interactive())
                && (abilities.background_commands || !tool.is_background_command())
        })
    };
    let known = ToolName::EXTRA
        .iter()
        .map(|t| t.as_str())
        .chain(SessionTool::ALL.iter().map(|t| t.as_str()));
    let ours: Vec<&'static str> = known.filter(|name| runs(name)).collect();
    server
        .iter()
        .filter_map(|name| ours.iter().find(|ours| **ours == name.as_str()).copied())
        .collect()
}

/// What the model reads at the end of the user's message while plan mode
/// is on, so it plans instead of trying actions gz will refuse.
pub const PLAN_MODE_NOTE: &str =
    "<gz_mode>Plan mode is on: gz refuses edits, writes, commands and \
     scripts. Explore read-only, then call exit_plan_mode with your plan.</gz_mode>";

pub fn enter_plan_mode(
    approver: &dyn Approver,
    on_event: &mut (dyn FnMut(AssistantEvent) + Send),
) -> ToolOutput {
    if approver.in_plan_mode() {
        return ToolOutput::ok("Plan mode is already on.".to_string());
    }
    if !approver.enter_plan_mode() {
        return ToolOutput::error("gz can't switch to plan mode here");
    }
    on_event(AssistantEvent::Notice(
        "Plan mode on: gz explores read-only and shows a plan before changing anything".into(),
    ));
    ToolOutput::ok(
        "Plan mode is on. Explore read-only, then call exit_plan_mode with your plan.".to_string(),
    )
}

pub async fn exit_plan_mode(input: &Value, approver: &dyn Approver) -> ToolOutput {
    if !approver.in_plan_mode() {
        return ToolOutput::ok("Plan mode is not on; go ahead with the work.".to_string());
    }
    let plan = input["plan"].as_str().unwrap_or_default().trim();
    if plan.is_empty() {
        return ToolOutput::error("exit_plan_mode needs the plan");
    }
    let reply = approver.ask(&Query::Plan(plan.to_string())).await;
    ToolOutput::ok(questions::plan_text(&reply))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> Vec<String> {
        [
            "read_file",
            "memory",
            "command_output",
            "stop_command",
            "notebook_edit",
            "web_fetch",
            "ask_user_question",
            "enter_plan_mode",
            "exit_plan_mode",
            "todo_write",
            "some_future_tool",
        ]
        .map(str::to_string)
        .to_vec()
    }

    #[test]
    fn the_main_session_offers_every_newer_tool_the_server_lists() {
        let all = Abilities {
            interactive: true,
            background_commands: true,
        };
        assert_eq!(
            extra_tools(&server(), all),
            vec![
                "command_output",
                "stop_command",
                "notebook_edit",
                "web_fetch",
                "ask_user_question",
                "enter_plan_mode",
                "exit_plan_mode",
                "todo_write",
            ]
        );
    }

    #[test]
    fn subagents_and_older_servers_get_less() {
        let none = Abilities {
            interactive: false,
            background_commands: false,
        };
        assert_eq!(
            extra_tools(&server(), none),
            vec!["notebook_edit", "web_fetch"]
        );
        let old = ["read_file".to_string()];
        assert!(extra_tools(&old, none).is_empty());
    }
}
