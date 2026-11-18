//! The lakehouse matches database and table names exactly (`DemoDB` is not
//! `demodb`), while a job's lineage can spell them in another case: the
//! compiler folds unquoted SQL names to lower case. Before a try makes its
//! branches, each table is matched against the names the warehouse stores:
//!
//! - `GET /lakehouse/warehouse/{wh}/namespaces` → `{response: [{namespace}]}`
//!   (groundzero-iceberg-provider-api app_routes/warehouses.py:228)
//! - `GET /lakehouse/schema/{wh}/{db}` → `{response: [{tableName, …}]}`, one
//!   row per column (app_routes/schemas.py:156)

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{bail, Result};
use reqwest::Method;
use serde_json::Value;

use crate::cmd::lakehouse_table::LakehouseTable;
use crate::cmd::{seg, Runtime};
use crate::output;

const LAKEHOUSE: &str = "lakehouse";

/// `wanted` as the warehouse stores it: the exact name when it is stored,
/// else the one stored name that differs only in case. `None` when nothing
/// matches (a table the run will create). Several names that differ only in
/// case are an error, since gz cannot tell which one the job means.
fn stored_spelling<'a>(stored: &'a [String], wanted: &str, kind: &str) -> Result<Option<&'a str>> {
    if let Some(exact) = stored.iter().find(|name| *name == wanted) {
        return Ok(Some(exact));
    }
    let folded: Vec<&String> = stored
        .iter()
        .filter(|name| name.eq_ignore_ascii_case(wanted))
        .collect();
    match folded[..] {
        [] => Ok(None),
        [only] => Ok(Some(only)),
        _ => {
            let names: Vec<&str> = folded.iter().map(|name| name.as_str()).collect();
            bail!(
                "the job writes {kind} {wanted}, and the warehouse has {} that differ only in case; \
                 pass the table with --table warehouse.database.table spelled exactly",
                names.join(", ")
            )
        }
    }
}

/// The distinct values of `field` across the reply's `response` rows.
fn names_in(reply: &Value, field: &str) -> Vec<String> {
    let rows = reply["response"].as_array().into_iter().flatten();
    let names: BTreeSet<String> = rows
        .filter_map(|row| row[field].as_str())
        .map(str::to_string)
        .collect();
    names.into_iter().collect()
}

/// The stored names one listing returns, or `None` (with a warning) when the
/// caller may not list them, so the names are used as the job wrote them.
async fn listed(rt: &Runtime, path: &str, field: &str, what: &str) -> Option<Vec<String>> {
    match rt
        .http
        .request_json(LAKEHOUSE, Method::GET, path, &[], None)
        .await
    {
        Ok(reply) => Some(names_in(&reply, field)),
        Err(err) => {
            output::warn(&format!(
                "cannot list {what} to match the names' case ({err:#}); using them as the job's lineage spells them"
            ));
            None
        }
    }
}

/// Each table with its database and table name spelled as stored.
pub async fn in_stored_case(
    rt: &Runtime,
    tables: Vec<LakehouseTable>,
) -> Result<Vec<LakehouseTable>> {
    let mut databases: BTreeMap<String, Option<Vec<String>>> = BTreeMap::new();
    let mut tables_of: BTreeMap<(String, String), Option<Vec<String>>> = BTreeMap::new();
    let mut matched = Vec::with_capacity(tables.len());
    for mut table in tables {
        if !databases.contains_key(&table.warehouse) {
            let path = format!("/lakehouse/warehouse/{}/namespaces", seg(&table.warehouse));
            let what = format!("the databases of warehouse {}", table.warehouse);
            let names = listed(rt, &path, "namespace", &what).await;
            databases.insert(table.warehouse.clone(), names);
        }
        if let Some(stored) = &databases[&table.warehouse] {
            if let Some(name) = stored_spelling(stored, &table.name.database, "database")? {
                table.name.database = name.to_string();
            }
        }
        let key = (table.warehouse.clone(), table.name.database.clone());
        if !tables_of.contains_key(&key) {
            let path = format!(
                "/lakehouse/schema/{}/{}",
                seg(&table.warehouse),
                seg(&table.name.database)
            );
            let what = format!("the tables of {}.{}", table.warehouse, table.name.database);
            let names = listed(rt, &path, "tableName", &what).await;
            tables_of.insert(key.clone(), names);
        }
        if let Some(stored) = &tables_of[&key] {
            if let Some(name) = stored_spelling(stored, &table.name.table, "table")? {
                table.name.table = name.to_string();
            }
        }
        matched.push(table);
    }
    Ok(matched)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn stored(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn a_name_in_another_case_takes_the_stored_spelling() {
        let names = stored(&["Demo", "DemoDB", "audit"]);
        assert_eq!(
            stored_spelling(&names, "demodb", "database").unwrap(),
            Some("DemoDB")
        );
    }

    #[test]
    fn an_exact_name_wins_over_one_that_differs_in_case() {
        let names = stored(&["orders", "Orders"]);
        assert_eq!(
            stored_spelling(&names, "Orders", "table").unwrap(),
            Some("Orders")
        );
    }

    #[test]
    fn a_table_the_run_will_create_keeps_its_name() {
        let names = stored(&["ProductionMetrics"]);
        assert_eq!(stored_spelling(&names, "new_table", "table").unwrap(), None);
    }

    #[test]
    fn names_that_differ_only_in_case_are_refused_with_both_shown() {
        let names = stored(&["orders", "Orders"]);
        let err = stored_spelling(&names, "ORDERS", "table").unwrap_err();
        assert!(err.to_string().contains("orders, Orders"), "{err}");
    }

    #[test]
    fn a_listing_gives_each_name_once() {
        let reply = json!({"status": 200, "response": [
            {"tableName": "ProductionMetrics", "columnName": "a"},
            {"tableName": "ProductionMetrics", "columnName": "b"},
            {"tableName": "AR_REPORT", "columnName": "a"},
        ]});
        assert_eq!(
            names_in(&reply, "tableName"),
            stored(&["AR_REPORT", "ProductionMetrics"])
        );
    }
}
