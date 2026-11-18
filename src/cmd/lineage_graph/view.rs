//! The lineage graph as gz draws it: the asset graph the ETL provider
//! returns, walked from its centre into a tree per direction, with repeats,
//! cycles and wide fan-out collapsed, then drawn as text, Mermaid or DOT.
//!
//! The graph is `GET /etl/lineage/assets/{id}` → `{graph: {centerAssetId,
//! assets, relations, jobVersions, dashboards, maskedSummary}}`
//! (groundzero-etl-provider-api app.py `etlAssetLineageHandler`,
//! utils/lineage_graph_reads.py; the web app reads it in
//! pages/apiCatalog/lineageApi.js `getAssetLineage`).

use std::collections::{BTreeMap, BTreeSet, HashSet};

use anyhow::{Context, Result};
use serde_json::{json, Value};

/// A node: a lineage asset, or a dashboard reading a dataset asset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum NodeId {
    Asset(u64),
    Dashboard(u64),
}

impl NodeId {
    /// An id Mermaid and DOT accept as it is.
    fn key(self) -> String {
        match self {
            NodeId::Asset(id) => format!("a{id}"),
            NodeId::Dashboard(id) => format!("d{id}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub id: NodeId,
    /// `TABLE`, `DATASET`, `OBJECT`, `DASHBOARD`, …
    pub kind: String,
    pub name: String,
}

impl Node {
    /// `orders  TABLE 3601`.
    fn label(&self) -> String {
        match self.id {
            NodeId::Asset(id) => format!("{}  {} {id}", self.name, self.kind),
            NodeId::Dashboard(id) => format!("{}  dashboard {id}", self.name),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Upstream,
    Downstream,
}

impl Direction {
    fn word(self) -> &'static str {
        match self {
            Direction::Upstream => "upstream",
            Direction::Downstream => "downstream",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Graph {
    pub center: NodeId,
    pub nodes: BTreeMap<NodeId, Node>,
    /// One per (from, to), with every job or dataset that moves the data.
    pub edges: BTreeMap<(NodeId, NodeId), BTreeSet<String>>,
    /// Assets the caller may not see, as the server counted them.
    pub hidden_assets: u64,
}

fn id_of(value: &Value) -> Option<u64> {
    value.as_u64()
}

/// Workload names by job version id, labelled as the TUI's lineage view
/// labels them: `NightlyLoad (job)`, `Sales (dataset)`.
fn workloads(graph: &Value) -> BTreeMap<u64, String> {
    ["jobVersions", "historicalJobVersions"]
        .iter()
        .flat_map(|key| graph[*key].as_array().into_iter().flatten())
        .filter_map(|version| {
            let name = version["jobName"].as_str()?;
            let label = match version["workloadType"].as_str() {
                Some(kind) => format!("{name} ({kind})"),
                None => name.to_string(),
            };
            Some((id_of(&version["jobVersionId"])?, label))
        })
        .collect()
}

impl Graph {
    pub fn parse(response: &Value) -> Result<Self> {
        let graph = &response["graph"];
        let center = id_of(&graph["centerAssetId"])
            .context("the lineage graph came without its centre asset")?;
        let mut nodes = BTreeMap::new();
        let mut datasets = BTreeMap::new();
        for asset in graph["assets"].as_array().into_iter().flatten() {
            let Some(id) = id_of(&asset["assetId"]) else {
                continue;
            };
            let qualified = asset["qualifiedName"].as_str().unwrap_or_default();
            if let Some(dataset) = qualified.strip_prefix("dataset:") {
                if let Ok(dataset) = dataset.parse::<u64>() {
                    datasets.insert(dataset, NodeId::Asset(id));
                }
            }
            let name = asset["displayName"]
                .as_str()
                .filter(|n| !n.is_empty())
                .unwrap_or(qualified);
            nodes.insert(
                NodeId::Asset(id),
                Node {
                    id: NodeId::Asset(id),
                    kind: asset["assetType"].as_str().unwrap_or("ASSET").to_string(),
                    name: name.to_string(),
                },
            );
        }
        let names = workloads(graph);
        let mut edges: BTreeMap<(NodeId, NodeId), BTreeSet<String>> = BTreeMap::new();
        for relation in graph["relations"].as_array().into_iter().flatten() {
            let (Some(from), Some(to)) = (
                id_of(&relation["sourceAssetId"]),
                id_of(&relation["targetAssetId"]),
            ) else {
                continue;
            };
            let (from, to) = (NodeId::Asset(from), NodeId::Asset(to));
            // Masked assets are left out of `assets`; their edges go too.
            if from == to || !nodes.contains_key(&from) || !nodes.contains_key(&to) {
                continue;
            }
            let via = edges.entry((from, to)).or_default();
            if let Some(name) = id_of(&relation["jobVersionId"]).and_then(|v| names.get(&v)) {
                via.insert(name.clone());
            }
        }
        for dashboard in graph["dashboards"].as_array().into_iter().flatten() {
            let Some(id) = id_of(&dashboard["dashboardId"]) else {
                continue;
            };
            let read: Vec<NodeId> = dashboard["datasetIds"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|d| datasets.get(&id_of(d)?).copied())
                .collect();
            if read.is_empty() {
                continue;
            }
            let node = NodeId::Dashboard(id);
            nodes.insert(
                node,
                Node {
                    id: node,
                    kind: "DASHBOARD".to_string(),
                    name: dashboard["name"]
                        .as_str()
                        .unwrap_or("dashboard")
                        .to_string(),
                },
            );
            for dataset in read {
                edges.entry((dataset, node)).or_default();
            }
        }
        let center = NodeId::Asset(center);
        if !nodes.contains_key(&center) {
            anyhow::bail!("the lineage graph does not include its centre asset");
        }
        Ok(Self {
            center,
            nodes,
            edges,
            hidden_assets: graph["maskedSummary"]["assets"].as_u64().unwrap_or(0),
        })
    }

    /// The nodes one hop away in `direction`, by name, with what links them.
    fn neighbours(&self, id: NodeId, direction: Direction) -> Vec<(NodeId, BTreeSet<String>)> {
        let mut list: Vec<(NodeId, BTreeSet<String>)> = self
            .edges
            .iter()
            .filter_map(|((from, to), via)| match direction {
                Direction::Downstream if *from == id => Some((*to, via.clone())),
                Direction::Upstream if *to == id => Some((*from, via.clone())),
                _ => None,
            })
            .collect();
        list.sort_by(|a, b| self.nodes[&a.0].name.cmp(&self.nodes[&b.0].name));
        list
    }
}

/// What a tree entry shows below itself.
#[derive(Debug, Clone, PartialEq)]
pub enum Below {
    Expanded(Vec<Branch>),
    /// Drawn earlier in this direction; not repeated.
    SeenAbove,
    /// On the path from the centre to here: the data flows in a circle.
    Cycle,
}

/// Neighbours left out past `--fan-out`, counted by kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collapsed {
    pub count: usize,
    pub by_kind: BTreeMap<String, usize>,
}

impl Collapsed {
    fn text(&self) -> String {
        let kinds: Vec<String> = self
            .by_kind
            .iter()
            .map(|(kind, n)| format!("{kind} {n}"))
            .collect();
        format!("… {} more ({})", self.count, kinds.join(", "))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Branch {
    pub node: NodeId,
    pub via: BTreeSet<String>,
    pub below: Below,
    pub collapsed: Option<Collapsed>,
}

/// One direction from the centre.
#[derive(Debug, Clone, PartialEq)]
pub struct Side {
    pub direction: Direction,
    pub branches: Vec<Branch>,
    pub collapsed: Option<Collapsed>,
}

pub struct Limits {
    pub depth: u8,
    pub fan_out: usize,
}

struct Walk<'a> {
    graph: &'a Graph,
    direction: Direction,
    limits: &'a Limits,
    path: Vec<NodeId>,
    seen: HashSet<NodeId>,
}

impl Walk<'_> {
    fn below(&mut self, id: NodeId, depth_left: u8) -> (Vec<Branch>, Option<Collapsed>) {
        if depth_left == 0 {
            return (Vec::new(), None);
        }
        let mut neighbours = self.graph.neighbours(id, self.direction);
        let rest = neighbours.split_off(neighbours.len().min(self.limits.fan_out));
        let collapsed = (!rest.is_empty()).then(|| Collapsed {
            count: rest.len(),
            by_kind: rest.iter().fold(BTreeMap::new(), |mut counts, (node, _)| {
                *counts
                    .entry(self.graph.nodes[node].kind.clone())
                    .or_insert(0) += 1;
                counts
            }),
        });
        let branches = neighbours
            .into_iter()
            .map(|(node, via)| {
                let (below, collapsed) = if self.path.contains(&node) {
                    (Below::Cycle, None)
                } else if !self.seen.insert(node) {
                    (Below::SeenAbove, None)
                } else {
                    self.path.push(node);
                    let (branches, collapsed) = self.below(node, depth_left - 1);
                    self.path.pop();
                    (Below::Expanded(branches), collapsed)
                };
                Branch {
                    node,
                    via,
                    below,
                    collapsed,
                }
            })
            .collect();
        (branches, collapsed)
    }
}

/// The tree in one direction from the centre.
pub fn side(graph: &Graph, direction: Direction, limits: &Limits) -> Side {
    let mut walk = Walk {
        graph,
        direction,
        limits,
        path: vec![graph.center],
        seen: HashSet::from([graph.center]),
    };
    let (branches, collapsed) = walk.below(graph.center, limits.depth);
    Side {
        direction,
        branches,
        collapsed,
    }
}

// ---------- text ----------

fn via_text(via: &BTreeSet<String>) -> String {
    if via.is_empty() {
        return String::new();
    }
    let names: Vec<&str> = via.iter().map(String::as_str).collect();
    format!("  via {}", names.join(", "))
}

fn draw_branches(
    graph: &Graph,
    branches: &[Branch],
    collapsed: Option<&Collapsed>,
    prefix: &str,
    lines: &mut Vec<String>,
) {
    let total = branches.len() + usize::from(collapsed.is_some());
    for (i, branch) in branches.iter().enumerate() {
        let last = i + 1 == total;
        let (joint, indent) = if last {
            ("└── ", "    ")
        } else {
            ("├── ", "│   ")
        };
        let mark = match branch.below {
            Below::SeenAbove => "  (see above)",
            Below::Cycle => "  (cycle: already on this path)",
            Below::Expanded(_) => "",
        };
        lines.push(format!(
            "{prefix}{joint}{}{}{mark}",
            graph.nodes[&branch.node].label(),
            via_text(&branch.via)
        ));
        if let Below::Expanded(children) = &branch.below {
            draw_branches(
                graph,
                children,
                branch.collapsed.as_ref(),
                &format!("{prefix}{indent}"),
                lines,
            );
        }
    }
    if let Some(collapsed) = collapsed {
        lines.push(format!("{prefix}└── {}", collapsed.text()));
    }
}

pub fn text(graph: &Graph, sides: &[Side]) -> String {
    let mut lines = vec![graph.nodes[&graph.center].label()];
    for (i, side) in sides.iter().enumerate() {
        let last = i + 1 == sides.len();
        let (joint, indent) = if last {
            ("└── ", "    ")
        } else {
            ("├── ", "│   ")
        };
        let direct = side.branches.len() + side.collapsed.as_ref().map_or(0, |c| c.count);
        if direct == 0 {
            lines.push(format!("{joint}{}: none recorded", side.direction.word()));
            continue;
        }
        lines.push(format!("{joint}{} ({direct})", side.direction.word()));
        draw_branches(
            graph,
            &side.branches,
            side.collapsed.as_ref(),
            indent,
            &mut lines,
        );
    }
    if graph.hidden_assets > 0 {
        lines.push(format!(
            "{} asset(s) you cannot see are left out.",
            graph.hidden_assets
        ));
    }
    lines.join("\n") + "\n"
}

// ---------- Mermaid, DOT and JSON ----------

/// What the trees draw: the nodes, the edges and the collapsed counts.
#[derive(Debug, Default, PartialEq)]
struct Drawn {
    nodes: BTreeSet<NodeId>,
    /// (from, to, via), in data-flow order.
    edges: BTreeSet<(NodeId, NodeId, Vec<String>)>,
    /// (summary key, its text, the node it hangs off, direction).
    collapsed: Vec<(String, String, NodeId, Direction)>,
}

impl Drawn {
    fn of(graph: &Graph, sides: &[Side]) -> Self {
        let mut drawn = Drawn::default();
        drawn.nodes.insert(graph.center);
        for side in sides {
            drawn.add(
                graph.center,
                side.direction,
                &side.branches,
                side.collapsed.as_ref(),
            );
        }
        drawn
    }

    fn add(
        &mut self,
        parent: NodeId,
        direction: Direction,
        branches: &[Branch],
        collapsed: Option<&Collapsed>,
    ) {
        for branch in branches {
            self.nodes.insert(branch.node);
            let via = branch.via.iter().cloned().collect();
            let edge = match direction {
                Direction::Downstream => (parent, branch.node, via),
                Direction::Upstream => (branch.node, parent, via),
            };
            self.edges.insert(edge);
            if let Below::Expanded(children) = &branch.below {
                self.add(branch.node, direction, children, branch.collapsed.as_ref());
            }
        }
        if let Some(collapsed) = collapsed {
            let key = format!("more_{}_{}", parent.key(), direction.word());
            self.collapsed
                .push((key, collapsed.text().replace("… ", "+"), parent, direction));
        }
    }
}

fn mermaid_text(text: &str) -> String {
    text.replace('"', "#quot;")
}

pub fn mermaid(graph: &Graph, sides: &[Side]) -> String {
    let drawn = Drawn::of(graph, sides);
    let mut lines = vec!["flowchart LR".to_string()];
    for id in &drawn.nodes {
        let node = &graph.nodes[id];
        lines.push(format!(
            "  {}[\"{}<br/>{}\"]",
            id.key(),
            mermaid_text(&node.name),
            node.kind
        ));
    }
    for (key, text, _, _) in &drawn.collapsed {
        lines.push(format!("  {key}[\"{}\"]", mermaid_text(text)));
    }
    for (from, to, via) in &drawn.edges {
        let label = if via.is_empty() {
            String::new()
        } else {
            format!("|\"{}\"|", mermaid_text(&via.join(", ")))
        };
        lines.push(format!("  {} -->{label} {}", from.key(), to.key()));
    }
    for (key, _, node, direction) in &drawn.collapsed {
        let (from, to) = match direction {
            Direction::Downstream => (node.key(), key.clone()),
            Direction::Upstream => (key.clone(), node.key()),
        };
        lines.push(format!("  {from} -.-> {to}"));
    }
    lines.push(format!("  style {} stroke-width:3px", graph.center.key()));
    lines.join("\n") + "\n"
}

fn dot_text(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

pub fn dot(graph: &Graph, sides: &[Side]) -> String {
    let drawn = Drawn::of(graph, sides);
    let mut lines = vec![
        "digraph lineage {".to_string(),
        "  rankdir=LR;".to_string(),
        "  node [shape=box];".to_string(),
    ];
    for id in &drawn.nodes {
        let node = &graph.nodes[id];
        let bold = if *id == graph.center {
            ", penwidth=3"
        } else {
            ""
        };
        lines.push(format!(
            "  {} [label=\"{}\\n{}\"{bold}];",
            id.key(),
            dot_text(&node.name),
            node.kind
        ));
    }
    for (key, text, _, _) in &drawn.collapsed {
        lines.push(format!(
            "  {key} [label=\"{}\", style=dashed];",
            dot_text(text)
        ));
    }
    for (from, to, via) in &drawn.edges {
        let label = if via.is_empty() {
            String::new()
        } else {
            format!(" [label=\"{}\"]", dot_text(&via.join(", ")))
        };
        lines.push(format!("  {} -> {}{label};", from.key(), to.key()));
    }
    for (key, _, node, direction) in &drawn.collapsed {
        let (from, to) = match direction {
            Direction::Downstream => (node.key(), key.clone()),
            Direction::Upstream => (key.clone(), node.key()),
        };
        lines.push(format!("  {from} -> {to} [style=dashed];"));
    }
    lines.push("}".to_string());
    lines.join("\n") + "\n"
}

fn node_json(node: &Node) -> Value {
    match node.id {
        NodeId::Asset(id) => json!({"assetId": id, "type": node.kind, "name": node.name}),
        NodeId::Dashboard(id) => json!({"dashboardId": id, "type": node.kind, "name": node.name}),
    }
}

pub fn to_json(graph: &Graph, sides: &[Side]) -> Value {
    let drawn = Drawn::of(graph, sides);
    let reference = |id: &NodeId| match id {
        NodeId::Asset(id) => json!({"assetId": id}),
        NodeId::Dashboard(id) => json!({"dashboardId": id}),
    };
    json!({
        "center": node_json(&graph.nodes[&graph.center]),
        "nodes": drawn.nodes.iter().map(|id| node_json(&graph.nodes[id])).collect::<Vec<_>>(),
        "edges": drawn.edges.iter().map(|(from, to, via)| json!({
            "from": reference(from),
            "to": reference(to),
            "via": via,
        })).collect::<Vec<_>>(),
        "collapsed": drawn.collapsed.iter().map(|(_, text, node, direction)| json!({
            "at": reference(node),
            "direction": direction.word(),
            "summary": text,
        })).collect::<Vec<_>>(),
        "hiddenAssets": graph.hidden_assets,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(id: u64, kind: &str, name: &str) -> Value {
        json!({"assetId": id, "assetType": kind, "displayName": name, "qualifiedName": name})
    }

    fn flow(from: u64, to: u64, version: u64) -> Value {
        json!({"sourceAssetId": from, "targetAssetId": to, "jobVersionId": version, "relationType": "DATA_FLOW"})
    }

    /// raw → orders (centre) → report; orders → audit → orders is a cycle;
    /// report feeds dataset:77, which dashboard 5 reads.
    fn graph() -> Graph {
        Graph::parse(&json!({"graph": {
            "centerAssetId": 2,
            "assets": [
                asset(1, "TABLE", "raw"),
                asset(2, "TABLE", "orders"),
                asset(3, "TABLE", "report"),
                asset(4, "TABLE", "audit"),
                {"assetId": 6, "assetType": "DATASET", "displayName": "Sales", "qualifiedName": "dataset:77"},
            ],
            "relations": [
                flow(1, 2, 10),
                // A column-level relation of the same tables is one edge.
                {"sourceAssetId": 1, "targetAssetId": 2, "jobVersionId": 10, "relationType": "COLUMN_LINEAGE", "sourceColumn": "id"},
                flow(2, 3, 11),
                flow(2, 4, 12),
                flow(4, 2, 12),
                flow(3, 6, 13),
                // An asset the caller cannot see is not in `assets`.
                flow(99, 2, 14),
            ],
            "jobVersions": [
                {"jobVersionId": 10, "jobName": "load_orders"},
                {"jobVersionId": 11, "jobName": "build_report", "workloadType": "job"},
            ],
            "dashboards": [{"dashboardId": 5, "name": "Ops", "datasetIds": [77]}],
            "maskedSummary": {"assets": 1},
        }}))
        .unwrap()
    }

    fn limits(depth: u8, fan_out: usize) -> Limits {
        Limits { depth, fan_out }
    }

    #[test]
    fn edges_are_one_per_pair_and_hidden_assets_are_left_out() {
        let graph = graph();
        assert_eq!(graph.edges.len(), 6, "{:?}", graph.edges.keys());
        assert_eq!(
            graph.edges[&(NodeId::Asset(1), NodeId::Asset(2))],
            BTreeSet::from(["load_orders".to_string()])
        );
        assert!(graph
            .edges
            .contains_key(&(NodeId::Asset(6), NodeId::Dashboard(5))));
        assert_eq!(graph.hidden_assets, 1);
    }

    #[test]
    fn the_text_tree_marks_cycles_and_names_the_jobs() {
        let graph = graph();
        let sides = [
            side(&graph, Direction::Upstream, &limits(3, 10)),
            side(&graph, Direction::Downstream, &limits(3, 10)),
        ];
        let text = text(&graph, &sides);
        let expected = "\
orders  TABLE 2
├── upstream (2)
│   ├── audit  TABLE 4
│   │   └── orders  TABLE 2  (cycle: already on this path)
│   └── raw  TABLE 1  via load_orders
└── downstream (2)
    ├── audit  TABLE 4
    │   └── orders  TABLE 2  (cycle: already on this path)
    └── report  TABLE 3  via build_report (job)
        └── Sales  DATASET 6
            └── Ops  dashboard 5
1 asset(s) you cannot see are left out.
";
        assert_eq!(text, expected);
    }

    #[test]
    fn wide_fan_out_is_counted_by_kind_and_depth_stops_the_walk() {
        let graph = graph();
        let down = side(&graph, Direction::Downstream, &limits(1, 1));
        let text = text(&graph, std::slice::from_ref(&down));
        assert!(text.contains("└── downstream (2)"), "{text}");
        assert!(
            text.contains("├── audit  TABLE 4\n    └── … 1 more (TABLE 1)"),
            "{text}"
        );
        assert!(!text.contains("Sales"), "depth 1 stops at report: {text}");
    }

    #[test]
    fn a_node_reached_twice_is_drawn_once() {
        let graph = Graph::parse(&json!({"graph": {
            "centerAssetId": 1,
            "assets": [asset(1, "TABLE", "a"), asset(2, "TABLE", "b"), asset(3, "TABLE", "c"), asset(4, "TABLE", "d")],
            "relations": [flow(1, 2, 1), flow(1, 3, 1), flow(2, 4, 1), flow(3, 4, 1)],
        }}))
        .unwrap();
        let down = side(&graph, Direction::Downstream, &limits(3, 10));
        let text = text(&graph, &[down]);
        assert!(text.contains("│   └── d  TABLE 4\n"), "{text}");
        assert!(
            text.contains("        └── d  TABLE 4  (see above)"),
            "{text}"
        );
    }

    #[test]
    fn mermaid_and_dot_draw_the_same_edges() {
        let graph = graph();
        let sides = [
            side(&graph, Direction::Upstream, &limits(1, 1)),
            side(&graph, Direction::Downstream, &limits(1, 10)),
        ];
        let mermaid = mermaid(&graph, &sides);
        assert!(mermaid.starts_with("flowchart LR\n"), "{mermaid}");
        assert!(mermaid.contains("  a2[\"orders<br/>TABLE\"]"), "{mermaid}");
        assert!(
            mermaid.contains("  a2 -->|\"build_report (job)\"| a3"),
            "{mermaid}"
        );
        assert!(
            mermaid.contains("  more_a2_upstream[\"+1 more (TABLE 1)\"]"),
            "{mermaid}"
        );
        assert!(mermaid.contains("  more_a2_upstream -.-> a2"), "{mermaid}");

        let dot = dot(&graph, &sides);
        assert!(
            dot.contains("  a2 [label=\"orders\\nTABLE\", penwidth=3];"),
            "{dot}"
        );
        assert!(
            dot.contains("  a2 -> a3 [label=\"build_report (job)\"];"),
            "{dot}"
        );
        assert!(dot.ends_with("}\n"));

        let value = to_json(&graph, &sides);
        assert_eq!(value["center"]["assetId"], 2);
        assert_eq!(value["collapsed"][0]["summary"], "+1 more (TABLE 1)");
    }

    #[test]
    fn names_are_escaped_for_each_format() {
        assert_eq!(mermaid_text("a\"b"), "a#quot;b");
        assert_eq!(dot_text("a\"b\\c"), "a\\\"b\\\\c");
    }
}
