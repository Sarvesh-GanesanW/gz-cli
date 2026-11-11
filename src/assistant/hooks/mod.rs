//! Hooks: the user's own commands that run before or after the
//! assistant's tool calls, and when a turn ends.
//!
//! User hooks come from `<gz config folder>/hooks.toml`. Project hooks come
//! from `<open folder>/.gz/hooks.toml`; that file is code from a repository,
//! so its hooks run only once the user trusted that exact content (see
//! [`trust`]). Hooks run as the user, unsandboxed, in the session's current
//! folder, with a timeout, and every run is audit-logged (never its output).
//!
//! `before_tool` runs after the permission decision, so a denied call runs
//! no hook. Its exit code 2 blocks the call and its stderr becomes the
//! result the model receives. `after_tool`'s exit code 2 appends its stderr
//! to the result. Any other non-zero exit only warns the user.

mod config;
mod runner;
pub mod trust;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::{json, Value};

use crate::assistant::audit::{AuditLog, Entry};
use crate::assistant::sandbox::Sandbox;
use crate::assistant::session::AssistantEvent;
use crate::assistant::tools::ToolOutput;
use crate::skills::files::sha256_hex;

pub use config::{Hook, HookEvent, HookId, Source};
use runner::{Ending, Ran, RunSpec};
use trust::{TrustStore, Verdict};

/// The project hooks file, from the open folder.
pub const PROJECT_FILE: &str = ".gz/hooks.toml";

/// The exit code that blocks a call (`before_tool`) or sends the hook's
/// stderr to the model (`after_tool`).
const EXIT_TO_MODEL: i32 = 2;
/// The tool output a hook gets on stdin, at most.
const RESULT_CHARS: usize = 20_000;
/// What an `after_tool` hook may add to the model's result, at most.
const FEEDBACK_CHARS: usize = 8_000;
/// Hook output shown to the user, at most.
const SHOWN_LINES: usize = 12;
const SHOWN_CHARS: usize = 1_500;
/// A command longer than this is shortened in notices.
const LABEL_CHARS: usize = 40;

type Events<'e> = &'e mut (dyn FnMut(AssistantEvent) + Send);

/// Where hooks files and the trust store are.
pub struct Places {
    pub user_file: Option<PathBuf>,
    pub project_file: PathBuf,
    pub trust: Option<TrustStore>,
}

impl Places {
    /// The user's file in the gz config folder and `root`'s project file.
    pub fn for_folder(root: &Path) -> Self {
        let config_dir = crate::config::config_path()
            .ok()
            .and_then(|path| path.parent().map(Path::to_path_buf));
        Self {
            user_file: config_dir.map(|dir| dir.join("hooks.toml")),
            project_file: root.join(PROJECT_FILE),
            trust: TrustStore::open().ok(),
        }
    }
}

/// Whether a project file's hooks run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectState {
    Trusted,
    /// The user said no to this version.
    Declined,
    /// Not asked about yet (or the answer is pending).
    Untrusted,
}

struct Project {
    hooks: Vec<Hook>,
    state: ProjectState,
}

/// A project file's hooks as read, for the user to trust or not.
#[derive(Debug, Clone)]
pub struct TrustRequest {
    pub path: PathBuf,
    sha256: String,
    hooks: Vec<Hook>,
}

impl TrustRequest {
    /// Every hook with its command in full.
    pub fn lines(&self) -> Vec<String> {
        self.hooks.iter().flat_map(Hook::describe).collect()
    }
}

/// One hook for `/hooks`.
#[derive(Debug, Clone)]
pub struct HookRow {
    pub source: Source,
    pub event: HookEvent,
    /// False for a project hook that isn't trusted.
    pub runs: bool,
    /// What it runs for, the command and its timeout.
    pub action: String,
    /// How its last run ended this session.
    pub last: Option<String>,
}

/// Everything `/hooks` shows.
#[derive(Debug, Clone)]
pub struct Status {
    pub enabled: bool,
    pub rows: Vec<HookRow>,
    pub user_file: Option<PathBuf>,
    pub project_file: Option<PathBuf>,
    pub project: Option<ProjectState>,
    pub problems: Vec<String>,
}

