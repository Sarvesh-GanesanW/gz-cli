//! `gz table rollback`: move a table's main back to an earlier snapshot,
//! after showing what that undoes.
//!
//! The change is one call to the lakehouse provider,
//! `POST /lakehouse/tables/{warehouse}/{database}/{table}/rollback`
//! (groundzero-iceberg-provider-api app_routes/table_versions.py
//! `rollback_table`, iceberg/table_rollback.py; the web app calls it from
//! pages/Utils/lakehouseTableUtils.js `rollbackTable`). gz sends the main
//! snapshot it previewed as `expectedSnapshotId`, so a commit made while the
//! user read the preview refuses the rollback instead of being undone
//! unseen.

use anyhow::{bail, Context, Result};
use reqwest::Method;
use serde_json::{json, Value};

use super::Located;
use crate::cli::TableRollbackArgs;
use crate::cmd::table_history::report::{commit_line, row_lines, snapshot_line, totals_lines};
use crate::cmd::table_history::table_width;
use crate::cmd::{seg, Runtime};
use crate::output::{self, OutputFormat};
use crate::sql::Runner;
use crate::table_history::diff::{self, Comparison, RowDiff, RowRequest};
use crate::table_history::{History, Snapshot, TableName};

const SVC: &str = "lakehouse";

/// Where main is, where it goes, and the commits in between.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Plan {
    current: Snapshot,
    target: Snapshot,
    /// main's commits the rollback takes away, newest first.
    undone: Vec<Snapshot>,
    /// Commits main gains, newest first; empty when the target is an
    /// earlier snapshot of main.
    applied: Vec<Snapshot>,
}

impl Plan {
    fn new(history: &History, to: &str) -> Result<Self> {
        let current = history.resolve("current")?.clone();
        let target = history.resolve(to).context("--to")?.clone();
        if target.id == current.id {
            bail!(
                "'{to}' is snapshot {}, which main is already at; `gz table versions` lists the others",
                target.id
            );
        }
        let main = history.line(&current);
        let target_line = history.line(&target);
        let on = |line: &[&Snapshot], snapshot: &Snapshot| line.iter().any(|s| s.id == snapshot.id);
        let undone = main
            .iter()
            .filter(|s| !on(&target_line, s))
            .map(|s| (*s).clone())
            .collect();
        let applied = target_line
            .iter()
            .filter(|s| !on(&main, s))
            .map(|s| (*s).clone())
            .collect();
        Ok(Self {
            current,
            target,
            undone,
            applied,
        })
    }

    fn is_ancestor(&self) -> bool {
        self.applied.is_empty()
    }

    /// Why a target off main's history needs `--allow-non-ancestor`.
    fn non_ancestor_refusal(&self) -> String {
        format!(
            "snapshot {} is not an earlier snapshot of main: it is on another branch, or an earlier \
             rollback left it out. Moving main there would undo {} commit(s) of main and bring in {} \
             commit(s) main never had. Check them with `gz diff --from current --to {}`, then add \
             --allow-non-ancestor to do it anyway.",
            self.target.id,
            self.undone.len(),
            self.applied.len(),
            self.target.id
        )
    }

    /// The comparison `gz diff` would make from main now to the target.
    fn comparison(&self) -> Comparison {
        Comparison {
            from: self.current.clone(),
            to: self.target.clone(),
            commits: None,
        }
    }
}

struct Subject<'a> {
    table: &'a TableName,
    warehouse: &'a str,
    history: &'a History,
}

fn commits_section(title: String, commits: &[Snapshot]) -> Vec<String> {
    let mut lines = vec![String::new(), title];
    lines.extend(commits.iter().map(commit_line));
    lines
}

