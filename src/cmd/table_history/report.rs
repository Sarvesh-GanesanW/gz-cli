//! What `gz table versions`, `gz diff` and `gz bisect` print: plain text for
//! a terminal and one JSON value for `-o json`.

use serde_json::{json, Value};

use crate::cmd::datafs::human_bytes;
use crate::sql::results::{self, Page};
use crate::table_history::bisect::{Check, Outcome, Probe, Search};
use crate::table_history::diff::{ChangedRow, Comparison, RowDiff, SchemaChange};
use crate::table_history::{History, Snapshot, TableName};
use crate::tui::format::group_digits;

/// The table a report is about.
pub struct Subject<'a> {
    pub table: &'a TableName,
    pub warehouse: &'a str,
    pub history: &'a History,
}

fn grouped(number: u64) -> String {
    group_digits(&number.to_string())
}

fn rows_word(count: u64) -> String {
    format!(
        "{} row{}",
        grouped(count),
        if count == 1 { "" } else { "s" }
    )
}

/// `104 · 2026-09-04 09:00 UTC · current, branch main`.
fn snapshot_line(history: &History, snapshot: &Snapshot) -> String {
    let mut parts = vec![snapshot.id.clone(), snapshot.committed()];
    let names = history.names(snapshot);
    if !names.is_empty() {
        parts.push(names.join(", "));
    }
    parts.join(" · ")
}

/// `append · +10,000 records · −12 records`, from what the writer recorded.
fn change_text(snapshot: &Snapshot) -> String {
    let mut parts: Vec<String> = snapshot.operation.clone().into_iter().collect();
    if let Some(added) = snapshot.count("added-records").filter(|n| *n > 0) {
        parts.push(format!("+{} records", grouped(added)));
    }
    if let Some(deleted) = snapshot.count("deleted-records").filter(|n| *n > 0) {
        parts.push(format!("−{} records", grouped(deleted)));
    }
    parts.join(" · ")
}

fn snapshot_json(history: &History, snapshot: &Snapshot) -> Value {
    json!({
        "snapshotId": snapshot.id,
        "parentSnapshotId": snapshot.parent,
        "committedAt": snapshot.committed_at,
        "operation": snapshot.operation,
        "names": history.names(snapshot),
        "summary": snapshot.summary,
    })
}

// ---------- gz table versions ----------

/// One row per snapshot, newest first, for the table view.
pub fn versions_rows(history: &History) -> Value {
    let count = |snapshot: &Snapshot, key: &str| snapshot.count(key).map(grouped);
    history
        .snapshots
        .iter()
        .map(|snapshot| {
            json!({
                "snapshot": snapshot.id,
                "committed": snapshot.committed(),
                "operation": snapshot.operation,
                "added": count(snapshot, "added-records"),
                "deleted": count(snapshot, "deleted-records"),
                "records": count(snapshot, "total-records"),
                "names": history.names(snapshot).join(", "),
                "written by": snapshot.summary.get("app-name"),
            })
        })
        .collect()
}

pub const VERSION_COLUMNS: [&str; 8] = [
    "snapshot",
    "committed",
    "operation",
    "added",
    "deleted",
    "records",
    "names",
    "written by",
];

// ---------- gz diff ----------

/// `30 → 40 (+10)`.
fn before_after(before: u64, after: u64) -> String {
    let delta = match after.cmp(&before) {
        std::cmp::Ordering::Greater => format!("+{}", grouped(after - before)),
        std::cmp::Ordering::Less => format!("−{}", grouped(before - after)),
        std::cmp::Ordering::Equal => "no change".to_string(),
    };
    format!("{} → {} ({delta})", grouped(before), grouped(after))
}

