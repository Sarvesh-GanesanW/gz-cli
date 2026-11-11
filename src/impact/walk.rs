//! The downstream walk: breadth first from the root tables, one table hop
//! per level, up to a depth, never visiting the same table (and column)
//! twice, so cycles in the lineage end the walk instead of looping.
//!
//! The walk is pure: the caller fetches the [`AssetView`] of every asset
//! [`Walker::wanted`] names, then calls [`Walker::advance`] with them.

use std::collections::{BTreeMap, BTreeSet};

use crate::impact::graph::{AssetId, AssetView, Relation, Workload};
use crate::impact::{ColumnUse, Dependent, Kind, Via};

/// What the walk knows about the columns of a table it reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Columns {
    /// The whole table was asked about.
    Table,
    /// These columns are computed from the asked column(s).
    Known(BTreeSet<String>),
    /// A column was asked about, but some hop on the way had only
    /// table-level lineage.
    Unknown,
}

impl Columns {
    fn use_of(&self) -> ColumnUse {
        match self {
            Columns::Table => ColumnUse::NotAsked,
            Columns::Known(columns) => ColumnUse::Columns(columns.iter().cloned().collect()),
            Columns::Unknown => ColumnUse::Unknown,
        }
    }
}

/// Where the walk starts: a lineage asset that is one of the targets.
#[derive(Debug, Clone)]
pub struct Root {
    pub asset: AssetId,
    /// The target as asked (`db.table` or `db.table.column`).
    pub target: String,
    /// The table's own name.
    pub table: String,
    pub column: Option<String>,
}

#[derive(Debug, Clone)]
struct Probe {
    asset: AssetId,
    name: String,
    columns: Columns,
    /// The target this probe descends from.
    root: String,
    /// `None` for a root itself.
    parent: Option<String>,
}

/// What makes a table visit unique: a whole-table or unknown-column visit
/// covers every column-level one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Scope {
    Table,
    Unknown,
    Column(String),
}

#[derive(Debug)]
pub struct Walker {
    max_depth: u32,
    depth: u32,
    frontier: Vec<Probe>,
    visited: BTreeSet<(AssetId, Scope)>,
    roots: BTreeSet<AssetId>,
    found: Vec<Dependent>,
    index: BTreeMap<(Kind, String), usize>,
}

impl Walker {
    pub fn new(roots: &[Root], max_depth: u32) -> Self {
        let mut walker = Self {
            max_depth,
            depth: 0,
            frontier: Vec::new(),
            visited: BTreeSet::new(),
            roots: roots.iter().map(|r| r.asset).collect(),
            found: Vec::new(),
            index: BTreeMap::new(),
        };
        for root in roots {
            let columns = match &root.column {
                None => Columns::Table,
                Some(column) => Columns::Known(BTreeSet::from([column.clone()])),
            };
            let probe = Probe {
                asset: root.asset,
                name: root.table.clone(),
                columns,
                root: root.target.clone(),
                parent: None,
            };
            if let Some(probe) = walker.unvisited(probe) {
                walker.frontier.push(probe);
            }
        }
        walker
    }

    pub fn is_done(&self) -> bool {
        self.frontier.is_empty() || self.depth >= self.max_depth
    }

    /// The assets whose views the next [`Self::advance`] reads.
    pub fn wanted(&self) -> BTreeSet<AssetId> {
        if self.is_done() {
            return BTreeSet::new();
        }
        self.frontier.iter().map(|p| p.asset).collect()
    }

    /// Takes one hop from every table at the current depth. A table whose
    /// view is missing (hidden or deleted) ends its branch.
    pub fn advance(&mut self, views: &BTreeMap<AssetId, AssetView>) {
        if self.is_done() {
            self.frontier.clear();
            return;
        }
        self.depth += 1;
        let frontier = std::mem::take(&mut self.frontier);
        for probe in &frontier {
            if let Some(view) = views.get(&probe.asset) {
                self.step(view, probe);
            }
        }
    }

    pub fn finish(self) -> Vec<Dependent> {
        self.found
    }