fn preview_text(subject: &Subject, plan: &Plan, rows: Option<&RowDiff>, width: usize) -> String {
    let mut lines = vec![
        format!("Roll back {} · {}", subject.table, subject.warehouse),
        format!(
            "  main now  {}",
            snapshot_line(subject.history, &plan.current)
        ),
        format!(
            "  back to   {}",
            snapshot_line(subject.history, &plan.target)
        ),
    ];
    if !plan.undone.is_empty() {
        lines.extend(commits_section(
            format!(
                "Undoes {} commit(s); they stay in the history until they expire",
                plan.undone.len()
            ),
            &plan.undone,
        ));
    }
    if !plan.is_ancestor() {
        lines.extend(commits_section(
            format!("Brings in {} commit(s) main never had", plan.applied.len()),
            &plan.applied,
        ));
    }
    lines.push(String::new());
    lines.extend(totals_lines(&plan.comparison()));
    if let Some(rows) = rows {
        lines.push(String::new());
        lines.extend(row_lines(rows, width));
    }
    lines.join("\n") + "\n"
}

fn snapshots_json(commits: &[Snapshot]) -> Value {
    commits
        .iter()
        .map(|s| {
            json!({
                "snapshotId": s.id,
                "committedAt": s.committed_at,
                "operation": s.operation,
                "addedRecords": s.count("added-records"),
                "deletedRecords": s.count("deleted-records"),
                "run": s.run(),
            })
        })
        .collect()
}

fn preview_json(subject: &Subject, plan: &Plan, rows: Option<&RowDiff>) -> Value {
    let records = |s: &Snapshot| s.count("total-records");
    json!({
        "table": subject.table.to_string(),
        "warehouse": subject.warehouse,
        "currentSnapshotId": plan.current.id,
        "targetSnapshotId": plan.target.id,
        "ancestor": plan.is_ancestor(),
        "undone": snapshots_json(&plan.undone),
        "applied": snapshots_json(&plan.applied),
        "records": {"before": records(&plan.current), "after": records(&plan.target)},
        "rows": rows.map(|rows| json!({
            "added": rows.counts.added,
            "removed": rows.counts.removed,
            "changed": rows.counts.changed,
            "key": rows.key,
        })),
    })
}

fn rollback_path(warehouse: &str, table: &TableName) -> String {
    format!(
        "/lakehouse/tables/{}/{}/{}/rollback",
        seg(warehouse),
        seg(&table.database),
        seg(&table.table)
    )
}

fn rollback_body(plan: &Plan, allow_non_ancestor: bool) -> Value {
    json!({
        "snapshotId": plan.target.id,
        "expectedSnapshotId": plan.current.id,
        "allowNonAncestor": allow_non_ancestor,
    })
}

/// The command that puts main back where it was.
fn undo_command(warehouse: &str, table: &TableName, previous: &str) -> String {
    format!("gz table rollback {warehouse}.{table} --to {previous} --allow-non-ancestor")
}

async fn compare_rows(
    rt: &Runtime,
    args: &TableRollbackArgs,
    located: &Located,
    plan: &Plan,
) -> Result<RowDiff> {
    let session = Runner::open(rt, &located.shell).await?;
    if !rt.quiet {
        output::info("Comparing rows…");
    }
    let request = RowRequest {
        table: &located.table,
        from: &plan.current,
        to: &plan.target,
        key: &args.key,
        sample: args.sample,
    };
    let compared = diff::compare_rows(&session, &request).await;
    session.close(rt, args.shell.keep_session).await;
    compared
}