fn totals_lines(comparison: &Comparison) -> Vec<String> {
    let both = |key: &str| Some((comparison.from.count(key)?, comparison.to.count(key)?));
    let mut lines = Vec::new();
    if let Some((before, after)) = both("total-records") {
        lines.push(format!("Records  {}", before_after(before, after)));
    }
    if let Some((before, after)) = both("total-data-files") {
        let sizes = both("total-files-size")
            .map(|(before, after)| format!(" · {} → {}", human_bytes(before), human_bytes(after)))
            .unwrap_or_default();
        lines.push(format!(
            "Files    {} → {}{sizes}",
            grouped(before),
            grouped(after)
        ));
    }
    lines
}

fn commit_lines(comparison: &Comparison) -> Vec<String> {
    let Some(commits) = &comparison.commits else {
        return vec![
            "The newer side was not built on the older one (another branch, or the sides are swapped),".to_string(),
            "so there is no list of commits between them; the two states are compared as they are.".to_string(),
        ];
    };
    let mut lines = vec![format!("Commits ({})", commits.len())];
    for commit in commits {
        let writer = commit
            .summary
            .get("app-name")
            .map(|name| format!("  {name}"))
            .unwrap_or_default();
        lines.push(format!(
            "  {}  {}  {}{writer}",
            commit.committed(),
            commit.id,
            change_text(commit)
        ));
    }
    lines
}

fn schema_lines(schema: &SchemaChange) -> Vec<String> {
    if schema.is_empty() {
        return vec!["Schema   unchanged".to_string()];
    }
    let mut lines = vec!["Schema".to_string()];
    lines.extend(
        schema
            .added
            .iter()
            .map(|c| format!("  + {} {}", c.name, c.kind)),
    );
    lines.extend(
        schema
            .removed
            .iter()
            .map(|c| format!("  − {} {}", c.name, c.kind)),
    );
    lines.extend(
        schema
            .retyped
            .iter()
            .map(|c| format!("  ~ {} {} → {}", c.name, c.from, c.to)),
    );
    lines
}

fn counts_line(rows: &RowDiff) -> String {
    let counts = &rows.counts;
    let added_removed = format!(
        "+{} added · −{} removed",
        grouped(counts.added),
        grouped(counts.removed)
    );
    match counts.changed {
        Some(changed) => format!(
            "Rows     {added_removed} · ~{} changed  (matched on {})",
            grouped(changed),
            rows.key.join(", ")
        ),
        None => format!(
            "Rows     {added_removed}  (whole rows compared: a changed row counts as one removed and one added; --key tells them apart)"
        ),
    }
}

fn sample_lines(title: &str, total: u64, page: &Page, width: usize) -> Vec<String> {
    let mut lines = vec![
        String::new(),
        format!(
            "{title} (showing {} of {})",
            page.rows.len(),
            grouped(total)
        ),
    ];
    let table = crate::sql::render::plain(&results::render(page, width, false));
    lines.extend(table.lines().map(str::to_string));
    lines
}

fn changed_lines(rows: &[ChangedRow], total: u64) -> Vec<String> {
    let mut lines = vec![
        String::new(),
        format!("Changed (showing {} of {})", rows.len(), grouped(total)),
    ];
    for row in rows {
        let key: Vec<String> = row.key.iter().map(|(k, v)| format!("{k}={v}")).collect();
        lines.push(format!("  {}", key.join(" · ")));
        let widest = row
            .changes
            .iter()
            .map(|(c, _, _)| c.len())
            .max()
            .unwrap_or(0);
        for (column, before, after) in &row.changes {
            lines.push(format!("    {column:<widest$}  {before} → {after}"));
        }
    }
    lines
}

