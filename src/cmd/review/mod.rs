//! `gz review`: one command for a job change in a `gz sync` folder. For
//! each changed job it shows the code diff, what the change touches
//! downstream, how a try run on write branches changed each table, and the
//! quality results, then gives a verdict CI can gate on.
//!
//! It is built from the existing commands: the code diff from `gz sync`'s
//! mapping and saved code (`GET /etl/etlJobs/jobs/{p}/{j}`), the impact from
//! `gz impact`'s lineage walk, the try and cleanup from `gz jobs try` and
//! `gz jobs discard` themselves, and the rows from `gz diff --rows`.

mod args;
mod changes;
mod model;
mod render;
mod rows;
mod trial;

pub(crate) use args::REVIEW_ABOUT;
pub use args::{ReviewArgs, ReviewFormat};

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;

use crate::cmd::code_report::ReportedExit;
use crate::cmd::{devloop, jobs, Runtime};
use crate::impact::changes::SyncFolder;
use crate::impact::{lookup, Kind, Target};
use crate::output::OutputFormat;
use changes::Candidate;
use model::{Code, Impact, JobReview, Step, TableRows, Tried};
use trial::{RunChoice, TryTarget};

/// What the whole review covers, for the renderers.
pub(crate) struct Review {
    pub project: i64,
    pub jobs: Vec<JobReview>,
    pub unmapped: Vec<String>,
    pub fail_on: Vec<Kind>,
}

pub async fn run(rt: &Runtime, args: &ReviewArgs) -> Result<()> {
    if rt.format != OutputFormat::Table && args.format == ReviewFormat::Markdown {
        bail!(
            "pick one output: -o {} or --format markdown",
            format_name(rt.format)
        );
    }
    let folder = SyncFolder::open(&args.dir)?;
    let fail_on = fail_on_kinds(args, &folder)?;
    let project = folder.project(args.project).with_context(|| {
        format!(
            "which project are these jobs in? pass --project, set `project` under [impact] in {}, or run `gz sync` there once",
            folder.root.join(devloop::MAPPING_FILE).display()
        )
    })?;
    let candidates = changes::candidates(&folder, &args.paths)?;
    let listed = jobs::project_jobs(rt, project)
        .await
        .with_context(|| format!("cannot list project {project}'s jobs"))?;
    let ids: BTreeMap<String, i64> = listed.into_iter().map(|j| (j.name, j.id)).collect();
    let asked = !args.paths.is_empty();
    let mut reviewed = Vec::new();
    for candidate in candidates.jobs {
        let job_id = ids.get(&candidate.job).copied();
        let saved = match job_id {
            Some(id) => Some(saved_code(rt, project, id).await?),
            None => None,
        };
        let code = changes::compare(&candidate.file, candidate.code.as_deref(), saved.as_deref());
        // Without paths, review covers changed jobs only.
        if !asked && matches!(code, Code::Unchanged) {
            continue;
        }
        if !rt.quiet {
            crate::output::info(&format!(
                "Reviewing {} (job {})",
                candidate.file, candidate.job
            ));
        }
        reviewed.push(review_job(rt, args, project, candidate, job_id, code).await);
    }
    let review = Review {
        project,
        jobs: reviewed,
        unmapped: candidates.unmapped,
        fail_on,
    };
    let (verdict, had_error) = render::totals(&review);
    if rt.format != OutputFormat::Table {
        rt.show(render::json(&review)).await?;
    } else if args.format == ReviewFormat::Markdown {
        print!("{}", render::markdown(&review));
    } else {
        print!("{}", render::text(&review));
    }
    match model::exit_code(verdict, had_error, args.strict) {
        0 => Ok(()),
        code => Err(ReportedExit(code).into()),
    }
}

fn format_name(format: OutputFormat) -> &'static str {
    match format {
        OutputFormat::Json => "json",
        OutputFormat::Yaml => "yaml",
        OutputFormat::Csv => "csv",
        OutputFormat::Table => "table",
    }
}

/// `--fail-on`, else the folder's `[impact] fail_on`, as `gz impact` does.
fn fail_on_kinds(args: &ReviewArgs, folder: &SyncFolder) -> Result<Vec<Kind>> {
    if !args.fail_on.is_empty() {
        return Kind::parse_list(&args.fail_on).context("--fail-on");
    }
    Kind::parse_list(&folder.config.fail_on).context("gz-sync.toml [impact] fail_on")
}

async fn saved_code(rt: &Runtime, project: i64, job: i64) -> Result<String> {
    let saved = devloop::saved_job(rt, project, job)
        .await
        .with_context(|| format!("cannot read job {job}'s saved code"))?;
    Ok(saved["fileContent"]
        .as_str()
        .unwrap_or_default()
        .to_string())
}

