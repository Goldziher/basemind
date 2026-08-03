//! Parameter + response shapes for the graph-traversal tools `neighbors`, `path`, and
//! `subgraph` (ADR-0003). All three read the shared typed code-graph (ADR-0001) and report
//! per-edge provenance + confidence (ADR-0002), so they share the [`GraphNode`] / [`GraphEdge`]
//! payload. Node references inside edges are indices into the response's `nodes` vec.

use rmcp::schemars;
use serde::{Deserialize, Serialize};

use crate::path::RelPath;

fn default_traverse_edges() -> String {
    "all".into()
}

fn default_neighbors_direction() -> String {
    "both".into()
}

/// One node in a traversal result. `Symbol` nodes carry a location; `file` and virtual
/// `external` (unresolved name) nodes carry only what they have.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct GraphNode {
    /// Symbol name, file basename, or the unresolved identifier for a virtual node.
    pub name: String,
    /// `"function"`/`"method"`/`"struct"`/… for a symbol, `"file"` for a file node, or
    /// `"external"` for a virtual node whose target did not resolve to an indexed definition.
    pub kind: String,
    /// Owning file. Absent for virtual `external` nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<RelPath>,
    /// 0-based row of the symbol definition. Absent for file/external nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_row: Option<u32>,
    /// 0-based byte column of the symbol definition. Absent for file/external nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_col: Option<u32>,
    /// Hop distance from the nearest root (`neighbors`/`subgraph`). Absent on `path`, where
    /// order is the path itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth: Option<u32>,
    /// Local centrality score (`subgraph` only): higher = more central in the neighborhood.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub centrality: Option<u64>,
}

/// One typed, provenance-tagged edge. `from`/`to` index into the response's `nodes` vec.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct GraphEdge {
    /// Index into `nodes` of the edge source.
    pub from: u32,
    /// Index into `nodes` of the edge target.
    pub to: u32,
    /// `"calls"` | `"imports"` | `"inherits"` | `"contains"`.
    pub kind: String,
    /// `"extracted"` (proven) | `"inferred"` (name-resolved) | `"ambiguous"` (one name → many).
    pub provenance: String,
    /// Numeric confidence on the fixed ladder: 1.0 / 0.5 / 0.2.
    pub confidence: f32,
    /// Aggregate multiplicity (e.g. call-site count); 1 for structural edges.
    pub weight: u32,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct NeighborsParams {
    /// Root symbol name. Every definition site of the name is a root (use `path` to pin one).
    #[serde(alias = "needle", alias = "query", alias = "symbol", alias = "q")]
    pub name: String,
    /// Optional path to disambiguate `name` when several symbols share it.
    #[serde(default)]
    pub path: Option<RelPath>,
    /// `"both"` (default), `"out"` (what the root reaches), or `"in"` (what reaches the root).
    /// Accepts the `call_graph` synonyms `callees`/`callers`.
    #[serde(default = "default_neighbors_direction")]
    pub direction: String,
    /// Hop radius. Default 2, capped at 4.
    #[serde(default)]
    pub depth: Option<u32>,
    /// Edge lanes to follow: `"all"` (default; calls+imports+inherits), `"calls"`, `"imports"`,
    /// `"inherits"`, `"both"` (calls+imports), or `"contains"`.
    #[serde(default = "default_traverse_edges")]
    pub edges: String,
    /// Minimum edge confidence to traverse (0.0–1.0, clamped). Default 0.0 (keep everything).
    #[serde(default)]
    pub min_confidence: Option<f32>,
    /// Hard cap on nodes returned. Default 100, max 500.
    #[serde(default)]
    pub max_nodes: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct PathParams {
    /// Source symbol name.
    #[serde(alias = "source", alias = "start")]
    pub from: String,
    /// Optional path to disambiguate the source.
    #[serde(default)]
    pub from_path: Option<RelPath>,
    /// Target symbol name.
    #[serde(alias = "target", alias = "dest")]
    pub to: String,
    /// Optional path to disambiguate the target.
    #[serde(default)]
    pub to_path: Option<RelPath>,
    /// Edge lanes the path may cross: `"all"` (default), `"calls"`, `"imports"`, `"inherits"`,
    /// or `"both"`. Containment is excluded by default (it yields structurally valid but
    /// meaningless routes); set `include_contains` to add it.
    #[serde(default = "default_traverse_edges")]
    pub edges: String,
    /// Include containment (file→symbol) edges in the search. Default false.
    #[serde(default)]
    pub include_contains: bool,
    /// Minimum edge confidence to cross (0.0–1.0, clamped). Default 0.0.
    #[serde(default)]
    pub min_confidence: Option<f32>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SubgraphParams {
    /// Root symbol name. Every definition site is a root.
    #[serde(alias = "needle", alias = "query", alias = "symbol", alias = "q")]
    pub name: String,
    /// Optional path to disambiguate `name`.
    #[serde(default)]
    pub path: Option<RelPath>,
    /// Hop radius gathered before the centrality cut. Default 2, capped at 4.
    #[serde(default)]
    pub depth: Option<u32>,
    /// Edge lanes to include: `"all"` (default; calls+imports+inherits), `"calls"`, `"imports"`,
    /// `"inherits"`, `"both"` (calls+imports), or `"contains"`.
    #[serde(default = "default_traverse_edges")]
    pub edges: String,
    /// Minimum edge confidence to include (0.0–1.0, clamped). Default 0.0.
    #[serde(default)]
    pub min_confidence: Option<f32>,
    /// Keep only this many most-central nodes (roots always kept). Default 30, max 200.
    #[serde(default)]
    pub max_nodes: Option<u32>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct NeighborsResponse {
    /// Echo of the requested root name.
    pub root: String,
    /// Echo of the resolved direction.
    pub direction: String,
    /// Reached nodes; roots come first (depth 0).
    pub nodes: Vec<GraphNode>,
    /// Typed, provenance-tagged edges among the returned nodes.
    pub edges: Vec<GraphEdge>,
    /// True when a cap stopped the walk before the neighborhood was exhausted.
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<super::types::LifecycleNotice>,
    /// Server-side handler latency in microseconds (excludes transport).
    #[serde(default)]
    pub elapsed_us: u64,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct PathResponse {
    /// Echo of the requested source name.
    pub from: String,
    /// Echo of the requested target name.
    pub to: String,
    /// True when a route was found.
    pub found: bool,
    /// Nodes along the path, source first. Empty when `found` is false.
    pub nodes: Vec<GraphNode>,
    /// Edges connecting consecutive path nodes, in order.
    pub edges: Vec<GraphEdge>,
    /// Total confidence-weighted cost of the path (lower = shorter/more proven). 0 when not found.
    pub cost: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<super::types::LifecycleNotice>,
    /// Server-side handler latency in microseconds (excludes transport).
    #[serde(default)]
    pub elapsed_us: u64,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct SubgraphResponse {
    /// Echo of the requested root name.
    pub root: String,
    /// Kept nodes, most central first; roots always present.
    pub nodes: Vec<GraphNode>,
    /// Edges induced among the kept nodes.
    pub edges: Vec<GraphEdge>,
    /// True when the neighborhood was cut to the centrality head (more nodes existed).
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<super::types::LifecycleNotice>,
    /// Server-side handler latency in microseconds (excludes transport).
    #[serde(default)]
    pub elapsed_us: u64,
}