fn row_lines(rows: &RowDiff, width: usize) -> Vec<String> {
    let mut lines = schema_lines(&rows.schema);
    lines.push(counts_line(rows));
    if !rows.skipped.is_empty() {
        let skipped: Vec<String> = rows
            .skipped
            .iter()
            .map(|c| format!("{} ({})", c.name, c.kind))
            .collect();
        lines.push(format!(
            "Not compared: {} (map columns can't be compared)",
            skipped.join(", ")
        ));
    }
    if !rows.column_changes.is_empty() {
        lines.push(String::new());
        lines.push("Changed columns".to_string());
        let widest = rows
            .column_changes
            .iter()
            .map(|c| c.column.len())
            .max()
            .unwrap_or(0);
        for change in &rows.column_changes {
            lines.push(format!(
                "  {:<widest$}  {}",
                change.column,
                rows_word(change.rows)
            ));
        }
    }
    if let Some(page) = &rows.added {
        lines.extend(sample_lines("Added", rows.counts.added, page, width));
    }
    if let Some(page) = &rows.removed {
        lines.extend(sample_lines("Removed", rows.counts.removed, page, width));
    }
    if !rows.changed.is_empty() {
        lines.extend(changed_lines(
            &rows.changed,
            rows.counts.changed.unwrap_or(0),
        ));
    }
    lines
}

/// The two snapshots, the commits between them and the totals, from the
/// history alone.
fn summary_lines(subject: &Subject, comparison: &Comparison) -> Vec<String> {
    let mut lines = vec![
        format!("{} · {}", subject.table, subject.warehouse),
        format!(
            "  from  {}",
            snapshot_line(subject.history, &comparison.from)
        ),
        format!("  to    {}", snapshot_line(subject.history, &comparison.to)),
        String::new(),
    ];
    lines.extend(commit_lines(comparison));
    lines.extend(totals_lines(comparison));
    lines
}

/// What `gz diff` prints without `--rows`, minus its hint about `--rows`.
pub fn diff_summary(subject: &Subject, comparison: &Comparison) -> String {
    summary_lines(subject, comparison).join("\n") + "\n"
}

pub fn diff_text(
    subject: &Subject,
    comparison: &Comparison,
    rows: Option<&RowDiff>,
    width: usize,
) -> String {
    let mut lines = summary_lines(subject, comparison);
    match rows {
        Some(rows) => {
            lines.push(String::new());
            lines.extend(row_lines(rows, width));
        }
        None => {
            lines.push(String::new());
            lines.push(
                "These are the counts each commit recorded. --rows compares the rows themselves; --key COLUMNS also tells which rows and columns changed."
                    .to_string(),
            );
        }
    }
    lines.join("\n") + "\n"
}

fn sample_json(page: &Option<Page>) -> Value {
    page.as_ref()
        .map(|page| Value::Array(page.rows.clone()))
        .unwrap_or(Value::Null)
}

