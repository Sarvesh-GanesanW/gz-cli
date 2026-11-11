//! Runs one hook command: through the user's shell, in a given folder,
//! with JSON on stdin, output captured (capped) and a hard timeout.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinHandle;

use crate::assistant::jobs;

/// Output kept per stream; the rest is read and dropped so a chatty hook
/// never stalls on a full pipe.
const MAX_CAPTURE: usize = 64 * 1024;
/// How long to wait for a killed hook's pipes to close; a grandchild that
/// left the process group could hold them open forever.
const PIPE_GRACE: Duration = Duration::from_secs(1);

/// How a hook ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ending {
    Exited(i32),
    /// Ended by a signal it did not get from gz.
    Signalled,
    TimedOut(Duration),
    /// The shell could not start, or waiting on it failed.
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct Ran {
    pub ending: Ending,
    pub stdout: String,
    pub stderr: String,
}

/// What to run and where.
pub struct RunSpec<'a> {
    pub command: &'a str,
    pub cwd: &'a Path,
    pub stdin: Vec<u8>,
    pub env: Vec<(&'static str, String)>,
    pub timeout: Duration,
}

pub async fn run(spec: RunSpec<'_>) -> Ran {
    let mut command = jobs::unconfined_shell(spec.command);
    command
        .current_dir(spec.cwd)
        .envs(spec.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            return Ran {
                ending: Ending::Failed(format!("cannot start the shell: {err}")),
                stdout: String::new(),
                stderr: String::new(),
            }
        }
    };
    // A hook that never reads stdin must not block on a large input.
    let writer = child.stdin.take().map(|mut pipe| {
        let input = spec.stdin;
        tokio::spawn(async move {
            pipe.write_all(&input).await.ok();
        })
    });
    let stdout = child.stdout.take().map(|p| tokio::spawn(capture(p)));
    let stderr = child.stderr.take().map(|p| tokio::spawn(capture(p)));
    let ending = match tokio::time::timeout(spec.timeout, child.wait()).await {
        Ok(Ok(status)) => status.code().map_or(Ending::Signalled, Ending::Exited),
        Ok(Err(err)) => Ending::Failed(format!("cannot wait for the hook: {err}")),
        Err(_) => {
            jobs::kill_group(&mut child);
            child.wait().await.ok();
            Ending::TimedOut(spec.timeout)
        }
    };
    if let Some(writer) = writer {
        writer.abort();
    }
    Ran {
        ending,
        stdout: collect(stdout).await,
        stderr: collect(stderr).await,
    }
}

async fn capture(mut pipe: impl AsyncRead + Unpin) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut buf = [0u8; 8192];
    while let Ok(n) = pipe.read(&mut buf).await {
        if n == 0 {
            break;
        }
        let room = MAX_CAPTURE.saturating_sub(kept.len());
        kept.extend_from_slice(&buf[..n.min(room)]);
    }
    kept
}

async fn collect(handle: Option<JoinHandle<Vec<u8>>>) -> String {
    let Some(mut handle) = handle else {
        return String::new();
    };
    match tokio::time::timeout(PIPE_GRACE, &mut handle).await {
        Ok(Ok(bytes)) => String::from_utf8_lossy(&bytes).into_owned(),
        _ => {
            handle.abort();
            String::new()
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn spec(command: &str, timeout: Duration) -> RunSpec<'_> {
        RunSpec {
            command,
            cwd: Path::new("/"),
            stdin: b"{\"event\":\"x\"}".to_vec(),
            env: vec![("GZ_HOOK_EVENT", "x".into())],
            timeout,
        }
    }

    #[tokio::test]
    async fn reads_stdin_and_reports_exit_and_streams() {
        let ran = run(spec(
            "cat; echo \" $GZ_HOOK_EVENT\"; echo oops >&2; exit 3",
            Duration::from_secs(5),
        ))
        .await;
        assert_eq!(ran.ending, Ending::Exited(3));
        assert_eq!(ran.stdout, "{\"event\":\"x\"} x\n");
        assert_eq!(ran.stderr, "oops\n");
    }

    #[tokio::test]
    async fn a_slow_hook_is_killed_at_its_timeout() {
        let started = std::time::Instant::now();
        let ran = run(spec("sleep 20", Duration::from_millis(300))).await;
        assert_eq!(ran.ending, Ending::TimedOut(Duration::from_millis(300)));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "killed, not waited out"
        );
    }
}
