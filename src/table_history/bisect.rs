//! `gz bisect`: find the snapshot where a table started failing a check.
//!
//! A check is SQL that returns the bad rows; a snapshot fails when the check
//! returns any. The newest snapshot must fail and an older one must pass;
//! the search then halves the snapshots in between, so a history of a
//! thousand commits takes about ten queries. It finds one change from
//! passing to failing: when the data broke, was repaired and broke again,
//! that is the last time it broke only if the check failed ever since.

use anyhow::{bail, Context, Result};
use serde_json::Value;

use super::{History, Snapshot, Statements, TableName};

/// Where a `--check` query reads the table.
pub const PLACEHOLDER: &str = "{table}";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    /// Rows matching this condition are bad.
    Where(String),
    /// A query over `{table}` that returns the bad rows.
    Query(String),
}

impl Check {
    pub fn condition(text: &str) -> Result<Self> {
        let condition = text.trim().trim_end_matches(';').trim();
        if condition.is_empty() {
            bail!("--where needs a condition, e.g. --where \"amount < 0\"");
        }
        Ok(Self::Where(condition.to_string()))
    }

    pub fn query(text: &str) -> Result<Self> {
        let query = text.trim().trim_end_matches(';').trim();
        if !query.contains(PLACEHOLDER) {
            bail!(
                "--check must read the table as {PLACEHOLDER}, e.g. --check \"SELECT * FROM {PLACEHOLDER} WHERE amount < 0\""
            );
        }
        Ok(Self::Query(query.to_string()))
    }

    /// Counts the rows the check finds in the table as it was at one
    /// snapshot (`at` is `TableName::at`).
    fn failing_sql(&self, at: &str) -> String {
        match self {
            Self::Where(condition) => {
                format!("SELECT count(*) AS failing FROM {at} WHERE {condition}")
            }
            Self::Query(query) => format!(
                "SELECT count(*) AS failing FROM ({})",
                query.replace(PLACEHOLDER, at)
            ),
        }
    }

    /// The check as the user wrote it, for the report.
    pub fn text(&self) -> String {
        match self {
            Self::Where(condition) => format!("rows where {condition}"),
            Self::Query(query) => query.clone(),
        }
    }
}

/// One snapshot tested, with the number of rows the check found there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub snapshot: Snapshot,
    pub failing: u64,
}

