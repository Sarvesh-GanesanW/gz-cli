//! A custom status line: a command you choose (`/statusline <command>`)
//! gets the session's state as JSON on stdin, and the first line it prints
//! shows under the input box.
//!
//! It runs through your shell, like a command you type, since it is yours
//! to set. It reruns when what it would be told changes, at most once per
//! [`MIN_INTERVAL`] and also every [`REFRESH`] (for clocks and git state),
//! and is stopped after [`TIMEOUT`]. A failure shows dim in its place.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Child;
use tokio::sync::mpsc;

/// Fewest time between two runs.
pub const MIN_INTERVAL: Duration = Duration::from_secs(1);
/// Reruns this often even when nothing changed.
pub const REFRESH: Duration = Duration::from_secs(10);
/// Longest a run may take.
pub const TIMEOUT: Duration = Duration::from_secs(3);
/// Most output read from a run; the status line is one line.
const MAX_OUTPUT_BYTES: usize = 8 * 1024;

/// What the command is told, as JSON on stdin.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Payload {
    /// The model that answers next, as the status line names it.
    pub model: Option<String>,
    pub cwd: String,
    pub conversation_id: String,
    /// What this session cost so far, in US dollars.
    pub cost_usd: f64,
    /// How full the context window is, 0–100, once a run reported it.
    pub context_percent: Option<u8>,
    pub git_branch: Option<String>,
    /// `ask`, `accept-edits`, `plan` or `auto`.
    pub mode: String,
    pub version: &'static str,
}

/// What shows under the input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shown {
    /// The command's first line.
    Line(String),
    /// Why it failed, shown dim.
    Failed(String),
}

/// Runs the command when due and keeps its latest line.
pub struct StatusCommand {
    command: Option<String>,
    shown: Option<Shown>,
    last_payload: Option<Payload>,
    last_run: Option<Instant>,
    running: bool,
    tx: mpsc::UnboundedSender<Shown>,
    rx: mpsc::UnboundedReceiver<Shown>,
}

impl StatusCommand {
    pub fn new(command: Option<String>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            command,
            shown: None,
            last_payload: None,
            last_run: None,
            running: false,
            tx,
            rx,
        }
    }

    pub fn command(&self) -> Option<&str> {
        self.command.as_deref()
    }

    /// Uses `command` from now on (`None`: no custom status line).
    pub fn set(&mut self, command: Option<String>) {
        self.command = command;
        self.shown = None;
        self.last_payload = None;
        self.last_run = None;
    }

    /// What to show now, if anything.
    pub fn shown(&self) -> Option<&Shown> {
        self.command.as_ref().and(self.shown.as_ref())
    }

    /// Takes in finished runs and starts one when due. Returns true when
    /// what shows changed.
    pub fn tick(&mut self, payload: Payload, cwd: &Path, now: Instant) -> bool {
        let mut changed = false;
        while let Ok(shown) = self.rx.try_recv() {
            self.running = false;
            if self.command.is_some() && self.shown.as_ref() != Some(&shown) {
                self.shown = Some(shown);
                changed = true;
            }
        }
        if self.is_due(&payload, now) {
            self.start(payload, cwd, now);
        }
        changed
    }

    fn is_due(&self, payload: &Payload, now: Instant) -> bool {
        if self.command.is_none() || self.running {
            return false;
        }
        let Some(last) = self.last_run else {
            return true;
        };
        let since = now.duration_since(last);
        let changed = self.last_payload.as_ref() != Some(payload);
        (changed && since >= MIN_INTERVAL) || since >= REFRESH
    }

    fn start(&mut self, payload: Payload, cwd: &Path, now: Instant) {
        let Some(command) = self.command.clone() else {
            return;
        };
        let input = serde_json::to_vec(&payload).unwrap_or_default();
        self.last_payload = Some(payload);
        self.last_run = Some(now);
        self.running = true;
        let tx = self.tx.clone();
        let cwd = cwd.to_path_buf();
        tokio::spawn(async move {
            tx.send(run(&command, &cwd, &input, TIMEOUT).await).ok();
        });
    }
}

/// Runs `command` once with `input` on stdin; its first line, or why it
/// failed. A run past `timeout` is stopped with everything it started.
pub async fn run(command: &str, cwd: &Path, input: &[u8], timeout: Duration) -> Shown {
    let mut child = match shell(command)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(err) => return Shown::Failed(format!("status line: cannot start it: {err}")),
    };
    match tokio::time::timeout(timeout, run_to_end(&mut child, input)).await {
        Ok(Ok(line)) => Shown::Line(line),
        Ok(Err(why)) => Shown::Failed(format!("status line: {why}")),
        Err(_) => {
            crate::assistant::jobs::kill_group(&mut child);
            child.wait().await.ok();
            Shown::Failed(format!(
                "status line: timed out after {}s · /statusline off turns it off",
                timeout.as_secs()
            ))
        }
    }
}

