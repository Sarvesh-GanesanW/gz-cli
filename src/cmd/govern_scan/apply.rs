//! `gz govern scan --apply`: tag flagged columns `pii=<kind>` through the
//! provider's column tag route (`GET`/`PUT
//! /lakehouse/tags/{w}/tables/{d}/{t}/columns/{id}/tags`, the web app's tag
//! editor). A column's tags are one document guarded by its version, so the
//! other tags are kept and a concurrent edit fails instead of being lost.

use anyhow::{Context, Result};
use reqwest::Method;
use serde_json::{json, Map, Value};

use super::detect::Confidence;
use super::{Flagged, LAKEHOUSE};
use crate::cmd::lakehouse::{column_tags_path, if_match};
use crate::cmd::lakehouse_table::LakehouseTable;
use crate::output;

/// The tag key the platform reads as sensitive (`pii=<anything but false>`).
const PII_TAG: &str = "pii";
/// Below this a column is reported but not tagged.
const LEAST_CONFIDENCE_TO_TAG: Confidence = Confidence::Medium;

/// What happened to one flagged column.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    Tagged,
    AlreadyTagged,
    /// Someone set `pii` to something else; their decision stands.
    KeptExisting(String),
    LowConfidence,
}

impl Outcome {
    fn describe(&self) -> String {
        match self {
            Outcome::Tagged => "tagged".to_string(),
            Outcome::AlreadyTagged => "already tagged".to_string(),
            Outcome::KeptExisting(value) => format!("kept its tag pii={value}"),
            Outcome::LowConfidence => "not tagged: low confidence".to_string(),
        }
    }
}

/// What a column needs: a tag write, or nothing and why.
enum Plan<'a> {
    Write(Planned<'a>),
    Skip(Outcome),
}

/// A tag write waiting for confirmation.
struct Planned<'a> {
    flagged: &'a Flagged,
    url: String,
    version: u64,
    tags: Map<String, Value>,
}

/// Tags every flagged column it should, after asking (unless `yes`), and
/// returns one entry per flagged column saying what happened.
pub async fn run(
    rt: &crate::cmd::Runtime,
    table: &LakehouseTable,
    flagged: &[Flagged],
    yes: bool,
) -> Result<Vec<Value>> {
    let mut outcomes: Vec<(&Flagged, Outcome)> = Vec::new();
    let mut planned = Vec::new();
    for column in flagged {
        if column.finding.confidence < LEAST_CONFIDENCE_TO_TAG {
            outcomes.push((column, Outcome::LowConfidence));
            continue;
        }
        match plan(rt, table, column).await? {
            Plan::Write(write) => planned.push(write),
            Plan::Skip(outcome) => outcomes.push((column, outcome)),
        }
    }
    if !planned.is_empty() {
        for write in &planned {
            output::info(&format!(
                "{}: pii={}",
                write.flagged.column.name,
                write.flagged.finding.kind.tag()
            ));
        }
        crate::cmd::confirm(
            yes,
            &format!(
                "Tag {} column(s) of {table} as personal data?",
                planned.len()
            ),
        )?;
    }
    let total = planned.len();
    for (done, write) in planned.into_iter().enumerate() {
        put_tags(rt, &write).await.with_context(|| {
            format!(
                "tagging {} failed after {done} of {total} column(s) were tagged",
                write.flagged.column.name
            )
        })?;
        outcomes.push((write.flagged, Outcome::Tagged));
    }
    Ok(outcomes
        .into_iter()
        .map(|(column, outcome)| {
            json!({
                "column": column.column.name,
                "tag": format!("{PII_TAG}={}", column.finding.kind.tag()),
                "outcome": outcome.describe(),
            })
        })
        .collect())
}

async fn plan<'a>(
    rt: &crate::cmd::Runtime,
    table: &LakehouseTable,
    flagged: &'a Flagged,
) -> Result<Plan<'a>> {
    let url = column_tags_path(
        &table.warehouse,
        &table.name.database,
        &table.name.table,
        flagged.column.id,
    );
    let current = rt
        .http
        .request_json(LAKEHOUSE, Method::GET, &url, &[], None)
        .await
        .with_context(|| format!("cannot read the tags of {}", flagged.column.name))?;
    let version = current["version"]
        .as_u64()
        .with_context(|| format!("the tags of {} came without a version", flagged.column.name))?;
    let mut tags = current["tags"].as_object().cloned().unwrap_or_default();
    let wanted = flagged.finding.kind.tag();
    match tags.get(PII_TAG) {
        Some(Value::String(existing)) if existing == wanted => {
            return Ok(Plan::Skip(Outcome::AlreadyTagged));
        }
        Some(Value::String(existing)) => {
            return Ok(Plan::Skip(Outcome::KeptExisting(existing.clone())));
        }
        Some(existing) => return Ok(Plan::Skip(Outcome::KeptExisting(existing.to_string()))),
        None => {}
    }
    tags.insert(PII_TAG.to_string(), json!(wanted));
    Ok(Plan::Write(Planned {
        flagged,
        url,
        version,
        tags,
    }))
}

async fn put_tags(rt: &crate::cmd::Runtime, write: &Planned<'_>) -> Result<()> {
    rt.http
        .request_json_with(
            LAKEHOUSE,
            Method::PUT,
            &write.url,
            &[],
            Some(json!({"tags": write.tags})),
            &[("If-Match", if_match(write.version))],
        )
        .await
        .map(|_| ())
}

pub fn show(applied: &[Value]) {
    for entry in applied {
        let line = format!(
            "{}: {} ({})",
            entry["column"].as_str().unwrap_or_default(),
            entry["outcome"].as_str().unwrap_or_default(),
            entry["tag"].as_str().unwrap_or_default()
        );
        // On stdout with the report, so the outcome of every column stays
        // together whatever is redirected.
        if entry["outcome"] == "tagged" {
            output::success(&line);
        } else {
            println!("- {line}");
        }
    }
}