/// A tool call as hooks see it.
pub struct ToolCall<'a> {
    pub tool: &'a str,
    pub summary: &'a str,
    pub input: &'a Value,
    pub cwd: PathBuf,
    /// The file or folder the call targets, when it has one.
    pub path: Option<PathBuf>,
}

impl<'a> ToolCall<'a> {
    pub fn new(sandbox: &Sandbox, tool: &'a str, summary: &'a str, input: &'a Value) -> Self {
        Self {
            tool,
            summary,
            input,
            cwd: sandbox.cwd(),
            path: input["path"].as_str().and_then(|p| sandbox.resolve(p).ok()),
        }
    }

    fn payload(&self, event: HookEvent, result: Option<(bool, &str)>) -> Value {
        let mut payload = json!({
            "event": event.as_str(),
            "tool": self.tool,
            "summary": self.summary,
            "input": self.input,
            "cwd": self.cwd,
        });
        if let Some((is_error, output)) = result {
            payload["result"] = json!({"isError": is_error, "output": cap(output, RESULT_CHARS)});
        }
        payload
    }

    fn env(&self, event: HookEvent) -> Vec<(&'static str, String)> {
        let mut env = vec![
            ("GZ_HOOK_EVENT", event.as_str().to_string()),
            ("GZ_HOOK_TOOL", self.tool.to_string()),
        ];
        if let Some(path) = &self.path {
            env.push(("GZ_HOOK_PATH", path.to_string_lossy().into_owned()));
        }
        env
    }
}

/// Where hook runs are recorded.
pub struct Log<'a> {
    pub audit: Option<&'a AuditLog>,
    pub conversation_id: &'a str,
    pub run_id: &'a str,
}

struct Inner {
    places: Option<Places>,
    user: Vec<Hook>,
    project: Option<Project>,
    /// `/hooks off` turns every hook off for the session.
    enabled: bool,
    last: HashMap<HookId, String>,
    problems: Vec<String>,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            places: None,
            user: Vec::new(),
            project: None,
            enabled: true,
            last: HashMap::new(),
            problems: Vec::new(),
        }
    }
}

/// The session's hooks, shared (cheap to clone) by the main session, its
/// background subagents and the UI. The default has none.
#[derive(Clone, Default)]
pub struct Hooks {
    inner: Arc<Mutex<Inner>>,
}

