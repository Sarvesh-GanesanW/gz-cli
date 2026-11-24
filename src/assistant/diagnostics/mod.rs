//! Diagnostics after edits: when a tool writes a file (`edit_file`,
//! `write_file`, `notebook_edit`), gz runs fast checkers on it and adds the
//! problems the edit introduced to that tool's result, so the model can fix
//! them in the same turn.
//!
//! Checkers come from [`checkers`]: the user's own (`diagnostics.toml`,
//! per project) and built-in fast ones when installed. Each run is
//! time-boxed and killed at its timeout, so a hung checker never holds up a
//! turn. When the file existed, the checker also runs just before the edit
//! and only problems the file did not already have are reported. A checker
//! that keeps timing out is backed off ([`health`]).

pub mod checkers;
pub mod health;
pub mod problems;
mod run;
pub mod settings;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::assistant::session::AssistantEvent;
use crate::assistant::tools::{Planned, ToolOutput};

use checkers::Run;
use health::{Health, Verdict};
use problems::Problem;
use run::{Ending, Ran};
use settings::Settings;

/// Problems added to one result, at most; the model fixes the first ones
/// and the next check shows the rest.
const MAX_SHOWN: usize = 20;
/// One problem line, at most.
const LINE_CHARS: usize = 300;

type Events<'e> = &'e mut (dyn FnMut(AssistantEvent) + Send);

/// The session's diagnostics setup; cheap to clone. The default checks
/// nothing.
#[derive(Clone, Default)]
pub struct Diagnostics {
    inner: Option<Arc<Inner>>,
}

struct Inner {
    settings: Settings,
    /// Where installed checkers are looked for.
    path_dirs: Vec<PathBuf>,
    /// A broken settings file, told to the user once.
    problem: Option<String>,
    told_problem: AtomicBool,
    health: Health,
}

/// A check started before an edit: its checkers and what the file had
/// before.
pub struct Pending {
    inner: Arc<Inner>,
    file: PathBuf,
    runs: Vec<Run>,
    /// Per run; `None` when there is nothing to compare with.
    before: Vec<Option<Vec<Problem>>>,
    shown_file: String,
}

impl Diagnostics {
    /// The user's settings from `diagnostics.toml` in the gz config folder
    /// and the programs on `PATH`. A broken file keeps the defaults and is
    /// reported at the first edit.
    pub fn load() -> Self {
        let file = crate::config::config_path()
            .ok()
            .and_then(|path| path.parent().map(|dir| dir.join(settings::FILE_NAME)));
        let (settings, problem) = match file.as_deref().map(Settings::read) {
            Some(Ok(settings)) => (settings, None),
            Some(Err(err)) => (
                Settings::default(),
                Some(format!(
                    "{err:#}; gz checks edits with its built-in checkers until you fix it and restart"
                )),
            ),
            None => (Settings::default(), None),
        };
        let path_dirs = std::env::var_os("PATH")
            .map(|path| std::env::split_paths(&path).collect())
            .unwrap_or_default();
        Self::with(settings, path_dirs, problem)
    }

    pub fn with(settings: Settings, path_dirs: Vec<PathBuf>, problem: Option<String>) -> Self {
        Self {
            inner: Some(Arc::new(Inner {
                settings,
                path_dirs,
                problem,
                told_problem: AtomicBool::new(false),
                health: Health::default(),
            })),
        }
    }

    /// Starts checking `planned` when it writes a file a checker covers:
    /// runs the checkers once on the file as it is now. Call it right
    /// before the write, then [`Pending::after`] with the tool's result.
    pub async fn before(
        &self,
        planned: &Planned,
        cwd: &Path,
        shown_file: impl FnOnce(&Path) -> String,
        on_event: Events<'_>,
    ) -> Option<Pending> {
        let inner = self.inner.as_ref()?;
        if let Some(problem) = &inner.problem {
            if !inner.told_problem.swap(true, Ordering::Relaxed) {
                on_event(AssistantEvent::Notice(problem.clone()));
            }
        }
        if !inner.settings.enabled {
            return None;
        }
        let (file, content) = planned.written()?;
        let runs: Vec<Run> =
            checkers::for_file(&inner.settings, &inner.path_dirs, file, content, cwd)
                .into_iter()
                .filter(|run| !inner.health.is_off(run))
                .collect();
        if runs.is_empty() {
            return None;
        }
        let mut before = Vec::with_capacity(runs.len());
        for run in &runs {
            let compare = run.compare_before && !inner.health.is_slow(run) && file.exists();
            before.push(if compare {
                baseline(run, file, &inner.health).await
            } else {
                None
            });
        }
        Some(Pending {
            inner: inner.clone(),
            file: file.to_path_buf(),
            runs,
            before,
            shown_file: shown_file(file),
        })
    }
}

impl Pending {
    /// `out` with the problems the edit introduced appended. A failed edit
    /// is returned as is.
    pub async fn after(self, mut out: ToolOutput, on_event: Events<'_>) -> ToolOutput {
        if !out.is_error {
            out.output.push_str(&self.report(on_event).await);
        }
        out
    }

