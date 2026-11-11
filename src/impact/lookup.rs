//! The HTTP side of impact. Lineage comes from the etl provider (the graph
//! the catalog's lineage pages show, ground-zero
//! src/pages/apiCatalog/lineageApi.js); visualizations from the Designer
//! gateway; agents from the agent builder.
//!
//! The etl provider takes API keys, so the lineage part works in CI with
//! `GZ_API_KEY`. The Designer and agent builder routes need a signed-in
//! user; without one those parts are skipped with a note.

use anyhow::{Context, Result};
use reqwest::Method;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

use crate::cmd::seg;
use crate::http::ApiClient;
use crate::impact::graph::{AssetId, AssetView};
use crate::impact::walk::{Root, Walker};
use crate::impact::{Dependent, Kind, Report, Target, Via};

const ETL: &str = "etl";
const VISUALIZATIONS: &str = "visualizations";
const AGENTS: &str = "agents";
/// Lineage asset type of tables.
const TABLE_TYPE: &str = "TABLE";
/// The catalog route's page size limit (app.py:2268).
const ASSET_PAGE: usize = 100;
/// The agent list's page (api.py:300 takes any; 100 keeps pages few).
const AGENT_PAGE: usize = 100;
/// The impact route's deepest walk (lineage_impact.py MAX_IMPACT_DEPTH).
const MAX_IMPACT_DEPTH: u32 = 5;
/// Ontology definitions read per target (the impact route's maximum).
const DEFINITION_LIMIT: u32 = 200;
/// Requests in flight at once.
const PARALLEL: usize = 8;
/// More lineage assets than this and the walk stops, saying so.
const MAX_ASSETS: usize = 500;

/// Who can be asked: the lineage parts take an API key, the rest a user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caller {
    User,
    KeyOnly,
}

impl Caller {
    pub async fn of(http: &ApiClient) -> Caller {
        if http.token().await.is_none() && http.resolved().api_key().is_some() {
            Caller::KeyOnly
        } else {
            Caller::User
        }
    }
}

/// Finds the targets in lineage, walks downstream, and adds the
/// visualizations and agents on top.
pub async fn impact_of(http: &ApiClient, mut targets: Vec<Target>, depth: u32) -> Result<Report> {
    find_tables(http, &mut targets).await?;
    impact_of_assets(http, targets, depth).await
}

/// The same, for targets whose lineage assets are already known (the
/// tables jobs write).
pub async fn impact_of_assets(
    http: &ApiClient,
    targets: Vec<Target>,
    depth: u32,
) -> Result<Report> {
    let roots = roots_of(&targets);
    let mut report = Report {
        targets,
        depth,
        ..Report::default()
    };
    walk(http, &roots, depth, &mut report).await?;
    match Caller::of(http).await {
        Caller::User => {
            add_visualizations(http, &mut report).await?;
            add_agents(http, &roots, depth, &mut report).await?;
        }
        Caller::KeyOnly => report.notes.push(KEY_ONLY_NOTE.to_string()),
    }
    report
        .dependents
        .sort_by_key(|d| (d.kind.flow_rank(), d.depth));
    Ok(report)
}

const KEY_ONLY_NOTE: &str = "visualizations and agents were not checked: they need a signed-in user (gz auth login); API keys reach lineage only";

fn roots_of(targets: &[Target]) -> Vec<Root> {
    targets
        .iter()
        .flat_map(|t| {
            t.assets.iter().map(|&asset| Root {
                asset,
                target: t.text.clone(),
                table: t.table.clone(),
                column: t.column.clone(),
            })
        })
        .collect()
}

// ---------- finding tables ----------

