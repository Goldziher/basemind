//! Parameter + response shapes for the `call_graph` MCP tool.
//!
//! Lives in its own file because `types.rs` is hovering against the 1000-line per-file
//! cap and the call-graph DAG payload is self-contained — none of these types are reused
//! by any other tool.

use rmcp::schemars;
use serde::{Deserialize, Serialize};

use crate::path::RelPath;

fn default_direction() -> String {
    "callers".into()
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct CallGraphParams {
    /// Root function name. Exact match against captured call-site identifiers.
    pub name: String,
    /// `"callers"` (default) BFS-walks upward (who calls into `name`).
    /// `"callees"` walks downward (what `name` itself calls).
    #[serde(default = "default_direction")]
    pub direction: String,
    /// Optional path to disambiguate `name` when several functions share it.
    /// When omitted, every matching definition site is added as a depth-0 node.
    #[serde(default)]
    pub path: Option<RelPath>,
    /// BFS depth from the root. Default 3, capped at 6.
    #[serde(default)]
    pub max_depth: Option<u32>,
    /// Hard upper bound on the total node count returned. Default 100, max 500.
    /// When hit, response is marked truncated.
    #[serde(default)]
    pub max_nodes: Option<u32>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct CallGraphResponse {
    /// Echo of the requested root name.
    pub root: String,
    /// Echo of the requested direction (`"callers"` or `"callees"`).
    pub direction: String,
    /// Nodes in BFS order. `nodes[0]` is always the root.
    pub nodes: Vec<CallGraphNode>,
    /// True when the BFS stopped before exhausting the graph.
    pub truncated: bool,
    /// `"max_depth"` | `"max_nodes"` | `"scan_cap"` — disclosed reason for truncation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation_reason: Option<&'static str>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct CallGraphNode {
    /// Symbol name.
    pub name: String,
    /// BFS depth from the root (`0` for the root itself).
    pub depth: u32,
    /// Indices into the parent `nodes` vec: the neighbors at the previous depth
    /// (for `direction="callers"`) or next depth (for `direction="callees"`) that
    /// connect to this node. Empty for the root.
    pub edges_to: Vec<u32>,
    /// Every definition site of this symbol. Usually one — overloaded names produce
    /// multiple. Empty when the name surfaces only as a callee with no indexed
    /// definition (e.g. external library functions).
    pub sites: Vec<CallGraphSite>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct CallGraphSite {
    pub path: RelPath,
    /// `"function"`, `"method"`, `"constructor"`, `"getter"`, `"setter"`.
    pub kind: String,
    /// 0-based row.
    pub start_row: u32,
    /// 0-based byte column from the start of the line.
    pub start_col: u32,
}
