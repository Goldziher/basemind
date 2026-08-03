//! `#[tool]` shim for the `graph_export` tool (ADR-0005). Thin wrapper: await the cache, resolve
//! the index handle + map cache, delegate to `helpers_graphview`.

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::tool;
use serde_json::Value;

use super::BasemindServer;
use super::helpers::record_call;
use super::helpers_graphview::run_graph_export;
use super::types_graphview::GraphExportParams;

#[rmcp::tool_router(vis = "pub(super)", router = "tool_router_graphview")]
impl BasemindServer {
    /// Render/export the typed code-graph into a text format.
    #[tool(
        output_schema = "rmcp::handler::server::tool::schema_for_output::<super::types_graphview::GraphExportResponse>()",
        description = "Export the unified code-graph in a chosen `format`: \"node_link\" \
                       (default; node-link JSON, the common interchange shape), \"dot\" (Graphviz), \
                       \"mermaid\", \"graphml\", \"cypher\", or \"html\" — a single self-contained, \
                       offline interactive page (pan/zoom/search/community legend, zero \
                       dependencies, no CDN). One canonical payload feeds every \
                       renderer — nodes carry identity/label/location/kind + community + \
                       centrality (ADR-0004), edges carry kind + provenance/confidence/weight \
                       (ADR-0002). `focus` scopes to a path prefix; `edges` picks the lanes \
                       (all/calls/imports/inherits/both/contains); `algorithm` \
                       (label_propagation/louvain) tags communities; `min_confidence` floors edge \
                       confidence; `max_nodes` (default 500, max 2000) keeps the most central \
                       nodes. Output is deterministic (snapshot-stable) and fully offline (no CDN, \
                       no assets). `truncated` flags a capped or scan-truncated view. `elapsed_us` \
                       = server-side handler latency in µs (excludes transport).",
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
            drop(store);
            let cache = self.state.shared.cache.load_full();
            run_graph_export(
                &self.state.shared,
                idx.as_ref(),
                &cache,
                params,
                self.state.lifecycle_notice(),
                __body,
            )
        }
        .await;
        record_call(&self.state, "graph_export", &__params_json, __started, &__result);
        __result
    }
}