/// Sets each target's assets: every lineage connection that recorded the
/// table has its own asset for it (the web app looks tables up the same
/// way, src/pages/lakehouse/Table/tableLineageLink.js).
async fn find_tables(http: &ApiClient, targets: &mut [Target]) -> Result<()> {
    let connections = table_connections(http).await?;
    for target in targets.iter_mut() {
        let wanted = normalized(&target.table);
        // The catalog matches by substring; quoted names ("db"."t") would
        // not match db.t, so search by the table part and compare here.
        let query = target.table.rsplit('.').next().unwrap_or(&target.table);
        let pages = parallel(connections.iter().map(|&c| {
            let http = http.clone();
            let query = query.to_string();
            async move { catalog_page(&http, c, &query).await }
        }))
        .await?;
        let mut assets: BTreeSet<AssetId> = BTreeSet::new();
        for page in pages {
            let page = page?;
            for asset in page["assets"].as_array().into_iter().flatten() {
                let name = asset["qualifiedName"].as_str().unwrap_or_default();
                if normalized(name) == wanted {
                    assets.extend(asset["assetId"].as_i64());
                }
            }
        }
        target.assets = assets.into_iter().collect();
    }
    Ok(())
}

/// `GET /etl/lineage/connections` (app.py:2496; lineageApi.js:184): the
/// connections with recorded table assets.
async fn table_connections(http: &ApiClient) -> Result<Vec<i64>> {
    let reply = http
        .request_json(ETL, Method::GET, "/etl/lineage/connections", &[], None)
        .await
        .context("cannot list the connections lineage has recorded")?;
    Ok(reply["connections"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| {
            c["assetTypes"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|t| t["assetType"] == TABLE_TYPE)
        })
        .filter_map(|c| c["connectionId"].as_i64())
        .collect())
}

/// `GET /etl/lineage/assets?connectionId&query&assetType` (app.py:2260;
/// lineageApi.js:144).
async fn catalog_page(http: &ApiClient, connection: i64, query: &str) -> Result<Value> {
    let params = [
        ("connectionId".to_string(), connection.to_string()),
        ("query".to_string(), query.to_string()),
        ("assetType".to_string(), TABLE_TYPE.to_string()),
        ("limit".to_string(), ASSET_PAGE.to_string()),
    ];
    http.request_json(ETL, Method::GET, "/etl/lineage/assets", &params, None)
        .await
        .with_context(|| format!("cannot search lineage connection {connection}"))
}

/// `"demo_sales"."Orders"` and `DEMO_SALES.orders` are the same table.
fn normalized(name: &str) -> String {
    name.replace('"', "").to_lowercase()
}

// ---------- the walk ----------

async fn walk(http: &ApiClient, roots: &[Root], depth: u32, report: &mut Report) -> Result<()> {
    let mut walker = Walker::new(roots, depth);
    let mut views: BTreeMap<AssetId, AssetView> = BTreeMap::new();
    let mut tried: BTreeSet<AssetId> = BTreeSet::new();
    while !walker.is_done() {
        let missing: Vec<AssetId> = walker
            .wanted()
            .into_iter()
            .filter(|id| tried.insert(*id))
            .collect();
        if tried.len() > MAX_ASSETS {
            report.notes.push(format!(
                "stopped after {MAX_ASSETS} lineage assets at depth {}; pass a smaller --depth",
                report.dependents.iter().map(|d| d.depth).max().unwrap_or(0)
            ));
            break;
        }
        let fetched = parallel(missing.iter().map(|&id| {
            let http = http.clone();
            async move { (id, asset_view(&http, id).await) }
        }))
        .await?;
        for (id, view) in fetched {
            match view {
                Ok(view) => {
                    views.insert(id, view);
                }
                // A table the caller may not see ends its branch; say so
                // rather than pretend nothing is there.
                Err(err) if is_hidden(&err) => report.notes.push(format!(
                    "lineage asset {id} is not visible to you; not followed"
                )),
                Err(err) => return Err(err),
            }
        }
        walker.advance(&views);
    }
    report.dependents = walker.finish();
    Ok(())
}

