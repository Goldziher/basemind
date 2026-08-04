//! `#[tool]` shim for the `graph_export` tool (ADR-0005). Thin wrapper: await the cache, resolve
//! the index handle + map cache, delegate to `helpers_graphview`.

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::tool;
use serde_json::Value;

use super::BasemindServer;
use super::helpers::record_call;
use super::helpers_graphview::{run_display, run_graph_export, run_ui};
use super::types_graphview::{DisplayParams, GraphExportParams, UiParams};
// The `/ui` HTTP route (its only consumer) is comms-gated, so its render helper + format enum are
// only referenced there.
#[cfg(all(feature = "comms", any(unix, windows)))]
use super::graph_view::GraphFormat;
#[cfg(all(feature = "comms", any(unix, windows)))]
use super::helpers_graphview::render_ui_parts;

#[rmcp::tool_router(vis = "pub(super)", router = "tool_router_graphview")]
impl BasemindServer {
    /// Render/export the typed code-graph into a text format.
    #[tool(
        output_schema = "rmcp::handler::server::tool::schema_for_output::<super::types_graphview::GraphExportResponse>()",
        description = "Export the unified code-graph in a chosen `format`: \"node_link\" \
                       (default; node-link JSON, the common interchange shape), \"dot\" (Graphviz), \
                       \"mermaid\", \"graphml\", \"cypher\", \"html\" — a single self-contained, \
                       offline interactive page (pan/zoom/search/community legend, zero \
                       dependencies, no CDN) — or \"svg\", a static self-contained picture with a \
                       server-side deterministic force layout. One canonical payload feeds every \
                       renderer — nodes carry identity/label/location/kind + community + \
                       centrality (ADR-0004), edges carry kind + provenance/confidence/weight \
                       (ADR-0002). `focus` scopes to a path prefix; `edges` picks the lanes \
                       (all/calls/imports/inherits/both/contains); `algorithm` \
                       (label_propagation/louvain) tags communities; `min_confidence` floors edge \
                       confidence; `max_nodes` (default 500, max 2000) keeps the most central \
                       nodes. `write: true` also persists the rendered content to basemind's cache \
                       (`<cache>/exports/graph-<hash>.<ext>`) and returns its absolute `output_path`; \
                       off by default (content is always returned inline). Output is deterministic \
                       (snapshot-stable) and fully offline (no CDN, no assets). `truncated` flags a \
                       capped or scan-truncated view. `elapsed_us` = server-side handler latency in \
                       µs (excludes transport).",
        // read_only_hint stays true: the optional `write` persists only a derived artifact into
        // basemind's own machine-global cache (content-addressed, never the user's repo/index or any
        // external system) — the same category as the per-call telemetry the server already writes.
        // Flipping it would mislabel the dominant pure-render path as a mutation.
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn graph_export(
        &self,
        Parameters(params): Parameters<GraphExportParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&params).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = async {
            let __body = std::time::Instant::now();
            self.state.await_cache_ready().await;
            let store = self.state.shared.store.read().await;
            let idx = store.index_db.as_ref().cloned();
            let basemind_dir = store.basemind_dir.clone();
            drop(store);
            let cache = self.state.shared.cache.load_full();
            run_graph_export(
                &self.state.shared,
                idx.as_ref(),
                &cache,
                &basemind_dir,
                params,
                self.state.lifecycle_notice(),
                __body,
            )
        }
        .await;
        record_call(&self.state, "graph_export", &__params_json, __started, &__result);
        __result
    }

    /// Show the code-graph to a human by opening a rendered view (ADR-0007).
    #[tool(
        output_schema = "rmcp::handler::server::tool::schema_for_output::<super::types_graphview::DisplayResponse>()",
        description = "Show a human the code-graph: render a *visual* view and open it in their \
                       default desktop viewer — the agent's human-facing output channel, for when a \
                       reviewer or pair should SEE what you found, not just read a description. \
                       Renders the same canonical payload as graph_export (nodes carry \
                       identity/label/location/kind + community + centrality; edges carry \
                       kind + provenance/confidence/weight) but only in a visual `format`: \"html\" \
                       (default; a self-contained, offline interactive page — pan/zoom/search/\
                       community legend, zero dependencies) or \"svg\" (a static self-contained \
                       picture). `focus` scopes to a path prefix; `edges` picks the lanes \
                       (all/calls/imports/inherits/both/contains); `algorithm` \
                       (label_propagation/louvain) tags communities; `min_confidence` floors edge \
                       confidence; `max_nodes` (default 500, max 2000) keeps the most central nodes. \
                       The view is always written to basemind's cache and its absolute `output_path` \
                       returned. `open` (default true) launches the viewer; set false for headless \
                       automation and tests so no viewer process is spawned. `displayed` reports \
                       whether a viewer launched; `method` is \"viewer\" (opened in the OS default \
                       handler) or \"export\" (written only — headless / no GUI session / open:false), \
                       with a `detail` reason when it degraded. Use graph_export instead when you want the graph \
                       DATA (node_link/dot/mermaid/graphml/cypher) rather than a picture for a human.",
        // read_only_hint is false (unlike graph_export): by default this launches an external desktop
        // viewer — a user-visible side effect on their session, not a pure read — so a client that
        // auto-approves read-only tools should NOT silently pop a window without confirmation. The
        // only persisted write is still a derived, content-addressed cache artifact (never the repo/
        // index). open_world_hint is true because it reaches outside the process to the desktop.
        annotations(read_only_hint = false, open_world_hint = true)
    )]
    pub(crate) async fn display(
        &self,
        Parameters(params): Parameters<DisplayParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&params).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = async {
            let __body = std::time::Instant::now();
            self.state.await_cache_ready().await;
            let store = self.state.shared.store.read().await;
            let idx = store.index_db.as_ref().cloned();
            let basemind_dir = store.basemind_dir.clone();
            drop(store);
            let cache = self.state.shared.cache.load_full();
            run_display(
                &self.state.shared,
                idx.as_ref(),
                &cache,
                &basemind_dir,
                params,
                self.state.lifecycle_notice(),
                __body,
            )
            .await
        }
        .await;
        record_call(&self.state, "display", &__params_json, __started, &__result);
        __result
    }

    /// Open the interactive basemind UI for a human (ADR-0006).
    #[tool(
        output_schema = "rmcp::handler::server::tool::schema_for_output::<super::types_graphview::UiResponse>()",
        description = "Open the interactive basemind UI for a human: render the code-graph and return \
                       a `url` to view and drive it — a live `http://<addr>/ui?root=…` page served by \
                       the running basemind daemon when one is reachable (`served: true`), else a \
                       `file://` URL to a self-contained offline export (`served: false`, \
                       `method: \"file\"`). The served page is the same canonical payload as \
                       graph_export/display (pan/zoom/search/community legend, zero dependencies, no \
                       CDN). `format` is a visual format — `\"html\"` (default) or `\"svg\"`; the graph \
                       DATA formats live on graph_export. `focus` scopes to a path prefix; `edges` \
                       picks the lanes (all/calls/imports/inherits/both/contains); `algorithm` \
                       (label_propagation/louvain) tags communities; `min_confidence` floors edge \
                       confidence; `max_nodes` (default 500, max 2000) keeps the most central nodes. \
                       The view is always written to basemind's cache and its absolute `output_path` \
                       returned. `open` (default true) launches the URL in the human's default viewer; \
                       set false to just return the URL without launching — for headless automation, \
                       tests, and agents that drive the served page over a browser themselves. \
                       `elapsed_us` = server-side handler latency in µs (excludes transport).",
        // read_only_hint is false (like display): by default this launches an external viewer — a
        // user-visible side effect — so a client that auto-approves read-only tools must not silently
        // pop a window. open_world_hint is true: it reaches the desktop and a loopback HTTP host.
        annotations(read_only_hint = false, open_world_hint = true)
    )]
    pub(crate) async fn ui(&self, Parameters(params): Parameters<UiParams>) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&params).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = async {
            let __body = std::time::Instant::now();
            self.state.await_cache_ready().await;
            let store = self.state.shared.store.read().await;
            let idx = store.index_db.as_ref().cloned();
            let basemind_dir = store.basemind_dir.clone();
            drop(store);
            let cache = self.state.shared.cache.load_full();
            run_ui(
                &self.state.shared,
                idx.as_ref(),
                &cache,
                &basemind_dir,
                params,
                self.state.lifecycle_notice(),
                __body,
            )
            .await
        }
        .await;
        record_call(&self.state, "ui", &__params_json, __started, &__result);
        __result
    }
}

/// The daemon's `/ui` HTTP route is the only consumer, so this render entry is comms-only.
#[cfg(all(feature = "comms", any(unix, windows)))]
impl BasemindServer {
    /// Render the interactive UI page for the daemon's `/ui` HTTP route (ADR-0006): await the cache,
    /// build the graph over the shared read stack, and return the rendered `(content, content_type)`.
    /// This is the same [`render_ui_parts`] path the `ui` tool resolves a URL to, so the served page
    /// and the tool agree by construction. No write/open side effects — the route only serves bytes.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn render_ui_http(
        &self,
        format: &str,
        edges: &str,
        algorithm: &str,
        min_confidence: Option<f32>,
        max_nodes: Option<u32>,
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
            focus,
        )?;
        let content_type = if matches!(parts.format, GraphFormat::Svg) {
            "image/svg+xml"
        } else {
            "text/html; charset=utf-8"
        };
        Ok((parts.content, content_type))
    }
}
