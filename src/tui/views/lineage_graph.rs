//! A lineage asset's graph as a walkable tree: what feeds it (upstream)
//! above, what it feeds (downstream) below, each edge labelled with the
//! workload that moves the data and whether it is table-level (DATA_FLOW)
//! or column-level (COLUMN_LINEAGE). Enter re-centres on a node.
//!
//! Contract (TUI research, "Lineage graph"):
//! `GET etl /etl/lineage/assets/{assetId}?depth&upstreamDepth&downstreamDepth&historyLimit&runLimit&eventLimit`
//! → `{graph: {centerAssetId, assets[], relations[], jobVersions[], runs[], maskedSummary}}`
//! (groundzero-etl-provider-api app.py:2290-2331; web lineageApi.js:161-180).
//! Relations collapse by (source, target, workload) into one flow, as the
//! web does (lineage/lineageFlows.js:77-200).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Alignment, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Padding, Paragraph},
    Frame,
};
use reqwest::Method;
use serde_json::Value;

use super::table_preview::TablePreview;
use super::{Ctx, Handled, Poller, RenderCtx, View};
use crate::cmd::seg;
use crate::tui::app::Tone;
use crate::tui::errors::{self, Subject};
use crate::tui::format::text_of;
use crate::tui::help::HelpSection;
use crate::tui::theme;
use crate::tui::ui::widgets::{fit, render_scrollbar};

/// The server allows 0..=5 hops each way.
const MAX_HOPS: u8 = 5;
/// Refetch this often while a run is queued or running, like the web.
const LIVE_EVERY: Duration = Duration::from_secs(10);
const ACTIVE_RUNS: &[&str] = &["QUEUED", "RUNNING"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Upstream,
    Center,
    Downstream,
}

#[derive(Debug, Clone)]
struct Node {
    name: String,
    kind: String,
    deleted: bool,
    raw: Value,
}

/// Every relation between two assets written by one workload.
#[derive(Debug, Clone, Default)]
struct Flow {
    source: i64,
    target: i64,
    workload: String,
    kinds: BTreeSet<String>,
    columns: Vec<(String, String)>,
    runtime: bool,
}

#[derive(Debug, Clone, Default)]
struct Graph {
    center: i64,
    nodes: HashMap<i64, Node>,
    flows: Vec<Flow>,
    active_runs: usize,
    hidden: u64,
}

fn as_id(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
}

/// The graph in an asset answer.
fn parse(answer: &Value) -> Graph {
    let graph = answer.get("graph").unwrap_or(answer);
    let nodes = graph["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|asset| {
            let id = as_id(&asset["assetId"])?;
            let name = ["displayName", "qualifiedName"]
                .iter()
                .find_map(|k| asset[k].as_str())
                .unwrap_or("?")
                .to_string();
            Some((
                id,
                Node {
                    name,
                    kind: asset["assetType"].as_str().unwrap_or("").to_string(),
                    deleted: asset["isDeleted"].as_bool().unwrap_or(false),
                    raw: asset.clone(),
                },
            ))
        })
        .collect();
    let workloads: HashMap<i64, String> = graph["jobVersions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| {
            let id = as_id(&v["jobVersionId"])?;
            let name = v["jobName"].as_str().unwrap_or("workload");
            let label = match v["workloadType"].as_str() {
                Some(kind) => format!("{name} ({kind})"),
                None => name.to_string(),
            };
            Some((id, label))
        })
        .collect();
    let mut flows: Vec<Flow> = Vec::new();
    for relation in graph["relations"].as_array().into_iter().flatten() {
        let (Some(source), Some(target)) = (
            as_id(&relation["sourceAssetId"]),
            as_id(&relation["targetAssetId"]),
        ) else {
            continue;
        };
        let version = as_id(&relation["jobVersionId"]);
        let workload = version
            .and_then(|v| workloads.get(&v).cloned())
            .unwrap_or_else(|| "unknown workload".to_string());
        let index = match flows
            .iter()
            .position(|f| f.source == source && f.target == target && f.workload == workload)
        {
            Some(index) => index,
            None => {
                flows.push(Flow {
                    source,
                    target,
                    workload,
                    ..Flow::default()
                });
                flows.len() - 1
            }
        };
        let flow = &mut flows[index];
        if let Some(kind) = relation["relationType"].as_str() {
            flow.kinds.insert(kind.to_string());
        }
        if let (Some(from), Some(to)) = (
            relation["sourceColumn"].as_str(),
            relation["targetColumn"].as_str(),
        ) {
            flow.columns.push((from.to_string(), to.to_string()));
        }
        let observed = relation["runIds"].as_array().is_some_and(|r| !r.is_empty())
            || relation["evidence"]["runtimeConfirmed"].as_bool() == Some(true);
        flow.runtime |= observed;
    }
    let active_runs = graph["runs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|run| {
            run["status"]
                .as_str()
                .is_some_and(|s| ACTIVE_RUNS.contains(&s))
        })
        .count();
    let hidden = ["assets", "jobVersions", "dashboards"]
        .iter()
        .filter_map(|k| graph["maskedSummary"][k].as_u64())
        .sum();
    Graph {
        center: as_id(&graph["centerAssetId"]).unwrap_or_default(),
        nodes,
        flows,
        active_runs,
        hidden,
    }
}

