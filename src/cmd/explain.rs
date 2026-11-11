//! `gz explain`: where a reported number comes from. It reads, and only
//! reads:
//!
//! - Designer records: `GET /dashboard`, `/dashboard/{id}`,
//!   `/visualization/{id}`, `/dataset`, `/dataset/{id}`, and `GET
//!   /connection`, `/connection/{id}` for a connection's warehouse
//!   (`config.warehouseName`, `config.databaseName`);
//! - each table's snapshots: `GET /lakehouse/tables/{wh}/{db}/{t}/versions`;
//! - its lineage: `GET /lakehouse/governance/lineage/table`
//!   (groundzero-iceberg-provider-api app_routes/governance.py:595);
//! - runs around each commit: `GET /etl/consumption-metrics` (ETL provider
//!   app.py:1491; web pages/Utils/etlUtlis.js:2423), which only reads;
//! - a run's job or dataset lineage, for the snapshot it recorded and its
//!   code version: `GET /etl/lineage/jobs/{id}` or `/etl/lineage/datasets/{id}`
//!   (app.py:2121, 2190; web pages/apiCatalog/lineageApi.js:132).
//!
//! It never calls `GET /etl/jobs/{id}/current-run` or `POST /etl/logs`,
//! which can settle a live run.

use std::collections::HashMap;

use anyhow::{bail, Context, Result};
use reqwest::Method;
use serde_json::Value;

use crate::cli::{ExplainArgs, ExplainOptions, ExplainTarget, ShellArgs};
use crate::cmd::{seg, Runtime};
use crate::explain::designer::{self, DatasetDef, DatasetKind, Figure};
use crate::explain::report;
use crate::explain::runs::{self, Commit, Run, RunMatch};
use crate::explain::trace::{
    self, gap, Changes, Connection, DatasetTrace, SnapshotView, Subject, TableKey, TableTrace,
    Trace,
};
use crate::output::OutputFormat;
use crate::sql::tables::TableRef;
use crate::table_history::diff::Comparison;
use crate::table_history::{when, History, Snapshot, TableName};
use crate::tui::format::{epoch_ms, iso8601_utc};

const ETL: &str = "etl";
const LAKEHOUSE: &str = "lakehouse";
/// The most runs `consumption-metrics` returns at once.
const RUN_PAGE: usize = 100;
/// Pages of runs read around one commit. Runs whose end was never
/// recorded overlap every window, so a window can hold a few hundred.
const MAX_RUN_PAGES: usize = 5;
/// Runs of one job or dataset read from its lineage, newest first.
const LINEAGE_RUNS: usize = 100;
/// Workloads whose lineage is read when looking for a recorded snapshot.
const MAX_LINEAGE_LOOKUPS: usize = 5;
/// Who reads a table in `gz explain sql`.
const SQL_READER: &str = "the query";

pub async fn run(rt: &Runtime, args: &ExplainArgs) -> Result<()> {
    let options = args.target.options();
    let at = match &options.at {
        Some(text) => Some(when::parse(text).with_context(|| {
            format!("--at '{text}' is not a UTC date or time; write it like 2026-09-16 or 2026-09-16T08:00")
        })?),
        None => None,
    };
    let mut explainer = Explainer::new(rt, at, options.depth);
    let trace = match &args.target {
        ExplainTarget::Dashboard { dashboard, viz, .. } => {
            explainer.dashboard(dashboard, viz.as_deref()).await?
        }
        ExplainTarget::Viz { visualization, .. } => explainer.visualization(visualization).await?,
        ExplainTarget::Dataset { dataset, .. } => explainer.dataset(dataset).await?,
        ExplainTarget::Sql {
            query,
            warehouse,
            database,
            ..
        } => {
            let query = crate::cmd::read_text_arg(query)?;
            explainer
                .sql(&query, warehouse.as_deref(), database.as_deref())
                .await?
        }
    };
    if rt.format != OutputFormat::Table {
        return rt.show(serde_json::to_value(&trace)?).await;
    }
    print!("{}", report::text(&trace));
    Ok(())
}

