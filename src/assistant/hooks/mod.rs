//! Hooks: the user's own commands that run at points in a session: when
//! it starts and ends, when a message is sent, around each tool call, when
//! the agent or a subagent stops, before compaction and when gz needs the
//! user (see [`HookEvent`]).
//!
//! User hooks come from `<gz config folder>/hooks.toml`. Project hooks come
//! from `<open folder>/.gz/hooks.toml`; that file is code from a repository,
//! so its hooks run only once the user trusted that exact content (see
//! [`trust`]). Hooks run as the user, unsandboxed, in the session's current
//! folder, with a timeout, and every run is audit-logged (never its output).
//!
//! Every hook reads a JSON description of the event on stdin. Exit 0 is
//! fine, exit 2 blocks (stderr says why), any other exit only warns the
//! user; or the hook prints a JSON answer (see [`reply`]). Matching hooks
//! run at the same time. `pre_tool_use` runs before the permission check
//! and may allow, deny or ask; `before_tool` runs after it, so a denied
//! call runs no `before_tool` hook.

mod config;
mod events;
mod reply;
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
use events::Lifecycle;
pub use events::{PreTool, PromptCheck, StartSource, SubagentEnd};
use runner::{Ending, Ran, RunSpec};
use trust::{TrustStore, Verdict};

/// The project hooks file, from the open folder.
pub const PROJECT_FILE: &str = ".gz/hooks.toml";

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

