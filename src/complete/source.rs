//! The lists completion reads from Groundzero, each with a cache key.
//!
//! - names of ids: [`crate::resolve::lookup::Catalog`], the lists name
//!   resolution uses
//! - a warehouse's databases and tables: `GET lakehouse /lakehouse/schema/{warehouse}`
//!   (one row per column; read by [`crate::sql::catalog::Catalog::from_schema`],
//!   as `gz sql` does)
//! - a table's branches and tags: `GET lakehouse /lakehouse/tables/{wh}/{db}/{table}/versions`
//!   ([`History::load`], as `gz diff` does)
//! - runtime images: `GET rtes /rte/image` → `[{Name}]` (the web app's and
//!   the TUI's run form)

use anyhow::Result;
use reqwest::Method;

use super::target::Item;
use crate::cmd::{seg, Runtime};
use crate::resolve::lookup::Catalog;
use crate::resolve::Kind;
use crate::table_history::{History, TableName};

/// One list completion can read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// Names of a kind; jobs and notebooks of one project, or of all.
    Names(Kind, Option<i64>),
    /// A warehouse's databases (`db`) and tables (`db.table`).
    Schema(String),
    /// A table's branches and tags.
    Refs {
        warehouse: String,
        table: TableName,
    },
    Runtimes,
}

impl Source {
    /// The cache key; [`Source::parse`] reads it back for a background refresh.
    pub fn key(&self) -> String {
        match self {
            Source::Names(kind, None) => format!("names:{}", kind.key()),
            Source::Names(kind, Some(project)) => format!("names:{}@{project}", kind.key()),
            Source::Schema(warehouse) => format!("schema:{warehouse}"),
            Source::Refs { warehouse, table } => {
                format!("refs:{warehouse}/{}/{}", table.database, table.table)
            }
            Source::Runtimes => "runtimes".to_string(),
        }
    }

    pub fn parse(key: &str) -> Option<Self> {
        let (family, rest) = key.split_once(':').unwrap_or((key, ""));
        match family {
            "names" => {
                let (kind, project) = match rest.split_once('@') {
                    Some((kind, project)) => (kind, Some(project.parse().ok()?)),
                    None => (rest, None),
                };
                Some(Source::Names(Kind::from_key(kind)?, project))
            }
            "schema" => Some(Source::Schema(rest.to_string())),
            "refs" => {
                let mut parts = rest.splitn(3, '/');
                let (warehouse, database, table) = (parts.next()?, parts.next()?, parts.next()?);
                Some(Source::Refs {
                    warehouse: warehouse.to_string(),
                    table: TableName {
                        database: database.to_string(),
                        table: table.to_string(),
                    },
                })
            }
            "runtimes" => Some(Source::Runtimes),
            _ => None,
        }
    }

    pub async fn fetch(&self, catalog: &mut Catalog) -> Result<Vec<Item>> {
        let rt = catalog.runtime();
        match self {
            Source::Names(kind, project) => names(catalog, *kind, *project).await,
            Source::Schema(warehouse) => schema(rt, warehouse).await,
            Source::Refs { warehouse, table } => {
                let history = History::load(&rt.http, warehouse, table).await?;
                let branches = history
                    .branches
                    .iter()
                    .map(|b| Item::new(&b.name, "branch"));
                let tags = history.tags.iter().map(|t| Item::new(&t.name, "tag"));
                Ok(branches.chain(tags).collect())
            }
            Source::Runtimes => {
                let list = rt
                    .http
                    .request_json("rtes", Method::GET, "/rte/image", &[], None)
                    .await?;
                Ok(list
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|row| row["Name"].as_str())
                    .map(|name| Item::new(name, "runtime"))
                    .collect())
            }
        }
    }
}

/// Names, described `id 1999` (and the project, for jobs from every project).
async fn names(catalog: &mut Catalog, kind: Kind, project: Option<i64>) -> Result<Vec<Item>> {
    let list = catalog.list(kind, project).await?;
    Ok(list
        .into_iter()
        .map(|candidate| {
            let about = match (&candidate.project, project) {
                (Some((_, name)), None) if !name.is_empty() => {
                    format!("{ID_PREFIX}{} · {name}", candidate.id)
                }
                _ => format!("{ID_PREFIX}{}", candidate.id),
            };
            Item::new(candidate.name, about)
        })
        .collect())
}

/// How a name's description starts; [`id_in`] reads the id back.
const ID_PREFIX: &str = "id ";

/// The id in a name's description, e.g. a project's for its jobs.
pub fn id_in(about: &str) -> Option<i64> {
    about
        .strip_prefix(ID_PREFIX)?
        .split(' ')
        .next()?
        .parse()
        .ok()
}

async fn schema(rt: &Runtime, warehouse: &str) -> Result<Vec<Item>> {
    let path = format!("/lakehouse/schema/{}", seg(warehouse));
    let answer = rt
        .http
        .request_json("lakehouse", Method::GET, &path, &[], None)
        .await?;
    let catalog = crate::sql::catalog::Catalog::from_schema(warehouse, &answer);
    let mut items = Vec::new();
    for database in &catalog.databases {
        items.push(Item::new(&database.name, "database"));
        for table in &database.tables {
            items.push(Item::new(
                format!("{}.{}", database.name, table.name),
                "table",
            ));
        }
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_read_back() {
        let sources = [
            Source::Names(Kind::Project, None),
            Source::Names(Kind::Job, Some(1999)),
            Source::Names(Kind::SavedQuery, None),
            Source::Schema("Lineage".into()),
            Source::Refs {
                warehouse: "Lineage".into(),
                table: TableName {
                    database: "sales".into(),
                    table: "orders".into(),
                },
            },
            Source::Runtimes,
        ];
        for source in sources {
            assert_eq!(
                Source::parse(&source.key()),
                Some(source.clone()),
                "{source:?}"
            );
        }
        assert_eq!(Source::parse("nope:x"), None);
    }

    #[test]
    fn reads_ids_from_descriptions() {
        assert_eq!(id_in("id 1999"), Some(1999));
        assert_eq!(id_in("id 3013 · Sales"), Some(3013));
        assert_eq!(id_in("table"), None);
    }
}
