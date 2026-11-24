//! Scripted `gz code` runs: `-p/--print`, `--output-format`,
//! `--input-format stream-json`, `--max-turns`, `--max-budget-usd`, and the
//! older `gz code "<prompt>" -o json|yaml` report. Nothing here ever waits
//! for a person: actions nothing allows are refused and reported.
//!
//! Output schemas are documented in `docs/headless.md`.

use std::io::Write;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::assistant::local_context::{self, Limits};
use crate::assistant::models::Catalog;
use crate::assistant::run_controls::{self, ExtraInstruction, RunControls, StepGuard};
use crate::assistant::sandbox::{FolderCheck, Sandbox};
use crate::assistant::{AssistantEvent, Session, TurnEnd};
use crate::cli::CodeArgs;
use crate::cmd::code_report::{exit, Report, ReportedExit, Summary, ToolCall};
use crate::cmd::code_unattended::DecisionLog;
use crate::cmd::Runtime;
use crate::output::OutputFormat;
use crate::subagents::{Board, SubagentEvent};

/// `--output-format`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputKind {
    /// The final answer only.
    Text,
    /// One result object at the end.
    Json,
    /// One JSON event per line, then the result.
    StreamJson,
}

/// `--input-format`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum InputKind {
    /// The prompt argument, or all of standard input.
    Text,
    /// One JSON user message per line on standard input.
    StreamJson,
}

/// What a scripted run prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    Text,
    Json,
    StreamJson,
    /// The `-o json|yaml` report (`code_report`), kept as it was.
    Report(OutputFormat),
}

/// How a scripted run reads and prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    pub output: Output,
    /// Messages come as JSON lines on standard input.
    pub input_stream: bool,
}

impl Plan {
    /// Whether a one-shot prompt should be read from piped standard input
    /// when no argument is given (stream input reads its own lines).
    pub fn reads_piped_prompt(&self) -> bool {
        !self.input_stream
    }
}

/// Decides whether this `gz code` is a scripted run, and how; `None` for
/// the interactive UI and the plain streamed one-shot (`gz code "<prompt>"`).
pub fn plan(args: &CodeArgs, format: OutputFormat, has_prompt: bool) -> Result<Option<Plan>> {
    let input_stream = args.input_format == Some(InputKind::StreamJson);
    if input_stream && args.output_format != Some(OutputKind::StreamJson) {
        anyhow::bail!(
            "--input-format stream-json needs --output-format stream-json, so each message's \
             events and result can be told apart"
        );
    }
    if input_stream && args.prompt.is_some() {
        anyhow::bail!(
            "--input-format stream-json reads every message from standard input; drop the \
             prompt argument and send it as the first line"
        );
    }
    let report_format = matches!(format, OutputFormat::Json | OutputFormat::Yaml);
    if args.output_format.is_some() && format != OutputFormat::Table {
        anyhow::bail!(
            "use either -o json|yaml (the run report) or --output-format (text, json, \
             stream-json), not both"
        );
    }
    let limited = args.max_turns.is_some() || args.max_budget_usd.is_some();
    let one_shot = has_prompt || input_stream || args.print || args.output_format.is_some();
    let scripted = args.print
        || args.output_format.is_some()
        || input_stream
        || (has_prompt && (report_format || limited));
    if !scripted {
        if limited && !one_shot {
            anyhow::bail!(
                "--max-turns and --max-budget-usd apply to one-shot runs: add -p and a prompt \
                 (gz code -p \"fix the test\" --max-turns 5); /budget sets caps for interactive \
                 sessions"
            );
        }
        if format == OutputFormat::Csv && has_prompt {
            anyhow::bail!("gz code reports a run as one object; use -o json or -o yaml");
        }
        return Ok(None);
    }
    if format == OutputFormat::Csv {
        anyhow::bail!("gz code reports a run as one object; use -o json or -o yaml");
    }
    if args.resume {
        anyhow::bail!(
            "--resume opens a picker, which needs an interactive terminal; continue a scripted \
             run with -c <conversation id> or --continue"
        );
    }
    if let Some(budget) = args.max_budget_usd {
        if !(budget.is_finite() && budget > 0.0) {
            anyhow::bail!("--max-budget-usd must be a positive number of US dollars, e.g. 0.50");
        }
    }
    let output = match args.output_format {
        Some(OutputKind::Text) => Output::Text,
        Some(OutputKind::Json) => Output::Json,
        Some(OutputKind::StreamJson) => Output::StreamJson,
        None if report_format => Output::Report(format),
        None => Output::Text,
    };
    Ok(Some(Plan {
        output,
        input_stream,
    }))
}

