//! Request shape for the consolidated `graph` domain tool, plus the `calls` mode's payload.
//!
//! [`GraphParams`] is what crosses the wire: one flat parameter object with a required
//! [`GraphMode`] selecting the operation and every per-mode field an optional sibling. The
//! per-operation structs (`CallGraphParams` here, `NeighborsParams` / `PathParams` /
//! `SubgraphParams` in `types_traverse.rs`, `CommunitiesParams`, `ArchitectureMapParams`,
//! `GraphExportParams` / `DisplayParams` / `UiParams`) stay as the helpers' internal shapes, so
//! the bodies keep taking exactly the arguments they always did.
//!
//! Lives in its own file because `types.rs` is hovering against the 1000-line per-file
//! cap and the call-graph DAG payload is self-contained — none of these types are reused
//! by any other tool.

use rmcp::schemars;
use serde::{Deserialize, Serialize};

use super::mode::GraphMode;
use crate::path::RelPath;

fn default_direction() -> String {
    "callers".into()
}

/// Wire parameters for the `graph` tool.
///
/// Only `mode` is required. Every other field belongs to a subset of the modes and is rejected —
/// not ignored — when passed to a mode that has no use for it (see
/// [`super::mode::reject_unsupported`]); a mode that needs one names the exact `mode`/field pair.
/// Per-mode defaults are resolved in the helper, not here, because they differ by mode (`edges`
/// defaults to `"calls"` for `map` and `"all"` everywhere else).
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct GraphParams {
    /// Which operation to run.
    pub mode: GraphMode,
    /// `calls` / `neighbors` / `subgraph`. Root symbol name, matched exactly (not a substring).
    /// Every definition site of the name is a root; pin one with `path`. Required by those modes.
    #[serde(
        default,
        alias = "needle",
        alias = "query",
        alias = "symbol",
        alias = "function",
        alias = "q"
    )]
    pub name: Option<String>,
    /// `calls` / `neighbors` / `subgraph`. Repo-relative path that disambiguates `name` when
    /// several definitions share it.
    #[serde(default)]
    pub path: Option<RelPath>,
    /// `path` only. Source symbol name. Required by that mode.
    #[serde(default, alias = "source", alias = "start")]
    pub from: Option<String>,
    /// `path` only. Repo-relative path that disambiguates the source symbol.
    #[serde(default)]
    pub from_path: Option<RelPath>,
    /// `path` only. Target symbol name. Required by that mode.
    #[serde(default, alias = "target", alias = "dest")]
    pub to: Option<String>,
    /// `path` only. Repo-relative path that disambiguates the target symbol.
    #[serde(default)]
    pub to_path: Option<RelPath>,
    /// `calls`: `"callers"` (default; who calls into `name`) or `"callees"` (what `name` calls).
    /// `neighbors`: `"both"` (default), `"out"`, or `"in"` — the `callees`/`callers` synonyms are
    /// accepted there too.
    #[serde(default)]
    pub direction: Option<String>,
    /// `neighbors` / `subgraph`: hop radius, default 2, capped at 4. `map`: directory-rollup depth
    /// for `granularity="module"` (leading path components), default 2, minimum 1.
    #[serde(default)]
    pub depth: Option<u32>,
    /// `calls` only. BFS depth from the root. Default 3, capped at 6.
    #[serde(default)]
    pub max_depth: Option<u32>,
    /// Node cap, per mode: `calls` default 100 / max 500; `neighbors` default 100 / max 500;
    /// `subgraph` keeps the default 30 / max 200 most central nodes; `map` default 60 / max 300;
    /// `export` / `display` / `open` default 500 / max 2000.
    #[serde(default)]
    pub max_nodes: Option<u32>,
    /// Hard cap on edges for `map` / `export` / `display` / `open`. Default 200 for `map` and
    /// `export`, 2000 for the visual modes; max 2000.
    #[serde(default)]
    pub max_edges: Option<u32>,
    /// `map` only. Token budget for the `nodes` list; sets `budgeted` when it trims the tail.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Edge lanes to follow: `"all"` (calls+imports+inherits), `"calls"`, `"imports"`,
    /// `"inherits"`, `"both"` (calls+imports), or `"contains"`. Default `"all"` everywhere except
    /// `map`, whose historical default is `"calls"`. Not accepted by `calls`, which is a
    /// call-lane-only walk.
    #[serde(default)]
    pub edges: Option<String>,
    /// `path` only. Include containment (file→symbol) edges in the search. Default false — they
    /// yield structurally valid but meaningless routes.
    #[serde(default)]
    pub include_contains: Option<bool>,
    /// `neighbors` / `path` / `subgraph` / `communities` / `export` / `display` / `open`. Minimum
    /// edge confidence to traverse (0.0–1.0, clamped). Default 0.0 — keep everything.
    #[serde(default)]
    pub min_confidence: Option<f32>,
    /// `communities` / `export` / `display` / `open`. Community-detection algorithm:
    /// `"label_propagation"` (default, near-linear) or `"louvain"` (higher-quality modularity).
    #[serde(default, alias = "algo")]
    pub algorithm: Option<String>,
    /// `communities` only. Cap on communities returned, largest first. Default 50, max 200.
    #[serde(default)]
    pub max_communities: Option<u32>,
    /// `communities` only. Cap on members listed per community, most central first. Default 10,
    /// max 100.
    #[serde(default)]
    pub members_per_community: Option<u32>,
    /// `map` only. `"module"` (default; directory-level dependency graph), `"file"`, or `"symbol"`
    /// (hub functions ranked by specificity-weighted fan-in).
    #[serde(default, alias = "tier")]
    pub granularity: Option<String>,
    /// `map` / `export` / `display` / `open`. Repo-relative path prefix scoping the graph (e.g.
    /// `"src/mcp"`). Omit for the whole repository.
    #[serde(default, alias = "dir", alias = "scope")]
    pub focus: Option<RelPath>,
    /// `map` only. Overlay git churn (commits touching each node) onto the ranking and as a
    /// per-node field. Default true; a silent no-op outside a git repo.
    #[serde(default)]
    pub include_churn: Option<bool>,
    /// `map` only. Commit window for the churn overlay. Default 200, max 2000.
    #[serde(default)]
    pub churn_window: Option<u32>,
    /// `export`: `"node_link"` (default), `"dot"`, `"mermaid"`, `"graphml"`, `"cypher"`, `"html"`,
    /// or `"svg"`. `display` / `open`: the visual formats only — `"html"` (default) or `"svg"`.
    #[serde(default)]
    pub format: Option<String>,
    /// `export` only. Also write the rendered content to basemind's machine-global cache
    /// (`<cache>/exports/graph-<hash>.<ext>`) and return its absolute `output_path`. Off by
    /// default; the content is returned inline regardless.
    #[serde(default)]
    pub write: Option<bool>,
    /// `display` / `open` only. When true (the default), launch the human's default viewer /
    /// browser. Set false to only render and return the path (`display`) or the URL (`open`)
    /// without spawning anything — the right choice for headless automation, tests, and agents
    /// that drive the served page themselves.
    #[serde(default)]
    pub open: Option<bool>,
}

