//! `gz impact`: what depends on tables, columns, or the jobs behind changed
//! files, with an exit code CI can gate on. See [`crate::impact`].

use anyhow::{Context, Result};
use owo_colors::OwoColorize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::cli::ImpactArgs;
use crate::cmd::code_report::ReportedExit;
use crate::cmd::{jobs, Runtime};
use crate::impact::changes::{self, ChangedFile, SyncFolder};
use crate::impact::graph::AssetId;
use crate::impact::{lookup, summary_line, Kind, Report, Target, ROW_COLUMNS};
use crate::output::{self, OutputFormat};

/// Exit code when dependents of a protected kind exist.
const PROTECTED_EXIT: i32 = 2;

pub async fn run(rt: &Runtime, args: &ImpactArgs) -> Result<()> {
    let flag_kinds = Kind::parse_list(&args.fail_on).context("--fail-on")?;
    if !args.targets.is_empty() {
        let targets = args
            .targets
            .iter()
            .map(|t| Target::parse(t))
            .collect::<Result<Vec<_>>>()?;
        let report = lookup::impact_of(&rt.http, targets, args.depth).await?;
        show(rt, &report, None)?;
        return gate(&report, &flag_kinds, "--fail-on");
    }
    let folder = SyncFolder::open(Path::new(&args.dir))?;
    let files = match &args.git {
        Some(base) => changes::files_since(base, &std::env::current_dir()?)?,
        None => args.changed.iter().map(PathBuf::from).collect(),
    };
    let mapped = changes::map_files(&folder, &files);
    let jobs = job_tables(rt, &folder, args.project, &mapped).await?;
    let targets = targets_of(&jobs);
    let mut report = lookup::impact_of_assets(&rt.http, targets, args.depth).await?;
    let changed_ids: Vec<i64> = jobs.iter().filter_map(|j| j.id).collect();
    report.drop_jobs(&changed_ids);
    let changes = Changes {
        files: mapped,
        jobs,
    };
    show(rt, &report, Some(&changes))?;
    let (kinds, source) = if flag_kinds.is_empty() {
        let kinds =
            Kind::parse_list(&folder.config.fail_on).context("gz-sync.toml [impact] fail_on")?;
        (kinds, "[impact] fail_on")
    } else {
        (flag_kinds, "--fail-on")
    };
    gate(&report, &kinds, source)
}

/// A job a changed file maps to, and what it writes.
#[derive(Debug)]
struct ChangedJob {
    name: String,
    id: Option<i64>,
    /// `None`: lineage has never compiled the job (or it isn't in the
    /// project yet).
    writes: Option<Vec<(AssetId, String)>>,
}

struct Changes {
    files: Vec<ChangedFile>,
    jobs: Vec<ChangedJob>,
}

async fn job_tables(
    rt: &Runtime,
    folder: &SyncFolder,
    project_flag: Option<i64>,
    files: &[ChangedFile],
) -> Result<Vec<ChangedJob>> {
    let mut names: Vec<String> = files.iter().filter_map(|f| f.job.clone()).collect();
    names.sort();
    names.dedup();
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let ids = job_ids(rt, folder, project_flag, &names).await?;
    let mut found = Vec::new();
    for name in names {
        let id = ids.get(&name).copied();
        let writes = match id {
            Some(id) => lookup::job_outputs(&rt.http, id).await?,
            None => None,
        };
        found.push(ChangedJob { name, id, writes });
    }
    Ok(found)
}

/// Job ids by name: `[impact.jobs]` first, then the project's job list
/// (the ETLProject gateway, which needs a signed-in user).
async fn job_ids(
    rt: &Runtime,
    folder: &SyncFolder,
    project_flag: Option<i64>,
    names: &[String],
) -> Result<BTreeMap<String, i64>> {
    let mut ids: BTreeMap<String, i64> = names
        .iter()
        .filter_map(|n| Some((n.clone(), *folder.config.jobs.get(n)?)))
        .collect();
    if ids.len() == names.len() {
        return Ok(ids);
    }
    let project = folder.project(project_flag).with_context(|| {
        format!(
            "which project are these jobs in? pass --project, set `project` under [impact] in {}, or run `gz sync` there once",
            folder.root.join(crate::cmd::devloop::MAPPING_FILE).display()
        )
    })?;
    let listed = jobs::project_jobs(rt, project).await.with_context(|| {
        format!(
            "cannot list project {project}'s jobs to find their ids; with only an API key, list them under [impact.jobs] in gz-sync.toml (name = id)"
        )
    })?;
    for job in listed {
        if names.contains(&job.name) {
            ids.entry(job.name).or_insert(job.id);
        }
    }
    Ok(ids)
}

/// One target per table the changed jobs write.
fn targets_of(jobs: &[ChangedJob]) -> Vec<Target> {
    let mut by_name: BTreeMap<String, Vec<AssetId>> = BTreeMap::new();
    for (id, name) in jobs.iter().flat_map(|j| j.writes.iter().flatten()) {
        let assets = by_name.entry(name.clone()).or_default();
        if !assets.contains(id) {
            assets.push(*id);
        }
    }
    by_name
        .into_iter()
        .map(|(name, assets)| Target {
            text: name.clone(),
            table: name,
            column: None,
            assets,
        })
        .collect()
}