impl ExplainTarget {
    fn options(&self) -> &ExplainOptions {
        match self {
            Self::Dashboard { options, .. }
            | Self::Viz { options, .. }
            | Self::Dataset { options, .. }
            | Self::Sql { options, .. } => options,
        }
    }
}

struct Explainer<'a> {
    rt: &'a Runtime,
    /// `YYYY-MM-DDTHH:MM:SS` UTC.
    at: Option<String>,
    depth: u8,
    now_ms: i64,
    connection_ids: Option<Vec<(String, String)>>,
    connections: HashMap<String, Connection>,
    lineage: HashMap<String, Option<Value>>,
    gaps: Vec<String>,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// `(Id, Name)` pairs of a Designer list.
fn named_rows(list: &Value) -> Vec<(String, String)> {
    list.as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| {
            let id = crate::output::scalar_text(&row["Id"]);
            let name = row["Name"].as_str()?.to_string();
            (!id.is_empty()).then_some((id, name))
        })
        .collect()
}

/// The row an id or a name means: an exact id, else the one name that
/// matches without regard to case.
fn pick<'r>(rows: &'r [(String, String)], wanted: &str) -> Option<&'r (String, String)> {
    rows.iter().find(|(id, _)| id == wanted).or_else(|| {
        let mut named = rows.iter().filter(|(_, n)| n.eq_ignore_ascii_case(wanted));
        let first = named.next();
        first.filter(|_| named.next().is_none())
    })
}

impl<'a> Explainer<'a> {
    fn new(rt: &'a Runtime, at: Option<String>, depth: u8) -> Self {
        Self {
            rt,
            at,
            depth,
            now_ms: now_ms(),
            connection_ids: None,
            connections: HashMap::new(),
            lineage: HashMap::new(),
            gaps: Vec::new(),
        }
    }

    fn note(&mut self, gap: &str) {
        if !self.gaps.iter().any(|g| g == gap) {
            self.gaps.push(gap.to_string());
        }
    }

    async fn get(&self, service: &str, path: &str, query: &[(String, String)]) -> Result<Value> {
        self.rt
            .http
            .request_json(service, Method::GET, path, query, None)
            .await
    }

    /// An id or name from a Designer list, as `(id, name)`.
    async fn find(
        &self,
        service: &str,
        list_path: &str,
        wanted: &str,
        noun: &str,
        list_command: &str,
    ) -> Result<(String, String)> {
        let rows = named_rows(&self.get(service, list_path, &[]).await?);
        if let Some(found) = pick(&rows, wanted) {
            return Ok(found.clone());
        }
        let same_name = rows
            .iter()
            .filter(|(_, n)| n.eq_ignore_ascii_case(wanted))
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>();
        if same_name.len() > 1 {
            bail!(
                "several {noun}s are named '{wanted}' (ids {}); pass the id",
                same_name.join(", ")
            );
        }
        bail!("no {noun} '{wanted}' that you can see; `{list_command}` lists them")
    }

    fn trace(
        &self,
        subject: Subject,
        figures: Vec<Figure>,
        datasets: Vec<DatasetTrace>,
        tables: Vec<TableTrace>,
    ) -> Trace {
        Trace {
            subject,
            at: self.at.clone(),
            figures,
            datasets,
            tables,
            gaps: self.gaps.clone(),
        }
    }

    // ---------- subjects ----------

    async fn dashboard(&mut self, wanted: &str, viz: Option<&str>) -> Result<Trace> {
        let (id, name) = self
            .find(
                "dashboards",
                "/dashboard",
                wanted,
                "dashboard",
                "gz designer dashboards",
            )
            .await?;
        let record = self
            .get("dashboards", &format!("/dashboard/{}", seg(&id)), &[])
            .await
            .with_context(|| format!("cannot read dashboard {name} ({id})"))?;
        let mut figures = Vec::new();
        for viz_id in designer::dashboard_visualizations(&record) {
            let figure = self.figure(&viz_id).await?;
            let chart_name = designer::dashboard_chart_name(&record, &viz_id);
            let wanted_here = viz.is_none_or(|w| {
                w == viz_id
                    || w.eq_ignore_ascii_case(&figure.name)
                    || chart_name
                        .as_deref()
                        .is_some_and(|c| c.eq_ignore_ascii_case(w))
            });
            if wanted_here {
                figures.push(figure);
            }
        }
        if let (Some(w), true) = (viz, figures.is_empty()) {
            bail!("dashboard {name} has no visualization '{w}'; `gz explain dashboard {id}` shows them all");
        }
        if record["config"]["dashboardFilters"]
            .as_object()
            .is_some_and(|tiles| !tiles.is_empty())
        {
            self.note(gap::DASHBOARD_TILES);
        }
        let subject = Subject {
            kind: "dashboard",
            id: Some(id),
            name: Some(name),
        };
        self.trace_figures(subject, figures).await
    }

