//! Which jobs a review covers: the files asked for, or every file in the
//! folder, mapped to jobs the way `gz sync` maps them.

use anyhow::Result;
use std::path::{Path, PathBuf};

use crate::cmd::devloop::{self, same_code};
use crate::impact::changes::{map_files, SyncFolder};

use super::model::Code;

/// A job file in the folder, and its code (`None`: the file is gone).
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub file: String,
    pub job: String,
    pub code: Option<String>,
}

/// The files to review, and the given files that map to no job.
pub struct Candidates {
    pub jobs: Vec<Candidate>,
    pub unmapped: Vec<String>,
}

/// With `paths`, those files; without, every job file in the folder.
pub fn candidates(folder: &SyncFolder, paths: &[PathBuf]) -> Result<Candidates> {
    if paths.is_empty() {
        let mapping = devloop::load_mapping(&folder.root)?;
        let jobs = devloop::local_files(&folder.root, mapping.as_ref())?
            .into_iter()
            .map(|f| Candidate {
                file: f.rel,
                job: f.job,
                code: Some(f.code),
            })
            .collect();
        return Ok(Candidates {
            jobs,
            unmapped: Vec::new(),
        });
    }
    let mut jobs: Vec<Candidate> = Vec::new();
    let mut unmapped = Vec::new();
    for changed in map_files(folder, paths) {
        let Some(job) = changed.job else {
            unmapped.push(changed.file);
            continue;
        };
        if jobs.iter().any(|c| c.job == job) {
            continue;
        }
        let code = read_code(Path::new(&changed.file))?;
        jobs.push(Candidate {
            file: changed.file,
            job,
            code,
        });
    }
    Ok(Candidates { jobs, unmapped })
}

fn read_code(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(code) => Ok(Some(code.replace("\r\n", "\n"))),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(anyhow::Error::new(err).context(format!("cannot read {}", path.display()))),
    }
}

/// The local code against the saved code (`None`: no such job).
pub fn compare(file: &str, local: Option<&str>, saved: Option<&str>) -> Code {
    match (local, saved) {
        (None, _) => Code::Removed,
        (Some(_), None) => Code::New,
        (Some(local), Some(saved)) if same_code(local, saved) => Code::Unchanged,
        (Some(local), Some(saved)) => Code::Changed {
            diff: crate::assistant::tools::unified_diff(
                file,
                Some(&saved.replace("\r\n", "\n")),
                local,
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(name: &str, files: &[(&str, &str)]) -> (PathBuf, SyncFolder) {
        let root = std::env::temp_dir().join(format!("gz-review-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (rel, text) in files {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        std::fs::create_dir_all(&root).unwrap();
        let folder = SyncFolder::open(&root).unwrap();
        (root, folder)
    }

    #[test]
    fn without_paths_every_job_file_is_a_candidate() {
        let (root, folder) = folder("all", &[("clean_orders.py", "a\n"), ("etl/load.py", "b\n")]);
        let found = candidates(&folder, &[]).unwrap();
        let jobs: Vec<&str> = found.jobs.iter().map(|c| c.job.as_str()).collect();
        assert_eq!(jobs, ["clean-orders", "load"]);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn given_paths_map_to_jobs_and_the_rest_are_listed() {
        let (root, folder) = folder(
            "paths",
            &[
                (
                    "gz-sync.toml",
                    "[jobs]\n\"etl/a.py\" = \"load-a\"\n\"etl/gone.py\" = \"gone\"\n",
                ),
                ("etl/a.py", "x\r\n"),
                ("README.md", "r"),
            ],
        );
        let found = candidates(
            &folder,
            &[
                root.join("etl/a.py"),
                root.join("README.md"),
                root.join("etl/gone.py"),
            ],
        )
        .unwrap();
        assert_eq!(found.jobs[0].job, "load-a");
        assert_eq!(found.jobs[0].code.as_deref(), Some("x\n"));
        assert_eq!(found.jobs[1].job, "gone");
        assert_eq!(found.jobs[1].code, None, "a removed file has no code");
        assert_eq!(found.unmapped.len(), 1);
        assert!(found.unmapped[0].ends_with("README.md"));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn code_is_compared_ignoring_line_endings() {
        assert!(matches!(
            compare("a.py", Some("x\n"), Some("x\r\n")),
            Code::Unchanged
        ));
        assert!(matches!(compare("a.py", Some("x\n"), None), Code::New));
        assert!(matches!(compare("a.py", None, Some("x\n")), Code::Removed));
        let Code::Changed { diff } = compare("a.py", Some("y\n"), Some("x\n")) else {
            panic!("not changed");
        };
        assert!(diff.contains("-x") && diff.contains("+y"), "{diff}");
    }
}
