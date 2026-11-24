//! Runs one checker: through the user's shell in its folder, time-boxed,
//! and killed with everything it started at the timeout. A whole-project
//! checker (tsc, eslint, cargo check) can print far more than one file's
//! problems, so output is filtered as it streams: only lines that name the
//! edited file are kept, plus what a failed run needs to be explained.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::task::JoinHandle;

use crate::assistant::jobs;

use super::checkers::Run;
use super::problems;

/// Bytes of matching lines kept per stream; far more than the problems
/// shown to the model.
const MAX_KEPT: usize = 256 * 1024;
/// Bytes of one line looked at; the rest of a longer line is dropped.
const MAX_LINE: usize = 8 * 1024;
/// How long to wait for a killed checker's pipes to close; a grandchild
/// that left the process group could hold them open forever.
const PIPE_GRACE: Duration = Duration::from_secs(1);

/// How a checker run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ending {
    Exited(i32),
    /// Ended by a signal it did not get from gz.
    Signalled,
    TimedOut(Duration),
    /// The shell could not start, or waiting on it failed.
    Failed(String),
}

/// What a checker run printed that matters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Seen {
    /// Lines naming the edited file, in order (standard output first).
    pub lines: Vec<String>,
    /// Problem lines about any file, kept or not.
    pub problem_lines: usize,
    /// The last non-empty line, standard error's when it has one.
    pub last_line: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Ran {
    pub ending: Ending,
    pub seen: Seen,
}

/// Runs `run`, keeping the lines that mention `file`.
pub async fn execute(run: &Run, file: &Path) -> Ran {
    let mut command = jobs::unconfined_shell(&run.command);
    command
        .current_dir(&run.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            return Ran {
                ending: Ending::Failed(format!("cannot start the shell: {err}")),
                seen: Seen::default(),
            }
        }
    };
    let filter = Filter::new(file, &run.cwd);
    let stdout = child
        .stdout
        .take()
        .map(|pipe| tokio::spawn(filter.clone().read(pipe)));
    let stderr = child
        .stderr
        .take()
        .map(|pipe| tokio::spawn(filter.read(pipe)));
    let ending = match tokio::time::timeout(run.timeout, child.wait()).await {
        Ok(Ok(status)) => status.code().map_or(Ending::Signalled, Ending::Exited),
        Ok(Err(err)) => Ending::Failed(format!("cannot wait for the checker: {err}")),
        Err(_) => {
            jobs::kill_group(&mut child);
            child.wait().await.ok();
            Ending::TimedOut(run.timeout)
        }
    };
    let out = collect(stdout).await;
    let err = collect(stderr).await;
    Ran {
        ending,
        seen: Seen {
            lines: out.lines.into_iter().chain(err.lines).collect(),
            problem_lines: out.problem_lines + err.problem_lines,
            last_line: err.last_line.or(out.last_line),
        },
    }
}

/// Keeps the lines of one stream that name the edited file.
#[derive(Clone)]
struct Filter {
    /// The file's name: every spelling a tool prints (absolute, relative,
    /// `./`) contains it.
    name: String,
    cwd: std::path::PathBuf,
}

impl Filter {
    fn new(file: &Path, cwd: &Path) -> Self {
        let name = file
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        Self {
            name,
            cwd: cwd.to_path_buf(),
        }
    }

    async fn read(self, mut pipe: impl AsyncRead + Unpin) -> Seen {
        let mut seen = Seen::default();
        let mut kept = 0;
        let mut line = Vec::new();
        let mut buf = [0u8; 8192];
        while let Ok(n) = pipe.read(&mut buf).await {
            if n == 0 {
                break;
            }
            for &byte in &buf[..n] {
                if byte == b'\n' {
                    self.take(&mut seen, &mut kept, &line);
                    line.clear();
                } else if line.len() < MAX_LINE {
                    line.push(byte);
                }
            }
        }
        self.take(&mut seen, &mut kept, &line);
        seen
    }

    fn take(&self, seen: &mut Seen, kept: &mut usize, line: &[u8]) {
        let line = String::from_utf8_lossy(line);
        let line = line.trim_end();
        if line.trim().is_empty() {
            return;
        }
        seen.last_line = Some(line.to_string());
        seen.problem_lines += problems::about_any(line, &self.cwd);
        let named = !self.name.is_empty() && line.contains(&self.name);
        if named && *kept + line.len() <= MAX_KEPT {
            *kept += line.len();
            seen.lines.push(line.to_string());
        }
    }
}

async fn collect(handle: Option<JoinHandle<Seen>>) -> Seen {
    let Some(mut handle) = handle else {
        return Seen::default();
    };
    match tokio::time::timeout(PIPE_GRACE, &mut handle).await {
        Ok(Ok(seen)) => seen,
        _ => {
            handle.abort();
            Seen::default()
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn run(command: &str) -> Run {
        Run {
            name: "check".into(),
            command: command.into(),
            cwd: "/".into(),
            timeout: Duration::from_secs(5),
            compare_before: true,
        }
    }

    #[tokio::test]
    async fn keeps_only_lines_naming_the_file_however_much_else_is_printed() {
        let ran = execute(
            &run("seq 1 20000 | sed 's/.*/other.py:&:1: E1 noise/'; \
                  echo 'src/app.py:3:1: E9 mine'; echo 'Found many' >&2; exit 1"),
            Path::new("/src/app.py"),
        )
        .await;
        assert_eq!(ran.ending, Ending::Exited(1));
        assert_eq!(ran.seen.lines, ["src/app.py:3:1: E9 mine"]);
        assert_eq!(ran.seen.problem_lines, 20_001);
        assert_eq!(ran.seen.last_line.as_deref(), Some("Found many"));
    }

    #[tokio::test]
    async fn a_hung_checker_is_killed_at_its_timeout() {
        let mut hung = run("sleep 30");
        hung.timeout = Duration::from_millis(200);
        let started = std::time::Instant::now();
        let ran = execute(&hung, Path::new("/a.py")).await;
        assert_eq!(ran.ending, Ending::TimedOut(Duration::from_millis(200)));
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