impl Hooks {
    /// Reads both files. A missing file has no hooks; a broken one is
    /// skipped and reported in [`Status::problems`], so gz still starts.
    pub fn load(places: Places) -> Self {
        let mut inner = Inner::default();
        if let Some(file) = &places.user_file {
            match read_file(file, Source::User) {
                Ok(Some((_, hooks))) => inner.user = hooks,
                Ok(None) => {}
                Err(why) => inner.problems.push(format!("{why}; fix it and restart gz")),
            }
        }
        match read_project(&places) {
            Ok(project) => inner.project = project,
            Err(why) => inner
                .problems
                .push(format!("{why}; fix it, then run /hooks trust")),
        }
        inner.places = Some(places);
        Self {
            inner: Arc::new(Mutex::new(inner)),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A panic elsewhere while holding the lock leaves plain data behind,
        // still fine to read.
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn set_enabled(&self, on: bool) {
        self.lock().enabled = on;
    }

    pub fn status(&self) -> Status {
        let inner = self.lock();
        let user = inner.user.iter().map(|hook| (hook, true));
        let project = inner.project.iter().flat_map(|project| {
            let runs = project.state == ProjectState::Trusted;
            project.hooks.iter().map(move |hook| (hook, runs))
        });
        let rows = user
            .chain(project)
            .map(|(hook, runs)| HookRow {
                source: hook.id.source,
                event: hook.id.event,
                runs,
                action: hook.action(),
                last: inner.last.get(&hook.id).cloned(),
            })
            .collect();
        Status {
            enabled: inner.enabled,
            rows,
            user_file: inner.places.as_ref().and_then(|p| p.user_file.clone()),
            project_file: inner.places.as_ref().map(|p| p.project_file.clone()),
            project: inner.project.as_ref().map(|p| p.state),
            problems: inner.problems.clone(),
        }
    }

    /// `/hooks trust`: reads the project file afresh, whatever was said
    /// about it before, for the user to review.
    pub fn review_project(&self) -> Result<TrustRequest, String> {
        let mut inner = self.lock();
        let places = inner
            .places
            .as_ref()
            .ok_or("hooks were not loaded for this session")?;
        let (sha256, hooks) =
            read_file(&places.project_file, Source::Project)?.ok_or_else(|| {
                format!(
                    "there is no project hooks file at {}",
                    places.project_file.display()
                )
            })?;
        let request = TrustRequest {
            path: places.project_file.clone(),
            sha256,
            hooks,
        };
        inner
            .problems
            .retain(|p| !p.contains(&*request.path.to_string_lossy()));
        Ok(request)
    }

    /// Applies the user's answer: remembers it for this version and turns
    /// the project hooks on (yes) or off (no). Fails, changing nothing, when
    /// the file changed since it was shown or the answer can't be saved.
    pub fn answer_trust(&self, request: TrustRequest, trusted: bool) -> Result<String, String> {
        let mut inner = self.lock();
        let store = inner.places.as_ref().and_then(|p| p.trust.clone()).ok_or(
            "cannot find the gz config folder to remember your answer; project hooks stay off",
        )?;
        let now = std::fs::read(&request.path)
            .map_err(|err| format!("cannot read {}: {err}", request.path.display()))?;
        if sha256_hex(&now) != request.sha256 {
            return Err(format!(
                "{} changed while you were asked; run /hooks trust to review it again",
                request.path.display()
            ));
        }
        store
            .record(&request.path, &request.sha256, trusted)
            .map_err(|err| format!("{err:#}; project hooks stay off"))?;
        let state = if trusted {
            ProjectState::Trusted
        } else {
            ProjectState::Declined
        };
        let count = request.hooks.len();
        inner.project = Some(Project {
            hooks: request.hooks,
            state,
        });
        Ok(if trusted {
            format!("Trusted {count} project hook(s) in {PROJECT_FILE}; a changed file asks again")
        } else {
            format!("Project hooks in {PROJECT_FILE} stay off; /hooks trust to review them again")
        })
    }

    /// The hooks that run for `event` (and `call`), in order: the user's,
    /// then trusted project hooks.
    fn matching(&self, event: HookEvent, call: Option<&ToolCall>) -> Vec<Hook> {
        let inner = self.lock();
        if !inner.enabled {
            return Vec::new();
        }
        let project = inner
            .project
            .iter()
            .filter(|p| p.state == ProjectState::Trusted)
            .flat_map(|p| p.hooks.iter());
        inner
            .user
            .iter()
            .chain(project)
            .filter(|hook| hook.id.event == event)
            .filter(|hook| call.is_none_or(|c| hook.matches(c.tool, c.path.as_deref(), &c.cwd)))
            .cloned()
            .collect()
    }

    /// Runs the matching `before_tool` hooks; `Some` is the result the
    /// model gets instead of running the call, when one blocked it.
    pub async fn before_tool(
        &self,
        call: &ToolCall<'_>,
        log: &Log<'_>,
        on_event: Events<'_>,
    ) -> Option<ToolOutput> {
        let event = HookEvent::BeforeTool;
        for hook in self.matching(event, Some(call)) {
            let ran = self
                .run(&hook, call.payload(event, None), &call.cwd, call.env(event))
                .await;
            self.settle(&hook, &ran, log, call.summary);
            if ran.ending == Ending::Exited(EXIT_TO_MODEL) {
                let reason = said(&ran).unwrap_or_else(|| "no reason given".to_string());
                on_event(AssistantEvent::Notice(format!(
                    "✋ {} blocked {}: {}",
                    label(&hook),
                    call.summary,
                    shown(&reason)
                )));
                return Some(ToolOutput::error(format!(
                    "The user's before_tool hook blocked this call: {}",
                    cap(&reason, FEEDBACK_CHARS)
                )));
            }
            report(&hook, &ran, on_event);
        }
        None
    }

    /// Runs the matching `after_tool` hooks and adds what they sent back
    /// (exit code 2) to `out`.
    pub async fn after_tool(
        &self,
        call: &ToolCall<'_>,
        out: ToolOutput,
        log: &Log<'_>,
        on_event: Events<'_>,
    ) -> ToolOutput {
        let feedback = self
            .after_tool_feedback(call, (out.is_error, &out.output), log, on_event)
            .await;
        with_feedback(out, &feedback)
    }

    /// Runs the matching `after_tool` hooks for a call that ended with
    /// `result` (is it an error, its output); returns what they sent back
    /// for the model (exit code 2).
    pub async fn after_tool_feedback(
        &self,
        call: &ToolCall<'_>,
        result: (bool, &str),
        log: &Log<'_>,
        on_event: Events<'_>,
    ) -> Vec<String> {
        let event = HookEvent::AfterTool;
        let mut feedback = Vec::new();
        for hook in self.matching(event, Some(call)) {
            let payload = call.payload(event, Some(result));
            let ran = self.run(&hook, payload, &call.cwd, call.env(event)).await;
            self.settle(&hook, &ran, log, call.summary);
            if ran.ending == Ending::Exited(EXIT_TO_MODEL) {
                if let Some(text) = said(&ran) {
                    on_event(AssistantEvent::Notice(format!(
                        "{} sent this to the model after {}:\n{}",
                        label(&hook),
                        call.summary,
                        shown(&text)
                    )));
                    feedback.push(cap(&text, FEEDBACK_CHARS));
                }
                continue;
            }
            report(&hook, &ran, on_event);
        }
        feedback
    }

    /// Runs the `turn_end` hooks in `cwd`, showing their output.
    pub async fn turn_end(&self, cwd: &Path, log: &Log<'_>, on_event: Events<'_>) {
        let event = HookEvent::TurnEnd;
        for hook in self.matching(event, None) {
            let payload = json!({"event": event.as_str(), "cwd": cwd});
            let env = vec![("GZ_HOOK_EVENT", event.as_str().to_string())];
            let ran = self.run(&hook, payload, cwd, env).await;
            self.settle(&hook, &ran, log, "");
            report(&hook, &ran, on_event);
        }
    }

    async fn run(
        &self,
        hook: &Hook,
        payload: Value,
        cwd: &Path,
        env: Vec<(&'static str, String)>,
    ) -> Ran {
        runner::run(RunSpec {
            command: &hook.command,
            cwd,
            stdin: payload.to_string().into_bytes(),
            env,
            timeout: hook.timeout,
        })
        .await
    }

    /// Remembers how `hook` ended for `/hooks` and logs the run (the
    /// outcome only, never output).
    fn settle(&self, hook: &Hook, ran: &Ran, log: &Log<'_>, target: &str) {
        let outcome = outcome(hook.id.event, &ran.ending);
        self.lock().last.insert(hook.id, outcome.clone());
        let Some(audit) = log.audit else {
            return;
        };
        let reason = format!(
            "{} {} hook {}: {outcome}",
            hook.id.source.as_str(),
            hook.id.event.as_str(),
            hook.id.number
        );
        let entry = Entry {
            conversation_id: log.conversation_id,
            run_id: log.run_id,
            tool: &format!("hook/{}", hook.id.event.as_str()),
            target,
            decision: "hook",
            reason: Some(&reason),
            is_error: ran.ending != Ending::Exited(0),
        };
        if let Err(err) = audit.record(&entry) {
            crate::output::warn(&format!("cannot write the audit log: {err:#}"));
        }
    }
}

/// Reads and parses one hooks file: `None` when there is none, else its
/// content hash and hooks. Errors name the file and what is wrong.
fn read_file(path: &Path, source: Source) -> Result<Option<(String, Vec<Hook>)>, String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("cannot read {}: {err}", path.display())),
    };
    let text =
        std::str::from_utf8(&bytes).map_err(|_| format!("{} is not UTF-8 text", path.display()))?;
    let hooks = config::parse(text, source)
        .map_err(|err| format!("{} has a problem: {err:#}", path.display()))?;
    Ok(Some((sha256_hex(&bytes), hooks)))
}

