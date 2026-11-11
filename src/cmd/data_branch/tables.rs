//! Which lakehouse tables a job writes, and the names its branches get.
//!
//! The tables come from the job's lineage (`GET /etl/lineage/jobs/{id}`,
//! groundzero-etl-provider-api app.py:2121; the web app reads it in
//! apiCatalog/lineageApi.js:133): the sinks the compiler found in its saved
//! code (`graph.compilerGraph.assets` with `directions` containing `sink`),
//! and the tables its past runs were seen writing (the targets of
//! `graph.relations`). An asset names its connection, whose config holds
//! the warehouse.

use std::collections::BTreeSet;

use anyhow::{bail, Result};
use serde_json::Value;

const LAKEHOUSE_PLATFORM: &str = "gzlakehouse";
const LAKEHOUSE_CONNECTION: &str = "GZLAKEHOUSE";
/// The longest branch name the providers accept.
const MAX_BRANCH_CHARS: usize = 64;
const BRANCH_PREFIX: &str = "try-";

/// A table the job writes, before its connection's warehouse is known.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct WrittenTable {
    pub connection: String,
    pub database: String,
    pub table: String,
}

fn text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// `MFG.OEE_DAILY` → (`mfg`, `oee_daily`); a quoted part keeps its case.
fn split_name(qualified: &str) -> Option<(String, String)> {
    let part = |raw: &str| {
        let quoted = raw.len() >= 2 && (raw.starts_with('"') || raw.starts_with('`'));
        if quoted {
            raw[1..raw.len() - 1].to_string()
        } else {
            raw.to_lowercase()
        }
    };
    let parts: Vec<&str> = qualified.split('.').filter(|p| !p.is_empty()).collect();
    let [.., database, table] = parts[..] else {
        return None;
    };
    Some((part(database), part(table)))
}

/// The name the job's lineage shows for a compiled asset, in its own case.
fn display_name(graph: &Value, connection: &str, key: &str) -> Option<String> {
    graph["assets"].as_array()?.iter().find_map(|asset| {
        let bound = asset["bindings"].as_array()?.iter().any(|binding| {
            text(&binding["connectionId"]).as_deref() == Some(connection)
                && binding["externalKey"].as_str() == Some(key)
        });
        bound.then(|| asset["displayName"].as_str().map(str::to_string))?
    })
}

fn compiled_sinks(graph: &Value) -> Vec<WrittenTable> {
    let sinks = graph["compilerGraph"]["assets"]
        .as_array()
        .into_iter()
        .flatten();
    sinks
        .filter(|asset| {
            asset["platform"].as_str() == Some(LAKEHOUSE_PLATFORM)
                && asset["directions"]
                    .as_array()
                    .is_some_and(|d| d.iter().any(|v| v.as_str() == Some("sink")))
        })
        .filter_map(|asset| {
            let connection = text(&asset["connection"]["id"])?;
            let key = asset["attributes"]["qualifiedName"]
                .as_str()
                .or_else(|| asset["name"].as_str())?;
            let shown = display_name(graph, &connection, key).unwrap_or_else(|| key.to_string());
            let (database, table) = split_name(&shown)?;
            Some(WrittenTable {
                connection,
                database,
                table,
            })
        })
        .collect()
}

fn observed_sinks(graph: &Value) -> Vec<WrittenTable> {
    let targets: BTreeSet<String> = graph["relations"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|relation| text(&relation["targetAssetId"]))
        .collect();
    let assets = graph["assets"].as_array().into_iter().flatten();
    assets
        .filter(|asset| {
            asset["platform"].as_str() == Some(LAKEHOUSE_PLATFORM)
                && text(&asset["assetId"]).is_some_and(|id| targets.contains(&id))
        })
        .filter_map(|asset| {
            let connection = asset["bindings"]
                .as_array()?
                .iter()
                .find(|b| b["connectionType"].as_str() == Some(LAKEHOUSE_CONNECTION))
                .and_then(|b| text(&b["connectionId"]))?;
            let (database, table) = split_name(asset["displayName"].as_str()?)?;
            Some(WrittenTable {
                connection,
                database,
                table,
            })
        })
        .collect()
}

/// Every lakehouse table the job's lineage says it writes, once each.
pub fn written_tables(lineage: &Value) -> Vec<WrittenTable> {
    let graph = &lineage["graph"];
    let mut seen = BTreeSet::new();
    compiled_sinks(graph)
        .into_iter()
        .chain(observed_sinks(graph))
        .filter(|t| {
            seen.insert((
                t.connection.clone(),
                t.database.to_lowercase(),
                t.table.to_lowercase(),
            ))
        })
        .collect()
}

