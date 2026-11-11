//! The trace `gz explain` prints: from a figure to its dataset, the
//! tables the dataset reads, the snapshot of each, the run that wrote it,
//! and that run's code, plus what gz could not tell. `-o json` is this
//! structure serialized; README.md documents it.

use serde::Serialize;
use serde_json::Value;

use super::designer::{DatasetDef, DatasetKind, Figure};
use super::runs::{CodeVersion, RunMatch};
use super::sql_text;
use crate::sql::tables::{self, QueryTables, TableRef};
use crate::table_history::{History, Snapshot};

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Trace {
    pub subject: Subject,
    /// The moment the trace is for (`YYYY-MM-DDTHH:MM:SS`, UTC); `None`
    /// for now.
    pub at: Option<String>,
    pub figures: Vec<Figure>,
    pub datasets: Vec<DatasetTrace>,
    pub tables: Vec<TableTrace>,
    /// What gz could not tell, in plain words.
    pub gaps: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Subject {
    /// `dashboard`, `visualization`, `dataset` or `sql`.
    pub kind: &'static str,
    pub id: Option<String>,
    pub name: Option<String>,
}

/// A data connection a dataset names, with the lakehouse warehouse and
/// default database it points at (`config.warehouseName`,
/// `config.databaseName`; secrets are never read).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Connection {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub warehouse: Option<String>,
    pub database: Option<String>,
}

impl Connection {
    pub fn is_lakehouse(&self) -> bool {
        self.kind.as_deref() == Some("GZLAKEHOUSE")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DatasetTrace {
    #[serde(flatten)]
    pub definition: DatasetDef,
    pub connection_details: Vec<Connection>,
    /// The lakehouse tables it reads, as `warehouse.database.table`.
    pub tables: Vec<String>,
    /// Sources gz could not resolve to a lakehouse table.
    pub unresolved: Vec<String>,
}

/// A lakehouse table, fully named.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TableKey {
    pub warehouse: String,
    pub database: String,
    pub table: String,
}

impl std::fmt::Display for TableKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.warehouse, self.database, self.table)
    }
}

/// The prefix the runtimes give a lakehouse connection's catalog
/// (groundzero-iceberg-provider-api utils/table_references.py:46).
const RUNTIME_CATALOG_PREFIX: &str = "gzlakehouse__";

/// Longest name gz can place: `catalog.database.table`.
const MAX_NAME_PARTS: usize = 3;

/// The tables `query` reads as gz explain can place them: a name with more
/// parts than `catalog.database.table` is reported with the sources it
/// cannot resolve.
pub fn query_tables(query: &str) -> QueryTables {
    let found = tables::tables_read(query);
    let (tables, too_long): (Vec<TableRef>, Vec<TableRef>) = found
        .tables
        .into_iter()
        .partition(|name| name.parts.len() <= MAX_NAME_PARTS);
    let mut unresolved = found.unresolved;
    unresolved.extend(too_long.iter().map(ToString::to_string));
    QueryTables { tables, unresolved }
}

/// The lakehouse table a query's name means on `connection`: a bare name is
/// in the connection's default database, `db.table` in its warehouse. An
/// Iceberg ref or metadata table (`db.t.branch_dev`, `db.t.files`) is
/// placed as the table it belongs to, since its numbers come from there.
pub fn resolve(name: &TableRef, connection: &Connection) -> Result<TableKey, String> {
    let warehouse = connection.warehouse.clone().ok_or_else(|| {
        format!(
            "connection {} names no warehouse, so {name} cannot be placed",
            connection.name
        )
    })?;
    let key = |warehouse: String, database: &str, table: &str| TableKey {
        warehouse,
        database: database.to_string(),
        table: table.to_string(),
    };
    let in_connection = |catalog: &str| {
        catalog.starts_with(RUNTIME_CATALOG_PREFIX) || catalog.eq_ignore_ascii_case(&warehouse)
    };
    let base = name.base(in_connection).table;
    match base.parts.as_slice() {
        [table] => {
            let database = connection.database.as_deref().ok_or_else(|| {
                format!(
                    "{name} has no database and connection {} names no default one",
                    connection.name
                )
            })?;
            Ok(key(warehouse, database, table))
        }
        [database, table] => Ok(key(warehouse, database, table)),
        // The connection's own catalog is gone from `base`, so this one
        // names another warehouse.
        [catalog, database, table] => Ok(key(catalog.clone(), database, table)),
        _ => Err(format!("{name} is not a table name gz can place")),
    }
}

/// The SQL a dataset runs, each piece as found: its query, or for a code
/// dataset the queries written in its code.
pub fn dataset_queries(definition: &DatasetDef) -> Vec<String> {
    match definition.kind {
        DatasetKind::Sql => definition.sql.iter().cloned().collect(),
        DatasetKind::Code => definition
            .code
            .as_deref()
            .map(sql_text::sql_in_python)
            .unwrap_or_default(),
    }
}