    async fn visualization(&mut self, wanted: &str) -> Result<Trace> {
        let id = if wanted.bytes().all(|b| b.is_ascii_digit()) {
            wanted.to_string()
        } else {
            self.find(
                "visualizations",
                "/visualization",
                wanted,
                "visualization",
                "gz designer visualizations",
            )
            .await?
            .0
        };
        let figure = self.figure(&id).await?;
        let subject = Subject {
            kind: "visualization",
            id: Some(figure.id.clone()),
            name: Some(figure.name.clone()),
        };
        self.trace_figures(subject, vec![figure]).await
    }

    async fn dataset(&mut self, wanted: &str) -> Result<Trace> {
        let id = if wanted.bytes().all(|b| b.is_ascii_digit()) {
            wanted.to_string()
        } else {
            self.find("datasets", "/dataset", wanted, "dataset", "gz datasets")
                .await?
                .0
        };
        let definition = self.dataset_definition(&id).await?;
        let subject = Subject {
            kind: "dataset",
            id: Some(definition.id.clone()),
            name: Some(definition.name.clone()),
        };
        let mut tables = Vec::new();
        let dataset = self.dataset_trace(definition, &mut tables).await;
        let tables = self.trace_tables(tables).await;
        self.note_definitions_are_current();
        Ok(self.trace(subject, Vec::new(), vec![dataset], tables))
    }

    async fn sql(
        &mut self,
        query: &str,
        warehouse: Option<&str>,
        database: Option<&str>,
    ) -> Result<Trace> {
        let shell = ShellArgs {
            warehouse: warehouse.map(String::from),
            ..ShellArgs::default()
        };
        let warehouse = crate::sql::warehouse(self.rt, &shell).await?;
        let connection = Connection {
            name: format!("warehouse {}", warehouse.name),
            kind: Some("GZLAKEHOUSE".to_string()),
            warehouse: Some(warehouse.name.clone()),
            database: database.map(String::from),
        };
        let found = trace::query_tables(query);
        let mut keys = Vec::new();
        for name in &found.tables {
            match trace::resolve(name, &connection) {
                Ok(key) => add_table(&mut keys, key, SQL_READER),
                Err(why) => self.note(&format!("{why}; pass --database")),
            }
        }
        for source in &found.unresolved {
            self.note(&format!(
                "The query reads {source}, which gz cannot trace to a lakehouse table."
            ));
        }
        if found.tables.is_empty() && found.unresolved.is_empty() {
            bail!("gz found no table in the query; check that it reads FROM a table");
        }
        let tables = self.trace_tables(keys).await;
        let subject = Subject {
            kind: "sql",
            id: None,
            name: None,
        };
        Ok(self.trace(subject, Vec::new(), Vec::new(), tables))
    }

    /// The figures' datasets, their tables, and each table's trace.
    async fn trace_figures(&mut self, subject: Subject, mut figures: Vec<Figure>) -> Result<Trace> {
        let mut definitions: Vec<DatasetDef> = Vec::new();
        for figure in &figures {
            let Some(id) = &figure.dataset_id else {
                continue;
            };
            if definitions.iter().any(|d| &d.id == id) {
                continue;
            }
            match self.dataset_definition(id).await {
                Ok(definition) => definitions.push(definition),
                Err(err) => self.note(&format!(
                    "Dataset {id} of visualization {} could not be read: {err:#}",
                    figure.name
                )),
            }
        }
        for figure in &mut figures {
            if let Some(definition) = definitions
                .iter()
                .find(|d| Some(&d.id) == figure.dataset_id.as_ref())
            {
                trace::explain_measures(figure, &trace::dataset_queries(definition));
            }
        }
        let mut tables = Vec::new();
        let mut datasets = Vec::new();
        for definition in definitions {
            datasets.push(self.dataset_trace(definition, &mut tables).await);
        }
        let tables = self.trace_tables(tables).await;
        self.note_definitions_are_current();
        Ok(self.trace(subject, figures, datasets, tables))
    }

