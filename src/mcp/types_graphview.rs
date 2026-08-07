//! Parameter + response shapes for the `graph_export` tool (ADR-0005). Renders the shared typed
//! code-graph (ADR-0001) — with community assignments (ADR-0004) and per-edge provenance
//! (ADR-0002) — into one of several text formats over the canonical [`GraphView`](super::graph_view::GraphView)
//! payload.

use rmcp::schemars;
use serde::{Deserialize, Serialize};

use crate::path::RelPath;

fn default_graphview_edges() -> String {
    "all".into()
}

fn default_graphview_format() -> String {
    "node_link".into()
}

fn default_graphview_algorithm() -> String {
    "label_propagation".into()
}

fn default_display_format() -> String {
    "html".into()
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct GraphExportParams {
    /// Output format: `"node_link"` (default; node-link JSON), `"dot"` (Graphviz), `"mermaid"`,
    /// `"graphml"`, `"cypher"`, `"html"` (a self-contained, offline interactive page), or `"svg"`
    /// (a static, self-contained SVG picture with a server-side deterministic layout).
    #[serde(default = "default_graphview_format")]
    pub format: String,
    /// Repo-relative path prefix to scope the graph; omit for the whole repo.
    #[serde(default)]
    pub focus: Option<RelPath>,
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
    /// Hard cap on rendered edges. Default 200, max 2000.
    #[serde(default)]
    pub max_edges: Option<u32>,
    /// When true, also write the rendered content to a file in basemind's machine-global cache
    /// (`<workspace-cache>/exports/graph-<hash>.<ext>`) and return its absolute path in
    /// `output_path`. Off by default — the content is always returned inline regardless. Useful for
    /// large `html`/`svg` exports and for handing a stable file path to a viewer.
    #[serde(default)]
    pub write: bool,
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
    /// Edges available before applying `max_edges`.
    pub edge_count_total: u32,
    /// Communities present in the rendered view.
    pub community_count: u32,
    /// True when the underlying scan was truncated or the view was capped by `max_nodes` /
    /// `max_edges`.
    pub truncated: bool,
    /// Absolute path of the file written to the cache when `write` was set; omitted otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_path: Option<RelPath>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<super::types::LifecycleNotice>,
    /// Server-side handler latency in microseconds (excludes transport).
    #[serde(default)]
    pub elapsed_us: u64,
}

/// Parameters for the `display` tool (ADR-0007): the agent's human-facing output channel. Shapes the
/// same code-graph as [`GraphExportParams`] but renders a *visual* format and opens it for the human.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct DisplayParams {
    /// Visual format to render: `"html"` (default; a self-contained, offline interactive page) or
    /// `"svg"` (a static self-contained picture). The graph *data* formats live on `graph_export`;
    /// `display` shows a human a picture, so only the visual formats are accepted.
    #[serde(default = "default_display_format")]
    pub format: String,
    /// Repo-relative path prefix to scope the graph; omit for the whole repo.
    #[serde(default)]
    pub focus: Option<RelPath>,
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
    /// Hard cap on rendered edges. Default 2000, max 2000.
    #[serde(default)]
    pub max_edges: Option<u32>,
    /// When true (default), open the rendered view in the human's default viewer. Set false to only
    /// write the export and return its path — the right choice for headless automation and tests, so
    /// the tool never spawns a viewer process.
    #[serde(default = "default_true")]
    pub open: bool,
}