/// Fills in each measure's expression from the SQL of the figure's dataset.
pub fn explain_measures(figure: &mut Figure, queries: &[String]) {
    for measure in &mut figure.measures {
        measure.expression = queries
            .iter()
            .find_map(|query| sql_text::select_expression(query, &measure.column))
            .filter(|expression| !expression.eq_ignore_ascii_case(&measure.column));
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotView {
    pub id: String,
    pub committed_at: String,
    pub operation: Option<String>,
    pub added_records: Option<u64>,
    pub total_records: Option<u64>,
    pub app_name: Option<String>,
    pub spark_app_id: Option<String>,
    /// `engine-name` (`spark`).
    pub engine: Option<String>,
    /// `current`, `branch main`, `tag release`.
    pub names: Vec<String>,
}

impl SnapshotView {
    pub fn of(history: &History, snapshot: &Snapshot) -> Self {
        let summary = |key: &str| snapshot.summary.get(key).filter(|v| !v.is_empty()).cloned();
        Self {
            id: snapshot.id.clone(),
            committed_at: snapshot.committed_at.clone(),
            operation: snapshot.operation.clone(),
            added_records: snapshot.count("added-records"),
            total_records: snapshot.count("total-records"),
            app_name: summary("app-name"),
            spark_app_id: summary("spark.app.id").or_else(|| summary("app-id")),
            engine: summary("engine-name"),
            names: history.names(snapshot),
        }
    }
}

/// The commits made to a table after the snapshot it was at.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Changes {
    /// Newest first.
    pub commits: Vec<SnapshotView>,
    pub records_then: Option<u64>,
    pub records_now: Option<u64>,
}

/// A table lineage edge into the table or one of its ancestors
/// (`GET /lakehouse/governance/lineage/table`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Upstream {
    pub source: String,
    pub target: String,
    pub operation: Option<String>,
    pub event_time: Option<String>,
    /// The ETL lineage run id when numeric, else a query id.
    pub correlation_id: Option<String>,
}

