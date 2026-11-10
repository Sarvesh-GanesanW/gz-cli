//! `gz diff`: what changed in a table between two snapshots.
//!
//! The commits in between and their record counts come from the history
//! alone. Comparing the rows themselves runs Spark SQL on a session, reading
//! each side with `VERSION AS OF`: whole rows with `EXCEPT ALL`, or, with a
//! key, a join that also tells which rows and columns changed.

use anyhow::{bail, Context, Result};
use serde_json::Value;

use super::{ident, History, Snapshot, Statements, TableName};
use crate::sql::results::Page;

/// Marks a row's side of the keyed join, so a missing side is told apart
/// from a row whose key is NULL.
const PRESENT: &str = "`__gz_present`";
/// Most columns shown per changed row.
const MAX_CHANGED_COLUMNS: usize = 12;

/// Two snapshots to compare and the commits that lead from one to the other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comparison {
    pub from: Snapshot,
    pub to: Snapshot,
    /// Newest first; `None` when `to` was not built on `from` (another
    /// branch, or `from` is the newer one).
    pub commits: Option<Vec<Snapshot>>,
}

impl Comparison {
    pub fn between(history: &History, from: &str, to: &str) -> Result<Self> {
        let from_snapshot = history.resolve(from).context("--from")?;
        let to_snapshot = history.resolve(to).context("--to")?;
        if from_snapshot.id == to_snapshot.id {
            bail!(
                "'{from}' and '{to}' are the same snapshot ({}); pick two with --from and --to (`gz table versions` lists them)",
                to_snapshot.id
            );
        }
        let line = history.line(to_snapshot);
        let commits = line
            .iter()
            .position(|snapshot| snapshot.id == from_snapshot.id)
            .map(|at| line[..at].iter().map(|s| (*s).clone()).collect());
        Ok(Self {
            from: from_snapshot.clone(),
            to: to_snapshot.clone(),
            commits,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    /// The type as the session names it (`long`, `string`, …).
    pub kind: String,
}

fn columns_of(page: &Page) -> Vec<Column> {
    page.columns
        .iter()
        .enumerate()
        .map(|(i, name)| Column {
            name: name.clone(),
            kind: page.column_types.get(i).cloned().unwrap_or_default(),
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Retyped {
    pub name: String,
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SchemaChange {
    pub added: Vec<Column>,
    pub removed: Vec<Column>,
    pub retyped: Vec<Retyped>,
}

impl SchemaChange {
    pub fn between(from: &[Column], to: &[Column]) -> Self {
        let find = |columns: &[Column], name: &str| -> Option<Column> {
            columns.iter().find(|c| c.name == name).cloned()
        };
        Self {
            added: to
                .iter()
                .filter(|c| find(from, &c.name).is_none())
                .cloned()
                .collect(),
            removed: from
                .iter()
                .filter(|c| find(to, &c.name).is_none())
                .cloned()
                .collect(),
            retyped: to
                .iter()
                .filter_map(|c| {
                    let before = find(from, &c.name)?;
                    (before.kind != c.kind).then(|| Retyped {
                        name: c.name.clone(),
                        from: before.kind,
                        to: c.kind.clone(),
                    })
                })
                .collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.retyped.is_empty()
    }
}

/// The columns both versions have, split by whether Spark can compare them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Shared {
    compared: Vec<String>,
    /// Map columns: Spark refuses them in set operations and comparisons.
    skipped: Vec<Column>,
}

fn shared(from: &[Column], to: &[Column]) -> Shared {
    let in_both = to
        .iter()
        .filter_map(|after| Some((from.iter().find(|before| before.name == after.name)?, after)));
    let (skipped, compared): (Vec<_>, Vec<_>) = in_both
        .partition(|(before, after)| before.kind.contains("map") || after.kind.contains("map"));
    Shared {
        compared: compared.into_iter().map(|(_, c)| c.name.clone()).collect(),
        skipped: skipped.into_iter().map(|(_, c)| c.clone()).collect(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowCounts {
    pub added: u64,
    pub removed: u64,
    /// Only known with a key: without one, a changed row counts as one
    /// removed and one added.
    pub changed: Option<u64>,
}

/// How many rows changed in one column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnChange {
    pub column: String,
    pub rows: u64,
}

/// One row whose key is in both versions with different values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedRow {
    pub key: Vec<(String, String)>,
    /// Column, value before, value after.
    pub changes: Vec<(String, String, String)>,
}

/// The rows of two snapshots compared.
#[derive(Debug, Clone, PartialEq)]
pub struct RowDiff {
    pub schema: SchemaChange,
    pub skipped: Vec<Column>,
    pub key: Vec<String>,
    pub counts: RowCounts,
    /// Most-changed first; only with a key.
    pub column_changes: Vec<ColumnChange>,
    pub added: Option<Page>,
    pub removed: Option<Page>,
    pub changed: Vec<ChangedRow>,
}

// ---------- SQL ----------

fn column_list(columns: &[String]) -> String {
    columns
        .iter()
        .map(|c| ident(c))
        .collect::<Vec<_>>()
        .join(", ")
}

fn columns_sql(at: &str) -> String {
    format!("SELECT * FROM {at} LIMIT 0")
}

/// Rows of `a` that `b` does not have, comparing whole rows.
fn only_in_sql(a: &str, b: &str, columns: &[String]) -> String {
    let columns = column_list(columns);
    format!("SELECT {columns} FROM {a} EXCEPT ALL SELECT {columns} FROM {b}")
}

fn whole_row_counts_sql(from: &str, to: &str, columns: &[String]) -> String {
    format!(
        "SELECT (SELECT count(*) FROM ({})) AS added, (SELECT count(*) FROM ({})) AS removed",
        only_in_sql(to, from, columns),
        only_in_sql(from, to, columns)
    )
}

/// `f.k <=> t.k AND …`: NULL keys match each other.
fn same_key(key: &[String]) -> String {
    key.iter()
        .map(|k| format!("f.{0} <=> t.{0}", ident(k)))
        .collect::<Vec<_>>()
        .join(" AND ")
}

fn differs(column: &str) -> String {
    format!("NOT (f.{0} <=> t.{0})", ident(column))
}

/// Whether `key` names more than one row in either version.
fn repeats_sql(from: &str, to: &str, key: &[String]) -> String {
    let key = column_list(key);
    let repeated =
        |at: &str| format!("SELECT 1 FROM {at} GROUP BY {key} HAVING count(*) > 1 LIMIT 1");
    format!(
        "SELECT (SELECT count(*) FROM ({})) AS from_repeats, (SELECT count(*) FROM ({})) AS to_repeats",
        repeated(from),
        repeated(to)
    )
}

/// Added, removed and changed rows by key, plus how many rows changed in
/// each compared column (`col_0`, `col_1`, … in the order of `compared`).
fn keyed_counts_sql(from: &str, to: &str, key: &[String], compared: &[String]) -> String {
    let in_both = format!("f.{PRESENT} IS NOT NULL AND t.{PRESENT} IS NOT NULL");
    let changed = match compared {
        [] => "0 AS changed".to_string(),
        columns => format!(
            "count_if({in_both} AND ({})) AS changed",
            columns
                .iter()
                .map(|c| differs(c))
                .collect::<Vec<_>>()
                .join(" OR ")
        ),
    };
    let per_column: String = compared
        .iter()
        .enumerate()
        .map(|(i, c)| format!(", count_if({in_both} AND {}) AS col_{i}", differs(c)))
        .collect();
    format!(
        "SELECT count_if(f.{PRESENT} IS NULL) AS added, count_if(t.{PRESENT} IS NULL) AS removed, {changed}{per_column} \
         FROM (SELECT *, true AS {PRESENT} FROM {from}) f \
         FULL OUTER JOIN (SELECT *, true AS {PRESENT} FROM {to}) t ON {}",
        same_key(key)
    )
}

/// Rows of `side` whose key the other version does not have.
fn missing_from_other_sql(side: &str, other: &str, key: &[String], limit: u32) -> String {
    // The aliases follow `same_key`: `t` is the side kept, `f` the other.
    format!(
        "SELECT t.* FROM {side} t LEFT ANTI JOIN {other} f ON {} LIMIT {limit}",
        same_key(key)
    )
}

/// Changed rows with each column's value before (`from_N`) and after
/// (`to_N`), and the key as `key_N`.
fn changed_rows_sql(
    from: &str,
    to: &str,
    key: &[String],
    columns: &[String],
    limit: u32,
) -> String {
    let keys = key
        .iter()
        .enumerate()
        .map(|(i, k)| format!("t.{} AS key_{i}", ident(k)));
    let values = columns
        .iter()
        .enumerate()
        .map(|(i, c)| format!("f.{0} AS from_{i}, t.{0} AS to_{i}", ident(c)));
    format!(
        "SELECT {} FROM {from} f JOIN {to} t ON {} WHERE {} LIMIT {limit}",
        keys.chain(values).collect::<Vec<_>>().join(", "),
        same_key(key),
        columns
            .iter()
            .map(|c| differs(c))
            .collect::<Vec<_>>()
            .join(" OR ")
    )
}

// ---------- reading the answers ----------

fn count(row: &Value, column: &str) -> Result<u64> {
    match &row[column] {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
    .with_context(|| format!("the session answered without a count for '{column}'"))
}

fn only_row(page: &Page) -> Result<&Value> {
    page.rows
        .first()
        .context("the session answered a count query without a row")
}

fn column_changes(row: &Value, compared: &[String]) -> Result<Vec<ColumnChange>> {
    let mut changes = Vec::new();
    for (i, column) in compared.iter().enumerate() {
        let rows = count(row, &format!("col_{i}"))?;
        if rows > 0 {
            changes.push(ColumnChange {
                column: column.clone(),
                rows,
            });
        }
    }
    // Stable, so columns with the same count keep the table's order.
    changes.sort_by(|a, b| b.rows.cmp(&a.rows));
    Ok(changes)
}

fn changed_rows(page: &Page, key: &[String], columns: &[String]) -> Vec<ChangedRow> {
    let text = |row: &Value, name: String| crate::output::scalar_text(&row[name.as_str()]);
    page.rows
        .iter()
        .map(|row| ChangedRow {
            key: key
                .iter()
                .enumerate()
                .map(|(i, k)| (k.clone(), text(row, format!("key_{i}"))))
                .collect(),
            changes: columns
                .iter()
                .enumerate()
                .filter(|(i, _)| {
                    row[format!("from_{i}").as_str()] != row[format!("to_{i}").as_str()]
                })
                .map(|(i, c)| {
                    (
                        c.clone(),
                        text(row, format!("from_{i}")),
                        text(row, format!("to_{i}")),
                    )
                })
                .collect(),
        })
        .collect()
}

// ---------- the comparison ----------

/// What to compare and how much of it to show.
pub struct RowRequest<'a> {
    pub table: &'a TableName,
    pub from: &'a Snapshot,
    pub to: &'a Snapshot,
    /// Columns that identify a row; empty compares whole rows.
    pub key: &'a [String],
    /// Sample rows shown per kind of change; 0 shows none.
    pub sample: u32,
}

pub async fn compare_rows(session: &impl Statements, request: &RowRequest<'_>) -> Result<RowDiff> {
    let from = request.table.at(request.from);
    let to = request.table.at(request.to);
    let before = columns_of(&session.rows(&columns_sql(&from)).await?);
    let after = columns_of(&session.rows(&columns_sql(&to)).await?);
    let schema = SchemaChange::between(&before, &after);
    let Shared { compared, skipped } = shared(&before, &after);
    if compared.is_empty() {
        bail!("the two versions share no column that can be compared");
    }
    let mut diff = RowDiff {
        schema,
        skipped,
        key: request.key.to_vec(),
        counts: RowCounts {
            added: 0,
            removed: 0,
            changed: None,
        },
        column_changes: Vec::new(),
        added: None,
        removed: None,
        changed: Vec::new(),
    };
    if request.key.is_empty() {
        whole_rows(session, request, &from, &to, &compared, &mut diff).await?;
    } else {
        by_key(session, request, &from, &to, &compared, &mut diff).await?;
    }
    Ok(diff)
}

async fn whole_rows(
    session: &impl Statements,
    request: &RowRequest<'_>,
    from: &str,
    to: &str,
    compared: &[String],
    diff: &mut RowDiff,
) -> Result<()> {
    let counts = session
        .rows(&whole_row_counts_sql(from, to, compared))
        .await?;
    let row = only_row(&counts)?;
    diff.counts.added = count(row, "added")?;
    diff.counts.removed = count(row, "removed")?;
    let sample =
        |a: &str, b: &str| format!("{} LIMIT {}", only_in_sql(a, b, compared), request.sample);
    if request.sample > 0 && diff.counts.added > 0 {
        diff.added = Some(session.rows(&sample(to, from)).await?);
    }
    if request.sample > 0 && diff.counts.removed > 0 {
        diff.removed = Some(session.rows(&sample(from, to)).await?);
    }
    Ok(())
}

async fn by_key(
    session: &impl Statements,
    request: &RowRequest<'_>,
    from: &str,
    to: &str,
    compared: &[String],
    diff: &mut RowDiff,
) -> Result<()> {
    let key = request.key;
    if let Some(missing) = key.iter().find(|k| !compared.contains(k)) {
        bail!(
            "--key {missing}: not a column of both versions that can be compared (have: {})",
            compared.join(", ")
        );
    }
    let repeats = session.rows(&repeats_sql(from, to, key)).await?;
    let row = only_row(&repeats)?;
    if count(row, "from_repeats")? > 0 || count(row, "to_repeats")? > 0 {
        bail!(
            "--key {}: more than one row has the same key, so rows can't be matched; add columns to --key, or leave it out to compare whole rows",
            key.join(",")
        );
    }
    let values: Vec<String> = compared
        .iter()
        .filter(|c| !key.contains(c))
        .cloned()
        .collect();
    let counts = session
        .rows(&keyed_counts_sql(from, to, key, &values))
        .await?;
    let row = only_row(&counts)?;
    diff.counts = RowCounts {
        added: count(row, "added")?,
        removed: count(row, "removed")?,
        changed: Some(count(row, "changed")?),
    };
    diff.column_changes = column_changes(row, &values)?;
    if request.sample == 0 {
        return Ok(());
    }
    if diff.counts.added > 0 {
        let sql = missing_from_other_sql(to, from, key, request.sample);
        diff.added = Some(session.rows(&sql).await?);
    }
    if diff.counts.removed > 0 {
        let sql = missing_from_other_sql(from, to, key, request.sample);
        diff.removed = Some(session.rows(&sql).await?);
    }
    if !diff.column_changes.is_empty() {
        let shown: Vec<String> = diff
            .column_changes
            .iter()
            .take(MAX_CHANGED_COLUMNS)
            .map(|change| change.column.clone())
            .collect();
        let sql = changed_rows_sql(from, to, key, &shown, request.sample);
        diff.changed = changed_rows(&session.rows(&sql).await?, key, &shown);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::test_support::{history, page, Scripted};
    use super::*;

    fn ids(commits: &Option<Vec<Snapshot>>) -> Option<Vec<&str>> {
        commits
            .as_ref()
            .map(|list| list.iter().map(|s| s.id.as_str()).collect())
    }

    #[test]
    fn the_commits_between_two_snapshots_are_the_ones_after_the_older() {
        let history = history();
        let recent = Comparison::between(&history, "previous", "current").unwrap();
        assert_eq!(ids(&recent.commits), Some(vec!["104"]));
        let wide = Comparison::between(&history, "101", "current").unwrap();
        assert_eq!(ids(&wide.commits), Some(vec!["104", "103", "102"]));

        let branches = Comparison::between(&history, "audit", "main").unwrap();
        assert_eq!(branches.commits, None, "main was not built on audit");
        let backwards = Comparison::between(&history, "current", "release").unwrap();
        assert_eq!(
            backwards.commits, None,
            "the older one is not built on the newer"
        );

        let same = Comparison::between(&history, "main", "current").unwrap_err();
        assert!(format!("{same:#}").contains("same snapshot (104)"));
        let unknown = Comparison::between(&history, "nope", "current").unwrap_err();
        assert!(format!("{unknown:#}").starts_with("--from"), "{unknown:#}");
    }

    fn column(name: &str, kind: &str) -> Column {
        Column {
            name: name.into(),
            kind: kind.into(),
        }
    }

    #[test]
    fn a_schema_change_lists_new_dropped_and_retyped_columns() {
        let before = [
            column("id", "integer"),
            column("note", "string"),
            column("old", "string"),
        ];
        let after = [
            column("id", "long"),
            column("note", "string"),
            column("tier", "string"),
        ];
        let change = SchemaChange::between(&before, &after);
        assert_eq!(change.added, [column("tier", "string")]);
        assert_eq!(change.removed, [column("old", "string")]);
        assert_eq!(
            change.retyped,
            [Retyped {
                name: "id".into(),
                from: "integer".into(),
                to: "long".into()
            }]
        );
        assert!(SchemaChange::between(&before, &before).is_empty());
    }

    fn request<'a>(
        table: &'a TableName,
        history: &'a History,
        key: &'a [String],
        sample: u32,
    ) -> RowRequest<'a> {
        RowRequest {
            table,
            from: history.resolve("previous").unwrap(),
            to: history.resolve("current").unwrap(),
            key,
            sample,
        }
    }

    fn orders_schema() -> Page {
        page(
            &[
                ("id", "long"),
                ("amount", "double"),
                ("tags", "map<string,string>"),
            ],
            vec![],
        )
    }

    #[tokio::test]
    async fn whole_rows_are_compared_on_the_shared_columns_with_samples_only_where_needed() {
        let table = TableName::parse("sales.orders").unwrap();
        let history = history();
        let session = Scripted::new(vec![
            Ok(orders_schema()),
            Ok(orders_schema()),
            Ok(page(&[], vec![json!({"added": 3, "removed": 0})])),
            Ok(page(&[("id", "long")], vec![json!({"id": 9})])),
        ]);
        let diff = compare_rows(&session, &request(&table, &history, &[], 5))
            .await
            .unwrap();
        assert_eq!(
            diff.counts,
            RowCounts {
                added: 3,
                removed: 0,
                changed: None
            }
        );
        assert_eq!(diff.skipped, [column("tags", "map<string,string>")]);
        assert!(diff.added.is_some());
        assert_eq!(diff.removed, None, "nothing removed, so no query for it");
        let asked = session.asked();
        assert_eq!(
            asked[0],
            "SELECT * FROM `sales`.`orders` VERSION AS OF 103 LIMIT 0"
        );
        assert_eq!(
            asked[2],
            "SELECT (SELECT count(*) FROM (SELECT `id`, `amount` FROM `sales`.`orders` VERSION AS OF 104 \
             EXCEPT ALL SELECT `id`, `amount` FROM `sales`.`orders` VERSION AS OF 103)) AS added, \
             (SELECT count(*) FROM (SELECT `id`, `amount` FROM `sales`.`orders` VERSION AS OF 103 \
             EXCEPT ALL SELECT `id`, `amount` FROM `sales`.`orders` VERSION AS OF 104)) AS removed"
        );
        assert!(asked[3].ends_with("LIMIT 5"), "{}", asked[3]);
        assert_eq!(asked.len(), 4);
    }

    #[tokio::test]
    async fn a_key_tells_changed_rows_and_columns_apart() {
        let table = TableName::parse("sales.orders").unwrap();
        let history = history();
        let key = ["id".to_string()];
        let schema = || {
            page(
                &[("id", "long"), ("amount", "double"), ("status", "string")],
                vec![],
            )
        };
        let session = Scripted::new(vec![
            Ok(schema()),
            Ok(schema()),
            Ok(page(&[], vec![json!({"from_repeats": 0, "to_repeats": 0})])),
            Ok(page(
                &[],
                vec![json!({"added": 0, "removed": 1, "changed": 4, "col_0": 1, "col_1": 4})],
            )),
            Ok(page(&[("id", "long")], vec![json!({"id": 7})])),
            Ok(page(
                &[],
                vec![
                    json!({"key_0": 12, "from_0": "open", "to_0": "paid", "from_1": 5.0, "to_1": 5.0}),
                ],
            )),
        ]);
        let diff = compare_rows(&session, &request(&table, &history, &key, 3))
            .await
            .unwrap();
        assert_eq!(
            diff.counts,
            RowCounts {
                added: 0,
                removed: 1,
                changed: Some(4)
            }
        );
        assert_eq!(
            diff.column_changes,
            [
                ColumnChange {
                    column: "status".into(),
                    rows: 4
                },
                ColumnChange {
                    column: "amount".into(),
                    rows: 1
                }
            ],
            "most-changed first"
        );
        assert_eq!(
            diff.changed,
            [ChangedRow {
                key: vec![("id".into(), "12".into())],
                changes: vec![("status".into(), "open".into(), "paid".into())],
            }],
            "amount did not change in this row, so it is not listed"
        );
        let asked = session.asked();
        assert_eq!(
            asked[3],
            "SELECT count_if(f.`__gz_present` IS NULL) AS added, count_if(t.`__gz_present` IS NULL) AS removed, \
             count_if(f.`__gz_present` IS NOT NULL AND t.`__gz_present` IS NOT NULL AND (NOT (f.`amount` <=> t.`amount`) OR NOT (f.`status` <=> t.`status`))) AS changed, \
             count_if(f.`__gz_present` IS NOT NULL AND t.`__gz_present` IS NOT NULL AND NOT (f.`amount` <=> t.`amount`)) AS col_0, \
             count_if(f.`__gz_present` IS NOT NULL AND t.`__gz_present` IS NOT NULL AND NOT (f.`status` <=> t.`status`)) AS col_1 \
             FROM (SELECT *, true AS `__gz_present` FROM `sales`.`orders` VERSION AS OF 103) f \
             FULL OUTER JOIN (SELECT *, true AS `__gz_present` FROM `sales`.`orders` VERSION AS OF 104) t ON f.`id` <=> t.`id`"
        );
        assert_eq!(
            asked[4],
            "SELECT t.* FROM `sales`.`orders` VERSION AS OF 103 t LEFT ANTI JOIN `sales`.`orders` VERSION AS OF 104 f ON f.`id` <=> t.`id` LIMIT 3",
            "removed rows: in the older version only"
        );
        assert_eq!(
            asked[5],
            "SELECT t.`id` AS key_0, f.`status` AS from_0, t.`status` AS to_0, f.`amount` AS from_1, t.`amount` AS to_1 \
             FROM `sales`.`orders` VERSION AS OF 103 f JOIN `sales`.`orders` VERSION AS OF 104 t ON f.`id` <=> t.`id` \
             WHERE NOT (f.`status` <=> t.`status`) OR NOT (f.`amount` <=> t.`amount`) LIMIT 3"
        );
    }

    #[tokio::test]
    async fn a_key_that_repeats_or_is_missing_is_refused_before_counting() {
        let table = TableName::parse("sales.orders").unwrap();
        let history = history();
        let key = ["id".to_string()];
        let session = Scripted::new(vec![
            Ok(orders_schema()),
            Ok(orders_schema()),
            Ok(page(&[], vec![json!({"from_repeats": 0, "to_repeats": 1})])),
        ]);
        let err = compare_rows(&session, &request(&table, &history, &key, 5))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("more than one row has the same key"));
        assert_eq!(session.asked().len(), 3, "no counts on a key that repeats");

        let unknown = ["order_id".to_string()];
        let session = Scripted::new(vec![Ok(orders_schema()), Ok(orders_schema())]);
        let err = compare_rows(&session, &request(&table, &history, &unknown, 5))
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("--key order_id: not a column of both versions"),
            "{err:#}"
        );
    }
}