    fn step(&mut self, view: &AssetView, probe: &Probe) {
        let via = match &probe.parent {
            None => Via::Direct,
            Some(_) => Via::Through {
                what: Kind::Table.singular(),
                name: probe.name.clone(),
            },
        };
        let mut datasets = BTreeSet::new();
        for (target, relations) in outgoing(view) {
            let Some(columns) = columns_after(&probe.columns, &relations) else {
                continue;
            };
            // A dropped table can't break; lineage only remembers it.
            let Some(asset) = view.assets.get(&target).filter(|a| !a.is_deleted) else {
                continue;
            };
            if asset.is_dataset() {
                let workload = relations.iter().find_map(|r| r.workload);
                if let Some(id) = view.dataset_id(asset, workload) {
                    datasets.insert(id);
                    self.record(dependent(
                        Kind::Dataset,
                        id.to_string(),
                        &asset.name,
                        asset.owner.clone(),
                        probe,
                        &via,
                        columns.use_of(),
                        self.depth,
                    ));
                }
                continue;
            }
            if self.roots.contains(&target) {
                continue;
            }
            let mut table = dependent(
                Kind::Table,
                target.to_string(),
                &asset.name,
                asset.owner.clone(),
                probe,
                &via,
                columns.use_of(),
                self.depth,
            );
            table.asset_type = Some(asset.asset_type.clone());
            table.written_by = writers(view, &relations);
            self.record(table);
            let next = Probe {
                asset: target,
                name: asset.name.clone(),
                columns,
                root: probe.root.clone(),
                parent: Some(probe.name.clone()),
            };
            if let Some(next) = self.unvisited(next) {
                self.frontier.push(next);
            }
        }
        for (version, workload) in readers(view) {
            let column_use = workload_columns(view, version, &probe.columns);
            let (kind, id) = match (workload.job_id, workload.dataset_id) {
                (Some(job), _) => (Kind::Job, job),
                (None, Some(dataset)) => {
                    datasets.insert(dataset);
                    (Kind::Dataset, dataset)
                }
                (None, None) => continue,
            };
            self.record(dependent(
                kind,
                id.to_string(),
                &workload.name,
                workload.owner.clone(),
                probe,
                &via,
                column_use,
                self.depth,
            ));
        }
        self.dashboards_on(view, &datasets, probe);
    }

    fn dashboards_on(&mut self, view: &AssetView, datasets: &BTreeSet<i64>, probe: &Probe) {
        for board in &view.dashboards {
            let Some(dataset) = board.dataset_ids.iter().find(|d| datasets.contains(d)) else {
                continue;
            };
            // A dashboard knows of the column only what its dataset does.
            let (dataset_name, columns) = self
                .index
                .get(&(Kind::Dataset, dataset.to_string()))
                .map(|&at| (self.found[at].name.clone(), self.found[at].columns.clone()))
                .unwrap_or_else(|| (dataset.to_string(), probe.columns.use_of()));
            let mut entry = dependent(
                Kind::Dashboard,
                board.id.to_string(),
                &board.name,
                board.owner.clone(),
                probe,
                &Via::Through {
                    what: Kind::Dataset.singular(),
                    name: dataset_name,
                },
                columns,
                self.depth,
            );
            entry.visualization_ids = board.visualization_ids.clone();
            self.record(entry);
        }
    }

    /// The probe, narrowed to what this table has not been visited for;
    /// `None` when it adds nothing.
    fn unvisited(&mut self, mut probe: Probe) -> Option<Probe> {
        let asset = probe.asset;
        let covered = |visited: &BTreeSet<(AssetId, Scope)>, scope: Scope| {
            visited.contains(&(asset, Scope::Table)) || visited.contains(&(asset, scope))
        };
        match &mut probe.columns {
            Columns::Table => {
                if !self.visited.insert((asset, Scope::Table)) {
                    return None;
                }
            }
            Columns::Unknown => {
                if covered(&self.visited, Scope::Unknown) {
                    return None;
                }
                self.visited.insert((asset, Scope::Unknown));
            }
            Columns::Known(columns) => {
                columns.retain(|c| !covered(&self.visited, Scope::Column(c.to_lowercase())));
                if columns.is_empty() {
                    return None;
                }
                for column in columns.iter() {
                    self.visited
                        .insert((asset, Scope::Column(column.to_lowercase())));
                }
            }
        }
        Some(probe)
    }