/// Exits 2, with a short summary on stderr, when protected dependents exist.
fn gate(report: &Report, kinds: &[Kind], source: &str) -> Result<()> {
    let protected = report.protected(kinds);
    if protected.is_empty() {
        return Ok(());
    }
    eprintln!("{}", protected_summary(report, &protected, source));
    Err(ReportedExit(PROTECTED_EXIT).into())
}

fn protected_summary(report: &Report, protected: &BTreeMap<Kind, usize>, source: &str) -> String {
    let parts: Vec<String> = protected
        .iter()
        .map(|(kind, n)| {
            let names: Vec<&str> = report
                .dependents
                .iter()
                .filter(|d| d.kind == *kind)
                .map(|d| d.name.as_str())
                .collect();
            format!(
                "{} ({})",
                crate::impact::count_text(*kind, *n),
                names.join(", ")
            )
        })
        .collect();
    format!(
        "gz impact: this change affects protected dependents ({source}): {}",
        parts.join("; ")
    )
}

fn show(rt: &Runtime, report: &Report, changes: Option<&Changes>) -> Result<()> {
    if rt.format != OutputFormat::Table {
        let mut value = report.to_json();
        if let Some(changes) = changes {
            value["files"] = files_json(changes);
            value["unmapped"] = json!(changes
                .files
                .iter()
                .filter(|f| f.job.is_none())
                .map(|f| &f.file)
                .collect::<Vec<_>>());
        }
        return output::print_value(&value, rt.format, None);
    }
    if let Some(changes) = changes {
        print_changes(changes);
    }
    for target in &report.targets {
        let assets: Vec<String> = target.assets.iter().map(|a| a.to_string()).collect();
        if assets.is_empty() {
            output::warn(&format!(
                "{}: not in lineage (no run or compile has recorded it); nothing to report",
                target.text
            ));
        } else {
            println!(
                "{} {}",
                format!("Impact of {}", target.text).bold(),
                format!(
                    "· lineage asset {} · {} hops",
                    assets.join(", "),
                    report.depth
                )
                .dimmed()
            );
        }
    }
    if !report.dependents.is_empty() {
        output::print_with_columns(&report.rows(), rt.format, rt.limit, ROW_COLUMNS)?;
    }
    if !report.targets.is_empty() {
        println!("{}", summary_line(&report.counts()));
    }
    for note in &report.notes {
        output::warn(note);
    }
    Ok(())
}

fn files_json(changes: &Changes) -> Value {
    let jobs: BTreeMap<&str, &ChangedJob> =
        changes.jobs.iter().map(|j| (j.name.as_str(), j)).collect();
    Value::Array(
        changes
            .files
            .iter()
            .filter_map(|f| {
                let job = jobs.get(f.job.as_deref()?)?;
                Some(json!({
                    "file": f.file,
                    "job": job.name,
                    "jobId": job.id,
                    "writes": job.writes.as_ref().map(|w| w.iter().map(|(_, n)| n).collect::<Vec<_>>()),
                }))
            })
            .collect(),
    )
}

fn print_changes(changes: &Changes) {
    let jobs: BTreeMap<&str, &ChangedJob> =
        changes.jobs.iter().map(|j| (j.name.as_str(), j)).collect();
    for file in &changes.files {
        let Some(job) = file.job.as_deref().and_then(|name| jobs.get(name)) else {
            continue;
        };
        let what = match (job.id, &job.writes) {
            (None, _) => "not in the project yet".to_string(),
            (Some(_), None) => "no lineage yet (it has not been compiled or run)".to_string(),
            (Some(_), Some(tables)) if tables.is_empty() => {
                "writes no tables lineage knows of".to_string()
            }
            (Some(_), Some(tables)) => format!(
                "writes {}",
                tables
                    .iter()
                    .map(|(_, n)| n.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        println!("{} → job {} {}", file.file, job.name, what.dimmed());
    }
    let unmapped: Vec<&str> = changes
        .files
        .iter()
        .filter(|f| f.job.is_none())
        .map(|f| f.file.as_str())
        .collect();
    if !unmapped.is_empty() {
        println!(
            "{}",
            format!("Unmapped (no job): {}", unmapped.join(", ")).dimmed()
        );
    }
    if changes.jobs.is_empty() {
        println!("No changed file maps to a job; nothing to check.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_written_by_several_jobs_are_one_target() {
        let jobs = vec![
            ChangedJob {
                name: "a".into(),
                id: Some(1),
                writes: Some(vec![(10, "db.x".into()), (11, "db.y".into())]),
            },
            ChangedJob {
                name: "b".into(),
                id: Some(2),
                writes: Some(vec![(10, "db.x".into())]),
            },
            ChangedJob {
                name: "c".into(),
                id: None,
                writes: None,
            },
        ];
        let targets = targets_of(&jobs);
        let shown: Vec<(&str, &[AssetId])> = targets
            .iter()
            .map(|t| (t.text.as_str(), t.assets.as_slice()))
            .collect();
        assert_eq!(shown, vec![("db.x", &[10][..]), ("db.y", &[11][..])]);
    }
}
