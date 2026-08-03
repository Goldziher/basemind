//! Parameter + response shapes for the `graph_export` tool (ADR-0005). Renders the shared typed
//! code-graph (ADR-0001) — with community assignments (ADR-0004) and per-edge provenance
//! (ADR-0002) — into one of several text formats over the canonical [`GraphView`](super::graph_view::GraphView)
//! payload.

use rmcp::schemars;
use serde::{Deserialize, Serialize};

fn default_graphview_edges() -> String {
    "all".into()
}

fn default_graphview_format() -> String {
    "node_link".into()
}

fn default_graphview_algorithm() -> String {
    "label_propagation".into()
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct GraphExportParams {
    /// Output format: `"node_link"` (default; node-link JSON), `"dot"` (Graphviz), `"mermaid"`,
    /// `"graphml"`, or `"cypher"`.
    #[serde(default = "default_graphview_format")]
    pub format: String,
    /// Repo-relative path prefix to scope the graph; omit for the whole repo.
    #[serde(default)]
    pub focus: Option<String>,
    /// Edge lanes the graph is built over: `"all"` (default; calls+imports+inherits), `"calls"`,
    /// `"imports"`, `"inherits"`, `"both"` (calls+imports), or `"contains"`.
    #[serde(default = "default_graphview_edges")]
    pub edges: String,
    /// Community-detection algorithm used to tag nodes: `"label_propagation"` (default) or
    /// `"louvain"`.
    #[serde(default = "default_graphview_algorithm", alias = "algo")]
    pub algorithm: String,
    /// Minimum edge confidence to include (0.0–1.0, clamped). Default 0.0 (keep everything).
    #[serde(default)]
    pub min_confidence: Option<f32>,
    /// Cap on nodes in the rendered view, most central first. Default 500, max 2000.
    #[serde(default)]
    pub max_nodes: Option<u32>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct GraphExportResponse {
    /// Echo of the format rendered.
    pub format: String,
    /// The rendered graph in the requested format.
    pub content: String,
    /// Nodes in the rendered view.
    pub node_count: u32,
    /// Edges in the rendered view.
    pub edge_count: u32,
    /// Communities present in the rendered view.
    pub community_count: u32,
    /// True when the underlying scan was truncated or the view was capped by `max_nodes`.
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<super::types::LifecycleNotice>,
    /// Server-side handler latency in microseconds (excludes transport).
    #[serde(default)]
    pub elapsed_us: u64,
}
