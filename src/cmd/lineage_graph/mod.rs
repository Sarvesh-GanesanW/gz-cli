//! `gz jobs lineage graph`: an asset's upstream and downstream, drawn in the
//! terminal or as Mermaid or DOT.
//!
//! One read of `GET /etl/lineage/assets/{id}` with `upstreamDepth` and
//! `downstreamDepth` (groundzero-etl-provider-api app.py
//! `etlAssetLineageHandler` and utils/lineage_graph_reads.py `parse_walk`;
//! the web app sends the same parameters in pages/apiCatalog/lineageApi.js
//! `getAssetLineage`). A name is looked up with `GET /etl/lineage/assets`
//! on each connection `GET /etl/lineage/connections` lists (app.py
//! `etlLineageAssetCatalogHandler`; lineageApi.js
//! `getConnectionLineageAssets`).

mod view;

use anyhow::{bail, Context, Result};
use reqwest::Method;
use serde_json::Value;

use crate::cli::{LineageGraphArgs, LineageGraphFormat};
use crate::cmd::Runtime;
use crate::output::OutputFormat;
use view::{Direction, Graph, Limits};

const SVC: &str = "etl";
const ASSETS: &str = "/etl/lineage/assets";
/// Most catalog matches read per connection when looking a name up.
const LOOKUP_LIMIT: &str = "100";
/// The graph route pages history, runs and events; the drawing needs none.
const NO_HISTORY: [(&str, &str); 3] = [
    ("historyLimit", "1"),
    ("runLimit", "1"),
    ("eventLimit", "1"),
];

/// The directions asked for: both unless one was picked.
fn directions(args: &LineageGraphArgs) -> Vec<Direction> {
    match (args.upstream, args.downstream) {
        (true, false) => vec![Direction::Upstream],
        (false, true) => vec![Direction::Downstream],
        _ => vec![Direction::Upstream, Direction::Downstream],
    }
}

fn graph_query(args: &LineageGraphArgs) -> Vec<(String, String)> {
    let depth_of = |direction| {
        if directions(args).contains(&direction) {
            args.depth.to_string()
        } else {
            "0".to_string()
        }
    };
    let mut query = vec![
        ("upstreamDepth".to_string(), depth_of(Direction::Upstream)),
        (
            "downstreamDepth".to_string(),
            depth_of(Direction::Downstream),
        ),
    ];
    query.extend(
        NO_HISTORY
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string())),
    );
    query
}

/// A catalog asset whose name is `name`, as `(id, description)`.
fn matches(catalog: &Value, name: &str) -> Vec<(u64, String)> {
    catalog["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|asset| {
            ["qualifiedName", "displayName"].iter().any(|key| {
                asset[*key]
                    .as_str()
                    .is_some_and(|n| n.eq_ignore_ascii_case(name))
            })
        })
        .filter_map(|asset| {
            let id = asset["assetId"].as_u64()?;
            let connection = asset["bindings"][0]["connectionName"]
                .as_str()
                .unwrap_or("?");
            Some((
                id,
                format!(
                    "{}  {} {id} on {connection}",
                    asset["displayName"].as_str().unwrap_or(name),
                    asset["assetType"].as_str().unwrap_or("ASSET")
                ),
            ))
        })
        .collect()
}

/// The asset id `text` names: an id as it is, else a name looked up on
/// every connection with lineage.
async fn resolve(rt: &Runtime, text: &str) -> Result<u64> {
    let text = text.trim();
    if let Ok(id) = text.parse::<u64>() {
        return Ok(id);
    }
    let connections = rt
        .http
        .request_json(SVC, Method::GET, "/etl/lineage/connections", &[], None)
        .await
        .context("cannot list the connections that have lineage")?;
    let mut found: Vec<(u64, String)> = Vec::new();
    for connection in connections["connections"].as_array().into_iter().flatten() {
        let Some(id) = connection["connectionId"].as_u64() else {
            continue;
        };
        let query = [
            ("connectionId".to_string(), id.to_string()),
            ("query".to_string(), text.to_string()),
            ("limit".to_string(), LOOKUP_LIMIT.to_string()),
        ];
        let catalog = rt
            .http
            .request_json(SVC, Method::GET, ASSETS, &query, None)
            .await
            .with_context(|| format!("cannot search connection {id} for {text}"))?;
        for (asset, description) in matches(&catalog, text) {
            if !found.iter().any(|(seen, _)| *seen == asset) {
                found.push((asset, description));
            }
        }
    }
    match found.as_slice() {
        [] => bail!("no lineage asset is named {text}; `gz jobs lineage export` lists assets with their ids"),
        [(id, _)] => Ok(*id),
        many => bail!(
            "{} lineage assets are named {text}; pass the id of one:\n  {}",
            many.len(),
            many.iter()
                .map(|(_, d)| d.as_str())
                .collect::<Vec<_>>()
                .join("\n  ")
        ),
    }
}

pub async fn run(rt: &Runtime, args: &LineageGraphArgs) -> Result<()> {
    let id = resolve(rt, &args.asset).await?;
    let reply = rt
        .http
        .request_json(
            SVC,
            Method::GET,
            &format!("{ASSETS}/{id}"),
            &graph_query(args),
            None,
        )
        .await
        .with_context(|| format!("cannot read the lineage of asset {id}"))?;
    let graph = Graph::parse(&reply)?;
    let limits = Limits {
        depth: args.depth,
        fan_out: args.fan_out as usize,
    };
    let sides: Vec<_> = directions(args)
        .into_iter()
        .map(|direction| view::side(&graph, direction, &limits))
        .collect();
    if rt.format != OutputFormat::Table {
        return rt.show(view::to_json(&graph, &sides)).await;
    }
    let drawing = match args.format {
        LineageGraphFormat::Text => view::text(&graph, &sides),
        LineageGraphFormat::Mermaid => view::mermaid(&graph, &sides),
        LineageGraphFormat::Dot => view::dot(&graph, &sides),
    };
    print!("{drawing}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn args(upstream: bool, downstream: bool) -> LineageGraphArgs {
        LineageGraphArgs {
            asset: "1".into(),
            depth: 3,
            upstream,
            downstream,
            format: LineageGraphFormat::Text,
            fan_out: 10,
        }
    }

    #[test]
    fn one_direction_walks_zero_hops_the_other_way() {
        let query = graph_query(&args(true, false));
        assert_eq!(query[0], ("upstreamDepth".into(), "3".into()));
        assert_eq!(query[1], ("downstreamDepth".into(), "0".into()));
        let both = graph_query(&args(false, false));
        assert_eq!((both[0].1.as_str(), both[1].1.as_str()), ("3", "3"));
    }

    #[test]
    fn a_name_matches_the_qualified_or_display_name_exactly() {
        let catalog = json!({"assets": [
            {"assetId": 7, "assetType": "TABLE", "displayName": "sales.orders", "qualifiedName": "sales.orders",
             "bindings": [{"connectionName": "Lakehouse"}]},
            {"assetId": 8, "assetType": "TABLE", "displayName": "sales.orders_old", "qualifiedName": "sales.orders_old"},
        ]});
        assert_eq!(
            matches(&catalog, "SALES.ORDERS"),
            vec![(7, "sales.orders  TABLE 7 on Lakehouse".to_string())]
        );
    }
}
