//! Append-only record of every tool call the assistant made or was refused,
//! next to the gz config (`audit.log`, owner-only). Holds the tool, target,
//! and decision — never file contents or command output.

use anyhow::{Context, Result};
use serde_json::json;
use std::io::Write;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct AuditLog {
    path: PathBuf,
}

/// One line of the log.
pub struct Entry<'a> {
    pub conversation_id: &'a str,
    pub run_id: &'a str,
    pub tool: &'a str,
    /// Path or command line, as shown to the user.
    pub target: &'a str,
    pub decision: &'a str,
    /// Why a mode blocked the call (`auto mode: <the review's reason>`).
    pub reason: Option<&'a str>,
    pub is_error: bool,
}

impl AuditLog {
    pub fn open() -> Result<Self> {
        let config = crate::config::config_path()?;
        let dir = config.parent().context("config path has no folder")?;
        Ok(Self {
            path: dir.join("audit.log"),
        })
    }

    #[cfg(test)]
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn record(&self, entry: &Entry) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("cannot create {}", dir.display()))?;
        }
        let mut line = json!({
            "time": crate::config::now_unix(),
            "conversationId": entry.conversation_id,
            "runId": entry.run_id,
            "tool": entry.tool,
            "target": entry.target,
            "decision": entry.decision,
            "isError": entry.is_error,
        });
        if let Some(reason) = entry.reason {
            line["reason"] = json!(reason);
        }
        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&self.path)
            .with_context(|| format!("cannot open {}", self.path.display()))?;
        writeln!(file, "{line}").context("cannot write the audit log")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_one_json_line_per_call() {
        let dir = std::env::temp_dir().join(format!("gz-audit-{}", std::process::id()));
        let log = AuditLog::at(dir.join("audit.log"));
        for decision in ["allowed", "denied"] {
            log.record(&Entry {
                conversation_id: "c1",
                run_id: "r1",
                tool: "read_file",
                target: "src/main.rs",
                decision,
                reason: None,
                is_error: decision == "denied",
            })
            .unwrap();
        }
        log.record(&Entry {
            conversation_id: "c1",
            run_id: "r1",
            tool: "run_command",
            target: "git push -f",
            decision: "blocked",
            reason: Some("auto mode: Force push was not asked for."),
            is_error: true,
        })
        .unwrap();
        let text = std::fs::read_to_string(dir.join("audit.log")).unwrap();
        let blocked: serde_json::Value =
            serde_json::from_str(text.lines().last().unwrap()).unwrap();
        assert_eq!(blocked["decision"], "blocked");
        assert_eq!(
            blocked["reason"],
            "auto mode: Force push was not asked for."
        );
        assert!(
            serde_json::from_str::<serde_json::Value>(text.lines().next().unwrap()).unwrap()
                ["reason"]
                .is_null(),
            "no reason unless something blocked it"
        );
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1]["decision"], "denied");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("audit.log"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::remove_dir_all(dir).ok();
    }
}
