//! Parameter + response shapes for the `communities` tool (ADR-0004). Community detection reads
//! the shared typed code-graph (ADR-0001) and reuses the [`GraphNode`](super::types_traverse::GraphNode)
//! payload for members, each carrying its centrality score.

use rmcp::schemars;
use serde::{Deserialize, Serialize};

use super::types_traverse::GraphNode;

fn default_community_edges() -> String {
    "all".into()
}

fn default_community_algorithm() -> String {
    "label_propagation".into()
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct CommunitiesParams {
    /// Edge lanes the graph is built over: `"all"` (default; calls+imports+inherits), `"calls"`,
    /// `"imports"`, `"inherits"`, `"both"` (calls+imports), or `"contains"`.
    #[serde(default = "default_community_edges")]
    pub edges: String,
    /// Detection algorithm: `"label_propagation"` (default; near-linear) or `"louvain"`
    /// (opt-in, higher-quality modularity optimisation).
    #[serde(default = "default_community_algorithm", alias = "algo")]
    pub algorithm: String,
    /// Minimum edge confidence to include (0.0–1.0, clamped). Default 0.0 (keep everything).
    #[serde(default)]
    pub min_confidence: Option<f32>,
    /// Cap on communities returned, largest first. Default 50, max 200.
    #[serde(default)]
    pub max_communities: Option<u32>,
    /// Cap on members listed per community, most central first. Default 10, max 100.
    #[serde(default)]
    pub members_per_community: Option<u32>,
}

/// One detected community: a group of graph nodes that relate to each other far more than to the
/// rest of the repo — a de-facto module.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Community {
    /// Dense community id (`0..num_communities`).
    pub id: u32,
    /// Deterministic, LLM-free label: dominant path prefix + most central member.
    pub label: String,
    /// Total members in the community (may exceed `members.len()` when capped).
    pub size: u32,
    /// Members, most central first. Capped by `members_per_community`; each carries `centrality`.
    pub members: Vec<GraphNode>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct CommunitiesResponse {
    /// Detected communities, largest first (capped by `max_communities`).
    pub communities: Vec<Community>,
    /// Total communities detected, before the `max_communities` cap.
    pub num_communities: u32,
    /// Nodes in the graph the detection ran over.
    pub node_count: u32,
    /// Edges in the graph the detection ran over.
    pub edge_count: u32,
    /// Echo of the algorithm used.
    pub algorithm: String,
    /// True when the underlying call scan was truncated, or communities/members were capped.
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<super::types::LifecycleNotice>,
    /// Server-side handler latency in microseconds (excludes transport).
    #[serde(default)]
    pub elapsed_us: u64,
}