fn read_project(places: &Places) -> Result<Option<Project>, String> {
    let Some((sha256, hooks)) = read_file(&places.project_file, Source::Project)? else {
        return Ok(None);
    };
    let verdict = match &places.trust {
        Some(store) => store
            .verdict(&places.project_file, &sha256)
            .map_err(|err| format!("{err:#}"))?,
        None => Verdict::Unknown,
    };
    let state = match verdict {
        Verdict::Trusted => ProjectState::Trusted,
        Verdict::Declined => ProjectState::Declined,
        Verdict::Unknown => ProjectState::Untrusted,
    };
    Ok(Some(Project { hooks, state }))
}

/// How a run ended, in a few words, for `/hooks` and the audit log.
fn outcome(event: HookEvent, ending: &Ending) -> String {
    match (event, ending) {
        (HookEvent::BeforeTool, Ending::Exited(EXIT_TO_MODEL)) => "exit 2, blocked the call".into(),
        (HookEvent::AfterTool, Ending::Exited(EXIT_TO_MODEL)) => "exit 2, sent to the model".into(),
        (_, Ending::Exited(code)) => format!("exit {code}"),
        (_, Ending::Signalled) => "killed by a signal".into(),
        (_, Ending::TimedOut(limit)) => format!("stopped after {}s", limit.as_secs()),
        (_, Ending::Failed(why)) => why.clone(),
    }
}

