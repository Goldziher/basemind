//! `#[tool]` shim for `architecture_map`. Thin wrapper: resolve the index handle + map
//! cache, compute the optional churn overlay, delegate to `helpers_archmap`.

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::tool;
use serde_json::Value;

use super::BasemindServer;
use super::helpers::record_call;
use super::helpers_archmap::{churn_commit_counts, run_architecture_map};
use super::types_archmap::ArchitectureMapParams;

#[rmcp::tool_router(vis = "pub(super)", router = "tool_router_archmap")]
impl BasemindServer {
    /// Deterministic architecture overview from the call graph.
    #[tool(
        description = "Architecture map of the repo (or a `focus` subtree), ranked by graph \
                       centrality + git churn. `granularity`: \"module\" (default, directory-level \
                       dependency graph), \"file\", or \"symbol\" (top hub functions by fan-in). \
                       Module/file tiers return PageRank, fan-in/out, and circular-dependency \
                       clusters (SCCs); the symbol tier returns hub functions with signatures + \
                       their edges. Edges are name-based (like call_graph): overloaded names may \
                       produce a few spurious edges — discount by `weight`. Deterministic, no LLM. \
                       Results are knee-cut to the significant head and capped (`max_nodes`, \
                       `max_edges`, `max_tokens`).",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn architecture_map(
        &self,
        Parameters(params): Parameters<ArchitectureMapParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&params).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = async {
            let store = self.state.store.read().await;
            let idx = store.index_db.as_ref().cloned();
            drop(store);
            let cache = self.state.cache.load_full();
            // Churn overlay is best-effort: absent outside a git repo (degrade silently).
            let churn = if params.include_churn {
                let window = params.churn_window.unwrap_or(200).min(2000);
                churn_commit_counts(&self.state, window).ok()
            } else {
                None
            };
            run_architecture_map(idx.as_ref(), &cache, churn.as_ref(), params)
        }
        .await;
        record_call(&self.state, "architecture_map", &__params_json, __started, &__result);
        __result
    }
}