impl GraphParams {
    /// A call carrying only `mode`. Callers set the fields their mode uses and leave the rest
    /// `None`: the helper rejects a field belonging to another mode, so populating them blindly
    /// would fail the call.
    pub fn new(mode: GraphMode) -> Self {
        Self {
            mode,
            name: None,
            path: None,
            from: None,
            from_path: None,
            to: None,
            to_path: None,
            direction: None,
            depth: None,
            max_depth: None,
            max_nodes: None,
            max_edges: None,
            max_tokens: None,
            edges: None,
            include_contains: None,
            min_confidence: None,
            algorithm: None,
            max_communities: None,
            members_per_community: None,
            granularity: None,
            focus: None,
            include_churn: None,
            churn_window: None,
            format: None,
            write: None,
            open: None,
        }
    }
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct CallGraphParams {
    /// Root function name. Exact match against captured call-site identifiers.
    #[serde(alias = "needle", alias = "query", alias = "symbol", alias = "function", alias = "q")]
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
    /// Lifecycle notice when the server isn't fully ready (warming/building/rescanning); absent when
    /// ready. Lets a caller tell "index still loading — retry" from a genuine empty result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<super::types::LifecycleNotice>,
    /// Server-side handler latency in microseconds — the tool body's own execution (index / vector
    /// search / graph walk + response construction), excluding MCP transport, argument
    /// deserialization, and response serialization. A first call against a cold server also
    /// includes index warm-up; such responses carry a `notice`. See
    /// [`crate::mcp::helpers::timing`] for the full contract.
    #[serde(default)]
    pub elapsed_us: u64,
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

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct CallGraphSite {
    pub path: RelPath,
    /// `"function"`, `"method"`, `"constructor"`, `"getter"`, `"setter"`.
    pub kind: String,
    /// 0-based row.
    pub start_row: u32,
    /// 0-based byte column from the start of the line.
    pub start_col: u32,
}