/// Shows the user what a run printed (exit 0) or warns how it failed.
fn report(hook: &Hook, ran: &Ran, on_event: Events<'_>) {
    let printed = said_all(ran);
    let text = match &ran.ending {
        Ending::Exited(0) if printed.is_empty() => return,
        Ending::Exited(0) => format!("{}:\n{}", label(hook), shown(&printed)),
        Ending::Exited(code) => format!("⚠ {} exited with {code}; carrying on", label(hook)),
        Ending::Signalled => format!("⚠ {} was killed by a signal; carrying on", label(hook)),
        Ending::TimedOut(limit) => format!(
            "⚠ {} ran past its {}s timeout and was stopped; carrying on",
            label(hook),
            limit.as_secs()
        ),
        Ending::Failed(why) => format!("⚠ {}: {why}; carrying on", label(hook)),
    };
    let text = match (&ran.ending, printed.is_empty()) {
        (Ending::Exited(0), _) | (_, true) => text,
        _ => format!("{text}\n{}", shown(&printed)),
    };
    on_event(AssistantEvent::Notice(text));
}

/// `user after_tool hook `cargo fmt``.
fn label(hook: &Hook) -> String {
    let command = hook.command.trim();
    let short = if command.chars().count() > LABEL_CHARS {
        let cut: String = command.chars().take(LABEL_CHARS - 1).collect();
        format!("{cut}…")
    } else {
        command.to_string()
    };
    format!(
        "{} {} hook `{short}`",
        hook.id.source.as_str(),
        hook.id.event.as_str()
    )
}

/// What a hook said for the model: stderr, else stdout.
fn said(ran: &Ran) -> Option<String> {
    [&ran.stderr, &ran.stdout]
        .into_iter()
        .map(|text| text.trim())
        .find(|text| !text.is_empty())
        .map(str::to_string)
}

/// Everything a hook printed, stdout first.
fn said_all(ran: &Ran) -> String {
    [ran.stdout.trim(), ran.stderr.trim()]
        .into_iter()
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Output trimmed for the user's screen.
fn shown(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut kept = cap(
        &lines[..lines.len().min(SHOWN_LINES)].join("\n"),
        SHOWN_CHARS,
    );
    if lines.len() > SHOWN_LINES {
        kept.push_str(&format!("\n… {} more lines", lines.len() - SHOWN_LINES));
    }
    kept
}

fn cap(text: &str, chars: usize) -> String {
    if text.chars().count() <= chars {
        return text.to_string();
    }
    let kept: String = text.chars().take(chars).collect();
    format!("{kept}\n… cut by gz")
}

/// `out` with each `after_tool` hook's message for the model after it.
pub fn with_feedback(mut out: ToolOutput, feedback: &[String]) -> ToolOutput {
    if feedback.is_empty() {
        return out;
    }
    let notes: Vec<String> = feedback
        .iter()
        .map(|text| format!("[after_tool hook]\n{text}"))
        .collect();
    out.output = format!("{}\n\n{}", out.output, notes.join("\n\n"));
    out
}

#[cfg(test)]
pub(crate) mod test_support;

#[cfg(all(test, unix))]
mod tests;