/// One line of the drawn tree.
#[derive(Debug, Clone)]
enum Row {
    Heading(&'static str),
    Node {
        id: i64,
        direction: Direction,
        /// Tree drawing before the node ("│  ├─▶ ").
        prefix: String,
        /// The flow that links it to its parent line.
        flow: Option<usize>,
        /// Already drawn elsewhere in this tree; not expanded again.
        repeated: bool,
    },
    Column {
        prefix: String,
        from: String,
        to: String,
    },
    Note(String),
}

/// Lays the graph out: upstream tree, the centre, downstream tree.
fn layout(graph: &Graph, show_columns: bool) -> Vec<Row> {
    let mut rows = Vec::new();
    let upstream = walk(graph, Direction::Upstream, show_columns);
    rows.push(Row::Heading("Upstream · feeds this asset"));
    if upstream.is_empty() {
        rows.push(Row::Note("Nothing upstream within these hops".into()));
    }
    rows.extend(upstream);
    rows.push(Row::Heading("This asset"));
    rows.push(Row::Node {
        id: graph.center,
        direction: Direction::Center,
        prefix: String::new(),
        flow: None,
        repeated: false,
    });
    let downstream = walk(graph, Direction::Downstream, show_columns);
    rows.push(Row::Heading("Downstream · fed by this asset"));
    if downstream.is_empty() {
        rows.push(Row::Note("Nothing downstream within these hops".into()));
    }
    rows.extend(downstream);
    rows
}

fn walk(graph: &Graph, direction: Direction, show_columns: bool) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut seen = HashSet::from([graph.center]);
    branch(
        graph,
        direction,
        graph.center,
        "",
        show_columns,
        &mut seen,
        &mut rows,
    );
    rows
}

/// The flows leaving `from` in `direction`, each with the node it reaches.
fn next_hops(graph: &Graph, direction: Direction, from: i64) -> Vec<(usize, i64)> {
    graph
        .flows
        .iter()
        .enumerate()
        .filter_map(|(index, flow)| match direction {
            Direction::Downstream if flow.source == from => Some((index, flow.target)),
            Direction::Upstream if flow.target == from => Some((index, flow.source)),
            _ => None,
        })
        .collect()
}