    /// Keeps the first (shallowest) sighting of each dependent, adding the
    /// columns later sightings bring.
    fn record(&mut self, entry: Dependent) {
        let key = (entry.kind, entry.id.clone());
        match self.index.get(&key) {
            Some(&at) => {
                let known = &mut self.found[at];
                known.columns = known.columns.merged(entry.columns);
            }
            None => {
                self.index.insert(key, self.found.len());
                self.found.push(entry);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn dependent(
    kind: Kind,
    id: String,
    name: &str,
    owner: Option<String>,
    probe: &Probe,
    via: &Via,
    columns: ColumnUse,
    depth: u32,
) -> Dependent {
    Dependent {
        kind,
        id,
        name: name.to_string(),
        owner,
        depth,
        root: probe.root.clone(),
        via: via.clone(),
        columns,
        asset_type: None,
        written_by: Vec::new(),
        visualization_ids: Vec::new(),
    }
}

/// The relations leaving the centre, by target.
fn outgoing(view: &AssetView) -> BTreeMap<AssetId, Vec<&Relation>> {
    let mut by_target: BTreeMap<AssetId, Vec<&Relation>> = BTreeMap::new();
    for relation in &view.relations {
        if relation.source == view.center && relation.target != view.center {
            by_target.entry(relation.target).or_default().push(relation);
        }
    }
    by_target
}

/// The columns of a target the asked columns reach through `relations`;
/// `None` when column lineage exists for this pair and shows none of them
/// flowing into it.
fn columns_after(asked: &Columns, relations: &[&Relation]) -> Option<Columns> {
    let Columns::Known(columns) = asked else {
        return Some(asked.clone());
    };
    let has_column_lineage = relations.iter().any(|r| r.source_column.is_some());
    if !has_column_lineage {
        return Some(Columns::Unknown);
    }
    let hits: Vec<&&Relation> = relations.iter().filter(|r| reads_any(r, columns)).collect();
    if hits.is_empty() {
        return None;
    }
    let targets: Option<BTreeSet<String>> = hits.iter().map(|r| r.target_column.clone()).collect();
    Some(targets.map_or(Columns::Unknown, Columns::Known))
}

fn reads_any(relation: &Relation, columns: &BTreeSet<String>) -> bool {
    relation
        .source_column
        .as_deref()
        .is_some_and(|source| columns.iter().any(|c| c.eq_ignore_ascii_case(source)))
}

/// Jobs and datasets that read the centre: those lineage saw reading it,
/// and those whose writes come from it.
fn readers(view: &AssetView) -> Vec<(i64, &Workload)> {
    let from_relations = view
        .relations
        .iter()
        .filter(|r| r.source == view.center)
        .filter_map(|r| r.workload);
    let versions: BTreeSet<i64> = view.readers.iter().copied().chain(from_relations).collect();
    versions
        .into_iter()
        .filter_map(|v| Some((v, view.workloads.get(&v)?)))
        .collect()
}

/// Whether a job reads the asked columns. A job that reads the table
/// without column lineage for them is "not known", never "unaffected":
/// it may filter or join on them.
fn workload_columns(view: &AssetView, version: i64, asked: &Columns) -> ColumnUse {
    let Columns::Known(columns) = asked else {
        return asked.use_of();
    };
    let used: BTreeSet<String> = view
        .relations
        .iter()
        .filter(|r| r.source == view.center && r.workload == Some(version) && reads_any(r, columns))
        .filter_map(|r| r.source_column.clone())
        .collect();
    if used.is_empty() {
        ColumnUse::Unknown
    } else {
        ColumnUse::Columns(used.into_iter().collect())
    }
}

/// The jobs that wrote a table from the centre, by name.
fn writers(view: &AssetView, relations: &[&Relation]) -> Vec<String> {
    let names: BTreeSet<String> = relations
        .iter()
        .filter_map(|r| r.workload)
        .filter_map(|v| view.workloads.get(&v))
        .map(|w| w.name.clone())
        .collect();
    names.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::impact::graph::{Asset, DashboardRef};

    fn table(id: AssetId) -> Asset {
        Asset {
            id,
            asset_type: "TABLE".into(),
            name: format!("db.t{id}"),
            qualified_name: format!("db.t{id}"),
            owner: Some("ana".into()),
            is_deleted: false,
        }
    }

    fn flow(source: AssetId, target: AssetId, workload: i64) -> Relation {
        Relation {
            source,
            target,
            source_column: None,
            target_column: None,
            workload: Some(workload),
        }
    }

    fn column(source: AssetId, target: AssetId, from: &str, to: &str) -> Relation {
        Relation {
            source_column: Some(from.into()),
            target_column: Some(to.into()),
            ..flow(source, target, 1)
        }
    }

    fn job(id: i64) -> Workload {
        Workload {
            job_id: Some(id),
            dataset_id: None,
            name: format!("job{id}"),
            owner: Some("bo".into()),
        }
    }

    /// Views for a chain of tables, each written from the one before.
    fn views(edges: &[(AssetId, AssetId)]) -> BTreeMap<AssetId, AssetView> {
        let mut views: BTreeMap<AssetId, AssetView> = BTreeMap::new();
        for &(source, target) in edges {
            let view = views.entry(source).or_insert_with(|| AssetView {
                center: source,
                ..AssetView::default()
            });
            view.assets.insert(source, table(source));
            view.assets.insert(target, table(target));
            view.relations.push(flow(source, target, 1));
            view.workloads.insert(1, job(100));
        }
        views
    }

    fn root(asset: AssetId, column: Option<&str>) -> Root {
        Root {
            asset,
            target: match column {
                Some(c) => format!("db.t{asset}.{c}"),
                None => format!("db.t{asset}"),
            },
            table: format!("db.t{asset}"),
            column: column.map(str::to_string),
        }
    }

    fn walk(roots: &[Root], depth: u32, views: &BTreeMap<AssetId, AssetView>) -> Vec<Dependent> {
        let mut walker = Walker::new(roots, depth);
        let mut hops = 0;
        while !walker.is_done() {
            walker.advance(views);
            hops += 1;
            assert!(hops <= 20, "the walk must end");
        }
        walker.finish()
    }

    fn tables(found: &[Dependent]) -> Vec<(String, u32)> {
        found
            .iter()
            .filter(|d| d.kind == Kind::Table)
            .map(|d| (d.id.clone(), d.depth))
            .collect()
    }

    #[test]
    fn follows_tables_transitively_and_says_how() {
        let found = walk(&[root(1, None)], 5, &views(&[(1, 2), (2, 3), (3, 4)]));
        assert_eq!(
            tables(&found),
            vec![("2".into(), 1), ("3".into(), 2), ("4".into(), 3)]
        );
        assert_eq!(found[0].via, Via::Direct);
        assert_eq!(found[0].written_by, vec!["job100"]);
        let deepest = found.iter().find(|d| d.id == "4").unwrap();
        assert_eq!(
            deepest.via,
            Via::Through {
                what: "table",
                name: "db.t3".into()
            }
        );
        assert_eq!(deepest.root, "db.t1");
        let job = found.iter().find(|d| d.kind == Kind::Job).unwrap();
        assert_eq!(
            (job.id.as_str(), job.depth),
            ("100", 1),
            "first sighting kept"
        );
    }

    #[test]
    fn a_dropped_table_is_not_a_dependent() {
        let mut views = views(&[(1, 2), (1, 3)]);
        if let Some(gone) = views.get_mut(&1).and_then(|v| v.assets.get_mut(&3)) {
            gone.is_deleted = true;
        }
        let found = walk(&[root(1, None)], 3, &views);
        assert_eq!(tables(&found), vec![("2".into(), 1)]);
    }

    #[test]
    fn stops_at_the_depth() {
        let found = walk(&[root(1, None)], 2, &views(&[(1, 2), (2, 3), (3, 4)]));
        assert_eq!(tables(&found), vec![("2".into(), 1), ("3".into(), 2)]);
    }

    #[test]
    fn a_cycle_ends_the_walk_and_never_lists_the_root() {
        let found = walk(
            &[root(1, None)],
            10,
            &views(&[(1, 2), (2, 3), (3, 1), (3, 2)]),
        );
        assert_eq!(tables(&found), vec![("2".into(), 1), ("3".into(), 2)]);
    }

    #[test]
    fn a_column_follows_column_lineage_and_skips_tables_that_do_not_use_it() {
        let mut views = BTreeMap::new();
        let mut one = AssetView {
            center: 1,
            ..AssetView::default()
        };
        for id in [1, 2, 3, 4] {
            one.assets.insert(id, table(id));
        }
        // t2 gets amount as total; t3 has column lineage, but not from amount;
        // t4 has table-level lineage only.
        one.relations.push(column(1, 2, "amount", "total"));
        one.relations.push(column(1, 3, "region", "region"));
        one.relations.push(flow(1, 4, 1));
        views.insert(1, one);
        let mut two = AssetView {
            center: 2,
            ..AssetView::default()
        };
        two.assets.insert(2, table(2));
        two.assets.insert(5, table(5));
        two.relations.push(column(2, 5, "total", "grand_total"));
        two.relations.push(column(2, 5, "region", "region"));
        views.insert(2, two);

        let found = walk(&[root(1, Some("Amount"))], 5, &views);
        let by_id = |id: &str| found.iter().find(|d| d.id == id);
        assert_eq!(
            by_id("2").unwrap().columns,
            ColumnUse::Columns(vec!["total".into()])
        );
        assert!(by_id("3").is_none(), "column lineage shows amount not used");
        assert_eq!(by_id("4").unwrap().columns, ColumnUse::Unknown);
        assert_eq!(
            by_id("5").unwrap().columns,
            ColumnUse::Columns(vec!["grand_total".into()]),
            "followed through t2.total, not t2.region"
        );
    }

    #[test]
    fn a_table_asked_about_whole_has_no_column_answer() {
        let found = walk(&[root(1, None)], 1, &views(&[(1, 2)]));
        assert_eq!(found[0].columns, ColumnUse::NotAsked);
    }

    #[test]
    fn datasets_and_their_dashboards_hang_off_the_table_they_read() {
        let mut view = AssetView {
            center: 1,
            ..AssetView::default()
        };
        view.assets.insert(1, table(1));
        view.assets.insert(
            9,
            Asset {
                id: 9,
                asset_type: "DATASET".into(),
                name: "Sales".into(),
                qualified_name: "dataset:1957".into(),
                owner: None,
                is_deleted: false,
            },
        );
        view.relations.push(flow(1, 9, 5));
        view.workloads.insert(
            5,
            Workload {
                job_id: None,
                dataset_id: Some(1957),
                name: "Sales".into(),
                owner: None,
            },
        );
        view.dashboards = vec![
            DashboardRef {
                id: 1159,
                name: "AR-REPORT".into(),
                owner: Some("prasath".into()),
                dataset_ids: vec![1957],
                visualization_ids: vec![1783],
            },
            DashboardRef {
                id: 2,
                name: "unrelated".into(),
                owner: None,
                dataset_ids: vec![42],
                visualization_ids: vec![],
            },
        ];
        let found = walk(&[root(1, None)], 3, &BTreeMap::from([(1, view)]));
        let kinds: Vec<(Kind, &str)> = found.iter().map(|d| (d.kind, d.id.as_str())).collect();
        assert_eq!(
            kinds,
            vec![(Kind::Dataset, "1957"), (Kind::Dashboard, "1159")],
            "the dataset once, though both its asset and its workload point at it"
        );
        assert_eq!(
            found[1].via,
            Via::Through {
                what: "dataset",
                name: "Sales".into()
            }
        );
        assert_eq!(found[1].visualization_ids, vec![1783]);
    }
}