async fn run_to_end(child: &mut Child, input: &[u8]) -> Result<String, String> {
    if let Some(mut stdin) = child.stdin.take() {
        // A command that doesn't read its input closes the pipe early.
        stdin.write_all(input).await.ok();
    }
    // Both pipes are read together and to the end, so a command that
    // writes a lot to either never blocks on a full pipe.
    let (stdout, stderr) = tokio::join!(
        read_head(child.stdout.take()),
        read_head(child.stderr.take())
    );
    let status = child
        .wait()
        .await
        .map_err(|err| format!("cannot wait for it: {err}"))?;
    if !status.success() {
        let why = first_line(&stderr);
        let code = status
            .code()
            .map_or_else(|| "a signal".to_string(), |code| code.to_string());
        return Err(if why.is_empty() {
            format!("exited with {code}")
        } else {
            format!("exited with {code}: {why}")
        });
    }
    Ok(first_line(&stdout))
}

/// The first [`MAX_OUTPUT_BYTES`] of `pipe`; the rest is read and dropped.
async fn read_head(pipe: Option<impl AsyncRead + Unpin>) -> Vec<u8> {
    let mut head = Vec::new();
    let Some(mut pipe) = pipe else {
        return head;
    };
    let mut buf = [0u8; 8192];
    while let Ok(n) = pipe.read(&mut buf).await {
        if n == 0 {
            break;
        }
        let room = MAX_OUTPUT_BYTES.saturating_sub(head.len());
        head.extend_from_slice(&buf[..n.min(room)]);
    }
    head
}

fn shell(command: &str) -> tokio::process::Command {
    if cfg!(windows) {
        let mut shell = tokio::process::Command::new("cmd");
        shell.arg("/C").arg(command);
        shell
    } else {
        let mut shell = tokio::process::Command::new("sh");
        shell.arg("-c").arg(command);
        // Its own process group, so a timeout stops its pipeline too.
        #[cfg(unix)]
        shell.process_group(0);
        shell
    }
}

/// The first non-empty line, as plain text: escape sequences and control
/// characters are dropped so the output can't move the cursor or recolor
/// the screen.
pub fn first_line(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let line = text
        .lines()
        .map(strip_escapes)
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default();
    line.trim_end().to_string()
}

fn strip_escapes(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            // CSI (`ESC [ … final`) and OSC (`ESC ] … BEL`) sequences.
            match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' || (c == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        if ch == '\t' {
            out.push(' ');
        } else if !ch.is_control() {
            out.push(ch);
        }
    }
    out
}

/// [`git_branch`], read again only when the folder changes or
/// [`MIN_INTERVAL`] has passed: the payload is built every frame.
#[derive(Debug, Default)]
pub struct BranchCache {
    read: Option<(PathBuf, Instant, Option<String>)>,
}

impl BranchCache {
    pub fn get(&mut self, cwd: &Path, now: Instant) -> Option<String> {
        let fresh = |(folder, at, _): &(PathBuf, Instant, Option<String>)| {
            folder == cwd && now.duration_since(*at) < MIN_INTERVAL
        };
        if !self.read.as_ref().is_some_and(fresh) {
            self.read = Some((cwd.to_path_buf(), now, git_branch(cwd)));
        }
        self.read.as_ref().and_then(|(_, _, branch)| branch.clone())
    }
}

/// The branch checked out in the git repository holding `cwd`, read from
/// `.git/HEAD` (no git process).
pub fn git_branch(cwd: &Path) -> Option<String> {
    let head = cwd
        .ancestors()
        .find_map(|dir| head_file(&dir.join(".git")))?;
    let text = std::fs::read_to_string(head).ok()?;
    let reference = text.trim().strip_prefix("ref: ")?;
    Some(
        reference
            .strip_prefix("refs/heads/")
            .unwrap_or(reference)
            .to_string(),
    )
}