fn branch(
    graph: &Graph,
    direction: Direction,
    from: i64,
    indent: &str,
    show_columns: bool,
    seen: &mut HashSet<i64>,
    rows: &mut Vec<Row>,
) {
    let hops = next_hops(graph, direction, from);
    let arrow = match direction {
        Direction::Upstream => "◀ ",
        _ => "▶ ",
    };
    for (position, (flow, node)) in hops.iter().enumerate() {
        let last = position + 1 == hops.len();
        let elbow = if last { "└─" } else { "├─" };
        let repeated = !seen.insert(*node);
        rows.push(Row::Node {
            id: *node,
            direction,
            prefix: format!("{indent}{elbow}{arrow}"),
            flow: Some(*flow),
            repeated,
        });
        let child_indent = format!("{indent}{}", if last { "   " } else { "│  " });
        if show_columns {
            for (from_col, to_col) in &graph.flows[*flow].columns {
                rows.push(Row::Column {
                    prefix: format!("{child_indent}  "),
                    from: from_col.clone(),
                    to: to_col.clone(),
                });
            }
        }
        if !repeated {
            branch(
                graph,
                direction,
                *node,
                &child_indent,
                show_columns,
                seen,
                rows,
            );
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Load {
    Loading(u64),
    Ready,
    Failed(String),
}

pub struct LineageGraph {
    asset_id: i64,
    title: String,
    upstream: u8,
    downstream: u8,
    /// Centres visited before this one, for Backspace.
    history: Vec<(i64, String)>,
    load: Load,
    graph: Graph,
    rows: Vec<Row>,
    selected: usize,
    scroll: usize,
    viewport: usize,
    show_columns: bool,
    poller: Poller,
}

impl LineageGraph {
    pub fn new(asset_id: i64, title: impl Into<String>) -> Self {
        Self {
            asset_id,
            title: title.into(),
            upstream: 1,
            downstream: 1,
            history: Vec::new(),
            load: Load::Ready,
            graph: Graph::default(),
            rows: Vec::new(),
            selected: 0,
            scroll: 0,
            viewport: 1,
            show_columns: false,
            poller: Poller::every(LIVE_EVERY),
        }
    }

    fn path(&self) -> String {
        let depth = self.upstream.max(self.downstream);
        format!(
            "/etl/lineage/assets/{}?depth={depth}&upstreamDepth={}&downstreamDepth={}&historyLimit=20&runLimit=20&eventLimit=100",
            seg(&self.asset_id.to_string()),
            self.upstream,
            self.downstream
        )
    }

    fn fetch(&mut self, ctx: &mut Ctx) {
        let id = ctx.fetch_raw("etl", Method::GET, self.path(), None);
        self.load = Load::Loading(id);
        self.poller.restart(ctx.now.instant);
    }

    fn loaded(&mut self, answer: &Value) {
        let selected = self.selected_node();
        self.graph = parse(answer);
        if let Some(center) = self.graph.nodes.get(&self.graph.center) {
            self.title = center.name.clone();
        }
        self.relayout();
        // A refresh keeps the selection on the same node.
        let found = selected.and_then(|id| {
            self.rows
                .iter()
                .position(|r| matches!(r, Row::Node { id: n, .. } if *n == id))
        });
        self.selected = found.unwrap_or_else(|| self.center_row());
        self.load = Load::Ready;
    }

    fn relayout(&mut self) {
        self.rows = layout(&self.graph, self.show_columns);
    }

    fn center_row(&self) -> usize {
        self.rows
            .iter()
            .position(|r| {
                matches!(
                    r,
                    Row::Node {
                        direction: Direction::Center,
                        ..
                    }
                )
            })
            .unwrap_or(0)
    }

    fn selected_node(&self) -> Option<i64> {
        match self.rows.get(self.selected) {
            Some(Row::Node { id, .. }) => Some(*id),
            _ => None,
        }
    }

    /// Moves to the next node line in `step`'s direction.
    fn move_by(&mut self, step: isize) {
        let mut index = self.selected as isize;
        loop {
            index += step;
            let Some(row) = usize::try_from(index).ok().and_then(|i| self.rows.get(i)) else {
                return;
            };
            if matches!(row, Row::Node { .. }) {
                self.selected = index as usize;
                return;
            }
        }
    }

    fn recenter(&mut self, ctx: &mut Ctx) {
        let Some(id) = self.selected_node() else {
            return;
        };
        if id == self.asset_id {
            return ctx.toast(Tone::Info, "Already centred on this asset");
        }
        self.history.push((self.asset_id, self.title.clone()));
        self.asset_id = id;
        if let Some(node) = self.graph.nodes.get(&id) {
            self.title = node.name.clone();
        }
        self.fetch(ctx);
    }

    fn back(&mut self, ctx: &mut Ctx) -> Handled {
        let Some((id, title)) = self.history.pop() else {
            return Handled::No;
        };
        self.asset_id = id;
        self.title = title;
        self.fetch(ctx);
        Handled::Yes
    }

    fn set_hops(&mut self, upstream: u8, downstream: u8, ctx: &mut Ctx) {
        let (upstream, downstream) = (upstream.min(MAX_HOPS), downstream.min(MAX_HOPS));
        if (upstream, downstream) == (self.upstream, self.downstream) {
            return ctx.toast(Tone::Info, "Hops are already at their limit (0 to 5)");
        }
        self.upstream = upstream;
        self.downstream = downstream;
        self.fetch(ctx);
    }

    /// The selected node's lakehouse table, when it is one.
    fn open_table(&self, ctx: &mut Ctx) {
        let node = self
            .selected_node()
            .and_then(|id| self.graph.nodes.get(&id));
        match node.and_then(|n| lakehouse_table(&n.raw)) {
            Some(preview) => ctx.push(Box::new(preview)),
            None => ctx.toast(Tone::Info, "This asset isn't a lakehouse table"),
        }
    }

    fn explain(err: &str) -> String {
        if err.contains("413") {
            return "This lineage scope is too large. Press - for fewer hops.".to_string();
        }
        errors::explain(
            err,
            Subject {
                what: "this lineage asset",
                service: "etl",
            },
        )
    }

    fn lines(&self, ui: RenderCtx, width: usize) -> Vec<Line<'static>> {
        let mut lines = vec![self.header(ui)];
        match &self.load {
            Load::Failed(err) => {
                lines.push(Line::from(Span::styled(
                    err.clone(),
                    theme::badge(theme::BadgeTone::Bad),
                )));
                if self.rows.is_empty() {
                    return lines;
                }
            }
            Load::Loading(_) if self.rows.is_empty() => {
                let frame = theme::SPINNER[(ui.tick as usize) % theme::SPINNER.len()];
                lines.push(Line::from(Span::styled(
                    format!("{frame} Loading lineage…"),
                    theme::muted(),
                )));
                return lines;
            }
            _ => {}
        }
        for (index, row) in self.rows.iter().enumerate() {
            let mut line = self.row_line(row, width);
            if index == self.selected {
                line = line.patch_style(theme::selected_row());
            }
            lines.push(line);
        }
        lines
    }

    fn header(&self, ui: RenderCtx) -> Line<'static> {
        let mut spans = vec![Span::styled(
            format!("↑{} ↓{} hops", self.upstream, self.downstream),
            theme::muted(),
        )];
        if self.graph.active_runs > 0 {
            let frame = theme::SPINNER[(ui.tick as usize) % theme::SPINNER.len()];
            spans.push(Span::styled(
                format!(
                    "  {frame} {} run(s) active · refreshing every 10s",
                    self.graph.active_runs
                ),
                theme::badge(theme::BadgeTone::Info),
            ));
        }
        if self.graph.hidden > 0 {
            spans.push(Span::styled(
                format!("  {} hidden (no access)", self.graph.hidden),
                theme::badge(theme::BadgeTone::Warn),
            ));
        }
        Line::from(spans)
    }

    fn row_line(&self, row: &Row, width: usize) -> Line<'static> {
        match row {
            Row::Heading(text) => Line::from(Span::styled(*text, theme::brand())),
            Row::Note(text) => Line::from(Span::styled(format!("  {text}"), theme::muted())),
            Row::Column { prefix, from, to } => Line::from(vec![
                Span::styled(format!("  {prefix}"), theme::muted()),
                Span::styled(
                    format!("{from} → {to}"),
                    theme::badge(theme::BadgeTone::Info),
                ),
            ]),
            Row::Node {
                id,
                prefix,
                flow,
                repeated,
                direction,
            } => {
                let node = self.graph.nodes.get(id);
                let name = node.map_or_else(|| format!("asset {id}"), |n| n.name.clone());
                let mut name_style = theme::title();
                if node.is_some_and(|n| n.deleted) {
                    name_style = theme::muted().add_modifier(Modifier::CROSSED_OUT);
                }
                if *direction == Direction::Center {
                    name_style = theme::brand();
                }
                let mut spans = vec![
                    Span::styled(format!("  {prefix}"), theme::muted()),
                    Span::styled(
                        fit(&name, width.saturating_sub(prefix.len() + 4)),
                        name_style,
                    ),
                ];
                if let Some(node) = node.filter(|n| !n.kind.is_empty()) {
                    spans.push(Span::styled(format!("  {}", node.kind), theme::muted()));
                }
                if node.is_some_and(|n| n.deleted) {
                    spans.push(Span::styled(
                        "  deleted",
                        theme::badge(theme::BadgeTone::Warn),
                    ));
                }
                if let Some(flow) = flow.and_then(|f| self.graph.flows.get(f)) {
                    spans.extend(flow_spans(flow));
                }
                if *repeated {
                    spans.push(Span::styled("  ↺ shown above", theme::muted()));
                }
                Line::from(spans)
            }
        }
    }
}