async fn review_job(
    rt: &Runtime,
    args: &ReviewArgs,
    project: i64,
    candidate: Candidate,
    job_id: Option<i64>,
    code: Code,
) -> JobReview {
    let mut review = JobReview {
        file: candidate.file,
        job: candidate.job,
        job_id,
        code,
        impact: Step::NotAsked("the code is the same as the saved code".into()),
        tried: Step::NotAsked("the code is the same as the saved code".into()),
        rows: Vec::new(),
        rows_note: None,
        cleanup: Step::NotAsked("no branch was made".into()),
    };
    if matches!(review.code, Code::Unchanged) {
        return review;
    }
    review.impact = match job_id {
        Some(id) => impact(rt, id, args.depth).await,
        None => Step::NotAsked("the job is not in the project yet".into()),
    };
    let (Some(id), Some(local)) = (job_id, candidate.code.as_deref()) else {
        review.tried = Step::NotAsked(match review.code {
            Code::Removed => "the file was removed".into(),
            _ => "the job is not in the project yet".into(),
        });
        return review;
    };
    let (Some(compute), Some(rte)) = (args.compute, args.rte.as_deref()) else {
        review.tried =
            Step::Skipped("not tried: pass --compute and --rte to run it on a branch".into());
        return review;
    };
    let branch = trial::branch_name(id, now_secs());
    let target = TryTarget {
        project,
        job_id: id,
        job: &review.job,
        code: local,
        branch: &branch,
    };
    let choice = RunChoice {
        compute,
        rte,
        workers: args.workers,
        gpu: args.gpu,
    };
    review.tried = trial::try_job(rt, &target, &choice).await;
    if let Step::Done(tried) = &review.tried {
        let (rows, note) = compare_rows(rt, args, tried).await;
        review.rows = rows;
        review.rows_note = note;
    }
    review.cleanup = cleanup(rt, args, project, id, &review.tried, &branch).await;
    review
}

/// What depends on the tables the job writes, leaving out the job itself.
async fn impact(rt: &Runtime, job: i64, depth: u32) -> Step<Impact> {
    let writes = match lookup::job_outputs(&rt.http, job).await {
        Ok(Some(writes)) => writes,
        Ok(None) => {
            return Step::Skipped(
                "lineage has no record of the job yet (it has not been compiled or run)".into(),
            )
        }
        Err(err) => return Step::Failed(format!("{err:#}")),
    };
    let tables: Vec<String> = writes.iter().map(|(_, name)| name.clone()).collect();
    let mut by_table: BTreeMap<String, Vec<_>> = BTreeMap::new();
    for (asset, name) in writes {
        by_table.entry(name).or_default().push(asset);
    }
    let targets = by_table
        .into_iter()
        .map(|(name, assets)| Target {
            text: name.clone(),
            table: name,
            column: None,
            assets,
        })
        .collect();
    match lookup::impact_of_assets(&rt.http, targets, depth).await {
        Ok(mut report) => {
            report.drop_jobs(&[job]);
            Step::Done(Impact {
                writes: tables,
                report,
            })
        }
        Err(err) => Step::Failed(format!("{err:#}")),
    }
}

async fn compare_rows(
    rt: &Runtime,
    args: &ReviewArgs,
    tried: &Tried,
) -> (Vec<(String, Step<TableRows>)>, Option<String>) {
    if args.no_rows {
        return (Vec::new(), Some("rows not compared (--no-rows)".into()));
    }
    if !tried.succeeded() {
        return (Vec::new(), Some("rows not compared: the run failed".into()));
    }
    let tables = tried.changed_tables();
    if tables.is_empty() {
        return (Vec::new(), Some("the run wrote to no table".into()));
    }
    let compared = rows::compare(rt, args.sql_compute.as_deref(), &tried.branch, &tables).await;
    (compared, None)
}

/// Drops the review's branch unless asked to keep it. Try already dropped
/// it when it refused the run before starting it.
async fn cleanup(
    rt: &Runtime,
    args: &ReviewArgs,
    project: i64,
    job: i64,
    tried: &Step<Tried>,
    branch: &str,
) -> Step<String> {
    if !trial::may_have_left_branches(tried) {
        return Step::NotAsked("no branch was left to drop".into());
    }
    if args.keep_branches {
        return Step::NotAsked(format!(
            "branch {branch} kept (--keep-branches); drop it with `gz jobs discard {job} --project {project} --branch {branch}`"
        ));
    }
    trial::discard(rt, project, job, branch).await
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}