/// The run controls the flags ask for (any mode): `--tools`, `--bare` and
/// the appended instructions.
pub fn controls(args: &CodeArgs) -> Result<RunControls> {
    let tools = match &args.tools {
        Some(list) => run_controls::parse_tools(list)?,
        None => None,
    };
    let mut extra_instructions = Vec::new();
    if let Some(text) = &args.append_system_prompt {
        extra_instructions.push(ExtraInstruction::new("--append-system-prompt", text)?);
    }
    for path in &args.append_system_prompt_file {
        let text = std::fs::read_to_string(path).with_context(|| {
            format!(
                "cannot read --append-system-prompt-file {}; check the path",
                path.display()
            )
        })?;
        extra_instructions.push(ExtraInstruction::new(&path.display().to_string(), &text)?);
    }
    if tools.as_ref().is_some_and(Vec::is_empty) && !extra_instructions.is_empty() {
        anyhow::bail!(
            "--append-system-prompt goes out with the local tools, so it needs at least one: \
             drop --tools none or name some tools"
        );
    }
    let controls = RunControls {
        tools,
        extra_instructions,
        bare: args.bare,
        guard: None,
    };
    // The contract's limits now; the server's own once it is asked.
    controls.check_limits(&Limits::default())?;
    Ok(controls)
}

/// `--add-dir` folders as absolute paths, read from `here` (where gz was
/// started), so a session that runs elsewhere (`--worktree`) opens the
/// folders the user named; a folder that can't be opened fails here,
/// before anything is made.
pub fn anchor_add_dirs(here: &Path, dirs: &[String]) -> Result<Vec<String>> {
    if dirs.is_empty() {
        return Ok(Vec::new());
    }
    let start = Sandbox::new(here).context("cannot open the current folder")?;
    dirs.iter()
        .map(|dir| {
            let folder = match start
                .check_folder(dir)
                .with_context(|| format!("--add-dir {dir}"))?
            {
                FolderCheck::Inside(folder) | FolderCheck::Outside(folder) => folder,
            };
            Ok(folder.display().to_string())
        })
        .collect()
}

/// Applies the flags to a new session: folders, `--bare` and the controls.
pub fn apply_flags(session: &mut Session, args: &CodeArgs, controls: RunControls) -> Result<()> {
    for dir in &args.add_dir {
        session
            .add_dir(dir)
            .with_context(|| format!("--add-dir {dir}"))?;
    }
    if args.bare {
        session.set_hooks(Default::default());
        session.set_local_mcp(None);
        session.set_memory(false);
    }
    session.set_run_controls(controls);
    Ok(())
}

/// Reads the server's local-context limits into `session`, for a run
/// without the model list. Appended instructions can't go out without
/// them, so then a server whose capabilities can't be read ends the run
/// at once; other runs go on without local context.
pub async fn read_local_context(session: &mut Session, controls: &RunControls) -> Result<()> {
    let limits = match local_context::read_limits(&session.http()).await {
        Ok(limits) => limits,
        Err(err) if controls.needs_local_context() => {
            return Err(err.context(
                "cannot read the chat server's capabilities to check it takes \
                 --append-system-prompt; check the connection (gz doctor) and try again",
            ))
        }
        Err(_) => None,
    };
    session.set_local_context(limits);
    Ok(())
}

/// Says why extra instructions can't go out on a server without local
/// context, instead of dropping them.
pub fn check_server_takes(session: &Session, controls: &RunControls) -> Result<()> {
    if !controls.needs_local_context() {
        return Ok(());
    }
    match session.local_context().limits() {
        Some(limits) => controls.check_limits(&limits),
        None => anyhow::bail!(
            "this chat server does not take local context, so --append-system-prompt can't be \
             sent; update the chat server or run without it"
        ),
    }
}

/// Why a guard stopped a run.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Stop {
    MaxTurns(u32),
    Budget(f64),
}

impl Stop {
    fn subtype(self) -> &'static str {
        match self {
            Stop::MaxTurns(_) => "error_max_turns",
            Stop::Budget(_) => "error_max_budget_usd",
        }
    }

    fn exit_code(self) -> i32 {
        match self {
            Stop::MaxTurns(_) => exit::MAX_TURNS,
            Stop::Budget(_) => exit::MAX_BUDGET,
        }
    }

    fn message(self, spent: f64) -> String {
        match self {
            Stop::MaxTurns(max) => format!(
                "stopped after {max} model turn{}: --max-turns {max} reached before the answer \
                 was finished; raise --max-turns or continue with -c <conversation id>",
                if max == 1 { "" } else { "s" }
            ),
            Stop::Budget(max) => format!(
                "stopped: the run has cost ${spent:.4}, reaching --max-budget-usd {max}; raise \
                 the budget or continue with -c <conversation id>"
            ),
        }
    }
}

