//! Commands, including background ones: `run_command`
//! with `run_in_background` (or one still running at its timeout) keeps
//! going while the agent works. `command_output` reads what it printed
//! since the last read, `stop_command` stops it, and gz tells the main agent
//! when it ends ([`Finished`]).
//!
//! The person using gz shares the registry: `!command` starts one of their
//! own ([`Jobs::start_for_user`]), and `/shells` lists the background ones
//! ([`Jobs::shells`]) and reads their output without taking it from the
//! model ([`Jobs::tail`]).

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, watch};

use crate::assistant::tools::ToolOutput;

/// Most bytes of one stream kept per command; older output is dropped.
const KEPT_BYTES: usize = 1024 * 1024;
const MAX_WAIT_SECS: u64 = 600;

/// A background command that ended, for the main agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished {
    pub id: String,
    pub command: String,
    /// `0`, `1`, … or `signal`/`stopped`.
    pub exit: String,
}

const NOTICE_OPEN: &str = "<command_finished";

impl Finished {
    /// The message the main agent reads (the chat's guidance names it).
    pub fn message(&self) -> String {
        format!(
            "{NOTICE_OPEN} id=\"{}\" exit=\"{}\">{}</command_finished>\nRead its output with command_output.",
            self.id,
            self.exit,
            self.command.replace('<', "&lt;"),
        )
    }

    /// The command's id, when `message` is a notice [`Self::message`] made.
    pub fn id_in(message: &str) -> Option<String> {
        let rest = message.strip_prefix(NOTICE_OPEN)?;
        let start = rest.find("id=\"")? + "id=\"".len();
        let end = rest[start..].find('"')? + start;
        Some(rest[start..end].to_string())
    }
}

/// Where a command stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Running,
    Exited(Option<i32>),
    Stopped,
    Failed(String),
}

impl State {
    /// `running`, the exit code, `signal`, `stopped` or `failed`.
    pub fn exit_label(&self) -> String {
        match self {
            State::Running => "running".into(),
            State::Exited(Some(code)) => code.to_string(),
            State::Exited(None) => "signal".into(),
            State::Stopped => "stopped".into(),
            State::Failed(_) => "failed".into(),
        }
    }
}

/// What one stream printed, and how much of it was read.
#[derive(Debug, Default)]
struct Stream {
    text: Vec<u8>,
    read: usize,
    dropped: usize,
}

impl Stream {
    fn push(&mut self, bytes: &[u8]) {
        self.text.extend_from_slice(bytes);
        if self.text.len() > KEPT_BYTES {
            let cut = self.text.len() - KEPT_BYTES;
            self.text.drain(..cut);
            self.dropped += cut;
            self.read = self.read.saturating_sub(cut);
        }
    }

    fn unread(&mut self) -> String {
        let text = String::from_utf8_lossy(&self.text[self.read..]).into_owned();
        self.read = self.text.len();
        text
    }

    /// Everything kept, read or not; the read position stays where it is.
    fn kept(&self) -> String {
        String::from_utf8_lossy(&self.text).into_owned()
    }
}

/// A background command as `/shells` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shell {
    pub id: String,
    pub command: String,
    pub state: State,
    /// Time since it started.
    pub age: Duration,
    /// The last non-empty line it printed.
    pub last_line: String,
    /// Started by the person with `!command`, not by the model.
    pub by_user: bool,
}

/// The end of what a command printed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tail {
    pub text: String,
    /// Older output is missing from `text`.
    pub is_cut: bool,
}

/// The last `max_lines` lines of `text`, and of those the last `max_chars`
/// characters; the flag says whether anything was left out.
pub fn tail_of(text: &str, max_lines: usize, max_chars: usize) -> (String, bool) {
    let lines: Vec<&str> = text.lines().collect();
    let skipped = lines.len().saturating_sub(max_lines);
    let mut kept = lines[skipped..].join("\n");
    let mut is_cut = skipped > 0;
    let chars = kept.chars().count();
    if chars > max_chars {
        kept = kept.chars().skip(chars - max_chars).collect();
        is_cut = true;
    }
    (kept, is_cut)
}

/// Standard output, then standard error under its own heading.
fn joined(mut text: String, errors: &str) -> String {
    if !errors.trim().is_empty() {
        text.push_str("\n[stderr]\n");
        text.push_str(errors);
    }
    text
}

#[derive(Debug)]
struct Job {
    command: String,
    state: watch::Receiver<State>,
    stdout: Arc<Mutex<Stream>>,
    stderr: Arc<Mutex<Stream>>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    /// Set once the agent was told it runs in the background, so its end is
    /// reported.
    background: Arc<Mutex<bool>>,
    started: Instant,
    by_user: bool,
}

