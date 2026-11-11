//! A lakehouse table's history: its snapshots, branches and tags, as
//! `GET /lakehouse/tables/{warehouse}/{database}/{table}/versions` answers
//! (groundzero-iceberg-provider-api app_routes/table_versions.py:120 and
//! iceberg/table_versions.py:45; the web app reads it in
//! pages/Utils/lakehouseTableUtils.js:26). Snapshot ids are text.
//!
//! `gz diff` and `gz bisect` read a table as it was at a snapshot with
//! `VERSION AS OF <id>`, so everything that ends up in SQL is checked here:
//! ids are digits only and names are quoted.

pub mod bisect;
pub mod diff;
pub(crate) mod when;

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use reqwest::Method;
use serde_json::Value;

use crate::cmd::seg;
use crate::http::ApiClient;
use crate::sql::results::Page;

const SVC: &str = "lakehouse";

/// Something that runs one SQL statement and returns its first page of rows:
/// a lakehouse session, or a script of answers in tests.
pub(crate) trait Statements {
    async fn rows(&self, statement: &str) -> Result<Page>;
}

impl Statements for crate::sql::Runner {
    async fn rows(&self, statement: &str) -> Result<Page> {
        crate::sql::Runner::rows(self, statement).await
    }
}

/// `database.table`, as it is written in SQL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableName {
    pub database: String,
    pub table: String,
}

impl TableName {
    pub fn parse(text: &str) -> Result<Self> {
        let parts: Vec<&str> = text.split('.').collect();
        let [database, table] = parts[..] else {
            bail!("write the table as database.table (got '{text}')");
        };
        if database.is_empty() || table.is_empty() {
            bail!("write the table as database.table (got '{text}')");
        }
        Ok(Self {
            database: database.to_string(),
            table: table.to_string(),
        })
    }

    /// The table as it was at `snapshot`, for a `FROM` clause.
    pub fn at(&self, snapshot: &Snapshot) -> String {
        format!(
            "{}.{} VERSION AS OF {}",
            ident(&self.database),
            ident(&self.table),
            snapshot.id
        )
    }
}

impl std::fmt::Display for TableName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.database, self.table)
    }
}

/// A name quoted for Spark SQL.
pub fn ident(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// Digits only: it is written into SQL as it is.
    pub id: String,
    pub parent: Option<String>,
    /// ISO time in UTC, as the server sends it.
    pub committed_at: String,
    /// `append`, `overwrite`, `delete` or `replace`.
    pub operation: Option<String>,
    /// What the writer recorded with the commit (`added-records`,
    /// `total-records`, `app-name`, …).
    pub summary: BTreeMap<String, String>,
}

impl Snapshot {
    fn parse(value: &Value) -> Result<Self> {
        let id = snapshot_id(&value["snapshotId"]).context("a snapshot came without an id")?;
        let summary = value["summary"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(key, value)| (key.clone(), crate::output::scalar_text(value)))
            .collect();
        Ok(Self {
            id,
            parent: snapshot_id(&value["parentSnapshotId"]),
            committed_at: value["committedAt"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            operation: value["operation"].as_str().map(str::to_string),
            summary,
        })
    }

    /// A count the writer recorded, such as `total-records`.
    pub fn count(&self, key: &str) -> Option<u64> {
        self.summary.get(key)?.parse().ok()
    }

    /// `2026-09-16 10:19 UTC`.
    pub fn committed(&self) -> String {
        when::display(&self.committed_at)
    }

    /// The Spark application that wrote the snapshot, when it said so.
    pub fn writer(&self) -> Option<String> {
        let name = self.summary.get("app-name").filter(|n| !n.is_empty());
        let app = self
            .summary
            .get("spark.app.id")
            .or_else(|| self.summary.get("app-id"))
            .filter(|a| !a.is_empty());
        match (name, app) {
            (Some(name), Some(app)) => Some(format!("{name} (Spark application {app})")),
            (Some(name), None) => Some(name.clone()),
            (None, Some(app)) => Some(format!("Spark application {app}")),
            (None, None) => None,
        }
    }
}

/// A snapshot id from the reply; `None` unless it is all digits.
fn snapshot_id(value: &Value) -> Option<String> {
    let text = match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        _ => return None,
    };
    (!text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())).then_some(text)
}

