//! The warehouse's names for the SQL shell: databases, tables and their
//! columns, read once per session and kept until `\refresh`.
//!
//! They come from `GET /lakehouse/schema/{warehouse}`, the call the web
//! app's SQL editor makes for its schema tree (ground-zero
//! `src/pages/Utils/etlUtlis.js` `getDatabasesWithSchema`;
//! groundzero-iceberg-provider-api `app_routes/schemas.py`
//! `getSchemaOfAllNamespaces`). It answers one row per column,
//! `{catalog, databaseName, tableName, columnName, dataType, …}`, and a row
//! with an empty `tableName` for a database without tables.

use std::collections::HashMap;

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub data_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    pub name: String,
    pub columns: Vec<Column>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Database {
    pub name: String,
    pub tables: Vec<Table>,
}

/// Every name the shell completes, for one warehouse.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalog {
    /// The warehouse the session runs on; its name is the catalog prefix.
    pub warehouse: String,
    /// Every warehouse on the site, for `warehouse.db.table` names.
    pub warehouses: Vec<String>,
    pub databases: Vec<Database>,
}

impl Catalog {
    /// Reads the schema endpoint's answer (`{response: [rows]}` or the
    /// bare rows).
    pub fn from_schema(warehouse: &str, warehouses: Vec<String>, answer: &Value) -> Self {
        let rows = answer
            .get("response")
            .unwrap_or(answer)
            .as_array()
            .cloned()
            .unwrap_or_default();
        let mut databases: Vec<Database> = Vec::new();
        let mut database_at: HashMap<String, usize> = HashMap::new();
        let mut table_at: HashMap<(usize, String), usize> = HashMap::new();
        for row in &rows {
            let text = |key: &str| row[key].as_str().unwrap_or_default().to_string();
            let database = text("databaseName");
            if database.is_empty() {
                continue;
            }
            let db = *database_at.entry(database.clone()).or_insert_with(|| {
                databases.push(Database {
                    name: database.clone(),
                    tables: Vec::new(),
                });
                databases.len() - 1
            });
            let table = text("tableName");
            if table.is_empty() {
                continue;
            }
            let tables = &mut databases[db].tables;
            let at = *table_at.entry((db, table.clone())).or_insert_with(|| {
                tables.push(Table {
                    name: table.clone(),
                    columns: Vec::new(),
                });
                tables.len() - 1
            });
            let column = text("columnName");
            if !column.is_empty() {
                tables[at].columns.push(Column {
                    name: column,
                    data_type: text("dataType"),
                });
            }
        }
        Self {
            warehouse: warehouse.to_string(),
            warehouses,
            databases,
        }
    }

    pub fn is_current_warehouse(&self, name: &str) -> bool {
        self.warehouse.eq_ignore_ascii_case(name)
    }

    pub fn database(&self, name: &str) -> Option<&Database> {
        self.databases
            .iter()
            .find(|d| d.name.eq_ignore_ascii_case(name))
    }

    /// The table a dotted name means: `db.table` or `warehouse.db.table`
    /// in this warehouse.
    pub fn table(&self, parts: &[String]) -> Option<(&Database, &Table)> {
        let parts = match parts {
            [warehouse, rest @ ..] if rest.len() == 2 && self.is_current_warehouse(warehouse) => {
                rest
            }
            _ => parts,
        };
        let [db, table] = parts else {
            return None;
        };
        let database = self.database(db)?;
        let found = database
            .tables
            .iter()
            .find(|t| t.name.eq_ignore_ascii_case(table))?;
        Some((database, found))
    }

    /// Every table called `name`, in any database.
    pub fn tables_named(&self, name: &str) -> Vec<(&Database, &Table)> {
        self.databases
            .iter()
            .flat_map(|d| d.tables.iter().map(move |t| (d, t)))
            .filter(|(_, t)| t.name.eq_ignore_ascii_case(name))
            .collect()
    }

    pub fn table_count(&self) -> usize {
        self.databases.iter().map(|d| d.tables.len()).sum()
    }

    pub fn column_count(&self) -> usize {
        self.databases
            .iter()
            .flat_map(|d| &d.tables)
            .map(|t| t.columns.len())
            .sum()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) fn sample() -> Catalog {
        let rows = json!({"response": [
            {"catalog": "WH", "databaseName": "DemoDB", "tableName": "ProductionMetrics", "columnName": "plant_id", "dataType": "string"},
            {"catalog": "WH", "databaseName": "DemoDB", "tableName": "ProductionMetrics", "columnName": "units_produced", "dataType": "int"},
            {"catalog": "WH", "databaseName": "DemoDB", "tableName": "product", "columnName": "plant_id", "dataType": "string"},
            {"catalog": "WH", "databaseName": "DemoDB", "tableName": "product", "columnName": "price", "dataType": "double"},
            {"catalog": "WH", "databaseName": "sales", "tableName": "orders", "columnName": "order id", "dataType": "long"},
            {"catalog": "WH", "databaseName": "empty_db", "tableName": "", "columnName": "", "dataType": ""},
        ]});
        Catalog::from_schema("WH", vec!["WH".into(), "Other".into()], &rows)
    }

    #[test]
    fn groups_column_rows_into_databases_and_tables() {
        let catalog = sample();
        let names: Vec<&str> = catalog.databases.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["DemoDB", "sales", "empty_db"]);
        assert_eq!(catalog.table_count(), 3);
        assert_eq!(catalog.column_count(), 5);
        assert!(catalog.database("EMPTY_DB").unwrap().tables.is_empty());
        let (_, table) = catalog
            .table(&["demodb".into(), "productionmetrics".into()])
            .unwrap();
        assert_eq!(table.columns[1].name, "units_produced");
        assert_eq!(table.columns[1].data_type, "int");
    }

    #[test]
    fn finds_tables_with_or_without_the_warehouse_prefix() {
        let catalog = sample();
        let full = ["wh".to_string(), "sales".into(), "orders".into()];
        assert_eq!(catalog.table(&full).unwrap().1.name, "orders");
        let other = ["Other".to_string(), "sales".into(), "orders".into()];
        assert!(
            catalog.table(&other).is_none(),
            "only this warehouse's tables"
        );
        assert_eq!(catalog.tables_named("PRODUCT").len(), 1);
    }
}