/// The session's commands, by id; cheap to clone.
#[derive(Clone)]
pub struct Jobs {
    jobs: Arc<Mutex<BTreeMap<String, Job>>>,
    next: Arc<Mutex<u32>>,
    finished: mpsc::UnboundedSender<Finished>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Plain buffers and counters, still valid if a holder panicked.
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Jobs {
    /// The commands and where their background ends are announced.
    pub fn new() -> (Self, mpsc::UnboundedReceiver<Finished>) {
        let (finished, notices) = mpsc::unbounded_channel();
        let jobs = Self {
            jobs: Arc::default(),
            next: Arc::default(),
            finished,
        };
        (jobs, notices)
    }

    /// Runs `command` in `folder`. In the background it returns at once;
    /// otherwise it waits up to `timeout`, then (when `may_background`)
    /// moves the command to the background instead of stopping it.
    pub async fn run(
        &self,
        command: &str,
        folder: &Path,
        timeout: Duration,
        background: bool,
        may_background: bool,
    ) -> ToolOutput {
        let id = match self.start(command, folder, false) {
            Ok(id) => id,
            Err(err) => return ToolOutput::error(format!("cannot start the command: {err}")),
        };
        if background {
            self.mark_background(&id);
            return ToolOutput::ok(format!(
                "Started background command {id}. gz tells you when it ends; read its \
                 output with command_output and stop it with stop_command."
            ));
        }
        let state = self.wait(&id, timeout).await;
        if state == State::Running {
            if may_background {
                self.mark_background(&id);
                let so_far = self.unread(&id);
                return ToolOutput::ok(format!(
                    "Still running after {}s, so it moved to the background as {id}; gz \
                     tells you when it ends. Output so far:\n{so_far}",
                    timeout.as_secs()
                ));
            }
            self.stop(&id);
            self.forget(&id);
            return ToolOutput::error(format!(
                "the command timed out after {}s and was stopped",
                timeout.as_secs()
            ));
        }
        let out = self.result(&id, &state);
        self.forget(&id);
        out
    }

    /// `command_output`: the status, then what it printed since the last
    /// read, after waiting up to `wait` for it to end.
    pub async fn output(&self, id: &str, wait: Duration) -> ToolOutput {
        if !self.knows(id) {
            return self.unknown(id);
        }
        let wait = wait.min(Duration::from_secs(MAX_WAIT_SECS));
        let state = self.wait(id, wait).await;
        self.result(id, &state)
    }

    /// `stop_command`.
    pub fn stop_command(&self, id: &str) -> ToolOutput {
        if !self.knows(id) {
            return self.unknown(id);
        }
        if self.stop(id) {
            ToolOutput::ok(format!("stopped {id}"))
        } else {
            ToolOutput::ok(format!("{id} had already ended"))
        }
    }

    /// Starts a command the person typed themselves. Its end is never
    /// announced to the main agent: whoever started it watches
    /// [`Self::state_of`] and tells the model in their own words.
    pub fn start_for_user(&self, command: &str, folder: &Path) -> std::io::Result<String> {
        self.start(command, folder, true)
    }

    /// The commands running in the background or ended there, oldest first.
    pub fn shells(&self) -> Vec<Shell> {
        let jobs = lock(&self.jobs);
        let mut listed: Vec<(Instant, Shell)> = jobs
            .iter()
            .filter(|(_, job)| *lock(&job.background))
            .map(|(id, job)| {
                let shell = Shell {
                    id: id.clone(),
                    command: job.command.clone(),
                    state: job.state.borrow().clone(),
                    age: job.started.elapsed(),
                    last_line: last_line(&lock(&job.stdout), &lock(&job.stderr)),
                    by_user: job.by_user,
                };
                (job.started, shell)
            })
            .collect();
        // Ids sort as text (`bg_10` before `bg_2`); start order is what reads right.
        listed.sort_by_key(|(started, _)| *started);
        listed.into_iter().map(|(_, shell)| shell).collect()
    }

    /// How many background commands are still running.
    pub fn running_count(&self) -> usize {
        lock(&self.jobs)
            .values()
            .filter(|job| *lock(&job.background) && *job.state.borrow() == State::Running)
            .count()
    }

    /// Where `id` stands; `None` when the registry has no such command.
    pub fn state_of(&self, id: &str) -> Option<State> {
        lock(&self.jobs)
            .get(id)
            .map(|job| job.state.borrow().clone())
    }

    /// The end of what `id` printed so far. Unlike `command_output` this
    /// reads nothing away: the model still gets everything it has not read.
    pub fn tail(&self, id: &str, max_lines: usize, max_chars: usize) -> Option<Tail> {
        let jobs = lock(&self.jobs);
        let job = jobs.get(id)?;
        let (stdout, stderr) = (lock(&job.stdout), lock(&job.stderr));
        let all = joined(stdout.kept(), &stderr.kept());
        let (text, is_cut) = tail_of(&all, max_lines, max_chars);
        Some(Tail {
            text,
            is_cut: is_cut || stdout.dropped + stderr.dropped > 0,
        })
    }

    fn knows(&self, id: &str) -> bool {
        lock(&self.jobs).contains_key(id)
    }

    fn unknown(&self, id: &str) -> ToolOutput {
        let ids: Vec<String> = lock(&self.jobs).keys().cloned().collect();
        let known = if ids.is_empty() {
            "none are running".to_string()
        } else {
            format!("known: {}", ids.join(", "))
        };
        ToolOutput::error(format!("there is no background command {id} ({known})"))
    }

    fn start(&self, command: &str, folder: &Path, by_user: bool) -> std::io::Result<String> {
        let mut child = shell(command)
            .current_dir(folder)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let id = {
            let mut next = lock(&self.next);
            *next += 1;
            format!("bg_{}", *next)
        };
        let stdout = Arc::new(Mutex::new(Stream::default()));
        let stderr = Arc::new(Mutex::new(Stream::default()));
        let background = Arc::new(Mutex::new(false));
        let readers = [
            child
                .stdout
                .take()
                .map(|s| tokio::spawn(drain(s, stdout.clone()))),
            child
                .stderr
                .take()
                .map(|s| tokio::spawn(drain(s, stderr.clone()))),
        ];
        let (state_tx, state) = watch::channel(State::Running);
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let notice = (
            self.finished.clone(),
            id.clone(),
            command.to_string(),
            background.clone(),
        );
        tokio::spawn(async move {
            let end = supervise(&mut child, stopped).await;
            // The output is complete only once both pipes are drained.
            for reader in readers.into_iter().flatten() {
                reader.await.ok();
            }
            let (finished, id, command, background) = notice;
            if *lock(&background) && !by_user {
                let exit = end.exit_label();
                finished.send(Finished { id, command, exit }).ok();
            }
            state_tx.send(end).ok();
        });
        lock(&self.jobs).insert(
            id.clone(),
            Job {
                command: command.to_string(),
                state,
                stdout,
                stderr,
                stop: Some(stop),
                background,
                started: Instant::now(),
                by_user,
            },
        );
        Ok(id)
    }

    /// Keeps `id` once it ends and lists it in [`Self::shells`]; the main
    /// agent hears when one of its own ends.
    pub fn mark_background(&self, id: &str) {
        if let Some(job) = lock(&self.jobs).get(id) {
            *lock(&job.background) = true;
        }
    }

    async fn wait(&self, id: &str, wait: Duration) -> State {
        let state = lock(&self.jobs).get(id).map(|j| j.state.clone());
        let Some(mut state) = state else {
            return State::Failed("unknown command".into());
        };
        let ended = tokio::time::timeout(wait, async {
            state
                .wait_for(|s| *s != State::Running)
                .await
                .map(|end| end.clone())
        })
        .await;
        match ended {
            Ok(Ok(end)) => end,
            _ => state.borrow().clone(),
        }
    }

    /// Asks the command to stop; false when it had already ended.
    fn stop(&self, id: &str) -> bool {
        let mut jobs = lock(&self.jobs);
        let Some(job) = jobs.get_mut(id) else {
            return false;
        };
        let running = *job.state.borrow() == State::Running;
        if let Some(stop) = job.stop.take() {
            stop.send(()).ok();
        }
        running
    }

    /// Drops `id` and what it printed.
    pub fn forget(&self, id: &str) {
        lock(&self.jobs).remove(id);
    }

    fn unread(&self, id: &str) -> String {
        let jobs = lock(&self.jobs);
        let Some(job) = jobs.get(id) else {
            return String::new();
        };
        let text = lock(&job.stdout).unread();
        let errors = lock(&job.stderr).unread();
        joined(text, &errors)
    }

    /// `exit code N` (or `running`) and the unread output, in the shape
    /// foreground commands always had.
    fn result(&self, id: &str, state: &State) -> ToolOutput {
        let command = lock(&self.jobs)
            .get(id)
            .map(|j| j.command.clone())
            .unwrap_or_default();
        let dropped = lock(&self.jobs)
            .get(id)
            .map(|j| lock(&j.stdout).dropped + lock(&j.stderr).dropped)
            .unwrap_or(0);
        let mut text = match state {
            State::Running => format!("running: {command}\n"),
            State::Exited(_) => format!("exit code {}\n", state.exit_label()),
            State::Stopped => "stopped\n".to_string(),
            State::Failed(why) => format!("failed: {why}\n"),
        };
        if dropped > 0 {
            text.push_str(&format!("[{dropped} earlier bytes were not kept]\n"));
        }
        text.push_str(&self.unread(id));
        let failed = !matches!(state, State::Running | State::Exited(Some(0)));
        if failed {
            ToolOutput::error(text)
        } else {
            ToolOutput::ok(text)
        }
    }
}

fn shell(command: &str) -> Command {
    let mut shell = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", command]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", command]);
        c
    };
    // Its own process group, so stopping it stops what it started too.
    #[cfg(unix)]
    shell.process_group(0);
    shell
}

