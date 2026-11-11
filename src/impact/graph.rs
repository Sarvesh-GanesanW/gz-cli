//! One lineage asset's downstream neighbourhood, as the etl provider's asset
//! graph returns it: `GET /etl/lineage/assets/{id}` with `upstreamDepth=0`
//! and `downstreamDepth=1` (groundzero-etl-provider-api app.py:2323, the
//! catalog's lineage page calls it from ground-zero
//! src/pages/apiCatalog/lineageApi.js:161).
//!
//! Only the parts impact needs are kept: the assets, the relations leaving
//! the centre (table-level `DATA_FLOW` rows and per-column `COLUMN_LINEAGE`
//! rows), the job and dataset versions that read or write, and the
//! dashboards built on the datasets in view.

use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::BTreeMap;

pub type AssetId = i64;

/// The asset type lineage gives Designer datasets (`qualifiedName`
/// `dataset:{id}`).
const DATASET_TYPE: &str = "DATASET";
const DATASET_PREFIX: &str = "dataset:";
/// `assetWorkloads[].role` of a workload that reads the asset.
const READER_ROLE: &str = "SOURCE";

#[derive(Debug, Clone, PartialEq)]
pub struct Asset {
    pub id: AssetId,
    /// `TABLE`, `DATASET`, `OBJECT` (a file), …
    pub asset_type: String,
    pub name: String,
    /// `db.table` for tables, `dataset:{id}` for datasets.
    pub qualified_name: String,
    pub owner: Option<String>,
    /// Retired: lineage remembers it, but it no longer exists.
    pub is_deleted: bool,
}

impl Asset {
    pub fn is_dataset(&self) -> bool {
        self.asset_type.eq_ignore_ascii_case(DATASET_TYPE)
    }
}

/// One observed flow from `source` to `target`; the columns are set on
/// column-level rows only.
#[derive(Debug, Clone, PartialEq)]
pub struct Relation {
    pub source: AssetId,
    pub target: AssetId,
    pub source_column: Option<String>,
    pub target_column: Option<String>,
    /// The job or dataset version that made it (`jobVersionId`).
    pub workload: Option<i64>,
}

/// A version of a job or of a dataset's query.
#[derive(Debug, Clone, PartialEq)]
pub struct Workload {
    pub job_id: Option<i64>,
    pub dataset_id: Option<i64>,
    pub name: String,
    pub owner: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DashboardRef {
    pub id: i64,
    pub name: String,
    pub owner: Option<String>,
    pub dataset_ids: Vec<i64>,
    pub visualization_ids: Vec<i64>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct AssetView {
    pub center: AssetId,
    pub assets: BTreeMap<AssetId, Asset>,
    pub relations: Vec<Relation>,
    /// By `jobVersionId`.
    pub workloads: BTreeMap<i64, Workload>,
    /// Versions that read the centre, whether or not they wrote anything
    /// lineage could tie to it.
    pub readers: Vec<i64>,
    pub dashboards: Vec<DashboardRef>,
}

impl AssetView {
    /// Reads the `graph` object of the asset graph reply.
    pub fn from_reply(reply: &Value) -> Result<Self> {
        let graph = reply
            .get("graph")
            .filter(|g| g.is_object())
            .context("the lineage reply has no graph")?;
        let center = graph["centerAssetId"]
            .as_i64()
            .context("the lineage graph has no centre asset")?;
        let assets = items(graph, "assets")
            .filter_map(asset)
            .map(|a| (a.id, a))
            .collect();
        let relations = items(graph, "relations").filter_map(relation).collect();
        let workloads = items(graph, "jobVersions")
            .filter_map(|v| Some((v["jobVersionId"].as_i64()?, workload(v))))
            .collect();
        let readers = items(graph, "assetWorkloads")
            .filter(|w| w["assetId"].as_i64() == Some(center) && w["role"] == READER_ROLE)
            .filter_map(|w| w["jobVersionId"].as_i64())
            .collect();
        let dashboards = items(graph, "dashboards").filter_map(dashboard).collect();
        Ok(Self {
            center,
            assets,
            relations,
            workloads,
            readers,
            dashboards,
        })
    }

    /// The Designer dataset id an asset stands for, if it is one.
    pub fn dataset_id(&self, asset: &Asset, relation_workload: Option<i64>) -> Option<i64> {
        dataset_id_of(asset).or_else(|| {
            relation_workload
                .and_then(|v| self.workloads.get(&v))
                .and_then(|w| w.dataset_id)
        })
    }
}

fn items<'a>(graph: &'a Value, field: &str) -> impl Iterator<Item = &'a Value> {
    graph[field].as_array().into_iter().flatten()
}

