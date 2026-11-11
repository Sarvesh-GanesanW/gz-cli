//! A table's quality gate and its branches' verdicts, as the lakehouse
//! provider reports them (`GET /lakehouse/tables/{w}/{d}/{t}/quality`):
//! `{"required": bool, "branches": [{"name", "headSnapshotId", "verdict",
//! "current"}]}`, where a verdict is the `gz.quality.branch.<name>` table
//! property the runtime's `runTableDataQualityChecks` wrote.

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::cmd::lakehouse_table::LakehouseTable;
use crate::table_history::when;

pub const SHOW_COLUMNS: [&str; 5] = ["branch", "verdict", "snapshot", "failed checks", "checked"];

/// What one check run concluded about a branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub snapshot: String,
    pub passed: bool,
    pub failed_checks: Vec<String>,
    pub checked_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchQuality {
    pub name: String,
    pub head: String,
    pub verdict: Option<Verdict>,
    /// The verdict is for the branch's current head.
    pub current: bool,
}

/// Whether a branch may be published as far as quality goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Standing<'a> {
    Unchecked,
    Failed(&'a Verdict),
    Stale(&'a Verdict),
    Passed,
}

impl BranchQuality {
    fn parse(value: &Value) -> Result<Self> {
        let name = value["name"]
            .as_str()
            .context("a branch came without a name")?;
        let verdict = match &value["verdict"] {
            Value::Null => None,
            verdict => Some(Verdict {
                snapshot: text(&verdict["snapshotId"]).unwrap_or_default(),
                passed: verdict["passed"] == json!(true),
                failed_checks: verdict["failedChecks"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|name| name.as_str().map(str::to_string))
                    .collect(),
                checked_at: text(&verdict["checkedAt"]),
            }),
        };
        Ok(Self {
            name: name.to_string(),
            head: text(&value["headSnapshotId"]).unwrap_or_default(),
            verdict,
            current: value["current"] == json!(true),
        })
    }

    /// A failed verdict counts as failed whatever snapshot it was for.
    pub fn standing(&self) -> Standing<'_> {
        match &self.verdict {
            None => Standing::Unchecked,
            Some(verdict) if !verdict.passed => Standing::Failed(verdict),
            Some(verdict) if !self.current => Standing::Stale(verdict),
            Some(_) => Standing::Passed,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableQuality {
    pub required: bool,
    pub branches: Vec<BranchQuality>,
}

impl TableQuality {
    /// From the reply's `response`.
    pub fn parse(response: &Value) -> Result<Self> {
        let required = response["required"]
            .as_bool()
            .context("the quality reply has no 'required'")?;
        let branches = response["branches"]
            .as_array()
            .context("the quality reply has no 'branches'")?
            .iter()
            .map(BranchQuality::parse)
            .collect::<Result<_>>()?;
        Ok(Self { required, branches })
    }

    /// Why quality stops `branch` from being published, if it does.
    pub fn blocker(&self, table: &LakehouseTable, branch: &str) -> Option<String> {
        if !self.required {
            return None;
        }
        let named = self.branches.iter().find(|named| named.name == branch);
        let standing = named.map_or(Standing::Unchecked, BranchQuality::standing);
        let head = named.map_or("?", |named| named.head.as_str());
        match standing {
            Standing::Passed => None,
            Standing::Unchecked => Some(format!(
                "{table}: branch {branch} has no quality verdict; run runTableDataQualityChecks on it in a `gz jobs try` run"
            )),
            Standing::Failed(verdict) if verdict.failed_checks.is_empty() => Some(format!(
                "{table}: branch {branch} failed its quality checks"
            )),
            Standing::Failed(verdict) => Some(format!(
                "{table}: branch {branch} failed its quality checks ({})",
                verdict.failed_checks.join(", ")
            )),
            Standing::Stale(verdict) => Some(format!(
                "{table}: the quality verdict is for snapshot {}, but branch {branch} is now at {head}; check it again",
                verdict.snapshot
            )),
        }
    }

    /// One row per branch for `gz quality show`.
    pub fn rows(&self) -> Value {
        self.branches.iter().map(show_row).collect()
    }
}

fn show_row(branch: &BranchQuality) -> Value {
    let verdict = branch.verdict.as_ref();
    let outcome = match verdict {
        None => "none",
        Some(v) if v.passed => "passed",
        Some(_) => "failed",
    };
    let snapshot = verdict.map(|_| if branch.current { "current" } else { "stale" });
    let failed = verdict
        .filter(|v| !v.failed_checks.is_empty())
        .map(|v| v.failed_checks.join(", "));
    json!({
        "branch": branch.name,
        "verdict": outcome,
        "snapshot": snapshot,
        "failed checks": failed,
        "checked": verdict.and_then(|v| v.checked_at.as_deref()).map(when::display),
    })
}

fn text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quality(required: bool) -> TableQuality {
        TableQuality::parse(&json!({
            "required": required,
            "branches": [
                {"name": "try-fresh", "headSnapshotId": "13", "verdict": null, "current": false},
                {"name": "try-good", "headSnapshotId": "14", "current": true, "verdict": {
                    "version": 1, "snapshotId": "14", "passed": true, "failOn": "error",
                    "worstSeverity": "warn", "failedChecks": [], "checkedAt": "2026-11-11T10:42:07Z",
                }},
                {"name": "try-moved", "headSnapshotId": "16", "current": false, "verdict": {
                    "version": 1, "snapshotId": "15", "passed": true, "failOn": "error",
                    "worstSeverity": "ok", "failedChecks": [], "checkedAt": "2026-11-11T09:00:00Z",
                }},
                {"name": "try-bad", "headSnapshotId": "17", "current": true, "verdict": {
                    "version": 1, "snapshotId": "17", "passed": false, "failOn": "error",
                    "worstSeverity": "error", "failedChecks": ["ranges", "missing_values"],
                    "checkedAt": "2026-11-11T11:05:00Z",
                }},
            ],
        }))
        .unwrap()
    }

    fn orders() -> LakehouseTable {
        LakehouseTable::parse("Lineage.sales.orders").unwrap()
    }

    #[test]
    fn rows_show_none_current_and_stale_verdicts() {
        assert_eq!(
            quality(true).rows(),
            json!([
                {"branch": "try-fresh", "verdict": "none", "snapshot": null,
                 "failed checks": null, "checked": null},
                {"branch": "try-good", "verdict": "passed", "snapshot": "current",
                 "failed checks": null, "checked": "2026-11-11 10:42 UTC"},
                {"branch": "try-moved", "verdict": "passed", "snapshot": "stale",
                 "failed checks": null, "checked": "2026-11-11 09:00 UTC"},
                {"branch": "try-bad", "verdict": "failed", "snapshot": "current",
                 "failed checks": "ranges, missing_values", "checked": "2026-11-11 11:05 UTC"},
            ])
        );
    }

    #[test]
    fn only_a_passing_verdict_on_the_head_lets_a_required_table_publish() {
        let gate = quality(true);
        let blocker = |branch| gate.blocker(&orders(), branch);
        assert_eq!(blocker("try-good"), None);
        assert_eq!(
            blocker("try-fresh").as_deref(),
            Some("Lineage.sales.orders: branch try-fresh has no quality verdict; run runTableDataQualityChecks on it in a `gz jobs try` run")
        );
        assert_eq!(
            blocker("try-bad").as_deref(),
            Some("Lineage.sales.orders: branch try-bad failed its quality checks (ranges, missing_values)")
        );
        assert_eq!(
            blocker("try-moved").as_deref(),
            Some("Lineage.sales.orders: the quality verdict is for snapshot 15, but branch try-moved is now at 16; check it again")
        );
        assert!(
            blocker("try-unknown").is_some(),
            "no entry means no verdict"
        );
    }

    #[test]
    fn a_table_without_the_requirement_never_blocks() {
        let gate = quality(false);
        for branch in ["try-fresh", "try-bad", "try-moved"] {
            assert_eq!(gate.blocker(&orders(), branch), None);
        }
    }

    #[test]
    fn a_reply_without_the_gate_state_is_refused() {
        assert!(TableQuality::parse(&json!({"branches": []})).is_err());
    }
}