/// Response from the `display` tool (ADR-0007). Unlike `graph_export`, the rendered bytes are *not*
/// returned inline — the tool's product is the opened view plus a stable file path, and an
/// interactive `html` render can be hundreds of KB.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DisplayResponse {
    /// Echo of the visual format rendered (`"html"` or `"svg"`).
    pub format: String,
    /// Absolute path of the rendered view written to basemind's machine-global cache
    /// (`<cache>/exports/graph-<hash>.<ext>`). Always present — `display` always persists so there
    /// is a stable artifact to open (or hand to the human) regardless of whether a viewer launched.
    /// Typed as `RelPath` — basemind's byte-precise path type — so a cache directory whose bytes are
    /// not UTF-8 round-trips instead of arriving mangled.
    pub output_path: RelPath,
    /// True when a viewer was launched for the human; false when the tool degraded to export-only
    /// (headless / no GUI session / opener unavailable / `open: false`).
    pub displayed: bool,
    /// How the view reached the human: `"viewer"` (opened in the OS default handler — browser for
    /// html, image viewer for svg) or `"export"` (written only — open `output_path` yourself).
    /// `"window"` is reserved for the future native basemind UI push (ADR-0006).
    pub method: String,
    /// Human-readable reason the tool degraded to export-only, when it did (e.g. `"no GUI session"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Nodes in the rendered view.
    pub node_count: u32,
    /// Edges in the rendered view.
    pub edge_count: u32,
    /// Edges available before applying `max_edges`.
    pub edge_count_total: u32,
    /// Communities present in the rendered view.
    pub community_count: u32,
    /// True when the underlying scan was truncated or the view was capped by `max_nodes` /
    /// `max_edges`.
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<super::types::LifecycleNotice>,
    /// Server-side handler latency in microseconds (excludes transport and any viewer launch).
    #[serde(default)]
    pub elapsed_us: u64,
}

/// Parameters for the `ui` tool (ADR-0006): open the interactive basemind UI for a human. Shapes the
/// same code-graph as [`DisplayParams`] but the product is a durable, agent-drivable *surface* — a
/// served `http://…/ui` page when a basemind daemon is up, else the same self-contained export file.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct UiParams {
    /// Visual format to render: `"html"` (default; the interactive page) or `"svg"` (a static
    /// picture). The graph *data* formats live on `graph_export`; the UI shows a human a picture.
    #[serde(default = "default_display_format")]
    pub format: String,
    /// Repo-relative path prefix to scope the graph; omit for the whole repo.
    #[serde(default)]
    pub focus: Option<RelPath>,
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
    /// Hard cap on rendered edges. Default 2000, max 2000.
    #[serde(default)]
    pub max_edges: Option<u32>,
    /// When true (default), open the returned URL in the human's default viewer. Set false to only
    /// resolve/write the UI and return its `url` without launching anything — the right choice for
    /// headless automation, tests, and agents that drive the served page over a browser themselves.
    #[serde(default = "default_true")]
    pub open: bool,
}

/// Response from the `ui` tool (ADR-0006). Like `display`, the rendered bytes are not returned inline;
/// the product is a URL to the interactive UI plus the stable export path that always backs it.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct UiResponse {
    /// URL of the interactive UI. A live `http://<addr>/ui?root=…` page served by the basemind daemon
    /// when one is reachable (see `served`), otherwise a `file://` URL to the self-contained export at
    /// `output_path`. Navigate a browser here to view and drive the graph.
    pub url: String,
    /// True when `url` is a live daemon-served HTTP page (reflects the current index, reloadable);
    /// false when it degraded to the static `file://` export (no daemon reachable / no comms build).
    pub served: bool,
    /// How the UI is backed: `"http"` (a running daemon serves it) or `"file"` (the written export).
    pub method: String,
    /// Absolute path of the rendered view written to basemind's machine-global cache
    /// (`<cache>/exports/graph-<hash>.<ext>`). Always present — the UI always persists a stable
    /// artifact, which also backs the `file://` fallback. Typed as `RelPath` — basemind's
    /// byte-precise path type — so a cache directory whose bytes are not UTF-8 round-trips.
    pub output_path: RelPath,
    /// Human-readable reason the UI degraded to the file export, when it did (e.g. `"no daemon
    /// reachable"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Nodes in the rendered view.
    pub node_count: u32,
    /// Edges in the rendered view.
    pub edge_count: u32,
    /// Edges available before applying `max_edges`.
    pub edge_count_total: u32,
    /// Communities present in the rendered view.
    pub community_count: u32,
    /// True when the underlying scan was truncated or the view was capped by `max_nodes` /
    /// `max_edges`.
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<super::types::LifecycleNotice>,
    /// Server-side handler latency in microseconds (excludes transport and any viewer launch).
    #[serde(default)]
    pub elapsed_us: u64,
}
