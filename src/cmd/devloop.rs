//! The local dev loop: run a local Python file as a DE/ML job, and keep a
//! folder of job files in sync with a project.
//!
//! Job code lives in S3 as `{site}/{prefix}/{projectId}/{jobId}.py`; the
//! ETLProject gateway writes it (`PUT /etl/etlJobs/jobs/{p}/{j}` with
//! `code`, or `POST /etl/etlJobs/{p}` with `fileContent`) and a run executes
//! whatever is saved there (etl provider `getS3URIofJob`, app.py:5800).

use anyhow::{bail, Context, Result};
use owo_colors::OwoColorize;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::cli::{RunArgs, SyncArgs};
use crate::cmd::apps::{job_update_body, saved_job_path};
use crate::cmd::jobs::{self, JobRef};
use crate::cmd::{confirm, Runtime};
use crate::output::{self, OutputFormat};

const PROJECTS: &str = "etlprojects";
const SCRATCH_PREFIX: &str = "gz-run-";
const MAPPING_FILE: &str = "gz-sync.toml";
const STATE_FILE: &str = ".gz-sync.json";
const DIFF_PREVIEW_LINES: usize = 40;
/// How long a file must stay quiet before `--watch` syncs it.
const WATCH_QUIET: Duration = Duration::from_millis(500);
const WATCH_TICK: Duration = Duration::from_millis(100);

// ---------- gz run ----------

pub async fn run(rt: &Runtime, args: &RunArgs) -> Result<()> {
    let path = Path::new(&args.file);
    ensure_python(path)?;
    let code =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let jobs = jobs::project_jobs(rt, args.project).await?;
    let job = match &args.job {
        Some(wanted) => find_job(&jobs, wanted).with_context(|| {
            format!(
                "no job '{wanted}' in project {}; see `gz projects jobs {}`",
                args.project, args.project
            )
        })?,
        None => {
            let name = scratch_job_name(path)?;
            match jobs.iter().find(|j| j.name == name) {
                Some(existing) => existing.clone(),
                None => {
                    confirm(
                        args.yes,
                        &format!("Create job {name} in project {}?", args.project),
                    )?;
                    create_job(rt, args.project, &name, &code).await?
                }
            }
        }
    };
    let saved = saved_job(rt, args.project, job.id).await?;
    upload_if_changed(rt, args, &job, &saved, &code).await?;
    let spec = jobs::run_spec(
        &jobs::project_name(rt, args.project).await?,
        args.project,
        job.id,
        &job.name,
        &saved,
        &args.choice,
    );
    jobs::start_run(rt, spec, args.follow).await
}

/// Groundzero stores and runs job code as `.py`; SQL runs inside Python
/// (e.g. `spark.sql(...)`), so other files are refused up front.
fn ensure_python(path: &Path) -> Result<()> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("py") => Ok(()),
        _ => bail!(
            "{} isn't a Python file; Groundzero jobs run Python (.py) — wrap SQL in spark.sql(...)",
            path.display()
        ),
    }
}

/// `--job` accepts an id or a name.
fn find_job(jobs: &[JobRef], wanted: &str) -> Option<JobRef> {
    let by_id = wanted.parse::<i64>().ok();
    jobs.iter()
        .find(|j| Some(j.id) == by_id || j.name == wanted)
        .cloned()
}

fn scratch_job_name(path: &Path) -> Result<String> {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    job_name_from_stem(&format!("{SCRATCH_PREFIX}{stem}"))
        .with_context(|| format!("cannot make a job name from {}; pass --job", path.display()))
}

async fn upload_if_changed(
    rt: &Runtime,
    args: &RunArgs,
    job: &JobRef,
    saved: &Value,
    code: &str,
) -> Result<()> {
    let current = saved["fileContent"].as_str().unwrap_or_default();
    if same_code(current, code) {
        if rt.format == OutputFormat::Table {
            output::info(&format!("{} already has this code", job.name));
        }
        return Ok(());
    }
    if rt.format == OutputFormat::Table && !current.is_empty() {
        print_diff(&job.name, current, code);
    }
    if !current.is_empty() {
        confirm(args.yes, &format!("Replace the code of job {}?", job.name))?;
    }
    put_code(rt, args.project, job.id, saved, code).await
}

// ---------- gz sync ----------