/// `GET /etl/lineage/assets/{id}`, downstream only, one hop (app.py:2323;
/// lineageApi.js:159). Run history is not needed, so its pages are minimal.
pub async fn asset_view(http: &ApiClient, asset: AssetId) -> Result<AssetView> {
    let query: Vec<(String, String)> = [
        ("depth", "1"),
        ("upstreamDepth", "0"),
        ("downstreamDepth", "1"),
        ("historyLimit", "1"),
        ("runLimit", "1"),
        ("eventLimit", "1"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let path = format!("/etl/lineage/assets/{asset}");
    let reply = http
        .request_json(ETL, Method::GET, &path, &query, None)
        .await
        .with_context(|| format!("cannot read the lineage of asset {asset}"))?;
    AssetView::from_reply(&reply).with_context(|| format!("lineage asset {asset}"))
}

fn is_hidden(err: &anyhow::Error) -> bool {
    let text = format!("{err:#}");
    text.contains("HTTP 403") || text.contains("HTTP 404")
}

/// Runs the futures `PARALLEL` at a time, keeping their order.
async fn parallel<F>(futures: impl IntoIterator<Item = F>) -> Result<Vec<F::Output>>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    let mut pending: Vec<F> = futures.into_iter().collect();
    pending.reverse();
    let mut results = Vec::with_capacity(pending.len());
    while !pending.is_empty() {
        let mut set = tokio::task::JoinSet::new();
        for (index, future) in std::iter::from_fn(|| pending.pop())
            .take(PARALLEL)
            .enumerate()
        {
            set.spawn(async move { (index, future.await) });
        }
        let mut batch = Vec::new();
        while let Some(done) = set.join_next().await {
            batch.push(done.context("a lineage request stopped unexpectedly")?);
        }
        batch.sort_by_key(|(i, _)| *i);
        results.extend(batch.into_iter().map(|(_, r)| r));
    }
    Ok(results)
}

// ---------- visualizations ----------

/// A visualization and the dataset it charts.
#[derive(Debug, Clone, PartialEq)]
pub struct Chart {
    pub id: i64,
    pub name: String,
    pub dataset: Option<i64>,
    pub owner: Option<String>,
}

/// Visualizations on the affected dashboards that chart an affected
/// dataset. Lineage names each dashboard's visualizations but not their
/// datasets, so each one is read (`GET /visualization/{id}`, the Designer
/// gateway; ground-zero src/pages/Utils/visualizationUtil.js:79).
async fn add_visualizations(http: &ApiClient, report: &mut Report) -> Result<()> {
    let chart_ids: BTreeSet<i64> = report
        .dependents
        .iter()
        .filter(|d| d.kind == Kind::Dashboard)
        .flat_map(|d| d.visualization_ids.iter().copied())
        .collect();
    if chart_ids.is_empty() {
        return Ok(());
    }
    let owners = chart_owners(http).await?;
    let replies = parallel(chart_ids.into_iter().map(|id| {
        let http = http.clone();
        async move {
            let path = format!("/visualization/{}", seg(&id.to_string()));
            (
                id,
                http.request_json(VISUALIZATIONS, Method::GET, &path, &[], None)
                    .await,
            )
        }
    }))
    .await?;
    let mut charts = Vec::new();
    for (id, reply) in replies {
        match reply {
            Ok(value) => charts.extend(chart_from(&value, &owners)),
            Err(err) if is_hidden(&err) => report.notes.push(format!(
                "visualization {id} is not visible to you; not checked"
            )),
            Err(err) => return Err(err.context(format!("cannot read visualization {id}"))),
        }
    }
    let found = charts_on(&report.dependents, &charts);
    report.dependents.extend(found);
    Ok(())
}

/// `GET /visualization` (visualizationUtil.js:13): the list carries who
/// made each one; the single GET does not.
async fn chart_owners(http: &ApiClient) -> Result<BTreeMap<i64, String>> {
    let list = http
        .request_json(VISUALIZATIONS, Method::GET, "/visualization", &[], None)
        .await
        .context("cannot list visualizations")?;
    Ok(list
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| Some((v["Id"].as_i64()?, v["created_by"].as_str()?.to_string())))
        .collect())
}

