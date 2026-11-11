//! A lakehouse table named with its warehouse, and the provider's per-table
//! routes: shared by `gz jobs try/publish/discard` and `gz quality`.

use anyhow::Result;

use crate::cmd::seg;
use crate::table_history::TableName;

/// A lakehouse table, as `warehouse.database.table`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LakehouseTable {
    pub warehouse: String,
    pub name: TableName,
}

impl LakehouseTable {
    /// `warehouse.database.table`.
    pub fn parse(text: &str) -> Result<Self> {
        let wrong =
            || anyhow::anyhow!("write the table as warehouse.database.table (got '{text}')");
        let Some((warehouse, rest)) = text.split_once('.') else {
            return Err(wrong());
        };
        if warehouse.is_empty() {
            return Err(wrong());
        }
        Ok(Self {
            warehouse: warehouse.to_string(),
            name: TableName::parse(rest).map_err(|_| wrong())?,
        })
    }

    /// The same table whatever the case: the lineage compiler upper-cases
    /// names that were not quoted.
    pub fn key(&self) -> (String, String, String) {
        (
            self.warehouse.to_lowercase(),
            self.name.database.to_lowercase(),
            self.name.table.to_lowercase(),
        )
    }

    /// `/lakehouse/tables/{warehouse}/{database}/{table}/{rest}`, each part
    /// encoded; `rest` is already a path.
    pub fn path(&self, rest: &str) -> String {
        format!(
            "/lakehouse/tables/{}/{}/{}/{rest}",
            seg(&self.warehouse),
            seg(&self.name.database),
            seg(&self.name.table)
        )
    }
}

impl std::fmt::Display for LakehouseTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.warehouse, self.name)
    }
}

/// A route the provider doesn't have yet: Flask's own 404 page, not one of
/// its JSON replies (those say which table, warehouse or branch is missing).
pub fn route_missing(err: &anyhow::Error) -> bool {
    let text = format!("{err:#}").to_lowercase();
    text.contains("http 404")
        && (text.contains("requested url was not found") || text.contains("<!doctype html"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_warehouse_database_table() {
        let table = LakehouseTable::parse("Lineage.sales.orders").unwrap();
        assert_eq!(table.to_string(), "Lineage.sales.orders");
        assert_eq!(
            table.key(),
            ("lineage".into(), "sales".into(), "orders".into())
        );
        assert!(LakehouseTable::parse("sales.orders").is_err());
        assert!(LakehouseTable::parse(".sales.orders").is_err());
    }

    #[test]
    fn paths_encode_every_part() {
        let table = LakehouseTable::parse("Lineage.sales.or ders").unwrap();
        assert_eq!(
            table.path("quality"),
            "/lakehouse/tables/Lineage/sales/or%20ders/quality"
        );
    }

    #[test]
    fn only_flask_404_pages_mean_a_missing_route() {
        let page = anyhow::anyhow!(
            "PUT u → HTTP 404 Not Found: <!doctype html><title>404</title><p>The requested URL was not found on the server."
        );
        let table = anyhow::anyhow!("PUT u → HTTP 404 Not Found: Table not found");
        assert!(route_missing(&page));
        assert!(!route_missing(&table));
    }
}
