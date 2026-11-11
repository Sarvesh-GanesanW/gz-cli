//! Impact: what depends on a table, a column, or the jobs behind changed
//! files. Used by `gz impact` and by gz code before it edits a job file.
//!
//! - [`graph`] reads one lineage asset's downstream neighbourhood;
//! - [`walk`] walks those neighbourhoods, depth- and cycle-bounded;
//! - [`lookup`] does the HTTP: finding tables, the walk's fetches,
//!   visualizations and agents on top;
//! - [`changes`] maps changed files to jobs and jobs to the tables they write.

use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub mod changes;
pub mod graph;
pub mod lookup;
pub mod walk;

/// The kinds of dependents, in the order a summary names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    Dashboard,
    Visualization,
    Dataset,
    Agent,
    Job,
    Table,
}

impl Kind {
    pub const ALL: [Kind; 6] = [
        Kind::Dashboard,
        Kind::Visualization,
        Kind::Dataset,
        Kind::Agent,
        Kind::Job,
        Kind::Table,
    ];

    pub fn singular(self) -> &'static str {
        match self {
            Kind::Dashboard => "dashboard",
            Kind::Visualization => "visualization",
            Kind::Dataset => "dataset",
            Kind::Agent => "agent",
            Kind::Job => "job",
            Kind::Table => "table",
        }
    }

    pub fn plural(self) -> &'static str {
        match self {
            Kind::Dashboard => "dashboards",
            Kind::Visualization => "visualizations",
            Kind::Dataset => "datasets",
            Kind::Agent => "agents",
            Kind::Job => "jobs",
            Kind::Table => "tables",
        }
    }

    /// Where a kind sits in the data's flow: the order dependents are listed.
    pub fn flow_rank(self) -> u8 {
        match self {
            Kind::Table => 0,
            Kind::Job => 1,
            Kind::Dataset => 2,
            Kind::Visualization => 3,
            Kind::Dashboard => 4,
            Kind::Agent => 5,
        }
    }

    /// `dashboards` or `dashboard`, any case.
    pub fn parse(text: &str) -> Result<Kind> {
        let wanted = text.trim().to_ascii_lowercase();
        Kind::ALL
            .into_iter()
            .find(|k| k.plural() == wanted || k.singular() == wanted)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "unknown kind '{text}'; use any of: {}",
                    Kind::ALL.map(Kind::plural).join(", ")
                )
            })
    }

    pub fn parse_list(texts: &[String]) -> Result<Vec<Kind>> {
        let mut kinds: Vec<Kind> = texts
            .iter()
            .filter(|t| !t.trim().is_empty())
            .map(|t| Kind::parse(t))
            .collect::<Result<_>>()?;
        kinds.sort();
        kinds.dedup();
        Ok(kinds)
    }
}

/// How a dependent depends on what was asked about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Via {
    /// It reads the target itself.
    Direct,
    /// It depends on something else: a table, a dataset, an ontology model.
    Through { what: &'static str, name: String },
}

impl Via {
    pub fn describe(&self) -> String {
        match self {
            Via::Direct => "direct read".to_string(),
            Via::Through { what, name } => format!("via {what} {name}"),
        }
    }
}

/// For a column target: which of the dependent's columns it reaches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColumnUse {
    /// A whole table was asked about.
    NotAsked,
    /// These columns come from (or, for a job, read) the asked column.
    Columns(Vec<String>),
    /// Only table-level lineage connects them: gz does not guess.
    Unknown,
}

