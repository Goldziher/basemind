//! The `graph` domain tool shim for `BasemindServer`.
//!
//! One tool, one required `mode` — `calls` / `neighbors` / `path` / `subgraph` / `communities` /
//! `map` / `export` / `display` / `open` — dispatched to `helpers_graph::run_graph`. Thin wrapper:
//! the bodies live in `helpers_graph.rs` (`calls`), `helpers_traverse.rs`, `helpers_community.rs`,
//! `helpers_archmap.rs`, and `helpers_graphview.rs`.

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::tool;
use serde_json::Value;

use super::BasemindServer;
use super::helpers::record_call;
use super::lenient::Lenient;
use super::types_graph::GraphParams;
// The `/ui` HTTP route (its only consumer) is comms-gated, so its render helper + format enum are
// only referenced there.
#[cfg(all(feature = "comms", any(unix, windows)))]
use super::graph_view::GraphFormat;
#[cfg(all(feature = "comms", any(unix, windows)))]
use super::helpers_graphview::render_ui_parts;

#[rmcp::tool_router(vis = "pub(super)", router = "tool_router_graph")]
impl BasemindServer {
    // No `output_schema`: the nine modes return nine different response shapes, and SEP-2106 allows
    // exactly one per tool. Declaring a union would mean nested structs, which schemars emits as
    // `$ref` into `$defs` — the construct that silently dropped the whole registry in GH #50. The
    // per-mode shapes are documented in the description instead. ~keep
    #[tool(
        description = "Navigate the unified code-graph: who calls this, what does this reach, how \
        do these two connect, what are this repo's modules and hubs, and render or show the graph. \
        `mode` is required. `calls` BFS-walks the call chain from one function — \
        `direction=\"callers\"` (default) for who calls `name`, `\"callees\"` for what `name` calls; \
        `name` is exact (not a substring), `path` disambiguates overloads, bounded by `max_depth` \
        (default 3, max 6) and `max_nodes` (default 100, max 500); only resolved in-repo calls are \
        edges, cycles are detected, and recursion surfaces as a self-edge. `neighbors` returns the \
        n-hop neighborhood around a symbol — `direction` \"both\" (default) / \"out\" (what it \
        reaches) / \"in\" (what reaches it), `depth` (default 2, max 4), `max_nodes` (default 100, \
        max 500) — use it to see a symbol's blast radius before changing it. `path` finds the \
        confidence-weighted shortest route between two symbols (`from` → `to`) across mixed lanes, \
        preferring proven edges over inferred ones; containment is excluded unless \
        `include_contains`; `found`, ordered `nodes`/`edges`, and total `cost` come back. \
        `subgraph` gathers the neighborhood and cuts it to the `max_nodes` (default 30, max 200) \
        most central nodes so you get a readable picture, not a dump — roots always kept. \
        `communities` clusters the whole graph into de-facto modules with deterministic LLM-free \
        labels (`algorithm` label_propagation default / louvain), largest first. `map` is the \
        whole-repo architecture overview ranked by PageRank centrality + git churn — \
        `granularity` \"module\" (default) / \"file\" / \"symbol\" — reporting hub modules, fan-in / \
        fan-out, and circular-dependency clusters (SCCs); ask it first when you need to learn an \
        unfamiliar codebase's shape without reading files. `export` renders the graph as \
        `format` node_link (default JSON) / dot / mermaid / graphml / cypher / html / svg, \
        returned inline with `max_edges` default 200 / max 2000 (`write: true` also persists it \
        and returns `output_path`). Edge caps keep the highest-weight edges and responses report \
        both rendered `edge_count` and pre-cap `edge_count_total`. `display` \
        renders a visual html/svg view and opens it in the human's desktop viewer — the agent's \
        human-facing output channel; `open` returns a live browsable `url` for the interactive UI \
        (a daemon-served http page when one is reachable, else a `file://` export). Both visual \
        modes accept `max_edges` default/max 2000 and persist a \
        stable `output_path` and both take `open: false` to skip launching anything, which is what \
        headless automation, tests, and browser-driving agents should pass. Across the graph modes \
        `edges` picks the lanes (all/calls/imports/inherits/both/contains; `map` defaults to \
        calls, the rest to all), `min_confidence` floors edge confidence, and `focus` scopes to a \
        repo-relative path prefix. Every edge carries kind + provenance \
        (extracted/inferred/ambiguous) + numeric confidence; results are deterministic, LLM-free, \
        and bounded — `truncated` flags a capped or scan-truncated result, and an unresolved name \
        returns an empty result rather than an error. Parameters that belong to another mode are \
        rejected, not ignored.",
        // The read-only majority cannot set the hints: `display` and `open` launch an external
        // viewer by default — a user-visible side effect on the human's session — so a client that
        // auto-approves read-only tools must not silently pop a window. The union takes the
        // side-effecting side; `open: false` is the escape hatch that keeps them pure. ~keep
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    pub(crate) async fn graph(
        &self,
        Parameters(Lenient(p)): Parameters<Lenient<GraphParams>>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __key = p.mode.telemetry_key();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = super::helpers_graph::run_graph(&self.state, p).await;
        record_call(&self.state, __key, &__params_json, __started, &__result);
        __result
    }
}

/// The daemon's `/ui` HTTP route is the only consumer, so this render entry is comms-only.
#[cfg(all(feature = "comms", any(unix, windows)))]
impl BasemindServer {
    /// Render the interactive UI page for the daemon's `/ui` HTTP route (ADR-0006): await the cache,
    /// build the graph over the shared read stack, and return the rendered `(content, content_type)`.
    /// This is the same [`render_ui_parts`] path `graph` mode `open` resolves a URL to, so the
    /// served page and the tool agree by construction. No write/open side effects — the route only
    /// serves bytes.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn render_ui_http(
        &self,
        format: &str,
        edges: &str,
        algorithm: &str,
        min_confidence: Option<f32>,
        max_nodes: Option<u32>,
        max_edges: Option<u32>,
        focus: Option<String>,
    ) -> Result<(String, &'static str), McpError> {
        self.state.await_cache_ready().await;
        let store = self.state.shared.store.read().await;
        let idx = store.index_db.as_ref().cloned();
        drop(store);
        let cache = self.state.shared.cache.load_full();
        let parts = render_ui_parts(
            &self.state.shared,
            idx.as_ref(),
            &cache,
            format,
            edges,
            algorithm,
            min_confidence,
            max_nodes,
            max_edges,
            focus.map(crate::path::RelPath::from),
        )?;
        let content_type = if matches!(parts.format, GraphFormat::Svg) {
            "image/svg+xml"
        } else {
            "text/html; charset=utf-8"
        };
        Ok((parts.content, content_type))
    }
}