/// The Designer keeps a visualization's dataset id in
/// `config.chartConfig.datasetName`, as a number or a string.
fn chart_from(value: &Value, owners: &BTreeMap<i64, String>) -> Option<Chart> {
    let id = value["Id"].as_i64()?;
    let dataset = &value["config"]["chartConfig"]["datasetName"];
    Some(Chart {
        id,
        name: value["Name"].as_str().unwrap_or_default().to_string(),
        dataset: dataset
            .as_i64()
            .or_else(|| dataset.as_str().and_then(|s| s.trim().parse().ok())),
        owner: owners.get(&id).cloned(),
    })
}

fn charts_on(dependents: &[Dependent], charts: &[Chart]) -> Vec<Dependent> {
    let datasets: BTreeMap<String, &Dependent> = dependents
        .iter()
        .filter(|d| d.kind == Kind::Dataset)
        .map(|d| (d.id.clone(), d))
        .collect();
    charts
        .iter()
        .filter_map(|chart| {
            let dataset = datasets.get(&chart.dataset?.to_string())?;
            Some(Dependent {
                kind: Kind::Visualization,
                id: chart.id.to_string(),
                name: chart.name.clone(),
                owner: chart.owner.clone(),
                depth: dataset.depth,
                root: dataset.root.clone(),
                via: Via::Through {
                    what: Kind::Dataset.singular(),
                    name: dataset.name.clone(),
                },
                columns: dataset.columns.clone(),
                asset_type: None,
                written_by: Vec::new(),
                visualization_ids: Vec::new(),
            })
        })
        .collect()
}

// ---------- agents ----------

/// An ontology definition grounded on an affected lineage asset.
#[derive(Debug, Clone, PartialEq)]
pub struct Grounding {
    pub model_id: i64,
    pub model_name: String,
    pub asset: Option<AssetId>,
    pub root: String,
}

/// Agents read data through their tools at run time, with arguments the
/// model picks, so an agent's saved config names no tables or datasets.
/// The one link it keeps is the ontology models it is grounded on
/// (`metadata.groundzeroRuntime.ontologyModelIds`), and lineage knows which
/// models have definitions grounded on affected tables.
async fn add_agents(
    http: &ApiClient,
    roots: &[Root],
    depth: u32,
    report: &mut Report,
) -> Result<()> {
    let groundings = groundings(http, roots, depth).await?;
    if groundings.is_empty() {
        return Ok(());
    }
    // Only models grounded on what the walk reached, within --depth.
    let reached: BTreeSet<AssetId> = roots
        .iter()
        .map(|r| r.asset)
        .chain(
            report
                .dependents
                .iter()
                .filter(|d| d.kind == Kind::Table)
                .filter_map(|d| d.id.parse().ok()),
        )
        .collect();
    let groundings: Vec<Grounding> = groundings
        .into_iter()
        .filter(|g| g.asset.is_none_or(|a| reached.contains(&a)))
        .collect();
    if groundings.is_empty() {
        return Ok(());
    }
    let agents = all_agents(http).await?;
    let found = agents_on(&agents, &groundings, &report.dependents);
    report.dependents.extend(found);
    Ok(())
}

/// `GET /etl/lineage/assets/{id}/impact` (app.py:2396; lineageApi.js:201):
/// its `definitions` are the ontology definitions grounded on the asset or
/// anything downstream of it.
async fn groundings(http: &ApiClient, roots: &[Root], depth: u32) -> Result<Vec<Grounding>> {
    let depth = depth.min(MAX_IMPACT_DEPTH).to_string();
    let replies = parallel(roots.iter().map(|root| {
        let http = http.clone();
        let query = vec![
            ("depth".to_string(), depth.clone()),
            ("assetLimit".to_string(), "1".to_string()),
            ("definitionLimit".to_string(), DEFINITION_LIMIT.to_string()),
        ];
        let (asset, target) = (root.asset, root.target.clone());
        async move {
            let path = format!("/etl/lineage/assets/{asset}/impact");
            let reply = http
                .request_json(ETL, Method::GET, &path, &query, None)
                .await;
            (target, reply)
        }
    }))
    .await?;
    let mut found = Vec::new();
    for (target, reply) in replies {
        match reply {
            Ok(value) => found.extend(groundings_from(&value, &target)),
            Err(err) if is_hidden(&err) => {}
            Err(err) => return Err(err.context("cannot read the ontology models on the lineage")),
        }
    }
    Ok(found)
}