    /// Runs the checkers on the file as it is now: the text for the model
    /// (empty when nothing new was found), with a notice for the user.
    pub async fn report(self, on_event: Events<'_>) -> String {
        let mut text = String::new();
        for (run, before) in self.runs.iter().zip(self.before) {
            let ran = run::execute(run, &self.file).await;
            let verdict = self
                .inner
                .health
                .record(run, matches!(ran.ending, Ending::TimedOut(_)));
            if let Some(why) = trouble(run, &ran, verdict, &self.shown_file) {
                on_event(AssistantEvent::Notice(why));
                continue;
            }
            let content = std::fs::read_to_string(&self.file).unwrap_or_default();
            let found = problems_in(&ran, run, &self.file, &content);
            let compared = before.is_some();
            let found = match before {
                Some(before) => problems::new_since(&before, found),
                None => found,
            };
            if found.is_empty() {
                continue;
            }
            let summary = summary(run, found.len(), compared, &self.shown_file);
            on_event(AssistantEvent::Notice(format!(
                "{summary}; gz passed them to the model"
            )));
            text.push_str(&report(&summary, &found, &self.shown_file));
        }
        text
    }
}

/// The checks for a script's helper edits: each file is compared with how
/// it was before the script's first edit to it and checked once, when the
/// script ends, however often the script changed it.
#[derive(Default)]
pub struct Batch {
    diagnostics: Diagnostics,
    pending: Vec<Pending>,
}

impl Batch {
    pub fn new(diagnostics: Diagnostics) -> Self {
        Self {
            diagnostics,
            pending: Vec::new(),
        }
    }

    /// Call before each helper write, like [`Diagnostics::before`].
    pub async fn before(
        &mut self,
        planned: &Planned,
        cwd: &Path,
        shown_file: impl FnOnce(&Path) -> String,
        on_event: Events<'_>,
    ) {
        let Some((file, _)) = planned.written() else {
            return;
        };
        if self.pending.iter().any(|pending| pending.file == file) {
            return;
        }
        let started = self
            .diagnostics
            .before(planned, cwd, shown_file, on_event)
            .await;
        self.pending.extend(started);
    }

    /// The problems the script's edits introduced, for its result.
    pub async fn report(self, on_event: Events<'_>) -> String {
        let mut text = String::new();
        for pending in self.pending {
            text.push_str(&pending.report(on_event).await);
        }
        text
    }
}

/// The problems `run` finds in `file` before the edit; `None` when it could
/// not tell (then every later problem is reported).
async fn baseline(run: &Run, file: &Path, health: &Health) -> Option<Vec<Problem>> {
    let text = std::fs::read_to_string(file).ok()?;
    let ran = run::execute(run, file).await;
    // A timeout here is not told: the run after the edit still reports.
    health.record(run, matches!(ran.ending, Ending::TimedOut(_)));
    let finished = matches!(ran.ending, Ending::Exited(_));
    finished.then(|| problems_in(&ran, run, file, &text))
}

/// What `ran` reported about `file`, each with its line's text.
fn problems_in(ran: &Ran, run: &Run, file: &Path, text: &str) -> Vec<Problem> {
    let found = problems::about(&ran.seen.lines.join("\n"), &run.cwd, &[file.to_path_buf()]);
    problems::with_source(found, text)
}

/// Why `run` told nothing useful, for the user: it timed out, could not
/// start, or failed without printing any problem.
fn trouble(run: &Run, ran: &Ran, verdict: Verdict, shown_file: &str) -> Option<String> {
    let name = &run.name;
    let file = settings::FILE_NAME;
    match &ran.ending {
        Ending::TimedOut(after) if verdict == Verdict::TurnedOff => Some(format!(
            "{name} did not finish checking {shown_file} within {} again and was stopped; gz stops running it for this session. Raise timeout_secs in {file} and restart, or turn it off there",
            seconds(*after),
        )),
        Ending::TimedOut(after) => Some(format!(
            "{name} did not finish checking {shown_file} within {} and was stopped; gz now runs it only after edits. Raise timeout_secs in {file} or turn it off there",
            seconds(*after),
        )),
        Ending::Failed(why) => Some(format!("{name} could not check {shown_file}: {why}")),
        Ending::Signalled => Some(format!("{name} was stopped while checking {shown_file}")),
        Ending::Exited(0) => None,
        Ending::Exited(code) => {
            let printed_problems = ran.seen.problem_lines > 0;
            (!printed_problems).then(|| {
                let last = ran.seen.last_line.as_deref().unwrap_or("no output");
                format!(
                    "{name} could not check {shown_file} (exit {code}): {}; fix the checker or its entry in {}",
                    clip(last.trim()),
                    settings::FILE_NAME
                )
            })
        }
    }
}

fn summary(run: &Run, count: usize, compared: bool, shown_file: &str) -> String {
    let kind = if compared { "new problem" } else { "problem" };
    let plural = if count == 1 { "" } else { "s" };
    format!("{} found {count} {kind}{plural} in {shown_file}", run.name)
}

/// The text added to the model's result.
fn report(summary: &str, found: &[Problem], shown_file: &str) -> String {
    let mut text = format!("\n\nDiagnostics after this edit: {summary}:\n");
    for problem in found.iter().take(MAX_SHOWN) {
        text.push_str(&clip(&problem.describe(shown_file)));
        text.push('\n');
    }
    if found.len() > MAX_SHOWN {
        text.push_str(&format!(
            "… and {} more; fix these and the next check shows the rest\n",
            found.len() - MAX_SHOWN
        ));
    }
    text
}

fn clip(line: &str) -> String {
    match line.char_indices().nth(LINE_CHARS) {
        Some((at, _)) => format!("{}…", &line[..at]),
        None => line.to_string(),
    }
}

fn seconds(duration: Duration) -> String {
    let secs = duration.as_secs_f64();
    if secs.fract() == 0.0 {
        format!("{secs:.0} s")
    } else {
        format!("{secs:.1} s")
    }
}

#[cfg(all(test, unix))]
mod tests;