/// The edges of a lineage reply. Redacted nodes (no Select on them) show
/// as `(hidden)`.
pub fn upstream_edges(reply: &Value) -> Vec<Upstream> {
    let node = |value: &Value| -> String {
        match (value["namespace"].as_str(), value["tableName"].as_str()) {
            (Some(namespace), Some(table)) => format!("{namespace}.{table}"),
            _ => "(hidden)".to_string(),
        }
    };
    let text = |value: &Value| value.as_str().filter(|s| !s.is_empty()).map(String::from);
    reply["edges"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|edge| Upstream {
            source: node(&edge["source"]),
            target: node(&edge["target"]),
            operation: text(&edge["operation"]),
            event_time: text(&edge["eventTime"]),
            correlation_id: text(&edge["correlationId"]),
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TableTrace {
    #[serde(flatten)]
    pub key: TableKey,
    /// Ids of the datasets (or `sql`) that read it.
    pub read_by: Vec<String>,
    /// The snapshot the table was at, at the trace's moment.
    pub snapshot: Option<SnapshotView>,
    pub run: Option<RunMatch>,
    pub code_version: Option<CodeVersion>,
    /// `None` when lineage could not be read.
    pub upstream: Option<Vec<Upstream>>,
    /// Only with `--at`.
    pub changes_since: Option<Changes>,
    /// What could not be read for this table, and why.
    pub problems: Vec<String>,
}

impl TableTrace {
    pub fn new(key: TableKey, read_by: Vec<String>) -> Self {
        Self {
            key,
            read_by,
            snapshot: None,
            run: None,
            code_version: None,
            upstream: None,
            changes_since: None,
            problems: Vec::new(),
        }
    }
}

/// Why a table could not be read, in a line that says what to do; `None`
/// when the error is not one gz knows how to explain.
pub fn read_refusal(error: &str, key: &TableKey) -> Option<String> {
    if error.contains("HTTP 403") {
        return Some(format!(
            "you do not have Select on {}.{}; ask the warehouse owner for access",
            key.warehouse, key.database
        ));
    }
    if error.contains("HTTP 404") {
        return Some(format!(
            "{} is not in {}.{} (names are case-sensitive; `gz table schemas {}` lists them)",
            key.table, key.warehouse, key.database, key.warehouse
        ));
    }
    None
}

/// Gaps named the same way wherever they come up.
pub mod gap {
    pub const RUN_ID_NOT_ON_COMMITS: &str = "The platform does not record the job run id on table commits; runs are matched by commit time and, for Spark, the start time in the Spark application id.";
    pub const DEFINITIONS_ARE_CURRENT: &str = "Dataset and visualization definitions are shown as they are now; the Designer keeps no history of them.";
    pub const FILTER_DEFAULTS: &str =
        "Filter values are the defaults a viewer starts with; a viewer may have picked others.";
    pub const DASHBOARD_TILES: &str =
        "A dashboard's own filters choose which chart a tile shows; they do not narrow data.";
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::explain::designer;

    fn connection() -> Connection {
        Connection {
            name: "GZLakehouseProd".to_string(),
            kind: Some("GZLAKEHOUSE".to_string()),
            warehouse: Some("GZWarehousePROD".to_string()),
            database: Some("DemoDB".to_string()),
        }
    }

    fn parts(text: &str) -> TableRef {
        TableRef {
            parts: text.split('.').map(String::from).collect(),
        }
    }

    #[test]
    fn places_a_name_in_the_connections_warehouse_and_database() {
        let place = |text: &str| resolve(&parts(text), &connection()).map(|k| k.to_string());
        assert_eq!(
            place("bi_working_capital").unwrap(),
            "GZWarehousePROD.DemoDB.bi_working_capital"
        );
        assert_eq!(
            place("sales.orders").unwrap(),
            "GZWarehousePROD.sales.orders"
        );
        assert_eq!(
            place("gzlakehouse__GZLakehouseProd.sales.orders").unwrap(),
            "GZWarehousePROD.sales.orders"
        );
        assert_eq!(place("Other.sales.orders").unwrap(), "Other.sales.orders");

        let no_default = Connection {
            database: None,
            ..connection()
        };
        let err = resolve(&parts("orders"), &no_default).unwrap_err();
        assert!(err.contains("names no default one"), "{err}");
    }

    #[test]
    fn places_an_iceberg_ref_or_metadata_table_as_its_table() {
        // gz explain used to read `sales.orders.branch_dev` as catalog
        // `sales`, database `orders`, table `branch_dev`.
        let place = |text: &str| resolve(&parts(text), &connection()).map(|k| k.to_string());
        assert_eq!(
            place("sales.orders.branch_dev").unwrap(),
            "GZWarehousePROD.sales.orders"
        );
        assert_eq!(
            place("GZWarehousePROD.sales.orders.snapshots").unwrap(),
            "GZWarehousePROD.sales.orders"
        );
    }

    #[test]
    fn a_name_longer_than_catalog_database_table_is_unresolved() {
        let found = query_tables("SELECT * FROM a.b.c.d JOIN sales.orders o ON TRUE");
        assert_eq!(found.tables, [parts("sales.orders")]);
        assert_eq!(found.unresolved, ["a.b.c.d"]);
    }

    #[test]
    fn a_measure_takes_the_expression_that_computes_it() {
        let mut figure = designer::figure(&json!({
            "Id": 1, "Name": "f", "config": {"chartType": "ColumnChart", "chartConfig": {
                "hAxis": {"title": "Year"},
                "selectedSeriesArr": [{"name": "Revenue"}, {"name": "region"}],
            }},
        }));
        explain_measures(
            &mut figure,
            &["SELECT region, ROUND(SUM(revenue27), 0) AS Revenue FROM t".to_string()],
        );
        assert_eq!(
            figure.measures[0].expression.as_deref(),
            Some("ROUND(SUM(revenue27), 0)")
        );
        assert_eq!(
            figure.measures[1].expression, None,
            "a column selected as it is adds nothing"
        );
    }

    #[test]
    fn explains_a_refused_read_in_one_line() {
        let key = TableKey {
            warehouse: "manufacturingops".to_string(),
            database: "mfg_ops".to_string(),
            table: "suppliers".to_string(),
        };
        let denied = "cannot read the history: GET https://x/versions → HTTP 403 Forbidden (retry after 53s): You do not have All or Select permission";
        assert_eq!(
            read_refusal(denied, &key).as_deref(),
            Some("you do not have Select on manufacturingops.mfg_ops; ask the warehouse owner for access")
        );
        assert!(read_refusal("→ HTTP 404 Not Found: Table not found", &key)
            .unwrap()
            .contains("case-sensitive"));
        assert_eq!(read_refusal("→ HTTP 500: boom", &key), None);
    }

    #[test]
    fn reads_lineage_edges_and_hides_what_the_caller_cannot_see() {
        let edges = upstream_edges(&json!({"edges": [
            {"source": {"namespace": "manufacturing", "tableName": "machine_telemetry"},
             "target": {"namespace": "manufacturing", "tableName": "high_risk_readings"},
             "operation": "CREATE TABLE AS SELECT", "eventTime": "2026-09-25T18:39:18+00:00",
             "correlationId": "2247d9f4"},
            {"source": {"redacted": true},
             "target": {"namespace": "manufacturing", "tableName": "high_risk_readings"}},
        ]}));
        assert_eq!(edges[0].source, "manufacturing.machine_telemetry");
        assert_eq!(edges[0].correlation_id.as_deref(), Some("2247d9f4"));
        assert_eq!(edges[1].source, "(hidden)");
        assert_eq!(edges[1].operation, None);
    }
}