/// "via job (dataset) · DATA_FLOW + COLUMN_LINEAGE 14 cols · runtime".
fn flow_spans(flow: &Flow) -> Vec<Span<'static>> {
    let kinds = flow.kinds.iter().cloned().collect::<Vec<_>>().join(" + ");
    let mut detail = kinds;
    if !flow.columns.is_empty() {
        detail = format!("{detail} {} cols", flow.columns.len());
    }
    let (evidence, tone) = if flow.runtime {
        ("runtime", theme::BadgeTone::Good)
    } else {
        ("code", theme::BadgeTone::Muted)
    };
    vec![
        Span::styled(format!("  via {}", flow.workload), Style::default()),
        Span::styled(format!(" · {detail}"), theme::muted()),
        Span::styled(format!(" · {evidence}"), theme::badge(tone)),
    ]
}

impl View for LineageGraph {
    fn title(&self) -> String {
        format!("Lineage {}", self.title)
    }

    fn on_open(&mut self, ctx: &mut Ctx) {
        self.fetch(ctx);
    }

    fn on_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::Home | KeyCode::Char('g') => {
                self.selected = 0;
                self.move_by(1);
            }
            KeyCode::Char('c') => {
                self.show_columns = !self.show_columns;
                let node = self.selected_node();
                self.relayout();
                self.selected = node
                    .and_then(|id| {
                        self.rows
                            .iter()
                            .position(|r| matches!(r, Row::Node { id: n, .. } if *n == id))
                    })
                    .unwrap_or_else(|| self.center_row());
            }
            KeyCode::Enter => self.recenter(ctx),
            KeyCode::Backspace => return self.back(ctx),
            KeyCode::Char('+') => {
                self.set_hops(self.upstream + 1, self.downstream + 1, ctx);
            }
            KeyCode::Char('-') => self.set_hops(
                self.upstream.saturating_sub(1),
                self.downstream.saturating_sub(1),
                ctx,
            ),
            KeyCode::Char('u') => self.set_hops(self.upstream.max(1), 0, ctx),
            KeyCode::Char('d') => self.set_hops(0, self.downstream.max(1), ctx),
            KeyCode::Char('t') => self.open_table(ctx),
            KeyCode::Char('y') => ctx.copy(self.asset_id.to_string(), "asset id"),
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn on_fetched(&mut self, id: u64, result: Result<Value, String>, _ctx: &mut Ctx) {
        if self.load != Load::Loading(id) {
            return;
        }
        match result {
            Ok(answer) => self.loaded(&answer),
            Err(err) => self.load = Load::Failed(Self::explain(&err)),
        }
    }

