//! Finding where a table's check results were saved, without `--results`.
//!
//! When a Spark job saves results (`resultsTable=`, `saveDataQualityResults`)
//! the runtime records a lineage edge from the checked table to the results
//! table with operation [`RESULTS_LINK_OPERATION`] (gz-base-rte-spark
//! connectors/DATAQUALITY.py `_linkResultsTable`). The lakehouse provider's
//! table lineage (`GET /lakehouse/governance/lineage/table`,
//! app_routes/governance.py `getTableLineage`) returns a table's downstream
//! edges, newest first, with tables the caller may not read redacted to
//! `{"redacted": true}`. Edges stay inside one warehouse.

use anyhow::{Context, Result};
use reqwest::Method;
use serde_json::Value;

use crate::cmd::lakehouse_table::LakehouseTable;
use crate::cmd::Runtime;
use crate::table_history::TableName;

/// Must match `RESULTS_LINK_OPERATION` in the runtime's DATAQUALITY.py.
pub const RESULTS_LINK_OPERATION: &str = "QUALITY_RESULTS";
const LAKEHOUSE: &str = "lakehouse";
const LINEAGE_ROUTE: &str = "/lakehouse/governance/lineage/table";
/// The route's largest page (`_MAX_LINEAGE_LIMIT`): edges come newest
/// first, one per saving run, so this reaches far back.
const LINEAGE_LIMIT: u32 = 1000;
/// Columns every results table has (qualipilot `RESULT_ROW_COLUMNS`).
const RESULT_COLUMNS: [&str; 5] = ["source", "checked_at", "check_name", "severity", "status"];

/// The results tables linked to a checked table.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct LinkedResults {
    /// Each table once, in the order the newest link names them.
    pub tables: Vec<TableName>,
    /// Some linked table is one the caller may not read (the route doesn't
    /// say which, nor how many).
    pub hidden: bool,
}

/// Reads the links from the table's downstream lineage.
pub async fn linked_results(rt: &Runtime, checked: &LakehouseTable) -> Result<LinkedResults> {
    let query = [
        ("warehouseName", checked.warehouse.clone()),
        ("databaseName", checked.name.database.clone()),
        ("tableName", checked.name.table.clone()),
        ("direction", "downstream".to_string()),
        ("depth", "1".to_string()),
        ("limit", LINEAGE_LIMIT.to_string()),
    ]
    .map(|(key, value)| (key.to_string(), value));
    let reply = rt
        .http
        .request_json(LAKEHOUSE, Method::GET, LINEAGE_ROUTE, &query, None)
        .await
        .with_context(|| format!("cannot read the lineage of {checked}"))?;
    Ok(parse_links(&reply))
}

/// The results tables in a lineage reply.
pub fn parse_links(reply: &Value) -> LinkedResults {
    let mut linked = LinkedResults::default();
    for edge in reply["edges"].as_array().into_iter().flatten() {
        if edge["operation"].as_str() != Some(RESULTS_LINK_OPERATION) {
            continue;
        }
        let target = &edge["target"];
        if target["redacted"] == Value::Bool(true) {
            linked.hidden = true;
            continue;
        }
        let (Some(database), Some(table)) =
            (target["namespace"].as_str(), target["tableName"].as_str())
        else {
            continue;
        };
        let name = TableName {
            database: database.to_string(),
            table: table.to_string(),
        };
        if !linked.tables.contains(&name) {
            linked.tables.push(name);
        }
    }
    linked
}

/// Whether a table still exists with the results columns; the reason when
/// it doesn't. A dropped or replaced table would break the whole query.
pub async fn unreadable_reason(rt: &Runtime, warehouse: &str, table: &TableName) -> Option<String> {
    let path = format!(
        "/lakehouse/schema/{}/{}/{}",
        crate::cmd::seg(warehouse),
        crate::cmd::seg(&table.database),
        crate::cmd::seg(&table.table)
    );
    match rt
        .http
        .request_json(LAKEHOUSE, Method::GET, &path, &[], None)
        .await
    {
        Ok(reply) => missing_columns_reason(&reply),
        Err(err) => Some(format!("cannot read its schema ({err:#})")),
    }
}

/// `GET /lakehouse/schema/{w}/{d}/{t}` → `{"response": [{"columnName"}, …]}`.
fn missing_columns_reason(reply: &Value) -> Option<String> {
    let columns: Vec<String> = reply["response"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["columnName"].as_str().map(str::to_lowercase))
        .collect();
    let missing: Vec<&str> = RESULT_COLUMNS
        .into_iter()
        .filter(|wanted| !columns.iter().any(|c| c == wanted))
        .collect();
    (!missing.is_empty()).then(|| {
        format!(
            "it has no {} column, so it no longer holds check results",
            missing.join(", ")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn edge(operation: &str, target: Value) -> Value {
        json!({
            "source": {"namespace": "DemoDB", "tableName": "ProductionMetrics"},
            "target": target,
            "operation": operation,
        })
    }

    fn table(database: &str, table: &str) -> TableName {
        TableName {
            database: database.into(),
            table: table.into(),
        }
    }

    #[test]
    fn only_results_links_count_each_table_once() {
        let reply = json!({"edges": [
            edge("QUALITY_RESULTS", json!({"namespace": "DemoDB", "tableName": "ProductionMetrics_quality"})),
            edge("INSERT", json!({"namespace": "DemoDB", "tableName": "daily_rollup"})),
            edge("QUALITY_RESULTS", json!({"namespace": "dq", "tableName": "results"})),
            edge("QUALITY_RESULTS", json!({"namespace": "DemoDB", "tableName": "ProductionMetrics_quality"})),
        ]});
        assert_eq!(
            parse_links(&reply),
            LinkedResults {
                tables: vec![
                    table("DemoDB", "ProductionMetrics_quality"),
                    table("dq", "results")
                ],
                hidden: false,
            }
        );
    }

    #[test]
    fn tables_the_caller_cannot_read_are_counted_not_named() {
        let reply = json!({"edges": [
            edge("QUALITY_RESULTS", json!({"redacted": true})),
            edge("QUALITY_RESULTS", json!({"redacted": true})),
        ]});
        assert_eq!(
            parse_links(&reply),
            LinkedResults {
                tables: vec![],
                hidden: true
            }
        );
    }

    #[test]
    fn a_table_without_lineage_has_no_links() {
        let reply = json!({"edges": [], "nodes": [{"namespace": "DemoDB", "tableName": "x"}]});
        assert_eq!(parse_links(&reply), LinkedResults::default());
    }

    #[test]
    fn a_results_table_needs_the_result_columns() {
        let full = json!({"response": [
            {"columnName": "run_id"}, {"columnName": "checked_at"}, {"columnName": "source"},
            {"columnName": "check_name"}, {"columnName": "severity"}, {"columnName": "status"},
        ]});
        assert_eq!(missing_columns_reason(&full), None);
        let replaced = json!({"response": [{"columnName": "source"}, {"columnName": "amount"}]});
        assert_eq!(
            missing_columns_reason(&replaced).as_deref(),
            Some("it has no checked_at, check_name, severity, status column, so it no longer holds check results")
        );
    }
}