impl ColumnUse {
    /// A known answer beats an unknown one; known columns add up.
    pub fn merged(&self, other: ColumnUse) -> ColumnUse {
        match (self, other) {
            (ColumnUse::Columns(a), ColumnUse::Columns(b)) => {
                let mut all = a.clone();
                all.extend(b);
                all.sort();
                all.dedup();
                ColumnUse::Columns(all)
            }
            (ColumnUse::Columns(a), _) => ColumnUse::Columns(a.clone()),
            (_, ColumnUse::Columns(b)) => ColumnUse::Columns(b),
            (ColumnUse::Unknown, _) | (_, ColumnUse::Unknown) => ColumnUse::Unknown,
            (ColumnUse::NotAsked, ColumnUse::NotAsked) => ColumnUse::NotAsked,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Dependent {
    pub kind: Kind,
    pub id: String,
    pub name: String,
    pub owner: Option<String>,
    /// Table hops from the target.
    pub depth: u32,
    /// The target it descends from, as asked.
    pub root: String,
    pub via: Via,
    pub columns: ColumnUse,
    /// For tables: the lineage asset type (`TABLE`, `OBJECT` for files, …).
    pub asset_type: Option<String>,
    /// For tables: the jobs that wrote it from its parent.
    pub written_by: Vec<String>,
    /// For dashboards: their visualizations, to tell which ones show an
    /// affected dataset.
    pub visualization_ids: Vec<i64>,
}

/// A target and the lineage assets it was found as.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub text: String,
    pub table: String,
    pub column: Option<String>,
    pub assets: Vec<graph::AssetId>,
}

impl Target {
    /// `db.table` or `db.table.column`.
    pub fn parse(text: &str) -> Result<Target> {
        let parts: Vec<&str> = text.trim().split('.').collect();
        if parts.iter().any(|p| p.trim().is_empty()) || !(2..=3).contains(&parts.len()) {
            bail!("'{text}' is not database.table or database.table.column");
        }
        Ok(Target {
            text: text.trim().to_string(),
            table: format!("{}.{}", parts[0], parts[1]),
            column: parts.get(2).map(|c| c.to_string()),
            assets: Vec::new(),
        })
    }
}

/// Everything `gz impact` found.
#[derive(Debug, Default)]
pub struct Report {
    pub targets: Vec<Target>,
    pub depth: u32,
    pub dependents: Vec<Dependent>,
    /// What gz could not check, and why.
    pub notes: Vec<String>,
}

impl Report {
    pub fn counts(&self) -> BTreeMap<Kind, usize> {
        let mut counts = BTreeMap::new();
        for dependent in &self.dependents {
            *counts.entry(dependent.kind).or_insert(0) += 1;
        }
        counts
    }

    /// Leaves out the jobs being changed: a job that reads the table it
    /// writes is the change itself, not something downstream of it.
    pub fn drop_jobs(&mut self, ids: &[i64]) {
        let ids: Vec<String> = ids.iter().map(i64::to_string).collect();
        self.dependents
            .retain(|d| !(d.kind == Kind::Job && ids.contains(&d.id)));
    }

    /// Targets lineage has never seen.
    pub fn not_found(&self) -> Vec<&str> {
        self.targets
            .iter()
            .filter(|t| t.assets.is_empty())
            .map(|t| t.text.as_str())
            .collect()
    }

    /// The dependents of `kinds`, which make `--fail-on` fail.
    pub fn protected(&self, kinds: &[Kind]) -> BTreeMap<Kind, usize> {
        self.counts()
            .into_iter()
            .filter(|(kind, _)| kinds.contains(kind))
            .collect()
    }

    pub fn to_json(&self) -> Value {
        json!({
            "targets": self.targets.iter().map(|t| json!({
                "target": t.text,
                "table": t.table,
                "column": t.column,
                "assetIds": t.assets,
            })).collect::<Vec<_>>(),
            "notFound": self.not_found(),
            "depth": self.depth,
            "counts": counts_json(&self.counts()),
            "dependents": self.dependents.iter().map(dependent_json).collect::<Vec<_>>(),
            "notes": self.notes,
        })
    }

    /// One row per dependent, for the table view.
    pub fn rows(&self) -> Value {
        let column_asked = self.targets.iter().any(|t| t.column.is_some());
        let several_targets = self.targets.len() > 1;
        Value::Array(
            self.dependents
                .iter()
                .map(|d| {
                    let mut row = json!({
                        "kind": d.kind.singular(),
                        "name": d.name,
                        "id": d.id,
                        "how": d.via.describe(),
                        "owner": d.owner,
                    });
                    // With one target, every row is "of" it.
                    if several_targets {
                        row["of"] = json!(d.root);
                    }
                    if column_asked {
                        row["columns"] = json!(column_text(&d.columns));
                    }
                    row
                })
                .collect(),
        )
    }
}

pub const ROW_COLUMNS: &[&str] = &["kind", "name", "id", "how", "of", "columns", "owner"];

/// "Affects 2 dashboards, 3 datasets, 1 agent downstream".
pub fn summary_line(counts: &BTreeMap<Kind, usize>) -> String {
    let parts: Vec<String> = counts
        .iter()
        .filter(|(_, n)| **n > 0)
        .map(|(kind, n)| count_text(*kind, *n))
        .collect();
    if parts.is_empty() {
        return "Nothing downstream in lineage".to_string();
    }
    format!("Affects {} downstream", parts.join(", "))
}

pub fn count_text(kind: Kind, n: usize) -> String {
    let word = if n == 1 {
        kind.singular()
    } else {
        kind.plural()
    };
    format!("{n} {word}")
}

fn counts_json(counts: &BTreeMap<Kind, usize>) -> Value {
    Value::Object(
        Kind::ALL
            .iter()
            .map(|k| {
                (
                    k.plural().to_string(),
                    json!(counts.get(k).copied().unwrap_or(0)),
                )
            })
            .collect(),
    )
}

fn column_text(columns: &ColumnUse) -> String {
    match columns {
        ColumnUse::NotAsked => String::new(),
        ColumnUse::Columns(names) => names.join(", "),
        ColumnUse::Unknown => "not known (table-level lineage only)".to_string(),
    }
}

fn dependent_json(d: &Dependent) -> Value {
    let (column_level, columns) = match &d.columns {
        ColumnUse::NotAsked => (Value::Null, Value::Null),
        ColumnUse::Columns(names) => (json!("known"), json!(names)),
        ColumnUse::Unknown => (json!("unknown"), Value::Null),
    };
    json!({
        "kind": d.kind.singular(),
        "name": d.name,
        "id": d.id,
        "owner": d.owner,
        "depth": d.depth,
        "of": d.root,
        "how": d.via.describe(),
        "via": match &d.via {
            Via::Direct => Value::Null,
            Via::Through { what, name } => json!({"kind": what, "name": name}),
        },
        "columnLevel": column_level,
        "columns": columns,
        "assetType": d.asset_type,
        "writtenBy": (!d.written_by.is_empty()).then_some(&d.written_by),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_parse_singular_or_plural_and_refuse_others() {
        assert_eq!(Kind::parse("Dashboards").unwrap(), Kind::Dashboard);
        assert_eq!(Kind::parse("agent").unwrap(), Kind::Agent);
        let err = Kind::parse("charts").unwrap_err().to_string();
        assert!(err.contains("dashboards, visualizations"), "{err}");
        assert_eq!(
            Kind::parse_list(&["agents".into(), "dashboards".into(), "agent".into()]).unwrap(),
            vec![Kind::Dashboard, Kind::Agent]
        );
    }

    #[test]
    fn targets_are_two_or_three_parts() {
        let column = Target::parse("sales.orders.amount").unwrap();
        assert_eq!(
            (column.table.as_str(), column.column.as_deref()),
            ("sales.orders", Some("amount"))
        );
        assert_eq!(Target::parse("sales.orders").unwrap().column, None);
        for bad in ["orders", "a..b", "a.b.c.d"] {
            assert!(Target::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_summary_names_counts_in_order_and_skips_zeros() {
        let counts = BTreeMap::from([
            (Kind::Table, 4),
            (Kind::Agent, 1),
            (Kind::Dashboard, 2),
            (Kind::Dataset, 3),
            (Kind::Job, 0),
        ]);
        assert_eq!(
            summary_line(&counts),
            "Affects 2 dashboards, 3 datasets, 1 agent, 4 tables downstream"
        );
        assert_eq!(
            summary_line(&BTreeMap::new()),
            "Nothing downstream in lineage"
        );
    }

    #[test]
    fn a_known_column_answer_wins_over_an_unknown_one() {
        let known = ColumnUse::Columns(vec!["b".into()]);
        assert_eq!(ColumnUse::Unknown.merged(known.clone()), known);
        assert_eq!(
            known.merged(ColumnUse::Columns(vec!["a".into(), "b".into()])),
            ColumnUse::Columns(vec!["a".into(), "b".into()])
        );
        assert_eq!(
            ColumnUse::NotAsked.merged(ColumnUse::NotAsked),
            ColumnUse::NotAsked
        );
    }
}
