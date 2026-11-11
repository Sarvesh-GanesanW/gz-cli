//! Impact before an edit: when the model is about to change a file that
//! `gz sync` maps to a job, the permission prompt (or, in auto mode, a
//! notice) says what the tables the job writes feed, e.g. "Affects 2
//! dashboards, 3 datasets downstream (gz impact sales.orders)".
//!
//! The lookup is bounded by [`LOOKUP_TIMEOUT`] and remembered per job for
//! the session; a slow or failed lookup shows nothing and never holds up
//! or fails the edit.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use tokio::sync::{mpsc, Mutex};

use crate::assistant::sandbox::Sandbox;
use crate::assistant::AssistantEvent;
use crate::cmd::devloop::{MAPPING_FILE, STATE_FILE};
use crate::cmd::{jobs, Runtime};
use crate::http::ApiClient;
use crate::impact::changes::SyncFolder;
use crate::impact::{lookup, summary_line, Target};
use crate::output::OutputFormat;

pub const LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);
/// Hops the prompt's count covers (`gz impact`'s default).
const DEPTH: u32 = 3;

pub struct EditImpact {
    http: ApiClient,
    sandbox: Sandbox,
    notices: mpsc::UnboundedSender<AssistantEvent>,
    timeout: Duration,
    /// The line for each job, by its folder and name; `None` when there is
    /// nothing to say (or the lookup failed).
    lines: Mutex<HashMap<(PathBuf, String), Option<String>>>,
}

impl EditImpact {
    pub fn new(
        http: ApiClient,
        sandbox: Sandbox,
        notices: mpsc::UnboundedSender<AssistantEvent>,
    ) -> Self {
        Self {
            http,
            sandbox,
            notices,
            timeout: LOOKUP_TIMEOUT,
            lines: Mutex::new(HashMap::new()),
        }
    }

    #[cfg(test)]
    fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The impact line for writing `target` (as the prompt names it), when
    /// it maps to a job.
    pub async fn line_for(&self, target: &str) -> Option<String> {
        let file = self.sandbox.resolve(target).ok()?;
        let folder = sync_folder_of(&file, &self.sandbox.roots())?;
        let synced = SyncFolder::open(&folder).ok()?;
        let job = synced.jobs.job_for(&synced.relative(&file)?)?;
        let key = (folder, job.clone());
        if let Some(line) = self.lines.lock().await.get(&key) {
            return line.clone();
        }
        let line = tokio::time::timeout(self.timeout, self.look_up(&synced, &job))
            .await
            .ok()
            .and_then(Result::ok)
            .flatten();
        self.lines.lock().await.insert(key, line.clone());
        line
    }

    /// Shows `text` in the transcript (auto mode has no prompt to carry it).
    pub fn notify(&self, text: String) {
        let _ = self.notices.send(AssistantEvent::Notice(text));
    }

    async fn look_up(&self, folder: &SyncFolder, job: &str) -> Result<Option<String>> {
        let Some(id) = self.job_id(folder, job).await? else {
            return Ok(None);
        };
        let Some(tables) = lookup::job_outputs(&self.http, id).await? else {
            return Ok(None);
        };
        if tables.is_empty() {
            return Ok(None);
        }
        let names: Vec<String> = tables.iter().map(|(_, name)| name.clone()).collect();
        let targets = tables
            .into_iter()
            .map(|(asset, name)| Target {
                text: name.clone(),
                table: name,
                column: None,
                assets: vec![asset],
            })
            .collect();
        let mut report = lookup::impact_of_assets(&self.http, targets, DEPTH).await?;
        report.drop_jobs(&[id]);
        Ok(Some(format!(
            "{} (gz impact {})",
            summary_line(&report.counts()),
            names.join(" ")
        )))
    }

    /// `[impact.jobs]` first, then the folder's project's job list.
    async fn job_id(&self, folder: &SyncFolder, job: &str) -> Result<Option<i64>> {
        if let Some(id) = folder.config.jobs.get(job) {
            return Ok(Some(*id));
        }
        let Some(project) = folder.project(None) else {
            return Ok(None);
        };
        // Quiet and not a table: the UI owns the screen, so no spinner.
        let rt = Runtime {
            http: self.http.clone(),
            format: OutputFormat::Json,
            limit: None,
            quiet: true,
            asked_quiet: true,
        };
        Ok(jobs::project_jobs(&rt, project)
            .await?
            .into_iter()
            .find(|j| j.name == job)
            .map(|j| j.id))
    }
}

/// The nearest folder at or above `file`, inside an open folder, that
/// `gz sync` has set up (a mapping or a sync record).
fn sync_folder_of(file: &Path, roots: &[PathBuf]) -> Option<PathBuf> {
    let root = roots.iter().find(|r| file.starts_with(r))?;
    file.ancestors()
        .skip(1)
        .take_while(|dir| dir.starts_with(root))
        .find(|dir| dir.join(MAPPING_FILE).is_file() || dir.join(STATE_FILE).is_file())
        .map(Path::to_path_buf)
}

