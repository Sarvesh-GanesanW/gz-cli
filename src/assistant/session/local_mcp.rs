//! The session's side of the user's local MCP servers
//! ([`crate::assistant::mcp_local`]): `local_mcp_tools` runs without a
//! prompt, `local_mcp_call` is asked like a command (and can be allowed per
//! server and tool, for the chat or always in this folder).

use serde_json::Value;

use super::{result_detail, AssistantEvent, Session};
use crate::assistant::hooks;
use crate::assistant::mcp_local::{LocalMcp, LocalMcpTool};
use crate::assistant::permissions::{Approver, Decision, Policy};
use crate::assistant::tools::{self, Planned, ToolOutput};

/// Most characters of a call's arguments the prompt and the review get;
/// the chat server already refuses more than 64 KiB of them.
const MAX_SHOWN_ARGUMENTS_CHARS: usize = 20_000;

impl Session {
    /// The user's local MCP servers; offered to the model only when the
    /// chat server lists the tools too.
    pub fn set_local_mcp(&mut self, servers: Option<LocalMcp>) {
        self.local_mcp = servers;
    }

    /// The servers, for a front end's `/mcp` and its background subagents.
    pub fn local_mcp(&self) -> Option<LocalMcp> {
        self.local_mcp.clone()
    }

    /// Runs `local_mcp_tools` or `local_mcp_call`.
    pub(super) async fn local_mcp_tool(
        &mut self,
        run_id: &str,
        tool: LocalMcpTool,
        input: &Value,
        asked_by: Option<String>,
        approver: &dyn Approver,
        on_event: &mut (dyn FnMut(AssistantEvent) + Send),
    ) -> ToolOutput {
        let Some(servers) = self.local_mcp.clone() else {
            return ToolOutput::error("gz has no local MCP servers in this chat");
        };
        match tool {
            LocalMcpTool::ListTools => {
                self.list_local_tools(run_id, &servers, input, on_event)
                    .await
            }
            LocalMcpTool::Call => {
                self.call_local_tool(run_id, &servers, input, asked_by, approver, on_event)
                    .await
            }
        }
    }

    /// Read-only: shown and audited, never asked.
    async fn list_local_tools(
        &self,
        run_id: &str,
        servers: &LocalMcp,
        input: &Value,
        on_event: &mut (dyn FnMut(AssistantEvent) + Send),
    ) -> ToolOutput {
        let name = LocalMcpTool::ListTools.as_str();
        let only = input["server"].as_str().filter(|s| !s.is_empty());
        let summary = format!("{name} {}", only.unwrap_or("all servers"));
        on_event(AssistantEvent::ToolStarted {
            name: name.to_string(),
            summary: summary.clone(),
            local: true,
        });
        let out = servers.list_tools(only).await;
        self.audit(
            run_id,
            name,
            only.unwrap_or("*"),
            ("read-only", None),
            out.is_error,
        );
        on_event(AssistantEvent::ToolFinished {
            name: name.to_string(),
            summary,
            is_error: out.is_error,
            detail: result_detail(&out),
            output: out.output.clone(),
            diff: None,
        });
        out
    }

    /// Asked like a command: the server, the tool and its arguments.
    async fn call_local_tool(
        &mut self,
        run_id: &str,
        servers: &LocalMcp,
        input: &Value,
        asked_by: Option<String>,
        approver: &dyn Approver,
        on_event: &mut (dyn FnMut(AssistantEvent) + Send),
    ) -> ToolOutput {
        let name = LocalMcpTool::Call.as_str();
        let planned = match self.plan_local_call(servers, input).await {
            Ok(planned) => planned,
            Err(err) => {
                let text = format!("{err:#}");
                self.audit(run_id, name, &input.to_string(), ("rejected", None), true);
                on_event(AssistantEvent::Notice(format!("{name}: {text}")));
                return ToolOutput::error(text);
            }
        };
        let preapproved = self.policy.is_preapproved(&planned);
        let decision = if preapproved {
            Decision::AllowOnce
        } else {
            let folder = self.sandbox.display_folder(&self.grant_folder(&planned));
            let mut request = Policy::request_for(&planned, &folder, &folder);
            request.asked_by = asked_by;
            approver.decide(&request).await
        };
        if decision == Decision::Deny {
            let refusal = approver.take_refusal();
            let entry = (refusal.audit_label(), refusal.audit_reason());
            self.audit(run_id, name, &planned.summary, entry, true);
            on_event(AssistantEvent::Notice(refusal.notice(&planned.summary)));
            return ToolOutput::error(refusal.message());
        }
        let decided = if preapproved {
            "preapproved"
        } else {
            approver.take_decided_by().audit_label(decision)
        };
        let folder = self.grant_folder(&planned);
        self.policy.remember(&planned, decision, &folder);
        if decision == Decision::AllowAlways {
            self.save_always(&planned, on_event);
        }
        let hook_call = hooks::ToolCall::new(&self.sandbox, name, &planned.summary, input);
        let log = self.hook_log(run_id);
        if let Some(blocked) = self.hooks.before_tool(&hook_call, &log, on_event).await {
            return blocked;
        }
        on_event(AssistantEvent::ToolStarted {
            name: name.to_string(),
            summary: planned.summary.clone(),
            local: true,
        });
        let (server, tool) = local_call_names(input);
        let arguments = arguments_of(input);
        let out = servers.call(server, tool, arguments).await;
        let out = self.hooks.after_tool(&hook_call, out, &log, on_event).await;
        self.audit(
            run_id,
            name,
            &planned.summary,
            (decided, None),
            out.is_error,
        );
        on_event(AssistantEvent::ToolFinished {
            name: name.to_string(),
            summary: planned.summary,
            is_error: out.is_error,
            detail: result_detail(&out),
            output: out.output.clone(),
            diff: None,
        });
        out
    }

    /// Checks the call names a configured server and one of its tools
    /// (starting the server if needed) before the user is asked about it.
    async fn plan_local_call(&self, servers: &LocalMcp, input: &Value) -> anyhow::Result<Planned> {
        let (server, tool) = local_call_names(input);
        if server.is_empty() || tool.is_empty() {
            anyhow::bail!("local_mcp_call needs a server and a tool");
        }
        if !input["arguments"].is_null() && !input["arguments"].is_object() {
            anyhow::bail!("arguments must be a JSON object");
        }
        let known = servers.tools(server).await?;
        if !known.iter().any(|t| t.name == tool) {
            let names: Vec<&str> = known.iter().map(|t| t.name.as_str()).collect();
            anyhow::bail!(
                "{server} has no tool '{tool}'; its tools: {}",
                if names.is_empty() {
                    "none".to_string()
                } else {
                    names.join(", ")
                }
            );
        }
        let arguments = serde_json::to_string_pretty(&arguments_of(input))?;
        let shown: String = arguments.chars().take(MAX_SHOWN_ARGUMENTS_CHARS).collect();
        Ok(tools::local_tool_call(
            server,
            tool,
            shown,
            self.sandbox.cwd(),
        ))
    }
}

fn local_call_names(input: &Value) -> (&str, &str) {
    (
        input["server"].as_str().unwrap_or_default(),
        input["tool"].as_str().unwrap_or_default(),
    )
}

fn arguments_of(input: &Value) -> Value {
    match &input["arguments"] {
        Value::Object(_) => input["arguments"].clone(),
        _ => Value::Object(Default::default()),
    }
}

#[cfg(test)]
mod tests;