#[derive(Default)]
struct GuardState {
    /// Model turns this message took so far.
    turns: u32,
    /// Cost reported in this process so far.
    spent: f64,
    stopped: Option<Stop>,
}

/// Counts model turns and cost, and stops before a turn past the limits.
pub struct Guard {
    max_turns: Option<u32>,
    max_budget: Option<f64>,
    state: Mutex<GuardState>,
    events: Arc<Mutex<Emitter>>,
}

impl Guard {
    fn state(&self) -> std::sync::MutexGuard<'_, GuardState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn new_message(&self) {
        let mut state = self.state();
        state.turns = 0;
        state.stopped = None;
    }

    fn spent(&self, cost: f64) {
        self.state().spent += cost;
    }

    fn turns(&self) -> u32 {
        self.state().turns
    }

    fn stopped(&self) -> Option<(Stop, f64)> {
        let state = self.state();
        state.stopped.map(|stop| (stop, state.spent))
    }
}

impl StepGuard for Guard {
    fn before_leg(&self) -> Option<String> {
        let mut state = self.state();
        let stop = match (self.max_turns, self.max_budget) {
            (Some(max), _) if state.turns >= max => Some(Stop::MaxTurns(max)),
            (_, Some(max)) if state.spent >= max => Some(Stop::Budget(max)),
            _ => None,
        };
        match stop {
            Some(stop) => {
                state.stopped = Some(stop);
                Some(stop.message(state.spent))
            }
            None => {
                state.turns += 1;
                None
            }
        }
    }

    fn tool_requested(&self, tool: &str, input: &Value) {
        lock(&self.events).tool_use(tool, input, true);
    }
}

fn lock(events: &Mutex<Emitter>) -> std::sync::MutexGuard<'_, Emitter> {
    events.lock().unwrap_or_else(|p| p.into_inner())
}

/// Writes stream-json events (and nothing for the other outputs, which
/// print once at the end).
pub struct Emitter {
    streaming: bool,
    out: Box<dyn Write + Send>,
    /// Answer text not yet written as an `assistant` event.
    text: String,
    /// Each subagent's text not yet written, by subagent id.
    subagent_text: std::collections::BTreeMap<String, String>,
    /// Refusals already written as `permission_denied` events.
    denials_sent: usize,
    /// A write failed (e.g. the reader closed the pipe); stop writing.
    broken: bool,
}

impl Emitter {
    pub fn new(streaming: bool, out: Box<dyn Write + Send>) -> Self {
        Self {
            streaming,
            out,
            text: String::new(),
            subagent_text: Default::default(),
            denials_sent: 0,
            broken: false,
        }
    }

    fn write(&mut self, event: Value) {
        if !self.streaming || self.broken {
            return;
        }
        let written = writeln!(self.out, "{event}").and_then(|_| self.out.flush());
        self.broken = written.is_err();
    }