    fn note_definitions_are_current(&mut self) {
        if self.at.is_some() {
            self.note(gap::DEFINITIONS_ARE_CURRENT);
        }
    }

    // ---------- Designer ----------

    async fn figure(&self, id: &str) -> Result<Figure> {
        let record = self
            .get(
                "visualizations",
                &format!("/visualization/{}", seg(id)),
                &[],
            )
            .await
            .with_context(|| {
                format!("cannot read visualization {id}; `gz designer visualizations` lists them")
            })?;
        Ok(designer::figure(&record))
    }

    async fn dataset_definition(&self, id: &str) -> Result<DatasetDef> {
        let record = self
            .get("datasets", &format!("/dataset/{}", seg(id)), &[])
            .await
            .with_context(|| format!("cannot read dataset {id}"))?;
        // A missing dataset comes back as an empty 200.
        if record["Id"].is_null() {
            bail!(
                "dataset {id} does not exist (it may have been deleted); `gz datasets` lists them"
            );
        }
        Ok(designer::dataset(&record))
    }

    async fn connection(&mut self, name: &str) -> Result<Connection> {
        if let Some(known) = self.connections.get(name) {
            return Ok(known.clone());
        }
        if self.connection_ids.is_none() {
            let list = self.get("connections", "/connection", &[]).await?;
            self.connection_ids = Some(named_rows(&list));
        }
        let id = self
            .connection_ids
            .as_deref()
            .and_then(|rows| rows.iter().find(|(_, n)| n == name))
            .map(|(id, _)| id.clone())
            .with_context(|| format!("no data connection named {name} that you can see"))?;
        let reply = self
            .get("connections", &format!("/connection/{}", seg(&id)), &[])
            .await?;
        // The route answers with a one-element list; only the type and the
        // warehouse fields are kept.
        let record = reply.get(0).unwrap_or(&reply);
        let text = |value: &Value| value.as_str().filter(|s| !s.is_empty()).map(String::from);
        let connection = Connection {
            name: name.to_string(),
            kind: text(&record["type"]),
            warehouse: text(&record["config"]["warehouseName"]),
            database: text(&record["config"]["databaseName"]),
        };
        self.connections
            .insert(name.to_string(), connection.clone());
        Ok(connection)
    }

    /// The dataset with its connections and the tables it reads, which are
    /// added to `tables`.
    async fn dataset_trace(
        &mut self,
        definition: DatasetDef,
        tables: &mut Vec<(TableKey, Vec<String>)>,
    ) -> DatasetTrace {
        let mut connection_details = Vec::new();
        for name in &definition.connections {
            match self.connection(name).await {
                Ok(connection) => connection_details.push(connection),
                Err(err) => self.note(&format!(
                    "Connection {name} of dataset {} could not be read: {err:#}",
                    definition.name
                )),
            }
        }
        if definition
            .filters
            .iter()
            .any(|f| !f.placeholders.is_empty())
        {
            self.note(gap::FILTER_DEFAULTS);
        }
        let mut dataset = DatasetTrace {
            definition,
            connection_details,
            tables: Vec::new(),
            unresolved: Vec::new(),
        };
        let queries = trace::dataset_queries(&dataset.definition);
        self.note_how_sql_was_found(&dataset.definition, queries.is_empty());
        let lakehouse: Vec<&Connection> = dataset
            .connection_details
            .iter()
            .filter(|c| c.is_lakehouse())
            .collect();
        let Some(connection) = lakehouse.first().copied().cloned() else {
            if !queries.is_empty() {
                self.note(&format!(
                    "Dataset {} reads a connection that is not a lakehouse; gz traces lakehouse tables only.",
                    dataset.definition.name
                ));
            }
            return dataset;
        };
        if lakehouse.len() > 1 {
            self.note(&format!(
                "Dataset {} uses several lakehouse connections; gz placed its tables in {}.",
                dataset.definition.name, connection.name
            ));
        }
        for query in &queries {
            let found = trace::query_tables(query);
            for name in &found.tables {
                self.place(&mut dataset, name, &connection, tables);
            }
            dataset.unresolved.extend(found.unresolved);
        }
        dataset
    }

