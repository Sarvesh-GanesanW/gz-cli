//! `pre_tool_use`, `post_tool_use` and `post_tool_use_failure` hooks for
//! the tools gz answers outside the usual permission path: opening a
//! folder, memory, skills and the session's own tools (questions, plans,
//! todos, background commands). A hook's deny refuses the call; its allow
//! and ask change nothing, since these tools already ask the user where it
//! matters (opening a folder outside the open ones always asks).

use serde_json::Value;

use super::{AssistantEvent, Session};
use crate::assistant::hooks;
use crate::assistant::local_context::ContextTool;
use crate::assistant::permissions::Approver;
use crate::assistant::session_tools::SessionTool;
use crate::assistant::tools::ToolOutput;

type Events<'e> = &'e mut (dyn FnMut(AssistantEvent) + Send);

/// A tool outside the usual permission path.
pub(super) enum SideTool {
    Session(SessionTool),
    Context(ContextTool),
}

/// One call to a side tool, as the model sent it.
pub(super) struct SideCall<'a> {
    pub run_id: &'a str,
    pub tool: SideTool,
    pub tool_name: &'a str,
    pub input: &'a Value,
    pub asked_by: Option<String>,
}

impl Session {
    /// Runs `call` between its `pre_tool_use` and post hooks.
    pub(super) async fn hooked_side_tool(
        &mut self,
        call: SideCall<'_>,
        approver: &dyn Approver,
        on_event: Events<'_>,
    ) -> ToolOutput {
        let summary = side_summary(call.tool_name, call.input);
        let pre = {
            let hook_call =
                hooks::ToolCall::new(&self.sandbox, call.tool_name, &summary, call.input);
            self.hooks
                .pre_tool_use(&hook_call, &self.hook_log(call.run_id), on_event)
                .await
        };
        if let Some(refusal) = pre.refusal() {
            self.audit(call.run_id, call.tool_name, &summary, ("hook", None), true);
            return ToolOutput::error(refusal);
        }
        let (run_id, input) = (call.run_id, call.input);
        let out = match call.tool {
            SideTool::Session(tool) => {
                self.session_tool(run_id, tool, input, approver, on_event)
                    .await
            }
            SideTool::Context(ContextTool::ChangeDirectory) => {
                self.change_directory(run_id, input, call.asked_by, approver, on_event)
                    .await
            }
            SideTool::Context(ContextTool::Memory) => self.memory_call(run_id, input, on_event),
            SideTool::Context(ContextTool::LoadSkill) => self.load_skill(run_id, input, on_event),
        };
        // After change_directory the hooks see the folder the call left.
        let hook_call = hooks::ToolCall::new(&self.sandbox, call.tool_name, &summary, input);
        let feedback = self
            .hooks
            .post_tool_feedback(
                &hook_call,
                (out.is_error, &out.output),
                &self.hook_log(run_id),
                on_event,
            )
            .await;
        let post = match out.is_error {
            true => hooks::HookEvent::PostToolUseFailure,
            false => hooks::HookEvent::PostToolUse,
        };
        let out = hooks::with_notes(out, post, &feedback);
        hooks::with_notes(out, hooks::HookEvent::PreToolUse, &pre.context)
    }
}

/// The one-line summary hooks see: the tool and the path it names, if any.
fn side_summary(tool_name: &str, input: &Value) -> String {
    match input["path"].as_str() {
        Some(path) => format!("{tool_name} {path}"),
        None => tool_name.to_string(),
    }
}