pub async fn sync(rt: &Runtime, args: &SyncArgs) -> Result<()> {
    let root = PathBuf::from(&args.dir);
    if !root.is_dir() {
        bail!("{} is not a folder", root.display());
    }
    let mapping = load_mapping(&root)?;
    if args.pull {
        return pull(rt, &root, mapping.as_ref(), args).await;
    }
    let skipped = push(rt, &root, mapping.as_ref(), args, None).await?;
    if args.watch {
        return watch(rt, &root, mapping.as_ref(), args).await;
    }
    if skipped > 0 {
        bail!(
            "{skipped} file(s) were skipped because the project changed too; \
             run `gz sync --pull` to take its version, or `--force` to overwrite it"
        );
    }
    Ok(())
}

/// Optional `gz-sync.toml`: `[jobs] "etl/clean_orders.py" = "clean-orders"`.
#[derive(Debug, Deserialize)]
struct MappingFile {
    #[serde(default)]
    jobs: BTreeMap<String, String>,
}

fn load_mapping(root: &Path) -> Result<Option<BTreeMap<String, String>>> {
    let path = root.join(MAPPING_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    parse_mapping(&text)
        .with_context(|| format!("{} is invalid", path.display()))
        .map(Some)
}

fn parse_mapping(text: &str) -> Result<BTreeMap<String, String>> {
    let parsed: MappingFile = toml::from_str(text)?;
    let mut seen = BTreeMap::new();
    for (file, job) in &parsed.jobs {
        if !is_valid_job_name(job) {
            bail!("job name '{job}' (for {file}) must start with a letter and use only letters, numbers, and single hyphens");
        }
        if let Some(other) = seen.insert(job.clone(), file.clone()) {
            bail!("{other} and {file} both map to job {job}");
        }
    }
    Ok(parsed.jobs)
}

/// Last code hash synced per job, so a change made in the web app is never
/// silently overwritten (and a local edit never silently lost on pull).
#[derive(Debug, Default, Serialize, Deserialize)]
struct SyncState {
    project: i64,
    #[serde(default)]
    jobs: BTreeMap<String, String>,
}

impl SyncState {
    fn load(root: &Path, project: i64) -> Self {
        let text = std::fs::read_to_string(root.join(STATE_FILE)).unwrap_or_default();
        match serde_json::from_str::<SyncState>(&text) {
            Ok(state) if state.project == project => state,
            _ => SyncState {
                project,
                jobs: BTreeMap::new(),
            },
        }
    }

    fn save(&self, root: &Path) -> Result<()> {
        let path = root.join(STATE_FILE);
        std::fs::write(&path, serde_json::to_string_pretty(self)? + "\n")
            .with_context(|| format!("cannot write {}", path.display()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalFile {
    /// Path relative to the synced folder, with `/` separators.
    rel: String,
    job: String,
    code: String,
}

/// What a sync does with one file/job pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Create,
    Update,
    Download,
    Unchanged,
    /// Only the other side changed since the last sync; nothing to send.
    OtherSideNewer,
    /// Both sides changed since the last sync (or there is no record).
    Conflict,
}

impl Step {
    fn label(self, pulling: bool) -> &'static str {
        match self {
            Step::Create => "created",
            Step::Update => "updated",
            Step::Download => "downloaded",
            Step::Unchanged => "unchanged",
            Step::OtherSideNewer if pulling => "local is newer · skipped",
            Step::OtherSideNewer => "project is newer · skipped",
            Step::Conflict => "changed on both sides · skipped",
        }
    }

    fn writes(self) -> bool {
        matches!(self, Step::Create | Step::Update | Step::Download)
    }

    fn skipped(self) -> bool {
        matches!(self, Step::OtherSideNewer | Step::Conflict)
    }
}

/// Push decision for one file, from content hashes.
fn decide_push(local: &str, remote: Option<&str>, last: Option<&str>, force: bool) -> Step {
    let Some(remote) = remote else {
        return Step::Create;
    };
    if remote == local {
        return Step::Unchanged;
    }
    if force || last == Some(remote) || remote == code_hash("") {
        return Step::Update;
    }
    if last == Some(local) {
        return Step::OtherSideNewer;
    }
    Step::Conflict
}

/// Pull decision for one job, from content hashes.
fn decide_pull(remote: &str, local: Option<&str>, last: Option<&str>, force: bool) -> Step {
    let Some(local) = local else {
        return Step::Download;
    };
    if local == remote {
        return Step::Unchanged;
    }
    if force || last == Some(local) {
        return Step::Download;
    }
    if last == Some(remote) {
        return Step::OtherSideNewer;
    }
    Step::Conflict
}

/// Pushes local files to their jobs; returns how many were skipped.
async fn push(
    rt: &Runtime,
    root: &Path,
    mapping: Option<&BTreeMap<String, String>>,
    args: &SyncArgs,
    only: Option<&BTreeSet<String>>,
) -> Result<usize> {
    let mut files = local_files(root, mapping)?;
    if let Some(only) = only {
        files.retain(|f| only.contains(&f.rel));
    }
    let remote_jobs = jobs::project_jobs(rt, args.project).await?;
    let mut state = SyncState::load(root, args.project);
    let mut report = Vec::new();
    for file in &files {
        let local_hash = code_hash(&file.code);
        let remote = remote_jobs.iter().find(|j| j.name == file.job);
        let saved = match remote {
            Some(job) => Some(saved_job(rt, args.project, job.id).await?),
            None => None,
        };
        let remote_hash = saved
            .as_ref()
            .map(|s| code_hash(s["fileContent"].as_str().unwrap_or_default()));
        let last = state.jobs.get(&file.job).map(String::as_str);
        let step = decide_push(&local_hash, remote_hash.as_deref(), last, args.force);
        if !args.dry_run && step.writes() {
            match (remote, &saved) {
                (Some(job), Some(saved)) => {
                    put_code(rt, args.project, job.id, saved, &file.code).await?
                }
                _ => {
                    create_job(rt, args.project, &file.job, &file.code).await?;
                }
            }
        }
        if !step.skipped() {
            state.jobs.insert(file.job.clone(), local_hash);
        }
        report.push((file.rel.clone(), file.job.clone(), step));
    }
    if !args.dry_run {
        state.save(root)?;
    }
    print_report(rt, &report, false, args.dry_run).await?;
    Ok(report.iter().filter(|(_, _, s)| s.skipped()).count())
}

async fn pull(
    rt: &Runtime,
    root: &Path,
    mapping: Option<&BTreeMap<String, String>>,
    args: &SyncArgs,
) -> Result<()> {
    let by_job: BTreeMap<String, String> = mapping
        .map(|m| m.iter().map(|(f, j)| (j.clone(), f.clone())).collect())
        .unwrap_or_default();
    let mut state = SyncState::load(root, args.project);
    let mut report = Vec::new();
    for job in jobs::project_jobs(rt, args.project).await? {
        let rel = match (&mapping, by_job.get(&job.name)) {
            (Some(_), Some(rel)) => rel.clone(),
            (Some(_), None) => continue,
            (None, _) => format!("{}.py", job.name),
        };
        let saved = saved_job(rt, args.project, job.id).await?;
        let Some(remote) = saved["fileContent"].as_str().filter(|c| !c.is_empty()) else {
            continue;
        };
        let remote = normalize(remote);
        let target = root.join(&rel);
        let local_hash = std::fs::read_to_string(&target).ok().map(|c| code_hash(&c));
        let remote_hash = code_hash(&remote);
        let last = state.jobs.get(&job.name).map(String::as_str);
        let step = decide_pull(&remote_hash, local_hash.as_deref(), last, args.force);
        if !args.dry_run && step.writes() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("cannot create {}", parent.display()))?;
            }
            std::fs::write(&target, &remote)
                .with_context(|| format!("cannot write {}", target.display()))?;
        }
        if !step.skipped() {
            state.jobs.insert(job.name.clone(), remote_hash);
        }
        report.push((rel, job.name, step));
    }
    if !args.dry_run {
        state.save(root)?;
    }
    print_report(rt, &report, true, args.dry_run).await
}