    fn on_tick(&mut self, ctx: &mut Ctx) {
        let live = self.graph.active_runs > 0 && self.load == Load::Ready;
        if live && self.poller.due(ctx.now.instant) {
            self.fetch(ctx);
        }
    }

    fn refresh(&mut self, ctx: &mut Ctx) {
        self.fetch(ctx);
    }

    fn on_scroll(&mut self, delta: isize) {
        self.move_by(delta.signum());
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, ui: RenderCtx) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(theme::border(true))
            .title(Span::styled(format!(" {} ", self.title()), theme::title()))
            .title(
                Line::from(Span::styled(" ⏎ re-centre · c columns ", theme::muted()))
                    .alignment(Alignment::Right),
            )
            .padding(Padding::horizontal(1));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let lines = self.lines(ui, inner.width as usize);
        self.viewport = inner.height as usize;
        // Keep the selected line (one header line above the rows) in view.
        let selected_line = self.selected + 1;
        if selected_line >= self.scroll + self.viewport {
            self.scroll = selected_line + 1 - self.viewport;
        }
        if selected_line < self.scroll {
            self.scroll = selected_line;
        }
        let total = lines.len();
        let shown: Vec<Line> = lines
            .into_iter()
            .skip(self.scroll)
            .take(self.viewport)
            .collect();
        frame.render_widget(Paragraph::new(shown), inner);
        if total > self.viewport {
            render_scrollbar(frame, area, total - self.viewport, self.scroll);
        }
    }

    fn help(&self) -> Vec<HelpSection> {
        let keys = [
            ("↑ ↓", "move between assets"),
            ("⏎", "re-centre on the asset"),
            ("⌫", "back to the previous centre"),
            ("+  -", "more / fewer hops each way (0 to 5)"),
            ("u  d", "upstream only / downstream only"),
            ("c", "show column mappings"),
            ("t", "open the lakehouse table"),
            ("y", "copy the asset id"),
            ("r", "refresh"),
            ("esc", "close"),
        ];
        vec![HelpSection::new("Lineage graph", &keys)]
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![
            ("↑↓", "move"),
            ("⏎", "centre"),
            ("+/-", "hops"),
            ("c", "columns"),
            ("esc", "back"),
        ]
    }

    fn is_busy(&self) -> bool {
        matches!(self.load, Load::Loading(_))
    }
}

/// The preview of a lineage asset bound to a lakehouse connection; its
/// warehouse is read from that connection when the preview opens.
pub fn lakehouse_table(asset: &Value) -> Option<TablePreview> {
    let binding = asset["bindings"]
        .as_array()?
        .iter()
        .find(|b| b["connectionType"].as_str() == Some("GZLAKEHOUSE"))?;
    let qualified = asset["qualifiedName"]
        .as_str()
        .or(asset["displayName"].as_str())?;
    Some(TablePreview::from_connection(
        text_of(&binding["connectionId"]),
        qualified.to_string(),
    ))
}

#[cfg(test)]
mod tests;
