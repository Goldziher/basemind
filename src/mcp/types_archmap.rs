//! Parameter + response shapes for the `architecture_map` MCP tool.
//!
//! In its own file (like `types_graph.rs`) because the payload is self-contained and
//! `types.rs` is against the 1000-line cap. All fields are additive; new ones default.

use rmcp::schemars;
use serde::{Deserialize, Serialize};

use crate::path::RelPath;

fn default_granularity() -> String {
    "module".into()
}
fn default_edges() -> String {
    "calls".into()
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ArchitectureMapParams {
    /// `"module"` (default) — directory-level dependency graph; `"file"` — file-level
    /// graph; `"symbol"` — top hub functions ranked by fan-in. Module/file tiers report
    /// circular-dependency clusters (SCCs); the symbol tier reports hubs + their edges.
    #[serde(default = "default_granularity", alias = "tier", alias = "level")]
    pub granularity: String,
    /// Optional repo-relative path prefix to scope the map (e.g. `"src/mcp"`). Omit for
    /// the whole repository.
    #[serde(default, alias = "path", alias = "dir", alias = "scope")]
    pub focus: Option<String>,
    /// Directory-rollup depth for `granularity="module"` (number of leading path
    /// components). Default 2, minimum 1.
    #[serde(default)]
    pub depth: Option<u32>,
    /// Edge lanes. `"calls"` (default) uses name→definition call edges. `"imports"` /
    /// `"both"` are reserved — import edges are heuristic and not emitted yet.
    #[serde(default = "default_edges")]
    pub edges: String,
    /// Overlay git churn (commits-touching over the last `churn_window` commits) onto the
    /// ranking and as a per-node field. Default true; a silent no-op outside a git repo.
    #[serde(default = "default_true")]
    pub include_churn: bool,
    /// Commit window for the churn overlay. Default 200, max 2000 (mirrors `hot_files`).
    #[serde(default)]
    pub churn_window: Option<u32>,
    /// Hard cap on returned nodes after ranking + knee cut. Default 60, max 300.
    #[serde(default)]
    pub max_nodes: Option<u32>,
    /// Hard cap on returned edges. Default 200, max 2000.
    #[serde(default)]
    pub max_edges: Option<u32>,
    /// Token budget for the `nodes` list (sets `budgeted` when it trims the tail).
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ArchitectureMapResponse {
    /// Echo of the resolved granularity.
    pub granularity: String,
    /// Total graph nodes before ranking + cap.
    pub node_count_total: u32,
    /// Total graph edges before cap (0 for the symbol tier, which does not build a full
    /// symbol graph — only hub edges among the returned nodes).
    pub edge_count_total: u32,
    /// Nodes ranked best-first (centrality + churn), knee-cut then capped.
    pub nodes: Vec<ArchNode>,
    /// Edges among the returned nodes only (endpoints are response-local `id`s).
    pub edges: Vec<ArchEdge>,
    /// Strongly-connected components (size > 1) among the returned nodes —
    /// circular-dependency clusters. Empty for the symbol tier.
    pub cycles: Vec<CycleCluster>,
    /// True when the graph build hit a work cap and the map is over a partial graph.
    pub truncated: bool,
    /// `"scan_cap"` — disclosed reason for truncation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation_reason: Option<&'static str>,
    /// True when the `nodes` list was trimmed to fit `max_tokens`.
    pub budgeted: bool,
    /// Lifecycle notice when the server isn't fully ready (warming/building/rescanning); absent when
    /// ready. Lets a caller tell "index still loading — retry" from a genuine empty result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<super::types::LifecycleNotice>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ArchNode {
    /// Response-local id; `edges` and `cycles` reference these.
    pub id: u32,
    /// Directory label (module tier) or file path (file/symbol tier).
    pub label: String,
    /// File path — present for the file and symbol tiers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<RelPath>,
    /// Symbol name — symbol tier only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Symbol kind (`"function"`, `"method"`, …) — symbol tier only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// 0-based definition row — symbol tier only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_row: Option<u32>,
    /// Symbol signature — symbol tier only, when captured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// Inbound edge count (callers / dependents).
    pub fan_in: u32,
    /// Outbound edge count (callees / dependencies).
    pub fan_out: u32,
    /// Normalized PageRank (0..1) — module/file tiers only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pagerank: Option<f32>,
    /// Commits touching this node in the churn window — present when the overlay ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commits_touching: Option<u32>,
    /// Blended rank score in `[0, 1]` (centrality + churn).
    pub score: f32,
    /// Cycle-cluster membership id (index into `cycles`), when this node is in an SCC.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scc_id: Option<u32>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ArchEdge {
    /// Source node response-local id.
    pub from: u32,
    /// Destination node response-local id.
    pub to: u32,
    /// Aggregate call-site count on this edge.
    pub weight: u32,
    /// Edge lane — `"calls"`.
    pub kind: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct CycleCluster {
    /// Cluster id (matches `ArchNode::scc_id`).
    pub scc_id: u32,
    /// Response-local node ids in this cycle.
    pub members: Vec<u32>,
    /// Edge count internal to the cluster.
    pub internal_edges: u32,
}