/// The files to sync: the mapping file's entries, or every `.py` file under
/// the folder (respecting `.gitignore`) named after its job.
fn local_files(root: &Path, mapping: Option<&BTreeMap<String, String>>) -> Result<Vec<LocalFile>> {
    if let Some(mapping) = mapping {
        return mapping
            .iter()
            .map(|(rel, job)| {
                let path = root.join(rel);
                let code = std::fs::read_to_string(&path)
                    .with_context(|| format!("{MAPPING_FILE} lists {rel}, but it can't be read"))?;
                Ok(LocalFile {
                    rel: rel.clone(),
                    job: job.clone(),
                    code,
                })
            })
            .collect();
    }
    let mut files = Vec::new();
    let mut owners: BTreeMap<String, String> = BTreeMap::new();
    for entry in ignore::WalkBuilder::new(root).build() {
        let entry = entry.context("cannot read the folder")?;
        let path = entry.path();
        if !entry.file_type().is_some_and(|t| t.is_file()) || ensure_python(path).is_err() {
            continue;
        }
        let rel = relative(root, path);
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let Some(job) = job_name_from_stem(stem) else {
            output::warn(&format!(
                "skipping {rel}: can't make a job name from it (add it to {MAPPING_FILE})"
            ));
            continue;
        };
        if let Some(other) = owners.insert(job.clone(), rel.clone()) {
            bail!("{other} and {rel} would both sync to job {job}; map them in {MAPPING_FILE}");
        }
        let code = std::fs::read_to_string(path).with_context(|| format!("cannot read {rel}"))?;
        files.push(LocalFile { rel, job, code });
    }
    files.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(files)
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

async fn print_report(
    rt: &Runtime,
    report: &[(String, String, Step)],
    pulling: bool,
    dry_run: bool,
) -> Result<()> {
    if rt.format != OutputFormat::Table {
        let rows: Vec<Value> = report
            .iter()
            .map(|(file, job, step)| {
                json!({"file": file, "job": job, "action": step.label(pulling), "dryRun": dry_run})
            })
            .collect();
        return rt.show(Value::Array(rows)).await;
    }
    if report.is_empty() {
        output::info("nothing to sync");
        return Ok(());
    }
    let arrow = if pulling { "←" } else { "→" };
    for (file, job, step) in report {
        let label = step.label(pulling);
        let mark = match step {
            _ if step.skipped() => "!".yellow().to_string(),
            Step::Unchanged => "·".dimmed().to_string(),
            _ => "✓".green().to_string(),
        };
        let verb = if dry_run && step.writes() {
            format!("would be {label}")
        } else {
            label.to_string()
        };
        println!("{mark} {file} {arrow} {job}  {}", verb.dimmed());
    }
    Ok(())
}

// ---------- --watch ----------

/// Collects changed paths and releases them once nothing has changed for
/// `quiet`, so an editor's burst of saves becomes one sync.
#[derive(Debug)]
struct Debouncer {
    quiet: Duration,
    pending: BTreeSet<String>,
    last_change: Option<Instant>,
}

impl Debouncer {
    fn new(quiet: Duration) -> Self {
        Self {
            quiet,
            pending: BTreeSet::new(),
            last_change: None,
        }
    }

    fn record(&mut self, rel: String, now: Instant) {
        self.pending.insert(rel);
        self.last_change = Some(now);
    }

    fn take_ready(&mut self, now: Instant) -> Option<BTreeSet<String>> {
        let last = self.last_change?;
        if now.duration_since(last) < self.quiet || self.pending.is_empty() {
            return None;
        }
        self.last_change = None;
        Some(std::mem::take(&mut self.pending))
    }
}

async fn watch(
    rt: &Runtime,
    root: &Path,
    mapping: Option<&BTreeMap<String, String>>,
    args: &SyncArgs,
) -> Result<()> {
    use notify::{RecursiveMode, Watcher};
    let canonical_root = root
        .canonicalize()
        .with_context(|| format!("cannot resolve {}", root.display()))?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<PathBuf>();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if let Ok(event) = event {
            for path in event.paths {
                let _ = tx.send(path);
            }
        }
    })
    .context("cannot watch the folder for changes")?;
    watcher
        .watch(&canonical_root, RecursiveMode::Recursive)
        .context("cannot watch the folder for changes")?;
    output::info(&format!(
        "watching {} · saves sync to project {} · Ctrl+C to stop",
        root.display(),
        args.project
    ));
    let mut debouncer = Debouncer::new(WATCH_QUIET);
    let mut tick = tokio::time::interval(WATCH_TICK);
    loop {
        tokio::select! {
            Some(path) = rx.recv() => {
                if ensure_python(&path).is_ok() {
                    let rel = relative(&canonical_root, &path.canonicalize().unwrap_or(path));
                    debouncer.record(rel, Instant::now());
                }
            }
            _ = tick.tick() => {
                if let Some(changed) = debouncer.take_ready(Instant::now()) {
                    if let Err(err) = push(rt, root, mapping, args, Some(&changed)).await {
                        output::warn(&format!("{err:#}"));
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => return Ok(()),
        }
    }
}

// ---------- shared ----------

async fn saved_job(rt: &Runtime, project: i64, job: i64) -> Result<Value> {
    rt.http
        .request_json(
            PROJECTS,
            Method::GET,
            &saved_job_path(&project.to_string(), &job.to_string()),
            &[],
            None,
        )
        .await
}

async fn put_code(rt: &Runtime, project: i64, job: i64, saved: &Value, code: &str) -> Result<()> {
    let body = job_update_body(saved, &project.to_string(), Some(code.to_string()), None)?;
    rt.http
        .request_json(
            PROJECTS,
            Method::PUT,
            &saved_job_path(&project.to_string(), &job.to_string()),
            &[],
            Some(body),
        )
        .await
        .map(|_| ())
}

/// `POST /etl/etlJobs/{project}` with the code, as the job editor's "new job".
async fn create_job(rt: &Runtime, project: i64, name: &str, code: &str) -> Result<JobRef> {
    let created = rt
        .http
        .request_json(
            PROJECTS,
            Method::POST,
            &format!("/etl/etlJobs/{project}"),
            &[],
            Some(json!({"Name": name, "fileContent": code})),
        )
        .await?;
    let id = created["Id"]
        .as_i64()
        .with_context(|| format!("creating job {name} returned no id"))?;
    Ok(JobRef {
        id,
        name: name.to_string(),
    })
}

/// The ETLProject naming rule: a letter first, then letters, digits, and
/// single hyphens (etlJob.schema.mjs).
fn is_valid_job_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && !name.ends_with('-')
        && !name.contains("--")
}

/// `clean_orders` → `clean-orders`; None when no valid name can be made.
fn job_name_from_stem(stem: &str) -> Option<String> {
    let mut name = String::new();
    for c in stem.chars() {
        if c.is_ascii_alphanumeric() {
            name.push(c);
        } else if !name.ends_with('-') && !name.is_empty() {
            name.push('-');
        }
    }
    let name = name.trim_end_matches('-').to_string();
    is_valid_job_name(&name).then_some(name)
}

/// Saved code often has Windows line endings; compare and write with `\n`.
fn normalize(code: &str) -> String {
    code.replace("\r\n", "\n")
}

fn same_code(a: &str, b: &str) -> bool {
    normalize(a) == normalize(b)
}

fn code_hash(code: &str) -> String {
    Sha256::digest(normalize(code).as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn print_diff(name: &str, old: &str, new: &str) {
    let diff = crate::assistant::tools::unified_diff(name, Some(&normalize(old)), &normalize(new));
    let lines: Vec<&str> = diff.lines().collect();
    for line in lines.iter().take(DIFF_PREVIEW_LINES) {
        let shown = if line.starts_with('+') && !line.starts_with("+++") {
            line.green().to_string()
        } else if line.starts_with('-') && !line.starts_with("---") {
            line.red().to_string()
        } else {
            line.dimmed().to_string()
        };
        eprintln!("{shown}");
    }
    if lines.len() > DIFF_PREVIEW_LINES {
        eprintln!(
            "{}",
            format!("… +{} more lines", lines.len() - DIFF_PREVIEW_LINES).dimmed()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_names_follow_the_platform_rule() {
        assert_eq!(
            job_name_from_stem("clean_orders").as_deref(),
            Some("clean-orders")
        );
        assert_eq!(
            job_name_from_stem("gz-run-my  file").as_deref(),
            Some("gz-run-my-file")
        );
        assert_eq!(
            job_name_from_stem("2024_load"),
            None,
            "must start with a letter"
        );
        assert_eq!(job_name_from_stem("___"), None);
        assert!(is_valid_job_name("TestMultiJob2"));
        assert!(!is_valid_job_name("a--b"));
        assert!(!is_valid_job_name("a_b"));
    }

    #[test]
    fn finds_jobs_by_id_or_name() {
        let jobs = vec![
            JobRef {
                id: 1006,
                name: "nightly".into(),
            },
            JobRef {
                id: 7,
                name: "1006".into(),
            },
        ];
        assert_eq!(
            find_job(&jobs, "1006").unwrap().id,
            1006,
            "ids win over names"
        );
        assert_eq!(find_job(&jobs, "nightly").unwrap().id, 1006);
        assert!(find_job(&jobs, "missing").is_none());
    }

    #[test]
    fn only_python_files_run() {
        assert!(ensure_python(Path::new("etl/load.py")).is_ok());
        let err = ensure_python(Path::new("q.sql")).unwrap_err().to_string();
        assert!(err.contains("spark.sql"), "{err}");
    }

    #[test]
    fn parses_and_validates_mapping_files() {
        let mapping =
            parse_mapping("[jobs]\n\"etl/clean_orders.py\" = \"clean-orders\"\n").unwrap();
        assert_eq!(mapping["etl/clean_orders.py"], "clean-orders");
        assert!(parse_mapping("[jobs]\n\"a.py\" = \"bad_name\"\n").is_err());
        let dup = parse_mapping("[jobs]\n\"a.py\" = \"x\"\n\"b.py\" = \"x\"\n");
        assert!(dup.unwrap_err().to_string().contains("both map to job x"));
        assert!(parse_mapping("").unwrap().is_empty());
    }

    #[test]
    fn hashes_ignore_line_endings() {
        assert_eq!(code_hash("a\r\nb\r\n"), code_hash("a\nb\n"));
        assert_ne!(code_hash("a"), code_hash("b"));
        assert!(same_code("x\r\n", "x\n"));
    }

    #[test]
    fn push_never_overwrites_unseen_project_changes() {
        let (local, remote, base) = (code_hash("new"), code_hash("theirs"), code_hash("base"));
        assert_eq!(decide_push(&local, None, None, false), Step::Create);
        assert_eq!(
            decide_push(&local, Some(&local), None, false),
            Step::Unchanged
        );
        assert_eq!(
            decide_push(&local, Some(&base), Some(&base), false),
            Step::Update
        );
        assert_eq!(
            decide_push(&base, Some(&remote), Some(&base), false),
            Step::OtherSideNewer
        );
        assert_eq!(
            decide_push(&local, Some(&remote), Some(&base), false),
            Step::Conflict
        );
        assert_eq!(
            decide_push(&local, Some(&remote), None, false),
            Step::Conflict
        );
        assert_eq!(
            decide_push(&local, Some(&code_hash("")), None, false),
            Step::Update
        );
        assert_eq!(decide_push(&local, Some(&remote), None, true), Step::Update);
    }

    #[test]
    fn pull_never_overwrites_unsaved_local_edits() {
        let (remote, local, base) = (code_hash("theirs"), code_hash("mine"), code_hash("base"));
        assert_eq!(decide_pull(&remote, None, None, false), Step::Download);
        assert_eq!(
            decide_pull(&remote, Some(&remote), None, false),
            Step::Unchanged
        );
        assert_eq!(
            decide_pull(&remote, Some(&base), Some(&base), false),
            Step::Download
        );
        assert_eq!(
            decide_pull(&base, Some(&local), Some(&base), false),
            Step::OtherSideNewer
        );
        assert_eq!(
            decide_pull(&remote, Some(&local), Some(&base), false),
            Step::Conflict
        );
        assert_eq!(
            decide_pull(&remote, Some(&local), None, true),
            Step::Download
        );
    }

    #[test]
    fn debouncer_waits_for_quiet_then_releases_once() {
        let start = Instant::now();
        let mut d = Debouncer::new(Duration::from_millis(500));
        assert!(d.take_ready(start).is_none());
        d.record("a.py".into(), start);
        d.record("b.py".into(), start + Duration::from_millis(300));
        assert!(
            d.take_ready(start + Duration::from_millis(600)).is_none(),
            "b changed 300ms in"
        );
        let ready = d.take_ready(start + Duration::from_millis(800)).unwrap();
        assert_eq!(ready.into_iter().collect::<Vec<_>>(), vec!["a.py", "b.py"]);
        assert!(
            d.take_ready(start + Duration::from_secs(5)).is_none(),
            "released once"
        );
    }

    #[test]
    fn discovers_python_files_and_skips_ignored_ones() {
        let dir = std::env::temp_dir().join(format!("gz-sync-discover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("etl/build")).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join("etl/clean_orders.py"), "print(1)\n").unwrap();
        std::fs::write(dir.join("etl/build/out.py"), "x\n").unwrap();
        std::fs::write(dir.join("notes.txt"), "x\n").unwrap();
        std::fs::write(dir.join(".gitignore"), "etl/build/\n").unwrap();
        let files = local_files(&dir, None).unwrap();
        assert_eq!(
            files,
            vec![LocalFile {
                rel: "etl/clean_orders.py".into(),
                job: "clean-orders".into(),
                code: "print(1)\n".into(),
            }]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn state_from_another_project_is_ignored() {
        let dir = std::env::temp_dir().join(format!("gz-sync-state-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut state = SyncState::load(&dir, 1);
        state.jobs.insert("a".into(), "h".into());
        state.save(&dir).unwrap();
        assert_eq!(SyncState::load(&dir, 1).jobs.len(), 1);
        assert!(SyncState::load(&dir, 2).jobs.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}
