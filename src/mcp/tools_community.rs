//! `#[tool]` shim for the `communities` tool (ADR-0004). Thin wrapper: await the cache, resolve
//! the index handle + map cache, delegate to `helpers_community`.

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::tool;
use serde_json::Value;

use super::BasemindServer;
use super::helpers::record_call;
use super::helpers_community::run_communities;
use super::types_community::CommunitiesParams;

#[rmcp::tool_router(vis = "pub(super)", router = "tool_router_community")]
impl BasemindServer {
    /// Community detection over the typed code-graph.
    #[tool(
        output_schema = "rmcp::handler::server::tool::schema_for_output::<super::types_community::CommunitiesResponse>()",
        description = "Communities over the unified code-graph: cluster the repo into de-facto \
                       modules — groups of symbols/files that relate to each other far more than to \
                       the rest of the repo. `algorithm`: \"label_propagation\" (default, \
                       near-linear) or \"louvain\" (opt-in, higher-quality modularity). `edges` \
                       picks the lanes (all/calls/imports/inherits/both/contains); `min_confidence` \
                       floors edge confidence. Each community carries a deterministic, LLM-free \
                       `label` (dominant path prefix + most central member), a `size`, and its \
                       `members` (most central first, each with a `centrality` score), capped by \
                       `members_per_community` (default 10, max 100). Communities are returned \
                       largest first, capped by `max_communities` (default 50, max 200). \
                       Deterministic (same repo → same partition + labels), no LLM, bounded. \
                       `truncated` flags a capped or scan-truncated result. `elapsed_us` = \
                       server-side handler latency in µs (excludes transport).",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn communities(
        &self,
        Parameters(params): Parameters<CommunitiesParams>,
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
            run_communities(idx.as_ref(), &cache, params, self.state.lifecycle_notice(), __body)
        }
        .await;
        record_call(&self.state, "communities", &__params_json, __started, &__result);
        __result
    }
}