    fn flush_text(&mut self) {
        if self.text.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.text);
        self.write(json!({"type": "assistant", "text": text}));
    }

    fn flush_subagent_text(&mut self, id: &str) {
        let Some(text) = self.subagent_text.remove(id) else {
            return;
        };
        self.write(json!({"type": "assistant", "text": text, "subagent": id}));
    }

    fn tool_use(&mut self, tool: &str, input: &Value, local: bool) {
        self.flush_text();
        self.write(json!({"type": "tool_use", "tool": tool, "input": input, "local": local}));
    }

    fn init(&mut self, session: &Session, model: Option<&str>, tools: &[&str]) {
        self.write(json!({
            "type": "init",
            "conversation_id": session.conversation_id(),
            "cwd": session.sandbox().cwd().display().to_string(),
            "roots": session
                .sandbox()
                .roots()
                .iter()
                .map(|root| root.display().to_string())
                .collect::<Vec<_>>(),
            "model": model,
            "tools": tools,
            "gz_version": env!("CARGO_PKG_VERSION"),
        }));
    }

    /// One event of the run, as a stream-json line (or none).
    fn event(&mut self, event: &AssistantEvent, subagent: Option<&str>) {
        let (subagent, text) = match event {
            AssistantEvent::Text(text) => (subagent, Some(text)),
            AssistantEvent::Subagent(SubagentEvent::Text { id, text }) => {
                (Some(id.as_str()), Some(text))
            }
            _ => (subagent, None),
        };
        if let Some(text) = text {
            match subagent {
                Some(id) => self
                    .subagent_text
                    .entry(id.to_string())
                    .or_default()
                    .push_str(text),
                None => self.text.push_str(text),
            }
            return;
        }
        let mut line = match event {
            AssistantEvent::ToolStarted {
                name, local: false, ..
            } => json!({"type": "tool_use", "tool": name, "input": Value::Null, "local": false}),
            AssistantEvent::ToolFinished {
                name,
                summary,
                is_error,
                output,
                ..
            } => json!({
                "type": "tool_result", "tool": name, "summary": summary,
                "is_error": is_error, "output": output,
            }),
            AssistantEvent::Notice(text) | AssistantEvent::Folders(Ok(text)) => {
                json!({"type": "notice", "text": text})
            }
            AssistantEvent::Folders(Err(text)) => json!({"type": "notice", "text": text}),
            AssistantEvent::Usage(usage) => {
                let tokens = usage.tokens.as_ref();
                json!({
                    "type": "usage",
                    "input_tokens": tokens.map(|t| t.input),
                    "output_tokens": tokens.map(|t| t.output),
                    "cache_read_input_tokens": tokens.map(|t| t.cache_read),
                    "cache_write_input_tokens": tokens.map(|t| t.cache_write),
                    "cost_usd": usage.cost_usd,
                })
            }
            AssistantEvent::Subagent(SubagentEvent::Started { id, name, .. }) => {
                json!({"type": "subagent", "id": id, "name": name, "status": "running"})
            }
            AssistantEvent::Subagent(SubagentEvent::Finished { id, status, .. }) => json!({
                "type": "subagent", "id": id,
                "status": crate::cmd::code_report::status_word(*status),
            }),
            AssistantEvent::ForSubagent { id, event } => return self.event(event, Some(id)),
            _ => return,
        };
        self.flush_text();
        if let AssistantEvent::Subagent(SubagentEvent::Finished { id, .. }) = event {
            self.flush_subagent_text(id);
        }
        if let Some(id) = subagent {
            self.flush_subagent_text(id);
            line["subagent"] = json!(id);
        }
        self.write(line);
    }

    /// Writes refusals the report recorded since the last call.
    fn denials(&mut self, calls: &[ToolCall]) {
        let refused: Vec<Value> = calls
            .iter()
            .filter(|call| call.is_refused())
            .skip(self.denials_sent)
            .map(denial)
            .collect();
        if refused.is_empty() {
            return;
        }
        self.flush_text();
        self.denials_sent += refused.len();
        for mut line in refused {
            line["type"] = json!("permission_denied");
            self.write(line);
        }
    }

    fn result(&mut self, result: &Value) {
        let ids: Vec<String> = self.subagent_text.keys().cloned().collect();
        for id in ids {
            self.flush_subagent_text(&id);
        }
        self.flush_text();
        self.write(result.clone());
    }
}

fn denial(call: &ToolCall) -> Value {
    json!({"tool": call.tool, "summary": call.summary, "reason": call.reason})
}

/// The `json` result object, and the last `stream-json` event.
pub fn result_object(summary: &Summary, stop: Option<(Stop, f64)>, turns: u32, code: i32) -> Value {
    let subtype = match (stop, summary.status) {
        (Some((stop, _)), _) => stop.subtype(),
        (None, crate::cmd::code_report::RunStatus::Completed) => "success",
        (None, crate::cmd::code_report::RunStatus::Interrupted) => "interrupted",
        (None, crate::cmd::code_report::RunStatus::Failed) => "error",
    };
    let usage = &summary.usage;
    json!({
        "type": "result",
        "subtype": subtype,
        "is_error": summary.status != crate::cmd::code_report::RunStatus::Completed,
        "result": summary.answer,
        "conversation_id": summary.conversation_id,
        "model": summary.model,
        "num_turns": turns,
        "duration_ms": summary.duration_ms,
        "cost_usd": usage.cost_usd,
        "usage": {
            "input_tokens": usage.input_tokens,
            "output_tokens": usage.output_tokens,
            "cache_read_input_tokens": usage.cache_read_input_tokens,
            "cache_write_input_tokens": usage.cache_write_input_tokens,
        },
        "permission_denials": summary
            .tool_calls
            .iter()
            .filter(|call| call.is_refused())
            .map(denial)
            .collect::<Vec<_>>(),
        "tool_calls": summary
            .tool_calls
            .iter()
            .map(|call| json!({
                "tool": call.tool, "summary": call.summary,
                "decision": call.decision, "is_error": call.is_error,
            }))
            .collect::<Vec<_>>(),
        "error": summary.error,
        "exit_code": code,
    })
}