fn groundings_from(reply: &Value, root: &str) -> Vec<Grounding> {
    reply["definitions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| {
            Some(Grounding {
                model_id: d["modelId"].as_i64()?,
                model_name: d["modelName"].as_str().unwrap_or_default().to_string(),
                asset: d["groundedAssetId"].as_i64(),
                root: root.to_string(),
            })
        })
        .collect()
}

/// `GET /v1/agents?limit&offset` (agent builder api.py:300; the web app
/// pages it the same way, src/pages/Utils/agentUtils.js:376). Items are
/// full definitions.
async fn all_agents(http: &ApiClient) -> Result<Vec<Value>> {
    let mut agents = Vec::new();
    loop {
        let query = [
            ("limit".to_string(), AGENT_PAGE.to_string()),
            ("offset".to_string(), agents.len().to_string()),
        ];
        let page = http
            .request_json(AGENTS, Method::GET, "/v1/agents", &query, None)
            .await
            .context("cannot list agents")?;
        let items = page["items"].as_array().cloned().unwrap_or_default();
        let last = items.len() < AGENT_PAGE;
        agents.extend(items);
        if last {
            return Ok(agents);
        }
    }
}

fn agents_on(
    agents: &[Value],
    groundings: &[Grounding],
    dependents: &[Dependent],
) -> Vec<Dependent> {
    let mut found: Vec<Dependent> = Vec::new();
    for agent in agents {
        let models: BTreeSet<i64> = agent["metadata"]["groundzeroRuntime"]["ontologyModelIds"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_i64)
            .collect();
        let Some(grounding) = groundings.iter().find(|g| models.contains(&g.model_id)) else {
            continue;
        };
        let Some(id) = agent["id"].as_str() else {
            continue;
        };
        // As deep as the table the model is grounded on.
        let depth = grounding
            .asset
            .and_then(|asset| {
                dependents
                    .iter()
                    .find(|d| d.kind == Kind::Table && d.id == asset.to_string())
            })
            .map_or(1, |table| table.depth);
        found.push(Dependent {
            kind: Kind::Agent,
            id: id.to_string(),
            name: agent["name"].as_str().unwrap_or_default().to_string(),
            owner: agent["createdBy"].as_str().map(str::to_string),
            depth,
            root: grounding.root.clone(),
            via: Via::Through {
                what: "ontology model",
                name: grounding.model_name.clone(),
            },
            columns: crate::impact::ColumnUse::NotAsked,
            asset_type: None,
            written_by: Vec::new(),
            visualization_ids: Vec::new(),
        });
    }
    found
}

// ---------- jobs ----------

/// The tables a job writes, from its newest compiled version's lineage;
/// `None` when lineage has never compiled the job.
pub async fn job_outputs(http: &ApiClient, job: i64) -> Result<Option<Vec<(AssetId, String)>>> {
    let query = [
        ("limit".to_string(), "1".to_string()),
        ("runLimit".to_string(), "1".to_string()),
        ("eventLimit".to_string(), "1".to_string()),
    ];
    let path = format!("/etl/lineage/jobs/{job}");
    let reply = http
        .request_json(ETL, Method::GET, &path, &query, None)
        .await
        .with_context(|| format!("cannot read the lineage of job {job}"))?;
    Ok(outputs_of(&reply))
}

