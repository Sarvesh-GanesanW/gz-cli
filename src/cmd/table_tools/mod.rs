//! `gz table profile` and `gz table rollback`: a lakehouse table's column
//! statistics, and moving its main back to an earlier snapshot. Both name
//! the table the same way and read its history first.

pub(crate) mod profile;
pub(crate) mod rollback;

use anyhow::{bail, Result};

use crate::cli::ShellArgs;
use crate::cmd::Runtime;
use crate::table_history::{History, TableName};

/// A table as the user named it: `WAREHOUSE.DATABASE.TABLE`, or
/// `DATABASE.TABLE` with the warehouse from `--warehouse` or the profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    pub warehouse: Option<String>,
    pub table: TableName,
}

impl Target {
    pub fn parse(text: &str) -> Result<Self> {
        let parts: Vec<&str> = text.split('.').collect();
        if parts.iter().any(|part| part.trim().is_empty()) {
            bail!("write the table as WAREHOUSE.DATABASE.TABLE or DATABASE.TABLE (got '{text}')");
        }
        match parts[..] {
            [warehouse, database, table] => Ok(Self {
                warehouse: Some(warehouse.to_string()),
                table: TableName::parse(&format!("{database}.{table}"))?,
            }),
            [_, _] => Ok(Self {
                warehouse: None,
                table: TableName::parse(text)?,
            }),
            _ => bail!(
                "write the table as WAREHOUSE.DATABASE.TABLE or DATABASE.TABLE (got '{text}')"
            ),
        }
    }

    /// The session options with the warehouse this table names; refuses a
    /// `--warehouse` that says otherwise.
    fn shell(&self, shell: &ShellArgs) -> Result<ShellArgs> {
        let warehouse = match (&self.warehouse, &shell.warehouse) {
            (Some(named), Some(flag)) if named != flag => bail!(
                "the table names warehouse {named} but --warehouse says {flag}; give one of them"
            ),
            (Some(named), _) => Some(named.clone()),
            (None, flag) => flag.clone(),
        };
        Ok(ShellArgs {
            warehouse,
            ..shell.clone()
        })
    }
}

/// A table with its warehouse chosen and its history read.
pub(crate) struct Located {
    pub warehouse: String,
    pub table: TableName,
    pub history: History,
    /// The session options, set to that warehouse.
    pub shell: ShellArgs,
}

impl Located {
    pub async fn find(rt: &Runtime, text: &str, shell: &ShellArgs) -> Result<Self> {
        let target = Target::parse(text)?;
        let shell = target.shell(shell)?;
        let warehouse = crate::sql::warehouse(rt, &shell).await?;
        let history = History::load(&rt.http, &warehouse.name, &target.table).await?;
        Ok(Self {
            shell: ShellArgs {
                warehouse: Some(warehouse.name.clone()),
                ..shell
            },
            warehouse: warehouse.name,
            table: target.table,
            history,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_table_names_its_warehouse_or_takes_it_from_the_flag() {
        let full = Target::parse("GZWarehousePROD.DemoDB.ProductionMetrics").unwrap();
        assert_eq!(full.warehouse.as_deref(), Some("GZWarehousePROD"));
        assert_eq!(full.table.to_string(), "DemoDB.ProductionMetrics");

        let short = Target::parse("DemoDB.ProductionMetrics").unwrap();
        assert_eq!(short.warehouse, None);
        let flag = ShellArgs {
            warehouse: Some("Lineage".into()),
            ..ShellArgs::default()
        };
        assert_eq!(
            short.shell(&flag).unwrap().warehouse.as_deref(),
            Some("Lineage")
        );
        assert_eq!(
            full.shell(&ShellArgs::default())
                .unwrap()
                .warehouse
                .as_deref(),
            Some("GZWarehousePROD")
        );
        let clash = format!("{:#}", full.shell(&flag).unwrap_err());
        assert!(clash.contains("--warehouse says Lineage"), "{clash}");
    }

    #[test]
    fn other_shapes_are_refused() {
        for text in ["orders", "a.b.c.d", "wh..orders", ".b.c"] {
            assert!(Target::parse(text).is_err(), "{text}");
        }
    }
}