    fn place(
        &mut self,
        dataset: &mut DatasetTrace,
        name: &TableRef,
        connection: &Connection,
        tables: &mut Vec<(TableKey, Vec<String>)>,
    ) {
        match trace::resolve(name, connection) {
            Ok(key) => {
                let text = key.to_string();
                if !dataset.tables.contains(&text) {
                    dataset.tables.push(text);
                }
                add_table(tables, key, &dataset.definition.id);
            }
            Err(why) => dataset.unresolved.push(why),
        }
    }

    fn note_how_sql_was_found(&mut self, definition: &DatasetDef, nothing_found: bool) {
        match (definition.kind, nothing_found) {
            (DatasetKind::Code, false) => self.note(&format!(
                "Dataset {} is code; gz found its SQL in the code's strings, and SQL built at run time would be missed.",
                definition.name
            )),
            (DatasetKind::Code, true) => self.note(&format!(
                "Dataset {} is code with no SQL gz can read, so the tables it reads are unknown.",
                definition.name
            )),
            (DatasetKind::Sql, true) => self.note(&format!(
                "Dataset {} has no query, so the tables it reads are unknown.",
                definition.name
            )),
            (DatasetKind::Sql, false) => {}
        }
    }

    // ---------- tables ----------

    async fn trace_tables(&mut self, keys: Vec<(TableKey, Vec<String>)>) -> Vec<TableTrace> {
        let mut traced = Vec::new();
        for (key, readers) in keys {
            let mut table = TableTrace::new(key, readers);
            self.trace_table(&mut table).await;
            traced.push(table);
        }
        traced
    }

    async fn trace_table(&mut self, table: &mut TableTrace) {
        let name = TableName {
            database: table.key.database.clone(),
            table: table.key.table.clone(),
        };
        match History::load(&self.rt.http, &table.key.warehouse, &name).await {
            Ok(history) => self.trace_snapshot(table, &history).await,
            Err(err) => {
                let error = format!("{err:#}");
                // Lineage needs the same access, so a refusal ends the trace.
                if let Some(refused) = trace::read_refusal(&error, &table.key) {
                    table.problems.push(format!("table: {refused}"));
                    return;
                }
                table.problems.push(format!("snapshots: {error}"));
            }
        }
        self.trace_upstream(table).await;
    }

    async fn trace_snapshot(&mut self, table: &mut TableTrace, history: &History) {
        let reference = self.at.as_deref().unwrap_or("current");
        let snapshot = match history.resolve(reference) {
            Ok(snapshot) => snapshot.clone(),
            Err(err) => {
                table.problems.push(format!("snapshots: {err:#}"));
                return;
            }
        };
        table.snapshot = Some(SnapshotView::of(history, &snapshot));
        if self.at.is_some() {
            match changes_since(history, &snapshot) {
                Ok(changes) => table.changes_since = changes,
                Err(why) => table.problems.push(format!("since: {why}")),
            }
        }
        self.trace_run(table, &snapshot).await;
    }

    async fn trace_run(&mut self, table: &mut TableTrace, snapshot: &Snapshot) {
        let Some(committed_ms) = epoch_ms(&Value::String(snapshot.committed_at.clone())) else {
            table
                .problems
                .push(format!("runs: snapshot {} has no commit time", snapshot.id));
            return;
        };
        let view = table.snapshot.clone();
        let spark_app_id = view.as_ref().and_then(|v| v.spark_app_id.clone());
        let engine = view
            .as_ref()
            .and_then(|v| v.engine.clone())
            .or_else(|| spark_app_id.as_ref().map(|_| "spark".to_string()));
        let commit = Commit {
            snapshot_id: snapshot.id.clone(),
            committed_ms,
            app_name: view.and_then(|v| v.app_name),
            spark_app_id,
            engine,
        };
        let runs = match self.runs_around(committed_ms).await {
            Ok(runs) => runs,
            Err(err) => {
                table.problems.push(format!(
                    "runs: cannot list the runs around the commit: {err:#}"
                ));
                return;
            }
        };
        let recorded = self.recorded_run(&commit, &runs).await;
        let Some(found) = runs::match_run(&commit, &runs, recorded.as_deref(), self.now_ms) else {
            return;
        };
        if found.certainty != runs::Certainty::Recorded {
            self.note(gap::RUN_ID_NOT_ON_COMMITS);
        }
        table.code_version = self.code_version(&found, table).await;
        table.run = Some(found);
    }