/// One user message from a `--input-format stream-json` line:
/// `{"type":"user","message":{"role":"user","content":"…"}}`, where
/// `content` may also be a list of `{"type":"text","text":"…"}` blocks.
pub fn parse_input_line(line: &str) -> Result<String> {
    const SHAPE: &str = "each line must be {\"type\":\"user\",\"message\":{\"role\":\"user\",\
                         \"content\":\"…\"}}";
    let value: Value = serde_json::from_str(line).with_context(|| format!("not JSON; {SHAPE}"))?;
    if value["type"] != "user" {
        anyhow::bail!("unsupported message type {}; {SHAPE}", value["type"]);
    }
    let content = &value["message"]["content"];
    let text = match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => {
            let mut text = String::new();
            for block in blocks {
                match (block["type"].as_str(), block["text"].as_str()) {
                    (Some("text"), Some(part)) => {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(part);
                    }
                    _ => anyhow::bail!(
                        "only text content blocks are supported, got {block}; {SHAPE}"
                    ),
                }
            }
            text
        }
        _ => anyhow::bail!("message.content is missing; {SHAPE}"),
    };
    if text.trim().is_empty() {
        anyhow::bail!("the message is empty; {SHAPE}");
    }
    Ok(text)
}

/// What a scripted run needs from `gz code`'s setup.
pub struct Run<'a> {
    pub rt: &'a Runtime,
    pub args: &'a CodeArgs,
    pub catalog: Option<&'a Catalog>,
    /// The model the run goes out on, for the report.
    pub model: Option<String>,
    /// The prompt (argument, file or piped), unless messages are streamed.
    pub prompt: Option<String>,
    pub controls: RunControls,
    pub mid_line: Arc<AtomicBool>,
}

/// Where the next message comes from.
enum Messages {
    One(Option<String>),
    Lines(tokio::sync::mpsc::UnboundedReceiver<std::io::Result<String>>),
}

impl Messages {
    fn new(prompt: Option<String>, stream: bool) -> Self {
        if !stream {
            return Messages::One(prompt);
        }
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::stdin().lock().lines() {
                let failed = line.is_err();
                if tx.send(line).is_err() || failed {
                    break;
                }
            }
        });
        Messages::Lines(rx)
    }

    /// The next message, `Ok(None)` at the end, or why a line is unusable.
    async fn next(&mut self, line_no: &mut usize) -> Result<Option<String>> {
        match self {
            Messages::One(prompt) => Ok(prompt.take()),
            Messages::Lines(rx) => loop {
                let Some(line) = rx.recv().await else {
                    return Ok(None);
                };
                *line_no += 1;
                let line = line.context("cannot read standard input")?;
                if line.trim().is_empty() {
                    continue;
                }
                return parse_input_line(&line)
                    .map(Some)
                    .with_context(|| format!("standard input line {line_no}"));
            },
        }
    }
}

/// Runs the scripted session: every message, then the exit code.
pub async fn run(
    plan: Plan,
    ctx: Run<'_>,
    session: &mut Session,
    spend: &mut crate::cmd::code::PlainSpend,
) -> Result<()> {
    let streaming = plan.output == Output::StreamJson;
    let events = Arc::new(Mutex::new(Emitter::new(
        streaming,
        Box::new(std::io::stdout()),
    )));
    let guard = Arc::new(Guard {
        max_turns: ctx.args.max_turns,
        max_budget: ctx.args.max_budget_usd,
        state: Mutex::new(GuardState::default()),
        events: events.clone(),
    });
    session.set_run_controls(RunControls {
        guard: Some(guard.clone()),
        ..ctx.controls.clone()
    });
    let progress = match plan.output {
        Output::Report(_) => !ctx.rt.asked_quiet,
        _ => ctx.rt.http.verbosity() > 0,
    };
    let quiet = ctx.rt.asked_quiet;
    lock(&events).init(session, ctx.model.as_deref(), &session_tools(session));
    let mut messages = Messages::new(ctx.prompt.clone(), plan.input_stream);
    let mut line_no = 0;
    let mut code = exit::COMPLETED;
    loop {
        let decisions = DecisionLog::default();
        let mut report = Report::new(
            session.conversation_id(),
            ctx.model.clone(),
            decisions.clone(),
        );
        // Once a turn has listened for Ctrl+C the signal no longer ends
        // the process, so waiting for the next line listens for it too.
        let next = tokio::select! {
            next = messages.next(&mut line_no) => next,
            _ = tokio::signal::ctrl_c(), if plan.input_stream => {
                code = finish(&plan, report, &Ok(TurnEnd::Cancelled), None, 0, &events, quiet)?;
                break;
            }
        };
        let message = match next {
            Ok(Some(message)) => message,
            Ok(None) => break,
            Err(err) => {
                let ended = Err(err);
                code = finish(&plan, report, &ended, None, 0, &events, quiet)?;
                break;
            }
        };
        guard.new_message();
        let ended = async {
            let approver = crate::cmd::code::one_shot_approver(
                ctx.rt,
                ctx.args,
                session,
                ctx.catalog,
                &message,
                Some(decisions),
                &ctx.mid_line,
            )?;
            spend.check()?;
            turn(
                session,
                &message,
                approver.as_ref(),
                &mut report,
                Turn {
                    events: &events,
                    guard: &guard,
                    spend,
                    mid_line: &ctx.mid_line,
                    progress,
                },
            )
            .await
        }
        .await;
        let stopped = guard.stopped();
        code = finish(
            &plan,
            report,
            &ended,
            stopped,
            guard.turns(),
            &events,
            quiet,
        )?;
        let carries_on = code == exit::COMPLETED || code == exit::NEEDED_APPROVAL;
        if !carries_on {
            break;
        }
    }
    if code == exit::COMPLETED {
        return Ok(());
    }
    Err(ReportedExit(code).into())
}