/// A branch or a tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedRef {
    pub name: String,
    pub snapshot: String,
}

fn named_refs(value: &Value) -> Vec<NamedRef> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            Some(NamedRef {
                name: entry["name"].as_str()?.to_string(),
                snapshot: snapshot_id(&entry["snapshotId"])?,
            })
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct History {
    pub current: Option<String>,
    /// Newest first.
    pub snapshots: Vec<Snapshot>,
    pub branches: Vec<NamedRef>,
    pub tags: Vec<NamedRef>,
}

impl History {
    /// The server's own description of the table's versions.
    pub async fn fetch(http: &ApiClient, warehouse: &str, table: &TableName) -> Result<Value> {
        let path = format!(
            "/lakehouse/tables/{}/{}/{}/versions",
            seg(warehouse),
            seg(&table.database),
            seg(&table.table)
        );
        let reply = http
            .request_json(SVC, Method::GET, &path, &[], None)
            .await
            .with_context(|| format!("cannot read the history of {table} in {warehouse}"))?;
        Ok(reply["response"].clone())
    }

    pub async fn load(http: &ApiClient, warehouse: &str, table: &TableName) -> Result<Self> {
        Self::parse(&Self::fetch(http, warehouse, table).await?)
    }

    pub fn parse(response: &Value) -> Result<Self> {
        let snapshots = response["snapshots"]
            .as_array()
            .context("the server sent no snapshot list for this table")?
            .iter()
            .map(Snapshot::parse)
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            current: snapshot_id(&response["currentSnapshotId"]),
            snapshots,
            branches: named_refs(&response["branches"]),
            tags: named_refs(&response["tags"]),
        })
    }

    pub fn snapshot(&self, id: &str) -> Option<&Snapshot> {
        self.snapshots.iter().find(|snapshot| snapshot.id == id)
    }

    /// `head` and the snapshots it was built on, newest first. The line ends
    /// where a parent is no longer kept (expired) or was never there.
    pub fn line(&self, head: &Snapshot) -> Vec<&Snapshot> {
        let mut line = Vec::new();
        let mut next = self.snapshot(&head.id);
        // A snapshot is never its own ancestor; the length bound only guards
        // against a reply that says otherwise.
        while let Some(snapshot) = next.filter(|_| line.len() < self.snapshots.len()) {
            line.push(snapshot);
            next = snapshot.parent.as_deref().and_then(|id| self.snapshot(id));
        }
        line
    }

    /// The names a snapshot goes by: `current`, branches and tags.
    pub fn names(&self, snapshot: &Snapshot) -> Vec<String> {
        let mut names = Vec::new();
        if self.current.as_deref() == Some(snapshot.id.as_str()) {
            names.push("current".to_string());
        }
        let pointing_here = |refs: &[NamedRef], kind: &str| -> Vec<String> {
            refs.iter()
                .filter(|named| named.snapshot == snapshot.id)
                .map(|named| format!("{kind} {}", named.name))
                .collect()
        };
        names.extend(pointing_here(&self.branches, "branch"));
        names.extend(pointing_here(&self.tags, "tag"));
        names
    }

    fn current_snapshot(&self) -> Result<&Snapshot> {
        self.current
            .as_deref()
            .and_then(|id| self.snapshot(id))
            .context("the table has no snapshot yet")
    }

    /// The snapshot `reference` names: a branch or tag, `current`,
    /// `previous`, a snapshot id, or a UTC date or time (the snapshot that
    /// was current then).
    pub fn resolve(&self, reference: &str) -> Result<&Snapshot> {
        let reference = reference.trim();
        let named = self
            .branches
            .iter()
            .chain(&self.tags)
            .find(|named| named.name == reference);
        if let Some(named) = named {
            return self.snapshot(&named.snapshot).with_context(|| {
                format!("'{reference}' points at a snapshot the table no longer keeps")
            });
        }
        match reference {
            "current" => return self.current_snapshot(),
            "previous" => {
                let current = self.current_snapshot()?;
                return self.line(current).get(1).copied().context(
                    "the current snapshot is the oldest one kept, so there is no previous one",
                );
            }
            _ => {}
        }
        if reference.bytes().all(|b| b.is_ascii_digit()) && !reference.is_empty() {
            return self.snapshot(reference).with_context(|| {
                format!("the table has no snapshot {reference} (it may have expired); `gz table versions` lists the ones kept")
            });
        }
        let Some(moment) = when::parse(reference) else {
            bail!(
                "'{reference}' is not a branch, a tag, a snapshot id, `current`, `previous`, or a UTC date or time like 2026-09-16 or 2026-09-16T08:00"
            );
        };
        let line = self.line(self.current_snapshot()?);
        let oldest = line.last().map(|s| s.committed()).unwrap_or_default();
        line.into_iter()
            .find(|snapshot| when::sortable(&snapshot.committed_at) <= moment.as_str())
            .with_context(|| {
                format!("the table had no snapshot at {reference}: the oldest one kept is from {oldest}")
            })
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use serde_json::{json, Value};

    use super::{History, Statements};
    use crate::sql::results::Page;

    /// A result page with the given `(name, type)` columns.
    pub fn page(schema: &[(&str, &str)], rows: Vec<Value>) -> Page {
        Page {
            columns: schema.iter().map(|(name, _)| name.to_string()).collect(),
            column_types: schema.iter().map(|(_, kind)| kind.to_string()).collect(),
            rows,
            page_number: 1,
            has_more: false,
            total_records: None,
        }
    }

    /// A session that answers from a script and records what it was asked.
    pub struct Scripted {
        answers: Mutex<VecDeque<Result<Page, String>>>,
        asked: Mutex<Vec<String>>,
    }

    impl Scripted {
        pub fn new(answers: Vec<Result<Page, String>>) -> Self {
            Self {
                answers: Mutex::new(answers.into()),
                asked: Mutex::new(Vec::new()),
            }
        }

        pub fn asked(&self) -> Vec<String> {
            self.asked.lock().unwrap().clone()
        }
    }

    impl Statements for Scripted {
        async fn rows(&self, statement: &str) -> anyhow::Result<Page> {
            self.asked.lock().unwrap().push(statement.to_string());
            let answer = self.answers.lock().unwrap().pop_front();
            match answer {
                Some(Ok(page)) => Ok(page),
                Some(Err(message)) => Err(anyhow::anyhow!(message)),
                None => Err(anyhow::anyhow!("no answer scripted for: {statement}")),
            }
        }
    }

    /// One snapshot as the server lists it.
    pub fn snapshot(id: u64, parent: Option<u64>, committed_at: &str, summary: Value) -> Value {
        json!({
            "snapshotId": id.to_string(),
            "parentSnapshotId": parent.map(|p| p.to_string()),
            "committedAt": committed_at,
            "operation": "append",
            "summary": summary,
        })
    }

    /// Four snapshots on `main` (101 → 104, one a day from 2026-09-01), a
    /// side branch `audit` (201) off 102, and the tag `release` on 102.
    pub fn history() -> History {
        let records = |added: u64, total: u64| {
            json!({
                "added-records": added.to_string(),
                "added-data-files": "1",
                "total-records": total.to_string(),
                "total-data-files": (total / 10).to_string(),
                "total-files-size": (total * 100).to_string(),
                "app-name": "load_orders",
                "spark.app.id": format!("app-{total}"),
            })
        };
        History::parse(&json!({
            "currentSnapshotId": "104",
            "snapshots": [
                snapshot(201, Some(102), "2026-09-05T09:00:00+00:00", records(5, 25)),
                snapshot(104, Some(103), "2026-09-04T09:00:00.500000+00:00", records(10, 40)),
                snapshot(103, Some(102), "2026-09-03T09:00:00+00:00", records(10, 30)),
                snapshot(102, Some(101), "2026-09-02T09:00:00+00:00", records(10, 20)),
                snapshot(101, None, "2026-09-01T09:00:00+00:00", records(10, 10)),
            ],
            "branches": [
                {"name": "main", "snapshotId": "104"},
                {"name": "audit", "snapshotId": "201"},
            ],
            "tags": [{"name": "release", "snapshotId": "102"}],
        }))
        .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::test_support::history;
    use super::*;

    fn ids(line: &[&Snapshot]) -> Vec<String> {
        line.iter().map(|s| s.id.clone()).collect()
    }

    #[test]
    fn a_line_follows_parents_and_stops_at_an_expired_one() {
        let history = history();
        let main = history.line(history.resolve("current").unwrap());
        assert_eq!(ids(&main), ["104", "103", "102", "101"]);
        let audit = history.line(history.resolve("audit").unwrap());
        assert_eq!(
            ids(&audit),
            ["201", "102", "101"],
            "a branch shares its base"
        );

        let mut expired = history.clone();
        expired.snapshots.retain(|s| s.id != "102");
        let kept = expired.line(expired.resolve("current").unwrap());
        assert_eq!(ids(&kept), ["104", "103"], "102 expired, so the line ends");
    }

    #[test]
    fn a_reference_names_one_snapshot() {
        let history = history();
        let id = |reference: &str| history.resolve(reference).map(|s| s.id.clone());
        assert_eq!(id("current").unwrap(), "104");
        assert_eq!(id("previous").unwrap(), "103");
        assert_eq!(id("release").unwrap(), "102");
        assert_eq!(id("audit").unwrap(), "201");
        assert_eq!(id("102").unwrap(), "102");
        assert_eq!(
            id("2026-09-03").unwrap(),
            "102",
            "a date is its first moment: 103 came later that day"
        );
        assert_eq!(id("2026-09-03T09:00").unwrap(), "103");
        assert_eq!(id("2026-09-04 09:00:00Z").unwrap(), "104");
        assert_eq!(
            id("2026-09-09").unwrap(),
            "104",
            "times follow the current line, not the audit branch"
        );

        let error = |reference: &str| format!("{:#}", id(reference).unwrap_err());
        assert!(error("999").contains("no snapshot 999"), "{}", error("999"));
        assert!(error("2026-08-01").contains("oldest one kept is from 2026-09-01 09:00 UTC"));
        assert!(error("yesterday").contains("not a branch"));
    }

    #[test]
    fn previous_needs_a_kept_parent() {
        let mut history = history();
        history.snapshots.retain(|s| s.id == "104");
        let err = history.resolve("previous").unwrap_err();
        assert!(format!("{err:#}").contains("oldest one kept"));
    }

    #[test]
    fn only_digit_ids_are_accepted_because_they_go_into_sql() {
        let reply = json!({
            "currentSnapshotId": "1",
            "snapshots": [{"snapshotId": "1 OR 1=1", "committedAt": "", "summary": {}}],
        });
        assert!(History::parse(&reply).is_err());
        let table = TableName::parse("sales.or`ders").unwrap();
        let history = history();
        assert_eq!(
            table.at(history.resolve("current").unwrap()),
            "`sales`.`or``ders` VERSION AS OF 104"
        );
        assert!(TableName::parse("orders").is_err());
        assert!(TableName::parse("a.b.c").is_err());
    }

    #[test]
    fn a_snapshot_says_who_wrote_it_and_what_it_goes_by() {
        let history = history();
        let current = history.resolve("current").unwrap();
        assert_eq!(
            current.writer().as_deref(),
            Some("load_orders (Spark application app-40)")
        );
        assert_eq!(current.count("total-records"), Some(40));
        assert_eq!(current.committed(), "2026-09-04 09:00 UTC");
        assert_eq!(history.names(current), ["current", "branch main"]);
        assert_eq!(
            history.names(history.resolve("102").unwrap()),
            ["tag release"]
        );
    }
}