    /// Runs of jobs and code datasets going around the commit. The list
    /// returns every run that overlaps the window (site-manager-api
    /// etl-lineage/execution-read.mjs:1266), newest start first.
    async fn runs_around(&mut self, committed_ms: i64) -> Result<Vec<Run>> {
        let window = [
            ("from", iso8601_utc(committed_ms - runs::CLOCK_SLACK_MS)),
            ("to", iso8601_utc(committed_ms + runs::CLOCK_SLACK_MS)),
            ("limit", RUN_PAGE.to_string()),
        ];
        let mut runs = Vec::new();
        for page in 0..MAX_RUN_PAGES {
            let mut query: Vec<(String, String)> = window
                .iter()
                .map(|(key, value)| (key.to_string(), value.clone()))
                .collect();
            query.push(("offset".to_string(), (page * RUN_PAGE).to_string()));
            let reply = self.get(ETL, "/etl/consumption-metrics", &query).await?;
            let items = reply["items"].as_array().cloned().unwrap_or_default();
            runs.extend(items.iter().filter_map(Run::from_item));
            let total = reply["pagination"]["total"].as_u64().unwrap_or_default();
            if items.len() < RUN_PAGE || runs.len() as u64 >= total {
                return Ok(runs);
            }
        }
        self.note(&format!(
            "More than {} runs overlap a commit; only the {} that started last were compared.",
            MAX_RUN_PAGES * RUN_PAGE,
            MAX_RUN_PAGES * RUN_PAGE
        ));
        Ok(runs)
    }

    /// The lineage path of the job or dataset a run belongs to.
    fn workload_path(run: &Run) -> Option<String> {
        match (run.job_id, run.dataset_id) {
            (Some(job), _) => Some(format!("/etl/lineage/jobs/{job}")),
            (None, Some(dataset)) => Some(format!("/etl/lineage/datasets/{dataset}")),
            (None, None) => None,
        }
    }

    /// A job's or dataset's lineage (one version), read once.
    async fn workload_lineage(&mut self, path: &str, version: Option<i64>) -> Option<Value> {
        let key = format!("{path}?{version:?}");
        if let Some(known) = self.lineage.get(&key) {
            return known.clone();
        }
        let mut query = vec![("runLimit".to_string(), LINEAGE_RUNS.to_string())];
        if let Some(version) = version {
            query.push(("versionId".to_string(), version.to_string()));
        }
        let reply = match self.get(ETL, path, &query).await {
            Ok(reply) => Some(reply),
            Err(err) => {
                self.note(&format!("Lineage at {path} could not be read: {err:#}"));
                None
            }
        };
        self.lineage.insert(key, reply.clone());
        reply
    }

    /// The run the lineage store recorded for this snapshot, looking only at
    /// the workloads of runs going at commit time.
    async fn recorded_run(&mut self, commit: &Commit, runs: &[Run]) -> Option<String> {
        let mut paths: Vec<String> = Vec::new();
        for run in runs::going_at(runs, commit, self.now_ms) {
            if let Some(path) = Self::workload_path(run).filter(|p| !paths.contains(p)) {
                paths.push(path);
            }
        }
        for path in paths.into_iter().take(MAX_LINEAGE_LOOKUPS) {
            let lineage = self.workload_lineage(&path, None).await?;
            if let Some(id) = runs::recorded_execution(&lineage, &commit.snapshot_id) {
                return Some(id);
            }
        }
        None
    }