/// The line [`stub_impact`]'s job gets.
#[cfg(test)]
pub(crate) const STUB_LINE: &str = "Affects 1 dashboard, 1 dataset downstream (gz impact db.out)";

/// An [`EditImpact`] for `root`, where `file` syncs to job `load` (2233),
/// which writes `db.out`, read by dataset Sales on dashboard Board; the
/// stub serves one lookup.
#[cfg(test)]
pub(crate) async fn stub_impact(
    root: &Path,
    sandbox: Sandbox,
    file: &str,
) -> (
    EditImpact,
    mpsc::UnboundedReceiver<AssistantEvent>,
    tokio::task::JoinHandle<Vec<String>>,
) {
    use crate::tui::test_stub::{client, serve, Reply};
    std::fs::write(
        root.join(MAPPING_FILE),
        format!("[jobs]\n\"{file}\" = \"load\"\n\n[impact.jobs]\nload = 2233\n"),
    )
    .unwrap();
    let (base, server) = serve(vec![
        Reply::json(
            r#"{"graph": {"assets": [{"assetId": 10, "assetType": "TABLE", "qualifiedName": "db.out"}],
                "relations": [{"sourceAssetId": 9, "targetAssetId": 10}]}}"#,
        ),
        Reply::json(
            r#"{"graph": {"centerAssetId": 10,
                "assets": [{"assetId": 10, "assetType": "TABLE", "displayName": "db.out"},
                           {"assetId": 11, "assetType": "DATASET", "displayName": "Sales", "qualifiedName": "dataset:5"}],
                "relations": [{"sourceAssetId": 10, "targetAssetId": 11, "jobVersionId": 1}],
                "jobVersions": [{"jobVersionId": 1, "datasetId": 5, "jobName": "Sales"},
                                {"jobVersionId": 2, "jobId": 2233, "jobName": "load"}],
                "assetWorkloads": [{"assetId": 10, "jobVersionId": 2, "role": "SOURCE"}],
                "dashboards": [{"dashboardId": 3, "name": "Board", "datasetIds": [5], "visualizationIds": []}]}}"#,
        ),
        Reply::json(r#"{"definitions": []}"#),
    ])
    .await;
    let (tx, rx) = mpsc::unbounded_channel();
    (EditImpact::new(client(&base), sandbox, tx), rx, server)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::test_stub::client;

    fn workspace(name: &str) -> (PathBuf, Sandbox) {
        crate::assistant::sandbox::tests::temp_workspace(name)
    }

    #[test]
    fn finds_the_nearest_synced_folder_inside_the_workspace() {
        let (root, _) = workspace("impact-folder");
        std::fs::create_dir_all(root.join("jobs/etl")).unwrap();
        std::fs::write(root.join("jobs/.gz-sync.json"), "{\"project\": 7}").unwrap();
        let file = root.join("jobs/etl/load.py");
        assert_eq!(
            sync_folder_of(&file, std::slice::from_ref(&root)),
            Some(root.join("jobs"))
        );
        assert_eq!(
            sync_folder_of(&root.join("src/main.py"), std::slice::from_ref(&root)),
            None,
            "not under a synced folder"
        );
    }

    #[tokio::test]
    async fn a_job_file_gets_its_downstream_once_per_session() {
        let (root, sandbox) = workspace("impact-line");
        let (impact, _notices, server) = stub_impact(&root, sandbox, "load.py").await;
        let line = impact.line_for("load.py").await;
        assert_eq!(
            line.as_deref(),
            Some(STUB_LINE),
            "the job being edited is not its own dependent"
        );
        assert_eq!(
            impact.line_for("load.py").await,
            line,
            "the second edit is answered from the session's cache"
        );
        assert_eq!(impact.line_for("notes.md").await, None, "not a job file");
        let seen = server.await.unwrap();
        assert!(
            seen[0].starts_with("GET /etl/lineage/jobs/2233?"),
            "{}",
            seen[0]
        );
        assert!(
            seen[1].starts_with("GET /etl/lineage/assets/10?"),
            "{}",
            seen[1]
        );
    }

    #[tokio::test]
    async fn a_slow_lookup_gives_no_line_instead_of_holding_the_edit() {
        let (root, sandbox) = workspace("impact-slow");
        std::fs::write(
            root.join(MAPPING_FILE),
            "[jobs]\n\"load.py\" = \"load\"\n\n[impact.jobs]\nload = 1\n",
        )
        .unwrap();
        // Accepts the connection and never answers.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let _hold = tokio::spawn(async move {
            let _socket = listener.accept().await;
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        let (tx, _rx) = mpsc::unbounded_channel();
        let impact =
            EditImpact::new(client(&base), sandbox, tx).with_timeout(Duration::from_millis(200));
        let started = std::time::Instant::now();
        assert_eq!(impact.line_for("load.py").await, None);
        let waited = started.elapsed();
        assert!(
            waited >= Duration::from_millis(200) && waited < Duration::from_secs(2),
            "gave up at the timeout: {waited:?}"
        );
    }
}