/// Letters, digits, `_` and `-`, at most 64: Spark reads a branch as
/// `<table>.branch_<name>` and a run passes the name on as one word.
pub fn check_branch(name: &str) -> Result<()> {
    let allowed = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    if name.is_empty() || name.chars().count() > MAX_BRANCH_CHARS || !name.chars().all(allowed) {
        bail!(
            "--branch must be 1 to {MAX_BRANCH_CHARS} letters, digits, '_' or '-' (got '{name}')"
        );
    }
    if name.eq_ignore_ascii_case("main") {
        bail!("--branch can't be main: main is what publishing changes");
    }
    Ok(())
}

/// `try-clean-orders-20261111-093000`, from the job's name and the time.
pub fn default_branch(job: &str, epoch_secs: i64) -> String {
    let (year, month, day) = crate::tui::format::civil_from_days(epoch_secs.div_euclid(86_400));
    let seconds = epoch_secs.rem_euclid(86_400);
    let stamp = format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
        seconds / 3600,
        seconds % 3600 / 60,
        seconds % 60
    );
    let room = MAX_BRANCH_CHARS - BRANCH_PREFIX.len() - stamp.len() - 1;
    let job: String = job
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .take(room)
        .collect();
    format!("{BRANCH_PREFIX}{job}-{stamp}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lineage() -> Value {
        json!({"graph": {
            "assets": [
                {"assetId": 1545, "platform": "gzlakehouse", "displayName": "mfg_ops.production_runs",
                 "bindings": [{"connectionId": 1360, "connectionType": "GZLAKEHOUSE",
                               "externalKey": "MFG_OPS.PRODUCTION_RUNS"}]},
                {"assetId": 1551, "platform": "gzlakehouse", "displayName": "mfg_ops.oee_daily",
                 "bindings": [{"connectionId": 1360, "connectionType": "GZLAKEHOUSE",
                               "externalKey": "MFG_OPS.OEE_DAILY"}]},
                {"assetId": 1600, "platform": "gzlakehouse", "displayName": "Mfg_Ops.Shift_Log",
                 "bindings": [{"connectionId": 1360, "connectionType": "GZLAKEHOUSE",
                               "externalKey": "MFG_OPS.SHIFT_LOG"}]},
            ],
            "relations": [
                {"sourceAssetId": 1545, "targetAssetId": 1551},
                {"sourceAssetId": 1545, "targetAssetId": 1600},
            ],
            "compilerGraph": {"assets": [
                {"platform": "gzlakehouse", "directions": ["source"], "connection": {"id": "1360"},
                 "attributes": {"qualifiedName": "MFG_OPS.PRODUCTION_RUNS"}},
                {"platform": "gzlakehouse", "directions": ["sink", "source"], "connection": {"id": "1360"},
                 "attributes": {"qualifiedName": "MFG_OPS.OEE_DAILY"}},
                {"platform": "gzlakehouse", "directions": ["sink"], "connection": {"id": "1360"},
                 "attributes": {"qualifiedName": "MFG_OPS.NEW_TABLE"}},
                {"platform": "postgres", "directions": ["sink"], "connection": {"id": "7"},
                 "attributes": {"qualifiedName": "public.orders"}},
            ]},
        }})
    }

    fn written(connection: &str, database: &str, table: &str) -> WrittenTable {
        WrittenTable {
            connection: connection.into(),
            database: database.into(),
            table: table.into(),
        }
    }

    #[test]
    fn finds_compiled_and_observed_lakehouse_sinks_once_each() {
        assert_eq!(
            written_tables(&lineage()),
            vec![
                written("1360", "mfg_ops", "oee_daily"),
                written("1360", "mfg_ops", "new_table"),
                written("1360", "mfg_ops", "shift_log"),
            ],
            "sources and other platforms are left out; unquoted names are lower-cased, as Spark reads them"
        );
    }

    #[test]
    fn quoted_names_keep_their_case() {
        assert_eq!(
            split_name(r#""Sales"."Orders""#),
            Some(("Sales".into(), "Orders".into()))
        );
        assert_eq!(split_name("cat.DB.T"), Some(("db".into(), "t".into())));
        assert_eq!(split_name("lonely"), None);
    }

    #[test]
    fn a_job_without_lineage_writes_nothing_known() {
        assert!(written_tables(&json!({"graph": null})).is_empty());
    }

    #[test]
    fn branch_names_are_checked_like_the_providers_do() {
        check_branch("try-clean_orders-1").unwrap();
        for bad in [
            "",
            "main",
            "MAIN",
            "has space",
            "semi;colon",
            &"x".repeat(65),
        ] {
            assert!(check_branch(bad).is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn default_branches_name_the_job_and_the_time() {
        // 2026-11-11 09:30:05 UTC
        let at = 1_794_389_405;
        assert_eq!(
            default_branch("clean-orders", at),
            "try-clean-orders-20261111-093005"
        );
        let long = default_branch(&"j".repeat(100), at);
        assert_eq!(long.len(), 64);
        check_branch(&long).unwrap();
    }
}