/// `HEAD` of a `.git` folder, or of the folder a worktree's `.git` file
/// points to.
fn head_file(git: &Path) -> Option<PathBuf> {
    if git.is_dir() {
        return Some(git.join("HEAD"));
    }
    let pointer = std::fs::read_to_string(git).ok()?;
    let target = pointer.trim().strip_prefix("gitdir: ")?;
    let target = git.parent()?.join(target);
    Some(target.join("HEAD"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload() -> Payload {
        Payload {
            model: Some("Sonnet 5.5 · high".into()),
            cwd: "/w".into(),
            conversation_id: "c1".into(),
            cost_usd: 0.19,
            context_percent: Some(12),
            git_branch: Some("main".into()),
            mode: "ask".into(),
            version: "0.0.0",
        }
    }

    fn folder(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("gz-status-line-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_payload_is_camel_case_json() {
        let json = serde_json::to_value(payload()).unwrap();
        for key in [
            "model",
            "cwd",
            "conversationId",
            "costUsd",
            "contextPercent",
            "gitBranch",
            "mode",
            "version",
        ] {
            assert!(json.get(key).is_some(), "{key}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_command_reads_the_payload_and_its_first_line_shows() {
        let dir = folder("reads");
        let input = serde_json::to_vec(&payload()).unwrap();
        let shown = run(
            "printf '\\n'; grep -o '\"gitBranch\":\"[a-z]*\"'; echo second",
            &dir,
            &input,
            TIMEOUT,
        )
        .await;
        assert_eq!(shown, Shown::Line("\"gitBranch\":\"main\"".into()));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failures_and_timeouts_say_what_happened() {
        let dir = folder("fails");
        let shown = run("echo broken >&2; exit 3", &dir, b"{}", TIMEOUT).await;
        assert_eq!(
            shown,
            Shown::Failed("status line: exited with 3: broken".into())
        );
        let shown = run("sleep 5", &dir, b"{}", Duration::from_millis(200)).await;
        assert!(
            matches!(&shown, Shown::Failed(why) if why.contains("timed out")),
            "{shown:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_command_printing_a_lot_still_shows_its_first_line() {
        let dir = folder("loud");
        let shown = run(
            "echo first; head -c 300000 /dev/zero | tr '\\0' x; head -c 300000 /dev/zero >&2",
            &dir,
            b"{}",
            TIMEOUT,
        )
        .await;
        assert_eq!(shown, Shown::Line("first".into()));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_timeout_stops_the_whole_pipeline() {
        let dir = folder("pipeline");
        let marker = dir.join("still-running");
        std::fs::remove_file(&marker).ok();
        let command = format!("sleep 1 && touch '{}' | cat", marker.display());
        let shown = run(&command, &dir, b"{}", Duration::from_millis(200)).await;
        assert!(
            matches!(&shown, Shown::Failed(why) if why.contains("timed out")),
            "{shown:?}"
        );
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(
            !marker.exists(),
            "the pipeline kept running after the timeout"
        );
    }

    #[test]
    fn escape_sequences_never_reach_the_screen() {
        assert_eq!(
            first_line(b"\x1b[31mred\x1b[0m \x1b]0;title\x07done\tok\n"),
            "red done ok"
        );
    }

    #[test]
    fn runs_are_rate_limited_and_refreshed() {
        let mut status = StatusCommand::new(Some("true".into()));
        let now = Instant::now();
        assert!(status.is_due(&payload(), now));
        status.last_run = Some(now);
        status.last_payload = Some(payload());
        assert!(!status.is_due(&payload(), now + MIN_INTERVAL));
        let mut changed = payload();
        changed.cost_usd = 0.25;
        assert!(!status.is_due(&changed, now + MIN_INTERVAL / 2));
        assert!(status.is_due(&changed, now + MIN_INTERVAL));
        assert!(status.is_due(&payload(), now + REFRESH));
        status.running = true;
        assert!(!status.is_due(&changed, now + REFRESH), "one run at a time");
        assert!(!StatusCommand::new(None).is_due(&payload(), now));
    }

    #[test]
    fn the_branch_is_read_again_only_after_the_interval_or_a_folder_change() {
        let dir = folder("branch-cache");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/one\n").unwrap();
        let mut cache = BranchCache::default();
        let now = Instant::now();
        assert_eq!(cache.get(&dir, now).as_deref(), Some("one"));
        std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/two\n").unwrap();
        assert_eq!(
            cache.get(&dir, now + MIN_INTERVAL / 2).as_deref(),
            Some("one")
        );
        assert_eq!(cache.get(&dir, now + MIN_INTERVAL).as_deref(), Some("two"));
        let elsewhere = folder("branch-cache-none");
        assert_eq!(cache.get(&elsewhere, now + MIN_INTERVAL).as_deref(), None);
    }

    #[test]
    fn the_branch_comes_from_head() {
        let repo = folder("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        std::fs::create_dir_all(repo.join("src/deep")).unwrap();
        assert_eq!(
            git_branch(&repo.join("src/deep")).as_deref(),
            Some("feature/x")
        );
        std::fs::write(repo.join(".git/HEAD"), "3922c48\n").unwrap();
        assert_eq!(git_branch(&repo), None, "a detached head has no branch");
    }
}