/// Reports a run that could not start (bad prompt file, unknown model, …)
/// in the plan's format, then ends with its exit code.
/// `conversation_id` is `None` when it failed before a session was made.
pub fn fail_to_start(
    plan: &Plan,
    conversation_id: Option<&str>,
    model: Option<String>,
    err: anyhow::Error,
) -> Result<()> {
    let streaming = plan.output == Output::StreamJson;
    let events = Arc::new(Mutex::new(Emitter::new(
        streaming,
        Box::new(std::io::stdout()),
    )));
    let report = Report::new(
        conversation_id.unwrap_or_default(),
        model,
        DecisionLog::default(),
    );
    let code = finish(plan, report, &Err(err), None, 0, &events, false)?;
    Err(ReportedExit(code).into())
}

/// The client tools this session offers, for the `init` event.
fn session_tools(session: &Session) -> Vec<&'static str> {
    run_controls::SERVER_TOOL_NAMES
        .into_iter()
        .filter(|name| session.offers_tool(name))
        .collect()
}

/// What one turn writes to.
struct Turn<'a> {
    events: &'a Arc<Mutex<Emitter>>,
    guard: &'a Guard,
    spend: &'a mut crate::cmd::code::PlainSpend,
    mid_line: &'a Arc<AtomicBool>,
    /// Progress lines on stderr.
    progress: bool,
}

async fn turn(
    session: &mut Session,
    message: &str,
    approver: &dyn crate::assistant::Approver,
    report: &mut Report,
    out: Turn<'_>,
) -> Result<TurnEnd> {
    let prepared = session.prepare(message, &[])?;
    if out.progress {
        for note in &prepared.notes {
            eprintln!("  ⎿  {note}");
        }
    }
    let canceller = session.canceller();
    let mut subagents = Board::default();
    let Turn {
        events,
        guard,
        spend,
        mid_line,
        progress,
    } = out;
    let mut render = |event: AssistantEvent| {
        match &event {
            AssistantEvent::Usage(usage) => {
                let charged = spend.reported(usage, mid_line);
                if usage.cost_usd.is_some() {
                    guard.spent(charged);
                }
            }
            AssistantEvent::StepUsage(tokens) => spend.step(tokens),
            _ => {}
        }
        report.observe(&event);
        {
            let mut events = lock(events);
            events.denials(report.tool_calls());
            events.event(&event, None);
        }
        if progress && !is_answer_text(&event) {
            crate::cmd::code::render_event(event, mid_line, &mut subagents);
        }
    };
    let end = tokio::select! {
        end = session.send_prepared(&prepared, approver, &mut render) => end?,
        _ = tokio::signal::ctrl_c() => {
            canceller.cancel().await.ok();
            TurnEnd::Cancelled
        }
    };
    crate::cmd::code::end_line(mid_line);
    Ok(end)
}

/// The answer goes out once, at the end; a subagent's text is progress.
fn is_answer_text(event: &AssistantEvent) -> bool {
    matches!(event, AssistantEvent::Text(_))
}