fn text(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn asset(value: &Value) -> Option<Asset> {
    Some(Asset {
        id: value["assetId"].as_i64()?,
        asset_type: text(&value["assetType"]).unwrap_or_default(),
        // A masked asset (no access) has neither name.
        name: text(&value["displayName"])
            .or_else(|| text(&value["qualifiedName"]))
            .unwrap_or_else(|| "(hidden)".to_string()),
        qualified_name: text(&value["qualifiedName"]).unwrap_or_default(),
        owner: text(&value["owner"]),
        is_deleted: value["isDeleted"] == true,
    })
}

/// Lineage keeps the dataset's name for display and its id in the
/// qualified name; the display name is not an id.
fn dataset_id_of(asset: &Asset) -> Option<i64> {
    if !asset.is_dataset() {
        return None;
    }
    asset
        .qualified_name
        .strip_prefix(DATASET_PREFIX)?
        .parse()
        .ok()
}

fn relation(value: &Value) -> Option<Relation> {
    Some(Relation {
        source: value["sourceAssetId"].as_i64()?,
        target: value["targetAssetId"].as_i64()?,
        source_column: text(&value["sourceColumn"]),
        target_column: text(&value["targetColumn"]),
        workload: value["jobVersionId"].as_i64(),
    })
}

fn workload(value: &Value) -> Workload {
    Workload {
        job_id: value["jobId"].as_i64(),
        dataset_id: value["datasetId"].as_i64(),
        name: text(&value["jobName"])
            .or_else(|| text(&value["datasetName"]))
            .unwrap_or_else(|| "(hidden)".to_string()),
        owner: text(&value["owner"]),
    }
}

fn ids(value: &Value) -> Vec<i64> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_i64)
        .collect()
}

fn dashboard(value: &Value) -> Option<DashboardRef> {
    Some(DashboardRef {
        id: value["dashboardId"].as_i64()?,
        name: text(&value["name"]).unwrap_or_else(|| "(hidden)".to_string()),
        owner: text(&value["owner"]),
        dataset_ids: ids(&value["datasetIds"]),
        visualization_ids: ids(&value["visualizationIds"]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_the_asset_graph_reply() {
        let reply = json!({"graph": {
            "centerAssetId": 1468,
            "assets": [
                {"assetId": 1468, "assetType": "TABLE", "displayName": "AR_REPORT", "owner": "srijith"},
                {"assetId": 1469, "assetType": "DATASET", "displayName": "AR-TOP10Customer", "qualifiedName": "dataset:1957", "owner": "prasath"},
            ],
            "relations": [
                {"sourceAssetId": 1468, "targetAssetId": 1469, "sourceColumn": null,
                 "targetColumn": null, "relationType": "DATA_FLOW", "jobVersionId": 1025},
            ],
            "jobVersions": [
                {"jobVersionId": 1025, "jobId": null, "datasetId": 1957, "jobName": "AR-TOP10Customer", "owner": "prasath"},
                {"jobVersionId": 7, "jobId": 70, "datasetId": null, "jobName": "load", "owner": null},
            ],
            "assetWorkloads": [
                {"assetId": 1468, "jobVersionId": 7, "role": "SOURCE"},
                {"assetId": 1468, "jobVersionId": 8, "role": "TARGET"},
            ],
            "dashboards": [{"dashboardId": 1159, "name": "AR-REPORT", "owner": "prasath",
                            "datasetIds": [1957], "visualizationIds": [1783]}],
        }});
        let view = AssetView::from_reply(&reply).unwrap();
        assert_eq!(view.center, 1468);
        let dataset = &view.assets[&1469];
        assert_eq!(view.dataset_id(dataset, None), Some(1957));
        assert_eq!(dataset.name, "AR-TOP10Customer");
        assert_eq!(view.relations[0].workload, Some(1025));
        assert_eq!(view.relations[0].source_column, None);
        assert_eq!(view.workloads[&7].job_id, Some(70));
        assert_eq!(view.readers, vec![7], "only readers of the centre");
        assert_eq!(view.dashboards[0].visualization_ids, vec![1783]);
        assert!(AssetView::from_reply(&json!({"message": "x"})).is_err());
    }
}