pub async fn run(rt: &Runtime, args: &TableRollbackArgs) -> Result<()> {
    let located = Located::find(rt, &args.table, &args.shell).await?;
    let plan = Plan::new(&located.history, &args.to)?;
    if !plan.is_ancestor() && !args.allow_non_ancestor {
        bail!("{}", plan.non_ancestor_refusal());
    }
    let rows = if args.rows || !args.key.is_empty() {
        Some(compare_rows(rt, args, &located, &plan).await?)
    } else {
        None
    };
    let subject = Subject {
        table: &located.table,
        warehouse: &located.warehouse,
        history: &located.history,
    };
    let json_output = rt.format != OutputFormat::Table;
    let preview = preview_text(&subject, &plan, rows.as_ref(), table_width());
    // With -o json the preview goes to stderr so stdout stays one JSON value.
    if json_output {
        eprint!("{preview}");
    } else {
        print!("{preview}");
    }
    crate::cmd::confirm(
        args.yes,
        &format!(
            "Move main of {} to snapshot {}?",
            located.table, plan.target.id
        ),
    )?;

    let reply = rt
        .http
        .request_json(
            SVC,
            Method::POST,
            &rollback_path(&located.warehouse, &located.table),
            &[],
            Some(rollback_body(&plan, args.allow_non_ancestor)),
        )
        .await
        .with_context(|| {
            format!(
                "rolling back {} failed; `gz table versions {}` shows where main is now",
                located.table, located.table
            )
        })?;
    let result = &reply["response"];
    let undo = undo_command(&located.warehouse, &located.table, &plan.current.id);
    if json_output {
        let mut value = preview_json(&subject, &plan, rows.as_ref());
        value["result"] = result.clone();
        value["undo"] = json!(undo);
        return rt.show(value).await;
    }
    let now = result["currentSnapshotId"]
        .as_str()
        .unwrap_or(&plan.target.id);
    output::success(&format!(
        "main of {} is now at snapshot {now} (was {}). To undo: {undo}",
        located.table, plan.current.id
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table_history::test_support::history;

    fn ids(snapshots: &[Snapshot]) -> Vec<&str> {
        snapshots.iter().map(|s| s.id.as_str()).collect()
    }

    fn subject<'a>(table: &'a TableName, history: &'a History) -> Subject<'a> {
        Subject {
            table,
            warehouse: "Lineage",
            history,
        }
    }

    #[test]
    fn rolling_back_along_main_undoes_the_later_commits() {
        let history = history();
        let plan = Plan::new(&history, "release").unwrap();
        assert_eq!(plan.current.id, "104");
        assert_eq!(ids(&plan.undone), ["104", "103"]);
        assert!(plan.is_ancestor());

        let table = TableName::parse("sales.orders").unwrap();
        let text = preview_text(&subject(&table, &history), &plan, None, 80);
        assert!(
            text.contains("main now  104 · 2026-09-04 09:00 UTC · current, branch main"),
            "{text}"
        );
        assert!(
            text.contains("back to   102 · 2026-09-02 09:00 UTC · tag release"),
            "{text}"
        );
        assert!(text.contains("Undoes 2 commit(s)"), "{text}");
        assert!(text.contains("Records  40 → 20 (−20)"), "{text}");
        assert!(!text.contains("Brings in"), "{text}");
    }

    #[test]
    fn a_target_off_main_is_refused_with_what_it_would_do() {
        let history = history();
        let plan = Plan::new(&history, "audit").unwrap();
        assert!(!plan.is_ancestor());
        assert_eq!(ids(&plan.undone), ["104", "103"]);
        assert_eq!(ids(&plan.applied), ["201"]);
        let refusal = plan.non_ancestor_refusal();
        assert!(
            refusal.contains("not an earlier snapshot of main"),
            "{refusal}"
        );
        assert!(
            refusal.contains("undo 2 commit(s) of main and bring in 1"),
            "{refusal}"
        );
        assert!(refusal.contains("--allow-non-ancestor"), "{refusal}");

        let table = TableName::parse("sales.orders").unwrap();
        let text = preview_text(&subject(&table, &history), &plan, None, 80);
        assert!(
            text.contains("Brings in 1 commit(s) main never had"),
            "{text}"
        );
    }

    #[test]
    fn main_itself_is_not_a_rollback_target() {
        let err = format!("{:#}", Plan::new(&history(), "current").unwrap_err());
        assert!(err.contains("main is already at"), "{err}");
    }

    #[test]
    fn the_request_names_the_previewed_main_and_the_undo_names_it_too() {
        let plan = Plan::new(&history(), "previous").unwrap();
        assert_eq!(
            rollback_body(&plan, false),
            json!({"snapshotId": "103", "expectedSnapshotId": "104", "allowNonAncestor": false})
        );
        let table = TableName::parse("sales.or ders").unwrap();
        assert_eq!(
            rollback_path("Ware house", &table),
            "/lakehouse/tables/Ware%20house/sales/or%20ders/rollback"
        );
        assert_eq!(
            undo_command("Lineage", &TableName::parse("sales.orders").unwrap(), "104"),
            "gz table rollback Lineage.sales.orders --to 104 --allow-non-ancestor"
        );
    }
}
