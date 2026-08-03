//! `#[tool]` shims for the graph-traversal tools `neighbors`, `path`, and `subgraph`
//! (ADR-0003). Thin wrappers: await the cache, resolve the index handle + map cache, delegate
//! to `helpers_traverse`.

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::tool;
use serde_json::Value;

use super::BasemindServer;
use super::helpers::record_call;
use super::helpers_traverse::{run_neighbors, run_path, run_subgraph};
use super::types_traverse::{NeighborsParams, PathParams, SubgraphParams};

#[rmcp::tool_router(vis = "pub(super)", router = "tool_router_traverse")]
impl BasemindServer {
    /// N-hop neighborhood around a symbol over the typed code-graph.
    #[tool(
        output_schema = "rmcp::handler::server::tool::schema_for_output::<super::types_traverse::NeighborsResponse>()",
        description = "Neighbors of a symbol over the unified code-graph: the N-hop neighborhood \
                       reachable from every definition site of `name`. `direction`: \"both\" \
                       (default), \"out\" (what the symbol reaches), \"in\" (what reaches it; \
                       accepts call_graph's callees/callers). `edges` picks the lanes \
                       (all/calls/imports/inherits/both/contains); `depth` (default 2, max 4) the \
                       radius; `min_confidence` floors edge confidence. Every edge carries \
                       `kind` + `provenance` (extracted/inferred/ambiguous) + numeric \
                       `confidence` (ADR-0002); node refs in edges index into `nodes`. \
                       Deterministic, no LLM, bounded (`max_nodes`, default 100, max 500). An \
                       unresolved `name` returns an empty result, not an error. \
                       `elapsed_us` = server-side handler latency in µs (excludes transport).",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn neighbors(
        &self,
        Parameters(params): Parameters<NeighborsParams>,
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
            run_neighbors(
                &self.state.shared,
                idx.as_ref(),
                &cache,
                params,
                self.state.lifecycle_notice(),
                __body,
            )
        }
        .await;
        record_call(&self.state, "neighbors", &__params_json, __started, &__result);
        __result
    }

    /// Confidence-weighted shortest path between two symbols over the typed code-graph.
    #[tool(
        output_schema = "rmcp::handler::server::tool::schema_for_output::<super::types_traverse::PathResponse>()",
        description = "Shortest path from one symbol to another over the unified code-graph: how \
                       `from` reaches `to` across mixed edge kinds (calls/imports/inherits). \
                       Confidence-weighted — a proven (extracted) edge is preferred over an \
                       inferred one of equal hop length (ADR-0002). Directed (follows forward \
                       edges). Containment is excluded by default (structurally valid but \
                       meaningless routes); set `include_contains` to add it. `edges` restricts \
                       lanes; `min_confidence` floors edge confidence. Returns `found`, the \
                       ordered `nodes`/`edges`, and total `cost` (lower = shorter/more proven). \
                       Deterministic, no LLM, bounded. `elapsed_us` = server-side latency in µs.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn path(&self, Parameters(params): Parameters<PathParams>) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&params).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = async {
            let __body = std::time::Instant::now();
            self.state.await_cache_ready().await;
            let store = self.state.shared.store.read().await;
            let idx = store.index_db.as_ref().cloned();
            drop(store);
            let cache = self.state.shared.cache.load_full();
            run_path(
                &self.state.shared,
                idx.as_ref(),
                &cache,
                params,
                self.state.lifecycle_notice(),
                __body,
            )
        }
        .await;
        record_call(&self.state, "path", &__params_json, __started, &__result);
        __result
    }

    /// Readable neighborhood subgraph around a symbol, cut to the central head.
    #[tool(
        output_schema = "rmcp::handler::server::tool::schema_for_output::<super::types_traverse::SubgraphResponse>()",
        description = "Subgraph around a symbol over the unified code-graph: the neighborhood \
                       within `depth` (default 2, max 4) hops, cut to the `max_nodes` (default \
                       30, max 200) most central nodes so the result is a readable subgraph, not \
                       a dump. Roots are always kept. `edges` picks the lanes; `min_confidence` \
                       floors edge confidence. Nodes carry a `centrality` score; every edge \
                       carries `kind`/`provenance`/`confidence` (ADR-0002); edge node refs index \
                       into `nodes`. Deterministic, no LLM. `truncated` flags when nodes were \
                       cut; an unresolved `name` returns an empty result, not an error. \
                       `elapsed_us` = server-side handler latency in µs (excludes transport).",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn subgraph(
        &self,
        Parameters(params): Parameters<SubgraphParams>,
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
            run_subgraph(
                &self.state.shared,
                idx.as_ref(),
                &cache,
                params,
                self.state.lifecycle_notice(),
                __body,
            )
        }
        .await;
        record_call(&self.state, "subgraph", &__params_json, __started, &__result);
        __result
    }
}