/// What one session's hooks have said and seen: each session (the main
/// one, every background subagent) has its own, so a stop or a context
/// reading in one never reaches another.
#[derive(Default)]
struct SessionState {
    /// A hook's `"continue": false`, until the turn stops for it.
    halt: Option<String>,
    life: Lifecycle,
    /// The last run ended past the auto-compact threshold, so the chat
    /// compacts (or just did) around now.
    is_compact_due: bool,
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

/// The session's hooks, shared (cheap to clone) by the main session and
/// the UI; background subagents get theirs from [`Hooks::for_subagent`].
/// The default has none.
#[derive(Clone, Default)]
pub struct Hooks {
    inner: Arc<Mutex<Inner>>,
    session: Arc<Mutex<SessionState>>,
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
            session: Arc::default(),
        }
    }

    /// The same hooks (files, trust, `/hooks` status) for a background
    /// subagent's session, with its own stop and context state.
    pub fn for_subagent(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            session: Arc::default(),
        }
    }

    fn session_state(&self) -> MutexGuard<'_, SessionState> {
        // As with `lock`: plain data, still fine to read after a panic.
        self.session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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
        let mut rows: Vec<HookRow> = user
            .chain(project)
            .map(|(hook, runs)| HookRow {
                source: hook.id.source,
                event: hook.id.event,
                runs,
                action: hook.action(),
                last: inner.last.get(&hook.id).cloned(),
            })
            .collect();
        rows.sort_by_key(|row| HookEvent::ALL.iter().position(|e| *e == row.event));
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

    /// The hooks that run for any of `events` (about `subject`, or `call`),
    /// in order: the user's, then trusted project hooks. The same command
    /// for the same event runs once, however many entries name it.
    fn matching(
        &self,
        events: &[HookEvent],
        subject: Option<&str>,
        call: Option<&ToolCall>,
    ) -> Vec<Hook> {
        let inner = self.lock();
        if !inner.enabled {
            return Vec::new();
        }
        let project = inner
            .project
            .iter()
            .filter(|p| p.state == ProjectState::Trusted)
            .flat_map(|p| p.hooks.iter());
        let mut seen = std::collections::HashSet::new();
        inner
            .user
            .iter()
            .chain(project)
            .filter(|hook| events.contains(&hook.id.event))
            .filter(|hook| match call {
                Some(c) => hook.matches(c.tool, c.path.as_deref(), &c.cwd),
                None => subject.is_none_or(|s| hook.matches_subject(s)),
            })
            .filter(|hook| seen.insert((hook.id.event, hook.command.trim().to_string())))
            .cloned()
            .collect()
    }

    /// Runs every hook `batch` matches and waits for them all; each run is
    /// recorded for `/hooks` and the audit log. Hooks of the newer events
    /// run at once. `before_tool`, `after_tool` and `turn_end` hooks keep
    /// running one by one in file order (user's, then project's), since
    /// existing chains may rely on it (a formatter before a linter), and
    /// the `before_tool` chain stops at the first block. Hooks a chain
    /// never reached are left out.
    async fn fire(&self, batch: &Batch<'_>, log: &Log<'_>) -> Vec<Fired> {
        let hooks = self.matching(batch.events, batch.subject, batch.call);
        let spec = |hook: &Hook| RunSpec {
            command: hook.command.clone(),
            cwd: batch.cwd.to_path_buf(),
            stdin: batch.payload(hook.id.event, log).to_string().into_bytes(),
            env: batch.env(hook.id.event, log),
            timeout: hook.timeout,
        };
        let (in_order, at_once): (Vec<_>, Vec<_>) =
            (0..hooks.len()).partition(|&index| hooks[index].id.event.runs_in_order());
        // Dropping the set (a cancelled turn) aborts the runs, and with them
        // the hook processes (kill on drop).
        let mut runs = tokio::task::JoinSet::new();
        for index in at_once {
            let spec = spec(&hooks[index]);
            runs.spawn(async move { vec![(index, Some(runner::run(spec).await))] });
        }
        let chain: Vec<(usize, RunSpec, bool)> = in_order
            .into_iter()
            .map(|index| {
                let hook = &hooks[index];
                let stops = hook.id.event == HookEvent::BeforeTool;
                (index, spec(hook), stops)
            })
            .collect();
        runs.spawn(async move {
            // `None`: not run, because an earlier hook blocked the call.
            let mut chain_runs = Vec::new();
            let mut blocked = false;
            for (index, spec, stops_at_block) in chain {
                if blocked {
                    chain_runs.push((index, None));
                    continue;
                }
                let ran = runner::run(spec).await;
                blocked = stops_at_block
                    && reply::read(&ran)
                        .decision
                        .is_some_and(reply::Decision::is_refusal);
                chain_runs.push((index, Some(ran)));
            }
            chain_runs
        });
        // Outer `None`: the task running it stopped unexpectedly.
        let mut endings: Vec<Option<Option<Ran>>> = vec![None; hooks.len()];
        while let Some(joined) = runs.join_next().await {
            let Ok(finished) = joined else {
                continue;
            };
            for (index, ran) in finished {
                endings[index] = Some(ran);
            }
        }
        hooks
            .into_iter()
            .zip(endings)
            .filter_map(|(hook, ending)| {
                let ran = ending.unwrap_or_else(|| {
                    Some(Ran {
                        ending: Ending::Failed("the hook's task stopped unexpectedly".into()),
                        stdout: String::new(),
                        stderr: String::new(),
                    })
                })?;
                let reply = reply::read(&ran);
                self.settle(&hook, &ran, &reply, log, batch.target);
                Some(Fired { hook, ran, reply })
            })
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
        let fired = self
            .fire(&Batch::tool(&[HookEvent::BeforeTool], call, None), log)
            .await;
        let mut blocked = None;
        for one in &fired {
            let refused = one.reply.decision.is_some_and(reply::Decision::is_refusal);
            if !refused {
                report(one, on_event);
                continue;
            }
            let reason = reason_of(one);
            on_event(AssistantEvent::Notice(format!(
                "✋ {} blocked {}: {}",
                label(&one.hook),
                call.summary,
                shown(&reason)
            )));
            blocked.get_or_insert(reason);
        }
        self.halt_from(&fired, on_event);
        blocked.map(|reason| {
            ToolOutput::error(format!(
                "The user's before_tool hook blocked this call: {}",
                cap(&reason, FEEDBACK_CHARS)
            ))
        })
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

    /// Runs the matching `after_tool` hooks, and `post_tool_use` (or
    /// `post_tool_use_failure`) hooks, for a call that ended with `result`
    /// (is it an error, its output); returns what they sent back for the
    /// model (exit code 2, a block decision or `additionalContext`).
    pub async fn after_tool_feedback(
        &self,
        call: &ToolCall<'_>,
        result: (bool, &str),
        log: &Log<'_>,
        on_event: Events<'_>,
    ) -> Vec<String> {
        let events = [HookEvent::AfterTool, post_event(result.0)];
        self.tool_feedback(&events, call, result, log, on_event)
            .await
    }

    /// Runs only the `post_tool_use` (or `post_tool_use_failure`) hooks,
    /// for the tools `before_tool` and `after_tool` have never covered
    /// (folders, memory, skills, the session's own tools).
    pub async fn post_tool_feedback(
        &self,
        call: &ToolCall<'_>,
        result: (bool, &str),
        log: &Log<'_>,
        on_event: Events<'_>,
    ) -> Vec<String> {
        self.tool_feedback(&[post_event(result.0)], call, result, log, on_event)
            .await
    }

    async fn tool_feedback(
        &self,
        events: &[HookEvent],
        call: &ToolCall<'_>,
        result: (bool, &str),
        log: &Log<'_>,
        on_event: Events<'_>,
    ) -> Vec<String> {
        let fired = self
            .fire(&Batch::tool(events, call, Some(result)), log)
            .await;
        let mut feedback = Vec::new();
        for one in &fired {
            let text = match one.reply.decision {
                Some(decision) if decision.is_refusal() => Some(reason_of(one)),
                _ if one.reply.problem.is_none() && is_json(&one.ran) => one.reply.context.clone(),
                _ => None,
            };
            let Some(text) = text else {
                report(one, on_event);
                continue;
            };
            on_event(AssistantEvent::Notice(format!(
                "{} sent this to the model after {}:\n{}",
                label(&one.hook),
                call.summary,
                shown(&text)
            )));
            feedback.push(cap(&text, FEEDBACK_CHARS));
        }
        self.halt_from(&fired, on_event);
        feedback
    }

    /// Runs the `turn_end` hooks in `cwd`, showing their output.
    pub async fn turn_end(&self, cwd: &Path, log: &Log<'_>, on_event: Events<'_>) {
        let batch = Batch::plain(&[HookEvent::TurnEnd], None, cwd, json!({}), "");
        for one in self.fire(&batch, log).await {
            report(&one, on_event);
        }
    }

    /// Remembers that a hook said `"continue": false`, for the turn to stop
    /// at its next step.
    fn halt_from(&self, fired: &[Fired], on_event: Events<'_>) {
        let Some((hook, why)) = fired
            .iter()
            .find_map(|one| one.reply.halt.as_ref().map(|why| (&one.hook, why)))
        else {
            return;
        };
        on_event(AssistantEvent::Notice(format!(
            "⏹ {} stopped the turn: {}",
            label(hook),
            shown(why)
        )));
        self.session_state().halt.get_or_insert(why.clone());
    }

    /// The stop a hook asked for (`"continue": false`), left in place.
    pub fn halted(&self) -> Option<String> {
        self.session_state().halt.clone()
    }

    /// Takes a stop a hook asked for (`"continue": false`), if any.
    pub fn take_halt(&self) -> Option<String> {
        self.session_state().halt.take()
    }

    /// Remembers how `hook` ended for `/hooks` and logs the run (the
    /// outcome only, never output).
    fn settle(&self, hook: &Hook, ran: &Ran, answer: &reply::Reply, log: &Log<'_>, target: &str) {
        let outcome = outcome(hook.id.event, &ran.ending, answer);
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

/// One hook that ran, with its answer.
struct Fired {
    hook: Hook,
    ran: Ran,
    reply: reply::Reply,
}

/// What a set of hooks runs for: the events, what they are about, and
/// the fields their JSON input carries.
struct Batch<'a> {
    events: &'a [HookEvent],
    /// What a `matcher` is matched against (non-tool events).
    subject: Option<&'a str>,
    call: Option<&'a ToolCall<'a>>,
    /// A tool call's result: is it an error, its output.
    result: Option<(bool, &'a str)>,
    cwd: &'a Path,
    /// Event-specific fields, merged into the input.
    fields: Value,
    /// The audit log's target.
    target: &'a str,
}

impl<'a> Batch<'a> {
    fn tool(
        events: &'a [HookEvent],
        call: &'a ToolCall<'a>,
        result: Option<(bool, &'a str)>,
    ) -> Self {
        Self {
            events,
            subject: Some(call.tool),
            call: Some(call),
            result,
            cwd: &call.cwd,
            fields: json!({}),
            target: call.summary,
        }
    }

    fn plain(
        events: &'a [HookEvent],
        subject: Option<&'a str>,
        cwd: &'a Path,
        fields: Value,
        target: &'a str,
    ) -> Self {
        Self {
            events,
            subject,
            call: None,
            result: None,
            cwd,
            fields,
            target,
        }
    }

    /// The JSON a hook for `event` reads on stdin.
    fn payload(&self, event: HookEvent, log: &Log<'_>) -> Value {
        let mut payload = match self.call {
            Some(call) => call.payload(event, self.result),
            None => json!({"event": event.as_str(), "cwd": self.cwd}),
        };
        payload["conversationId"] = json!(log.conversation_id);
        if let (Some(fields), Some(object)) = (self.fields.as_object(), payload.as_object_mut()) {
            object.extend(fields.clone());
        }
        payload
    }

    fn env(&self, event: HookEvent, log: &Log<'_>) -> Vec<(&'static str, String)> {
        let mut env = match self.call {
            Some(call) => call.env(event),
            None => vec![("GZ_HOOK_EVENT", event.as_str().to_string())],
        };
        env.push(("GZ_HOOK_CONVERSATION", log.conversation_id.to_string()));
        env
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
fn outcome(event: HookEvent, ending: &Ending, answer: &reply::Reply) -> String {
    if let (Ending::Exited(0), Some(decision)) = (ending, answer.decision) {
        return format!("answered {}", format!("{decision:?}").to_lowercase());
    }
    match (event, ending) {
        (_, Ending::Exited(0)) if answer.problem.is_some() => {
            answer.problem.clone().unwrap_or_default()
        }
        (HookEvent::BeforeTool | HookEvent::PreToolUse, Ending::Exited(reply::EXIT_BLOCK)) => {
            "exit 2, blocked the call".into()
        }
        (
            HookEvent::AfterTool | HookEvent::PostToolUse | HookEvent::PostToolUseFailure,
            Ending::Exited(reply::EXIT_BLOCK),
        ) => "exit 2, sent to the model".into(),
        (_, Ending::Exited(reply::EXIT_BLOCK)) => "exit 2, blocked".into(),
        (_, Ending::Exited(code)) => format!("exit {code}"),
        (_, Ending::Signalled) => "killed by a signal".into(),
        (_, Ending::TimedOut(limit)) => format!("stopped after {}s", limit.as_secs()),
        (_, Ending::Failed(why)) => why.clone(),
    }
}

/// Shows the user what a run printed (exit 0) or warns, dimly, how it
/// failed. A JSON answer is not shown; `suppressOutput` hides stdout.
fn report(one: &Fired, on_event: Events<'_>) {
    let printed = match (
        one.reply.suppress_output || is_json(&one.ran),
        &one.ran.ending,
    ) {
        (true, _) => one.ran.stderr.trim().to_string(),
        (false, _) => said_all(&one.ran),
    };
    let hook = &one.hook;
    let text = match &one.reply.problem {
        None if printed.is_empty() => return,
        None => format!("{}:\n{}", label(hook), shown(&printed)),
        Some(why) if printed.is_empty() => format!("⚠ {} {why}; carrying on", label(hook)),
        Some(why) => format!("⚠ {} {why}; carrying on\n{}", label(hook), shown(&printed)),
    };
    on_event(AssistantEvent::Notice(text));
}

/// Whether the hook answered with a JSON object on stdout.
fn is_json(ran: &Ran) -> bool {
    ran.ending == Ending::Exited(0)
        && serde_json::from_str::<serde_json::Map<String, Value>>(ran.stdout.trim()).is_ok()
}

/// Why a hook refused: its reason, else what it printed.
fn reason_of(one: &Fired) -> String {
    one.reply
        .reason
        .clone()
        .or_else(|| reply::said(&one.ran).filter(|_| !is_json(&one.ran)))
        .unwrap_or_else(|| "no reason given".to_string())
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
/// The post event for a call that failed (`is_error`) or worked.
fn post_event(is_error: bool) -> HookEvent {
    match is_error {
        true => HookEvent::PostToolUseFailure,
        false => HookEvent::PostToolUse,
    }
}

pub fn with_feedback(out: ToolOutput, feedback: &[String]) -> ToolOutput {
    with_notes(out, HookEvent::AfterTool, feedback)
}

/// `out` with each `event` hook's message for the model after it.
pub fn with_notes(mut out: ToolOutput, event: HookEvent, feedback: &[String]) -> ToolOutput {
    if feedback.is_empty() {
        return out;
    }
    let notes: Vec<String> = feedback
        .iter()
        .map(|text| format!("[{} hook]\n{text}", event.as_str()))
        .collect();
    out.output = format!("{}\n\n{}", out.output, notes.join("\n\n"));
    out.refit()
}

#[cfg(test)]
pub(crate) mod test_support;

#[cfg(all(test, unix))]
mod tests;
