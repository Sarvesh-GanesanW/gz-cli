//! Commands, including background ones like Claude Code's: `run_command`
//! with `run_in_background` (or one still running at its timeout) keeps
//! going while the agent works. `command_output` reads what it printed
//! since the last read, `stop_command` stops it, and gz tells the main agent
//! when it ends ([`Finished`]).

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

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
enum State {
    Running,
    Exited(Option<i32>),
    Stopped,
    Failed(String),
}

impl State {
    fn exit_label(&self) -> String {
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
        let id = match self.start(command, folder) {
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

    fn start(&self, command: &str, folder: &Path) -> std::io::Result<String> {
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
            if *lock(&background) {
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
            },
        );
        Ok(id)
    }

    fn mark_background(&self, id: &str) {
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

    fn forget(&self, id: &str) {
        lock(&self.jobs).remove(id);
    }

    fn unread(&self, id: &str) -> String {
        let jobs = lock(&self.jobs);
        let Some(job) = jobs.get(id) else {
            return String::new();
        };
        let mut text = lock(&job.stdout).unread();
        let errors = lock(&job.stderr).unread();
        if !errors.trim().is_empty() {
            text.push_str("\n[stderr]\n");
            text.push_str(&errors);
        }
        text
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