impl Probe {
    pub fn fails(&self) -> bool {
        self.failing > 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The newest snapshot passes, so there is nothing to find.
    Passes {
        newest: Probe,
    },
    /// The oldest snapshot searched already fails: the break is older than
    /// the history kept, or than `--good`.
    FailsFromTheStart {
        oldest: Probe,
        newest: Probe,
    },
    Found {
        last_good: Probe,
        first_bad: Probe,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Search {
    pub outcome: Outcome,
    /// How many snapshots were in the range.
    pub searched: usize,
    /// How many of them were tested.
    pub tested: usize,
}

/// The snapshots to search, oldest first: `bad` and what it was built on,
/// back to `good` (or to the oldest snapshot kept).
pub fn range(history: &History, good: Option<&str>, bad: &str) -> Result<Vec<Snapshot>> {
    let newest = history.resolve(bad).context("--bad")?;
    let mut line = history.line(newest);
    if let Some(good) = good {
        let oldest = history.resolve(good).context("--good")?;
        if oldest.id == newest.id {
            bail!("--good and --bad name the same snapshot ({})", newest.id);
        }
        let at = line
            .iter()
            .position(|snapshot| snapshot.id == oldest.id)
            .with_context(|| {
                format!("--good {good}: '{bad}' was not built on it, so there is no line of commits between them to search")
            })?;
        line.truncate(at + 1);
    }
    Ok(line.into_iter().rev().cloned().collect())
}

async fn probe(
    session: &impl Statements,
    table: &TableName,
    check: &Check,
    snapshot: &Snapshot,
) -> Result<Probe> {
    let page = session
        .rows(&check.failing_sql(&table.at(snapshot)))
        .await
        .with_context(|| format!("the check failed to run on snapshot {}", snapshot.id))?;
    let failing = match page.rows.first().map(|row| &row["failing"]) {
        Some(Value::Number(n)) => n.as_u64(),
        Some(Value::String(s)) => s.parse().ok(),
        _ => None,
    }
    .context("the session answered the check without a count")?;
    Ok(Probe {
        snapshot: snapshot.clone(),
        failing,
    })
}

/// Searches `range` (oldest first) for the first snapshot that fails
/// `check`, calling `tested` after each query.
pub async fn search(
    session: &impl Statements,
    table: &TableName,
    check: &Check,
    range: &[Snapshot],
    mut tested: impl FnMut(&Probe),
) -> Result<Search> {
    let (Some(first), Some(last)) = (range.first(), range.last()) else {
        bail!("the table has no snapshot to search");
    };
    let mut count = 0;
    let mut test = |probe: Probe| {
        count += 1;
        tested(&probe);
        probe
    };
    let done = |outcome: Outcome, tested: usize| Search {
        outcome,
        searched: range.len(),
        tested,
    };

    let newest = test(probe(session, table, check, last).await?);
    if !newest.fails() {
        return Ok(done(Outcome::Passes { newest }, 1));
    }
    if range.len() == 1 {
        let oldest = newest.clone();
        return Ok(done(Outcome::FailsFromTheStart { oldest, newest }, 1));
    }
    let oldest = test(probe(session, table, check, first).await?);
    if oldest.fails() {
        return Ok(done(Outcome::FailsFromTheStart { oldest, newest }, 2));
    }

    // `good` passes and `bad` fails; halve the snapshots between them until
    // they are neighbours.
    let (mut good, mut bad) = ((0, oldest), (range.len() - 1, newest));
    while bad.0 - good.0 > 1 {
        let middle = good.0 + (bad.0 - good.0) / 2;
        let found = test(probe(session, table, check, &range[middle]).await?);
        if found.fails() {
            bad = (middle, found);
        } else {
            good = (middle, found);
        }
    }
    Ok(Search {
        outcome: Outcome::Found {
            last_good: good.1,
            first_bad: bad.1,
        },
        searched: range.len(),
        tested: count,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use serde_json::json;

    use super::super::test_support::{history, page, snapshot};
    use super::*;
    use crate::sql::results::Page;

    /// A table whose snapshots fail the check from `first_bad` on (ids grow
    /// with time), answering from the id in `VERSION AS OF`.
    struct BrokenFrom {
        first_bad: u64,
        asked: Mutex<Vec<String>>,
    }

    impl BrokenFrom {
        fn new(first_bad: u64) -> Self {
            Self {
                first_bad,
                asked: Mutex::new(Vec::new()),
            }
        }
    }

    impl Statements for BrokenFrom {
        async fn rows(&self, statement: &str) -> Result<Page> {
            self.asked.lock().unwrap().push(statement.to_string());
            let id: u64 = statement
                .split("VERSION AS OF ")
                .nth(1)
                .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
                .and_then(|digits| digits.parse().ok())
                .expect("the check reads a snapshot");
            let failing = if id >= self.first_bad { 7 } else { 0 };
            Ok(page(
                &[("failing", "long")],
                vec![json!({"failing": failing})],
            ))
        }
    }

    /// `count` snapshots in one line, ids 1..=count.
    fn long_history(count: u64) -> History {
        let snapshots: Vec<_> = (1..=count)
            .rev()
            .map(|id| {
                snapshot(
                    id,
                    (id > 1).then(|| id - 1),
                    "2026-09-01T00:00:00+00:00",
                    json!({}),
                )
            })
            .collect();
        History::parse(&json!({
            "currentSnapshotId": count.to_string(),
            "snapshots": snapshots,
            "branches": [],
            "tags": [],
        }))
        .unwrap()
    }

    fn orders() -> TableName {
        TableName::parse("sales.orders").unwrap()
    }

    #[tokio::test]
    async fn the_first_failing_snapshot_is_found_in_a_few_queries() {
        let history = long_history(1000);
        let range = range(&history, None, "current").unwrap();
        let check = Check::condition("amount < 0").unwrap();
        for first_bad in [2, 371, 1000] {
            let session = BrokenFrom::new(first_bad);
            let mut seen = 0;
            let search = search(&session, &orders(), &check, &range, |_| seen += 1)
                .await
                .unwrap();
            let Outcome::Found {
                last_good,
                first_bad: found,
            } = &search.outcome
            else {
                panic!("{:?}", search.outcome);
            };
            assert_eq!(found.snapshot.id, first_bad.to_string());
            assert_eq!(last_good.snapshot.id, (first_bad - 1).to_string());
            assert_eq!((found.failing, last_good.failing), (7, 0));
            assert!(
                search.tested <= 12,
                "{} queries for 1000 snapshots",
                search.tested
            );
            assert_eq!(search.tested, seen, "every query is reported");
            assert_eq!(search.tested, session.asked.lock().unwrap().len());
        }
    }

    #[tokio::test]
    async fn nothing_is_searched_when_the_ends_do_not_bracket_a_break() {
        let history = long_history(8);
        let range = range(&history, None, "current").unwrap();
        let check = Check::condition("amount < 0").unwrap();

        let healthy = BrokenFrom::new(9);
        let search_healthy = search(&healthy, &orders(), &check, &range, |_| {})
            .await
            .unwrap();
        assert!(matches!(search_healthy.outcome, Outcome::Passes { .. }));
        assert_eq!(search_healthy.tested, 1, "only the newest is tested");

        let always = BrokenFrom::new(1);
        let search_always = search(&always, &orders(), &check, &range, |_| {})
            .await
            .unwrap();
        let Outcome::FailsFromTheStart { oldest, newest } = &search_always.outcome else {
            panic!("{:?}", search_always.outcome);
        };
        assert_eq!(
            (oldest.snapshot.id.as_str(), newest.snapshot.id.as_str()),
            ("1", "8")
        );
        assert_eq!(search_always.tested, 2);
    }

    #[test]
    fn the_range_runs_from_good_to_bad_along_one_line() {
        let history = history();
        let ids = |good: Option<&str>, bad: &str| -> Vec<String> {
            range(&history, good, bad)
                .unwrap()
                .iter()
                .map(|s| s.id.clone())
                .collect()
        };
        assert_eq!(ids(None, "current"), ["101", "102", "103", "104"]);
        assert_eq!(ids(Some("release"), "current"), ["102", "103", "104"]);
        assert_eq!(ids(Some("101"), "audit"), ["101", "102", "201"]);

        let off_line = range(&history, Some("audit"), "current").unwrap_err();
        assert!(
            format!("{off_line:#}").contains("was not built on it"),
            "{off_line:#}"
        );
        let same = range(&history, Some("main"), "current").unwrap_err();
        assert!(format!("{same:#}").contains("same snapshot"));
    }

    #[test]
    fn a_check_counts_its_rows_in_the_table_at_a_snapshot() {
        let at = "`sales`.`orders` VERSION AS OF 104";
        assert_eq!(
            Check::condition(" amount < 0; ").unwrap().failing_sql(at),
            "SELECT count(*) AS failing FROM `sales`.`orders` VERSION AS OF 104 WHERE amount < 0"
        );
        let duplicates =
            Check::query("SELECT id FROM {table} GROUP BY id HAVING count(*) > 1;").unwrap();
        assert_eq!(
            duplicates.failing_sql(at),
            "SELECT count(*) AS failing FROM (SELECT id FROM `sales`.`orders` VERSION AS OF 104 GROUP BY id HAVING count(*) > 1)"
        );
        let err = Check::query("SELECT * FROM sales.orders WHERE amount < 0").unwrap_err();
        assert!(format!("{err:#}").contains("must read the table as {table}"));
        assert!(Check::condition(" ; ").is_err());
    }
}