fn rows_json(rows: &RowDiff) -> Value {
    let columns = |list: &[crate::table_history::diff::Column]| -> Vec<Value> {
        list.iter()
            .map(|c| json!({"name": c.name, "type": c.kind}))
            .collect()
    };
    json!({
        "schema": {
            "added": columns(&rows.schema.added),
            "removed": columns(&rows.schema.removed),
            "retyped": rows.schema.retyped.iter()
                .map(|c| json!({"name": c.name, "from": c.from, "to": c.to}))
                .collect::<Vec<_>>(),
        },
        "key": rows.key,
        "notCompared": columns(&rows.skipped),
        "added": rows.counts.added,
        "removed": rows.counts.removed,
        "changed": rows.counts.changed,
        "changedColumns": rows.column_changes.iter()
            .map(|c| json!({"column": c.column, "rows": c.rows}))
            .collect::<Vec<_>>(),
        "samples": {
            "added": sample_json(&rows.added),
            "removed": sample_json(&rows.removed),
            "changed": rows.changed.iter().map(|row| json!({
                "key": row.key.iter()
                    .map(|(column, value)| (column.clone(), json!(value)))
                    .collect::<serde_json::Map<String, Value>>(),
                "changes": row.changes.iter()
                    .map(|(column, from, to)| json!({"column": column, "from": from, "to": to}))
                    .collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        },
    })
}

pub fn diff_json(subject: &Subject, comparison: &Comparison, rows: Option<&RowDiff>) -> Value {
    let history = subject.history;
    json!({
        "table": subject.table.to_string(),
        "warehouse": subject.warehouse,
        "from": snapshot_json(history, &comparison.from),
        "to": snapshot_json(history, &comparison.to),
        "commits": comparison.commits.as_ref().map(|commits| {
            commits.iter().map(|c| snapshot_json(history, c)).collect::<Vec<_>>()
        }),
        "rows": rows.map(rows_json),
    })
}

// ---------- gz bisect ----------

fn verdict(probe: &Probe) -> String {
    if probe.fails() {
        format!("fails ({})", rows_word(probe.failing))
    } else {
        "passes".to_string()
    }
}

/// One line per snapshot tested, shown while the search runs.
pub fn probe_line(probe: &Probe) -> String {
    format!(
        "{} · {} · {}",
        probe.snapshot.id,
        probe.snapshot.committed(),
        verdict(probe)
    )
}

fn diff_command(table: &TableName, last_good: &Probe, first_bad: &Probe) -> String {
    format!(
        "gz diff {table} --from {} --to {} --rows",
        last_good.snapshot.id, first_bad.snapshot.id
    )
}

pub fn bisect_text(subject: &Subject, check: &Check, search: &Search) -> String {
    let history = subject.history;
    let mut lines = vec![
        format!("{} · {}", subject.table, subject.warehouse),
        format!("  check  {}", check.text()),
        String::new(),
    ];
    match &search.outcome {
        Outcome::Passes { newest } => lines.push(format!(
            "The check passes on {}: no rows fail, so there is nothing to search for.",
            snapshot_line(history, &newest.snapshot)
        )),
        Outcome::FailsFromTheStart { oldest, newest } => {
            lines.push(format!(
                "The check already fails on the oldest snapshot searched, {} ({}).",
                snapshot_line(history, &oldest.snapshot),
                rows_word(oldest.failing)
            ));
            lines.push(format!(
                "It still fails on {} ({}). The data broke before the history searched: try an older --good, or the break is older than the snapshots the table keeps.",
                snapshot_line(history, &newest.snapshot),
                rows_word(newest.failing)
            ));
        }
        Outcome::Found {
            last_good,
            first_bad,
        } => {
            let bad = &first_bad.snapshot;
            lines.push(format!("First failing snapshot  {}", bad.id));
            lines.push(format!("  committed   {}", bad.committed()));
            lines.push(format!("  change      {}", change_text(bad)));
            if let Some(writer) = bad.writer() {
                lines.push(format!("  written by  {writer}"));
            }
            lines.push(format!(
                "  check       {} {} here · none in the snapshot before ({}, {})",
                rows_word(first_bad.failing),
                if first_bad.failing == 1 {
                    "fails"
                } else {
                    "fail"
                },
                last_good.snapshot.id,
                last_good.snapshot.committed()
            ));
            lines.push(String::new());
            lines.push(format!(
                "Next: {}",
                diff_command(subject.table, last_good, first_bad)
            ));
        }
    }
    lines.push(String::new());
    lines.push(format!(
        "Tested {} of {} snapshot{}.",
        search.tested,
        grouped(search.searched as u64),
        if search.searched == 1 { "" } else { "s" }
    ));
    lines.join("\n") + "\n"
}

pub fn bisect_json(subject: &Subject, check: &Check, search: &Search) -> Value {
    let probe = |probe: &Probe| {
        let mut value = snapshot_json(subject.history, &probe.snapshot);
        value["failingRows"] = json!(probe.failing);
        value
    };
    let (outcome, first_bad, last_good) = match &search.outcome {
        Outcome::Passes { newest } => ("passes", Value::Null, probe(newest)),
        Outcome::FailsFromTheStart { oldest, .. } => {
            ("failsFromTheStart", probe(oldest), Value::Null)
        }
        Outcome::Found {
            last_good,
            first_bad,
        } => ("found", probe(first_bad), probe(last_good)),
    };
    json!({
        "table": subject.table.to_string(),
        "warehouse": subject.warehouse,
        "check": check.text(),
        "outcome": outcome,
        "firstFailing": first_bad,
        "lastPassing": last_good,
        "snapshotsSearched": search.searched,
        "snapshotsTested": search.tested,
    })
}

/// The message `gz bisect --code` puts in gz code's input box.
pub fn code_prompt(
    subject: &Subject,
    check: &Check,
    last_good: &Probe,
    first_bad: &Probe,
) -> String {
    let bad = &first_bad.snapshot;
    let writer = bad
        .writer()
        .map(|writer| format!("\n- Written by: {writer}"))
        .unwrap_or_default();
    format!(
        "The lakehouse table {table} (warehouse {warehouse}) fails a data check, and `gz bisect` found the commit where it started.\n\n\
         - Check (the rows it returns are bad): {check}\n\
         - First failing snapshot: {bad_id}, committed {bad_time}, {change}{writer}\n\
         - Rows failing there: {failing}\n\
         - Snapshot before it: {good_id} ({good_time}), where no row fails\n\n\
         Find the job that writes this table, work out which change produced the failing rows, and propose a fix. \
         `{diff}` shows what that commit changed.",
        table = subject.table,
        warehouse = subject.warehouse,
        check = check.text(),
        bad_id = bad.id,
        bad_time = bad.committed(),
        change = change_text(bad),
        failing = grouped(first_bad.failing),
        good_id = last_good.snapshot.id,
        good_time = last_good.snapshot.committed(),
        diff = diff_command(subject.table, last_good, first_bad),
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::table_history::diff::{Column, ColumnChange, RowCounts};
    use crate::table_history::test_support::{history, page};

    fn orders() -> TableName {
        TableName::parse("sales.orders").unwrap()
    }

    #[test]
    fn a_diff_without_rows_shows_the_commits_and_what_they_recorded() {
        let history = history();
        let table = orders();
        let subject = Subject {
            table: &table,
            warehouse: "Lake",
            history: &history,
        };
        let comparison = Comparison::between(&history, "release", "current").unwrap();
        let text = diff_text(&subject, &comparison, None, 100);
        for part in [
            "sales.orders · Lake",
            "  from  102 · 2026-09-02 09:00 UTC · tag release",
            "  to    104 · 2026-09-04 09:00 UTC · current, branch main",
            "Commits (2)",
            "  2026-09-04 09:00 UTC  104  append · +10 records  load_orders",
            "  2026-09-03 09:00 UTC  103  append · +10 records  load_orders",
            "Records  20 → 40 (+20)",
            "Files    2 → 4 · 2.0 KB → 3.9 KB",
            "--rows compares the rows themselves",
        ] {
            assert!(text.contains(part), "missing {part:?} in\n{text}");
        }
        let other_line = Comparison::between(&history, "audit", "main").unwrap();
        let text = diff_text(&subject, &other_line, None, 100);
        assert!(text.contains("was not built on the older one"), "{text}");
        assert!(!text.contains("Commits ("), "{text}");
    }

    #[test]
    fn a_keyed_row_diff_names_the_rows_and_columns_that_changed() {
        let history = history();
        let table = orders();
        let subject = Subject {
            table: &table,
            warehouse: "Lake",
            history: &history,
        };
        let comparison = Comparison::between(&history, "previous", "current").unwrap();
        let rows = RowDiff {
            schema: SchemaChange {
                added: vec![Column {
                    name: "tier".into(),
                    kind: "string".into(),
                }],
                removed: vec![],
                retyped: vec![],
            },
            skipped: vec![Column {
                name: "tags".into(),
                kind: "map<string,string>".into(),
            }],
            key: vec!["id".into()],
            counts: RowCounts {
                added: 1200,
                removed: 0,
                changed: Some(4),
            },
            column_changes: vec![ColumnChange {
                column: "status".into(),
                rows: 4,
            }],
            added: Some(page(&[("id", "long")], vec![json!({"id": 9})])),
            removed: None,
            changed: vec![ChangedRow {
                key: vec![("id".into(), "12".into())],
                changes: vec![("status".into(), "open".into(), "paid".into())],
            }],
        };
        let text = diff_text(&subject, &comparison, Some(&rows), 100);
        for part in [
            "Schema\n  + tier string",
            "Rows     +1,200 added · −0 removed · ~4 changed  (matched on id)",
            "Not compared: tags (map<string,string>)",
            "Changed columns\n  status  4 rows",
            "Added (showing 1 of 1,200)",
            "Changed (showing 1 of 4)\n  id=12\n    status  open → paid",
        ] {
            assert!(text.contains(part), "missing {part:?} in\n{text}");
        }
        assert!(!text.contains("Removed ("), "nothing removed:\n{text}");
        let value = diff_json(&subject, &comparison, Some(&rows));
        assert_eq!(value["rows"]["changed"], 4);
        assert_eq!(value["rows"]["samples"]["changed"][0]["key"]["id"], "12");
        assert_eq!(value["to"]["names"], json!(["current", "branch main"]));
    }

    #[test]
    fn a_bisect_report_names_the_commit_and_the_next_command() {
        let history = history();
        let table = orders();
        let subject = Subject {
            table: &table,
            warehouse: "Lake",
            history: &history,
        };
        let check = Check::condition("amount < 0").unwrap();
        let probe = |id: &str, failing| Probe {
            snapshot: history.resolve(id).unwrap().clone(),
            failing,
        };
        let found = Search {
            outcome: Outcome::Found {
                last_good: probe("103", 0),
                first_bad: probe("104", 7),
            },
            searched: 4,
            tested: 3,
        };
        let text = bisect_text(&subject, &check, &found);
        for part in [
            "  check  rows where amount < 0",
            "First failing snapshot  104",
            "  committed   2026-09-04 09:00 UTC",
            "  change      append · +10 records",
            "  written by  load_orders (Spark application app-40)",
            "  check       7 rows fail here · none in the snapshot before (103, 2026-09-03 09:00 UTC)",
            "Next: gz diff sales.orders --from 103 --to 104 --rows",
            "Tested 3 of 4 snapshots.",
        ] {
            assert!(text.contains(part), "missing {part:?} in\n{text}");
        }
        let value = bisect_json(&subject, &check, &found);
        assert_eq!(value["outcome"], "found");
        assert_eq!(value["firstFailing"]["snapshotId"], "104");
        assert_eq!(value["firstFailing"]["failingRows"], 7);

        let prompt = code_prompt(&subject, &check, &probe("103", 0), &probe("104", 7));
        assert!(prompt.contains("First failing snapshot: 104, committed 2026-09-04 09:00 UTC"));
        assert!(prompt.contains("gz diff sales.orders --from 103 --to 104 --rows"));

        let healthy = Search {
            outcome: Outcome::Passes {
                newest: probe("104", 0),
            },
            searched: 4,
            tested: 1,
        };
        let text = bisect_text(&subject, &check, &healthy);
        assert!(text.contains("The check passes on 104"), "{text}");
        let broken = Search {
            outcome: Outcome::FailsFromTheStart {
                oldest: probe("101", 2),
                newest: probe("104", 7),
            },
            searched: 4,
            tested: 2,
        };
        let text = bisect_text(&subject, &check, &broken);
        assert!(
            text.contains("already fails on the oldest snapshot searched, 101 · 2026-09-01 09:00 UTC (2 rows)"),
            "{text}"
        );
    }

    #[test]
    fn versions_list_one_row_per_snapshot_with_its_names() {
        let rows = versions_rows(&history());
        assert_eq!(rows[1]["snapshot"], "104");
        assert_eq!(rows[1]["names"], "current, branch main");
        assert_eq!(rows[1]["records"], "40");
        assert_eq!(rows[0]["names"], "branch audit");
    }
}