    /// The code version the matched run used: the newest version when the
    /// run is among its runs, else the one saved last before the run.
    async fn code_version(
        &mut self,
        found: &RunMatch,
        table: &mut TableTrace,
    ) -> Option<runs::CodeVersion> {
        let run = &found.run;
        let path = Self::workload_path(run)?;
        let newest = self.workload_lineage(&path, None).await?;
        if let Some(version) = runs::version_running(&newest, &run.execution_id) {
            return Some(version);
        }
        let earlier = runs::version_before(&newest, &run.started_at)?;
        let lineage = self.workload_lineage(&path, Some(earlier)).await?;
        let version = runs::version_running(&lineage, &run.execution_id);
        if version.is_none() {
            table.problems.push(format!(
                "code: the lineage store lists no code version for run {}",
                run.execution_id
            ));
        }
        version
    }

    async fn trace_upstream(&mut self, table: &mut TableTrace) {
        let query = vec![
            ("warehouseName".to_string(), table.key.warehouse.clone()),
            ("databaseName".to_string(), table.key.database.clone()),
            ("tableName".to_string(), table.key.table.clone()),
            ("direction".to_string(), "upstream".to_string()),
            ("depth".to_string(), self.depth.to_string()),
        ];
        match self
            .get(LAKEHOUSE, "/lakehouse/governance/lineage/table", &query)
            .await
        {
            Ok(reply) => table.upstream = Some(trace::upstream_edges(&reply)),
            Err(err) => {
                let error = format!("{err:#}");
                let why = trace::read_refusal(&error, &table.key).unwrap_or(error);
                table.problems.push(format!("lineage: {why}"));
            }
        }
    }
}

/// Adds a table once, remembering every reader.
fn add_table(tables: &mut Vec<(TableKey, Vec<String>)>, key: TableKey, reader: &str) {
    match tables.iter_mut().find(|(known, _)| *known == key) {
        Some((_, readers)) => {
            if !readers.iter().any(|r| r == reader) {
                readers.push(reader.to_string());
            }
        }
        None => tables.push((key, vec![reader.to_string()])),
    }
}

/// The commits after `snapshot` on the table's current line; `None` when
/// the table is still at it.
fn changes_since(history: &History, snapshot: &Snapshot) -> Result<Option<Changes>, String> {
    if history.current.as_deref() == Some(snapshot.id.as_str()) {
        return Ok(None);
    }
    let comparison =
        Comparison::between(history, &snapshot.id, "current").map_err(|e| format!("{e:#}"))?;
    let commits = comparison.commits.ok_or_else(|| {
        "the current snapshot is not built on that one (rolled back or replaced), so its commits cannot be listed".to_string()
    })?;
    Ok(Some(Changes {
        commits: commits
            .iter()
            .map(|s| SnapshotView::of(history, s))
            .collect(),
        records_then: snapshot.count("total-records"),
        records_now: comparison.to.count("total-records"),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows() -> Vec<(String, String)> {
        vec![
            (
                "1114".to_string(),
                "Executive-Working-Capital-Overview".to_string(),
            ),
            ("1138".to_string(), "test".to_string()),
            ("1139".to_string(), "Test".to_string()),
        ]
    }

    #[test]
    fn picks_by_id_or_by_a_name_only_one_row_has() {
        assert_eq!(pick(&rows(), "1114").map(|r| r.0.as_str()), Some("1114"));
        assert_eq!(
            pick(&rows(), "executive-working-capital-overview").map(|r| r.0.as_str()),
            Some("1114")
        );
        assert_eq!(
            pick(&rows(), "TEST"),
            None,
            "two dashboards share that name"
        );
        assert_eq!(pick(&rows(), "nope"), None);
    }

    #[test]
    fn a_table_is_traced_once_for_all_its_readers() {
        let key = |t: &str| TableKey {
            warehouse: "W".to_string(),
            database: "d".to_string(),
            table: t.to_string(),
        };
        let mut tables = Vec::new();
        add_table(&mut tables, key("a"), "1873");
        add_table(&mut tables, key("a"), "1471");
        add_table(&mut tables, key("a"), "1873");
        add_table(&mut tables, key("b"), "1471");
        assert_eq!(tables.len(), 2);
        assert_eq!(tables[0].1, ["1873", "1471"]);
    }
}