/// `GET /etl/lineage/jobs/{id}` (app.py:2121; lineageApi.js:131): the
/// targets of the version's relations, datasets left out.
fn outputs_of(reply: &Value) -> Option<Vec<(AssetId, String)>> {
    let graph = reply.get("graph").filter(|g| g.is_object())?;
    let assets: BTreeMap<i64, &Value> = graph["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| Some((a["assetId"].as_i64()?, a)))
        .collect();
    let targets: BTreeSet<i64> = graph["relations"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r["targetAssetId"].as_i64())
        .collect();
    Some(
        targets
            .into_iter()
            .filter_map(|id| {
                let asset = assets.get(&id)?;
                if asset["assetType"] == "DATASET" {
                    return None;
                }
                let name = asset["qualifiedName"]
                    .as_str()
                    .or_else(|| asset["displayName"].as_str())?;
                Some((id, name.replace('"', "")))
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::impact::ColumnUse;
    use serde_json::json;

    fn dataset(id: &str, name: &str, depth: u32) -> Dependent {
        Dependent {
            kind: Kind::Dataset,
            id: id.into(),
            name: name.into(),
            owner: None,
            depth,
            root: "db.t".into(),
            via: Via::Direct,
            columns: ColumnUse::NotAsked,
            asset_type: None,
            written_by: vec![],
            visualization_ids: vec![],
        }
    }

    #[test]
    fn a_visualization_counts_when_it_charts_an_affected_dataset() {
        let owners = BTreeMap::from([(1783, "prasath".to_string())]);
        let charts: Vec<Chart> = [
            json!({"Id": 1783, "Name": "Top customers", "config": {"chartConfig": {"datasetName": 1960}}}),
            json!({"Id": 1786, "Name": "Other", "config": {"chartConfig": {"datasetName": "42"}}}),
        ]
        .iter()
        .filter_map(|v| chart_from(v, &owners))
        .collect();
        assert_eq!(charts[1].dataset, Some(42), "string ids too");
        let found = charts_on(&[dataset("1960", "AR-InvoiceStatus", 2)], &charts);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "Top customers");
        assert_eq!(found[0].owner.as_deref(), Some("prasath"));
        assert_eq!(found[0].depth, 2);
        assert_eq!(
            found[0].via,
            Via::Through {
                what: "dataset",
                name: "AR-InvoiceStatus".into()
            }
        );
    }

    #[test]
    fn an_agent_counts_when_grounded_on_an_affected_model() {
        let groundings = groundings_from(
            &json!({"definitions": [{"modelId": 7, "modelName": "Policies", "groundedAssetId": 2434}]}),
            "db.t",
        );
        let agents = [
            json!({"id": "a-1", "name": "Persistency", "createdBy": "sagatest",
                   "metadata": {"groundzeroRuntime": {"ontologyModelIds": [3, 7]}}}),
            json!({"id": "a-2", "name": "Other", "metadata": {"groundzeroRuntime": {"ontologyModelIds": [3]}}}),
            json!({"id": "a-3", "name": "No runtime", "metadata": {}}),
        ];
        let mut table = dataset("2434", "db.curated", 2);
        table.kind = Kind::Table;
        let found = agents_on(&agents, &groundings, &[table]);
        assert_eq!(found.len(), 1);
        assert_eq!(
            (found[0].id.as_str(), found[0].depth),
            ("a-1", 2),
            "as deep as the grounded table"
        );
        assert_eq!(found[0].via.describe(), "via ontology model Policies");
    }

    #[test]
    fn a_job_writes_the_targets_of_its_relations() {
        let reply = json!({"graph": {
            "assets": [
                {"assetId": 1, "assetType": "TABLE", "qualifiedName": "db.raw"},
                {"assetId": 2, "assetType": "TABLE", "qualifiedName": "\"db\".\"clean\""},
                {"assetId": 3, "assetType": "DATASET", "qualifiedName": "dataset:9"},
            ],
            "relations": [
                {"sourceAssetId": 1, "targetAssetId": 2, "sourceColumn": "a", "targetColumn": "a"},
                {"sourceAssetId": 1, "targetAssetId": 2},
                {"sourceAssetId": 2, "targetAssetId": 3},
            ],
        }});
        assert_eq!(outputs_of(&reply), Some(vec![(2, "db.clean".to_string())]));
        assert_eq!(outputs_of(&json!({"graph": null})), None, "never compiled");
    }

    #[test]
    fn table_names_match_with_or_without_quotes_and_case() {
        assert_eq!(
            normalized("\"demo_sales\".\"Orders\""),
            normalized("DEMO_SALES.orders")
        );
    }
}
