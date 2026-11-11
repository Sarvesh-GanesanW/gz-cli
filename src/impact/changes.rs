//! From changed files to the tables to report on: files map to jobs the way
//! `gz sync` maps them, jobs to the tables their lineage says they write.
//!
//! `gz-sync.toml` may carry an `[impact]` section:
//!
//! ```toml
//! [impact]
//! fail_on = ["dashboards", "agents"]   # exit 2 when such dependents exist
//! project = 1603                       # else the folder's last `gz sync`
//!
//! [impact.jobs]                        # job ids by name; lets a CI run with
//! "clean-orders" = 2233                # only an API key skip the job list
//! ```

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::cmd::devloop::{synced_project, JobMap, MAPPING_FILE};

#[derive(Debug, Default, Deserialize, PartialEq)]
pub struct ImpactConfig {
    #[serde(default)]
    pub fail_on: Vec<String>,
    pub project: Option<i64>,
    #[serde(default)]
    pub jobs: BTreeMap<String, i64>,
}

#[derive(Debug, Default, Deserialize)]
struct SyncFile {
    #[serde(default)]
    impact: ImpactConfig,
}

/// A synced folder: where its files are, how they map to jobs, and its
/// `[impact]` settings.
pub struct SyncFolder {
    pub root: PathBuf,
    pub jobs: JobMap,
    pub config: ImpactConfig,
}

impl SyncFolder {
    pub fn open(root: &Path) -> Result<Self> {
        if !root.is_dir() {
            bail!(
                "{} is not a folder; pass --dir with the synced folder of job files",
                root.display()
            );
        }
        let root = root
            .canonicalize()
            .with_context(|| format!("cannot resolve {}", root.display()))?;
        Ok(Self {
            jobs: JobMap::load(&root)?,
            config: load_config(&root)?,
            root,
        })
    }

    /// The project the folder's jobs live in: `--project`, else `[impact]
    /// project`, else the project it was last synced with.
    pub fn project(&self, flag: Option<i64>) -> Option<i64> {
        flag.or(self.config.project)
            .or_else(|| synced_project(&self.root))
    }

    /// `file` (any path) inside the folder, `/`-separated; `None` outside.
    pub fn relative(&self, file: &Path) -> Option<String> {
        let absolute = resolve(file);
        let rel = absolute.strip_prefix(&self.root).ok()?;
        Some(
            rel.components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/"),
        )
    }
}

fn load_config(root: &Path) -> Result<ImpactConfig> {
    let path = root.join(MAPPING_FILE);
    if !path.exists() {
        return Ok(ImpactConfig::default());
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    parse_config(&text)
        .with_context(|| format!("{} has an invalid [impact] section", path.display()))
}

fn parse_config(text: &str) -> Result<ImpactConfig> {
    Ok(toml::from_str::<SyncFile>(text)?.impact)
}

/// The real path, also for a deleted file (its folder still exists).
fn resolve(file: &Path) -> PathBuf {
    if let Ok(real) = file.canonicalize() {
        return real;
    }
    match (file.parent(), file.file_name()) {
        (Some(parent), Some(name)) => {
            let parent = if parent.as_os_str().is_empty() {
                Path::new(".")
            } else {
                parent
            };
            parent
                .canonicalize()
                .map(|p| p.join(name))
                .unwrap_or_else(|_| file.to_path_buf())
        }
        _ => file.to_path_buf(),
    }
}

/// One changed file and the job it maps to, if any.
#[derive(Debug, Clone, PartialEq)]
pub struct ChangedFile {
    /// As given (or as git names it).
    pub file: String,
    pub job: Option<String>,
}

pub fn map_files(folder: &SyncFolder, files: &[PathBuf]) -> Vec<ChangedFile> {
    files
        .iter()
        .map(|file| ChangedFile {
            file: file.display().to_string(),
            job: folder
                .relative(file)
                .and_then(|rel| folder.jobs.job_for(&rel)),
        })
        .collect()
}

/// The files changed since `base`: what differs between the working tree
/// and the commit this branch forked from, so changes made on `base`
/// after the fork are not counted.
pub fn files_since(base: &str, cwd: &Path) -> Result<Vec<PathBuf>> {
    let top = git(cwd, &["rev-parse", "--show-toplevel"])
        .context("--git needs a git repository; run gz impact inside one, or pass --changed")?;
    let top = PathBuf::from(top.trim());
    let fork = git(cwd, &["merge-base", base, "HEAD"]).with_context(|| {
        format!("cannot find where this branch left {base}; fetch it first (git fetch origin) or pass another ref")
    })?;
    let names = git(cwd, &["diff", "--name-only", fork.trim()])?;
    Ok(names
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| top.join(l.trim()))
        .collect())
}

fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .context("cannot run git; is it installed?")?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_impact_section_and_ignores_the_rest() {
        let config = parse_config(
            "[jobs]\n\"a.py\" = \"load-a\"\n\n[impact]\nfail_on = [\"dashboards\"]\nproject = 7\n\n[impact.jobs]\n\"load-a\" = 2233\n",
        )
        .unwrap();
        assert_eq!(config.fail_on, vec!["dashboards"]);
        assert_eq!(config.project, Some(7));
        assert_eq!(config.jobs["load-a"], 2233);
        assert_eq!(parse_config("").unwrap(), ImpactConfig::default());
        assert!(parse_config("[impact]\nproject = \"x\"\n").is_err());
    }

    #[test]
    fn files_outside_the_folder_map_to_no_job() {
        let base = std::env::temp_dir().join(format!("gz-impact-map-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("jobs/etl")).unwrap();
        std::fs::write(base.join("jobs/etl/clean_orders.py"), "x\n").unwrap();
        let folder = SyncFolder::open(&base.join("jobs")).unwrap();
        let mapped = map_files(
            &folder,
            &[
                base.join("jobs/etl/clean_orders.py"),
                base.join("jobs/etl/removed_job.py"),
                base.join("README.md"),
            ],
        );
        let jobs: Vec<Option<&str>> = mapped.iter().map(|m| m.job.as_deref()).collect();
        assert_eq!(
            jobs,
            vec![Some("clean-orders"), Some("removed-job"), None],
            "a deleted file still names its job"
        );
        std::fs::remove_dir_all(&base).ok();
    }
}