/// Prints one message's outcome in the plan's format; returns its exit code.
fn finish(
    plan: &Plan,
    report: Report,
    ended: &Result<TurnEnd>,
    stopped: Option<(Stop, f64)>,
    turns: u32,
    events: &Arc<Mutex<Emitter>>,
    quiet: bool,
) -> Result<i32> {
    let summary = report.finish(ended);
    let code = stopped
        .map(|(stop, _)| stop.exit_code())
        .unwrap_or_else(|| summary.exit_code(ended));
    // Text output has no other place for the error, so -q keeps it.
    let shows_error = !quiet || plan.output == Output::Text;
    if let (true, Some(error)) = (shows_error, &summary.error) {
        crate::output::warn(error);
    }
    let printed = match plan.output {
        Output::StreamJson => {
            let mut events = lock(events);
            events.denials(&summary.tool_calls);
            events.result(&result_object(&summary, stopped, turns, code));
            return Ok(code);
        }
        Output::Json => {
            serde_json::to_string_pretty(&result_object(&summary, stopped, turns, code))?
        }
        Output::Report(format) => summary.render(format)?,
        Output::Text if summary.answer.is_empty() => return Ok(code),
        Output::Text => summary.answer.clone(),
    };
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{}", printed.trim_end()).context("cannot write the result")?;
    stdout.flush().context("cannot write the result")?;
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::code_report::RunStatus;

    fn args() -> CodeArgs {
        CodeArgs::default()
    }

    #[test]
    fn plans_pick_the_output_and_keep_the_old_report() {
        let table = OutputFormat::Table;
        assert_eq!(plan(&args(), table, false).unwrap(), None, "interactive");
        assert_eq!(plan(&args(), table, true).unwrap(), None, "plain one-shot");
        let print = CodeArgs {
            print: true,
            ..args()
        };
        assert_eq!(
            plan(&print, table, true).unwrap().unwrap().output,
            Output::Text
        );
        assert_eq!(
            plan(&args(), OutputFormat::Yaml, true)
                .unwrap()
                .unwrap()
                .output,
            Output::Report(OutputFormat::Yaml)
        );
        let streaming = CodeArgs {
            output_format: Some(OutputKind::StreamJson),
            input_format: Some(InputKind::StreamJson),
            ..args()
        };
        let planned = plan(&streaming, table, false).unwrap().unwrap();
        assert!(planned.input_stream && !planned.reads_piped_prompt());
        let limited = CodeArgs {
            max_turns: Some(3),
            ..args()
        };
        assert!(plan(&limited, table, true).unwrap().is_some());
        assert!(plan(&limited, table, false)
            .unwrap_err()
            .to_string()
            .contains("one-shot"));
    }

    #[test]
    fn conflicting_flags_say_what_to_do() {
        let both = CodeArgs {
            output_format: Some(OutputKind::Json),
            ..args()
        };
        let err = plan(&both, OutputFormat::Json, true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not both"), "{err}");
        let lone_input = CodeArgs {
            input_format: Some(InputKind::StreamJson),
            ..args()
        };
        assert!(plan(&lone_input, OutputFormat::Table, false).is_err());
        let resume = CodeArgs {
            print: true,
            resume: true,
            ..args()
        };
        assert!(plan(&resume, OutputFormat::Table, true)
            .unwrap_err()
            .to_string()
            .contains("-c <conversation id>"));
        let free = CodeArgs {
            print: true,
            max_budget_usd: Some(0.0),
            ..args()
        };
        assert!(plan(&free, OutputFormat::Table, true).is_err());
    }

    #[test]
    fn input_lines_take_text_or_text_blocks() {
        assert_eq!(
            parse_input_line(r#"{"type":"user","message":{"role":"user","content":"hi"}}"#)
                .unwrap(),
            "hi"
        );
        assert_eq!(
            parse_input_line(
                r#"{"type":"user","message":{"content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]}}"#
            )
            .unwrap(),
            "a\nb"
        );
        for bad in [
            "hi",
            r#"{"type":"assistant","message":{"content":"x"}}"#,
            r#"{"type":"user","message":{"content":[{"type":"image"}]}}"#,
            r#"{"type":"user","message":{"content":"  "}}"#,
        ] {
            assert!(parse_input_line(bad).is_err(), "{bad}");
        }
    }

    fn guard(max_turns: Option<u32>, max_budget: Option<f64>) -> Guard {
        Guard {
            max_turns,
            max_budget,
            state: Mutex::new(GuardState::default()),
            events: Arc::new(Mutex::new(Emitter::new(false, Box::new(std::io::sink())))),
        }
    }

    #[test]
    fn the_guard_stops_past_max_turns_and_resets_per_message() {
        let guard = guard(Some(2), None);
        assert_eq!(guard.before_leg(), None);
        assert_eq!(guard.before_leg(), None);
        let why = guard.before_leg().unwrap();
        assert!(why.contains("--max-turns 2"), "{why}");
        assert_eq!(guard.stopped().map(|(s, _)| s), Some(Stop::MaxTurns(2)));
        assert_eq!(guard.turns(), 2);
        guard.new_message();
        assert_eq!(guard.before_leg(), None);
        assert_eq!(guard.stopped(), None);
    }

    #[test]
    fn the_guard_stops_once_the_budget_is_spent() {
        let guard = guard(None, Some(0.01));
        assert_eq!(guard.before_leg(), None);
        guard.spent(0.004);
        assert_eq!(guard.before_leg(), None);
        guard.spent(0.006);
        let why = guard.before_leg().unwrap();
        assert!(why.contains("$0.0100") && why.contains("0.01"), "{why}");
        assert_eq!(
            guard.stopped().map(|(s, _)| s.exit_code()),
            Some(exit::MAX_BUDGET)
        );
    }

    /// A writer the test can read back.
    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn stream_events_group_text_and_carry_tool_inputs_and_results() {
        let sink = Shared::default();
        let mut emitter = Emitter::new(true, Box::new(sink.clone()));
        emitter.event(&AssistantEvent::Text("Let me ".into()), None);
        emitter.event(&AssistantEvent::Text("look.".into()), None);
        emitter.tool_use("read_file", &json!({"path": "a.txt"}), true);
        emitter.event(
            &AssistantEvent::ToolFinished {
                name: "read_file".into(),
                summary: "read_file a.txt".into(),
                is_error: false,
                detail: String::new(),
                output: "hi".into(),
                diff: None,
            },
            None,
        );
        emitter.event(
            &AssistantEvent::ForSubagent {
                id: "s1".into(),
                event: Box::new(AssistantEvent::Notice("x".into())),
            },
            None,
        );
        emitter.event(
            &AssistantEvent::ForSubagent {
                id: "s1".into(),
                event: Box::new(AssistantEvent::Text("Found ".into())),
            },
            None,
        );
        emitter.event(
            &AssistantEvent::Subagent(SubagentEvent::Text {
                id: "s1".into(),
                text: "it.".into(),
            }),
            None,
        );
        emitter.event(&AssistantEvent::Text("Done.".into()), None);
        emitter.result(&json!({"type": "result"}));
        let text = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
        let lines: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(
            lines,
            vec![
                json!({"type": "assistant", "text": "Let me look."}),
                json!({"type": "tool_use", "tool": "read_file", "input": {"path": "a.txt"}, "local": true}),
                json!({"type": "tool_result", "tool": "read_file", "summary": "read_file a.txt", "is_error": false, "output": "hi"}),
                json!({"type": "notice", "text": "x", "subagent": "s1"}),
                json!({"type": "assistant", "text": "Found it.", "subagent": "s1"}),
                json!({"type": "assistant", "text": "Done."}),
                json!({"type": "result"}),
            ]
        );
    }

    #[test]
    fn the_result_object_names_how_the_run_ended() {
        let report = Report::new("c1", Some("m".into()), DecisionLog::default());
        let summary = report.finish(&Ok(TurnEnd::Failed("stopped".into())));
        assert_eq!(summary.status, RunStatus::Failed);
        let value = result_object(&summary, Some((Stop::MaxTurns(3), 0.0)), 3, exit::MAX_TURNS);
        assert_eq!(value["subtype"], "error_max_turns");
        assert_eq!(value["is_error"], true);
        assert_eq!(value["num_turns"], 3);
        assert_eq!(value["exit_code"], exit::MAX_TURNS);
        assert_eq!(value["conversation_id"], "c1");
        assert_eq!(value["permission_denials"], json!([]));
    }

    #[test]
    fn controls_validate_the_flags() {
        let none_with_prompt = CodeArgs {
            tools: Some("none".into()),
            append_system_prompt: Some("Be brief.".into()),
            ..args()
        };
        assert!(controls(&none_with_prompt).is_err());
        let missing = CodeArgs {
            append_system_prompt_file: vec!["/nonexistent/gz-prompt.md".into()],
            ..args()
        };
        assert!(controls(&missing)
            .err()
            .unwrap()
            .to_string()
            .contains("--append-system-prompt-file"));
        let fine = CodeArgs {
            tools: Some("read_file".into()),
            append_system_prompt: Some("Be brief.".into()),
            bare: true,
            ..args()
        };
        let controls = controls(&fine).unwrap();
        assert!(controls.bare && controls.allows("read_file") && !controls.allows("run_command"));
    }
}