/// The newest non-empty line, from standard output when it has one.
fn last_line(stdout: &Stream, stderr: &Stream) -> String {
    [stdout, stderr]
        .iter()
        .find_map(|stream| {
            let text = stream.kept();
            let line = text.lines().rev().find(|line| !line.trim().is_empty())?;
            Some(line.trim().to_string())
        })
        .unwrap_or_default()
}

async fn drain(mut pipe: impl AsyncRead + Unpin, into: Arc<Mutex<Stream>>) {
    let mut buf = [0u8; 8192];
    while let Ok(n) = pipe.read(&mut buf).await {
        if n == 0 {
            break;
        }
        lock(&into).push(&buf[..n]);
    }
}

/// Waits for the command to end or to be stopped.
async fn supervise(child: &mut Child, stopped: tokio::sync::oneshot::Receiver<()>) -> State {
    tokio::select! {
        status = child.wait() => match status {
            Ok(status) => State::Exited(status.code()),
            Err(err) => State::Failed(err.to_string()),
        },
        _ = stopped => {
            kill_group(child);
            child.wait().await.ok();
            State::Stopped
        }
    }
}

fn kill_group(child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // SAFETY: killpg only sends a signal; the group is the one spawned above.
        unsafe {
            libc::killpg(pid as libc::pid_t, libc::SIGTERM);
        }
    }
    child.start_kill().ok();
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn folder() -> std::path::PathBuf {
        std::env::temp_dir()
    }

    #[tokio::test]
    async fn a_foreground_command_returns_its_exit_code_and_output() {
        let (jobs, _notices) = Jobs::new();
        let out = jobs
            .run(
                "echo hi; echo oops >&2; exit 3",
                &folder(),
                Duration::from_secs(10),
                false,
                true,
            )
            .await;
        assert!(out.is_error);
        assert_eq!(out.output, "exit code 3\nhi\n\n[stderr]\noops\n");
        assert!(
            lock(&jobs.jobs).is_empty(),
            "finished commands are forgotten"
        );
    }

    #[tokio::test]
    async fn a_background_command_reports_its_end_and_output_reads_on() {
        let (jobs, mut notices) = Jobs::new();
        let started = jobs
            .run(
                "echo one; sleep 0.3; echo two",
                &folder(),
                Duration::from_secs(5),
                true,
                true,
            )
            .await;
        assert!(started
            .output
            .starts_with("Started background command bg_1"));
        let notice = tokio::time::timeout(Duration::from_secs(5), notices.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(notice.id, "bg_1");
        assert_eq!(notice.exit, "0");
        assert_eq!(Finished::id_in(&notice.message()).as_deref(), Some("bg_1"));
        let out = jobs.output("bg_1", Duration::ZERO).await;
        assert_eq!(out.output, "exit code 0\none\ntwo\n");
        let again = jobs.output("bg_1", Duration::ZERO).await;
        assert_eq!(again.output, "exit code 0\n", "only new output");
    }

    #[tokio::test]
    async fn a_command_past_its_timeout_moves_to_the_background_and_can_be_stopped() {
        let (jobs, mut notices) = Jobs::new();
        let out = jobs
            .run(
                "echo started; sleep 30",
                &folder(),
                Duration::from_millis(300),
                false,
                true,
            )
            .await;
        assert!(
            out.output.contains("moved to the background as bg_1"),
            "{}",
            out.output
        );
        assert!(out.output.contains("started"));
        assert_eq!(jobs.stop_command("bg_1").output, "stopped bg_1");
        let notice = tokio::time::timeout(Duration::from_secs(5), notices.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(notice.exit, "stopped");
        assert!(jobs
            .output("bg_9", Duration::ZERO)
            .await
            .output
            .contains("no background command bg_9"));
    }

    async fn ended(jobs: &Jobs, id: &str) -> State {
        jobs.wait(id, Duration::from_secs(5)).await
    }

    #[tokio::test]
    async fn reading_the_tail_leaves_the_models_unread_output_intact() {
        let (jobs, _notices) = Jobs::new();
        jobs.run(
            "echo one; echo two; echo oops >&2",
            &folder(),
            Duration::from_secs(5),
            true,
            true,
        )
        .await;
        assert_eq!(ended(&jobs, "bg_1").await, State::Exited(Some(0)));
        let whole = Tail {
            text: "one\ntwo\n\n[stderr]\noops".into(),
            is_cut: false,
        };
        assert_eq!(jobs.tail("bg_1", 200, 16_000), Some(whole.clone()));
        assert_eq!(jobs.tail("bg_1", 200, 16_000), Some(whole), "reads repeat");
        let last = jobs.tail("bg_1", 1, 16_000).unwrap();
        assert_eq!((last.text.as_str(), last.is_cut), ("oops", true));
        let model = jobs.output("bg_1", Duration::ZERO).await;
        assert_eq!(model.output, "exit code 0\none\ntwo\n\n[stderr]\noops\n");
        assert_eq!(jobs.tail("bg_9", 200, 16_000), None);
    }

    #[tokio::test]
    async fn shells_lists_background_commands_running_and_ended_only() {
        let (jobs, _notices) = Jobs::new();
        let foreground = jobs
            .run("echo done", &folder(), Duration::from_secs(5), false, true)
            .await;
        assert_eq!(foreground.output, "exit code 0\ndone\n");
        jobs.run(
            "echo built; exit 2",
            &folder(),
            Duration::from_secs(5),
            true,
            true,
        )
        .await;
        jobs.run(
            "echo serving; sleep 30",
            &folder(),
            Duration::from_secs(5),
            true,
            true,
        )
        .await;
        assert_eq!(ended(&jobs, "bg_2").await, State::Exited(Some(2)));
        let serving = async {
            while jobs
                .tail("bg_3", 1, 100)
                .is_none_or(|tail| tail.text.is_empty())
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), serving)
            .await
            .unwrap();
        let listed: Vec<(String, State, String)> = jobs
            .shells()
            .into_iter()
            .map(|shell| (shell.id, shell.state, shell.last_line))
            .collect();
        assert_eq!(
            listed,
            vec![
                (
                    "bg_2".to_string(),
                    State::Exited(Some(2)),
                    "built".to_string()
                ),
                ("bg_3".to_string(), State::Running, "serving".to_string()),
            ],
            "the finished foreground bg_1 is not a shell"
        );
        assert_eq!(jobs.running_count(), 1);
        jobs.stop_command("bg_3");
        assert_eq!(ended(&jobs, "bg_3").await, State::Stopped);
        assert_eq!(jobs.running_count(), 0);
    }

    #[tokio::test]
    async fn a_command_the_user_started_is_listed_but_never_announced_to_the_agent() {
        let (jobs, mut notices) = Jobs::new();
        let id = jobs.start_for_user("echo mine", &folder()).unwrap();
        assert!(jobs.shells().is_empty(), "in the foreground until moved");
        jobs.mark_background(&id);
        assert_eq!(ended(&jobs, &id).await, State::Exited(Some(0)));
        let listed = jobs.shells();
        assert!(
            listed[0].by_user && listed[0].last_line == "mine",
            "{listed:?}"
        );
        // A later command of the model's is the first notice: the user's sent none.
        jobs.run("true", &folder(), Duration::from_secs(5), true, true)
            .await;
        let notice = tokio::time::timeout(Duration::from_secs(5), notices.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(notice.id, "bg_2");
        jobs.forget(&id);
        assert_eq!(jobs.state_of(&id), None);
    }

    #[test]
    fn the_tail_keeps_the_newest_lines_and_characters() {
        assert_eq!(tail_of("a\nb\nc", 5, 100), ("a\nb\nc".to_string(), false));
        assert_eq!(tail_of("a\nb\nc", 2, 100), ("b\nc".to_string(), true));
        assert_eq!(tail_of("héllo wörld", 5, 5), ("wörld".to_string(), true));
    }

    #[tokio::test]
    async fn without_backgrounding_a_timeout_stops_the_command() {
        let (jobs, _notices) = Jobs::new();
        let out = jobs
            .run(
                "sleep 30",
                &folder(),
                Duration::from_millis(200),
                false,
                false,
            )
            .await;
        assert!(out.is_error && out.output.contains("timed out after 0s"));
    }
}
