//! Semantic code-search tool shims for `BasemindServer` (`search_code`, `get_chunk`).
//!
//! Always compiled — the shims register regardless of the `code-search` feature and return a
//! graceful MCP error when it is not enabled (mirrors `tools_memory.rs`). The bodies delegate to
//! `helpers_code::run_*` when the feature is on.

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::tool;
use serde_json::Value;

use super::BasemindServer;
use super::helpers::record_call;
use super::lenient::Lenient;
use super::types_code::{GetChunkParams, SearchCodeParams};

fn not_enabled(feature: &'static str) -> Result<CallToolResult, McpError> {
    Err(McpError::invalid_request(
        format!(
            "this tool requires the `{feature}` feature, which is not compiled into this \
             basemind binary. Rebuild with `--features {feature}` (the published release \
             binary includes it)."
        ),
        None,
    ))
}

#[rmcp::tool_router(vis = "pub(super)", router = "tool_router_code")]
impl BasemindServer {
    #[tool(
        description = "Semantic (vector KNN) search over indexed source-code chunks. Embeds \
        `query` and finds the nearest code chunks in the scope-filtered LanceDB `code_chunks` \
        table. Returns POINTERS (path + line/byte range + symbol + kind + distance), NOT bodies — \
        call `get_chunk` to fetch a chunk's source. Default 10, max 100. `max_tokens` budgets the \
        hits (best-first, sets `budgeted`; no cursor — raise it for more). `format:\"toon\"` for \
        compact rows. Needs --features code-search.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn search_code(
        &self,
        Parameters(Lenient(p)): Parameters<Lenient<SearchCodeParams>>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = async {
            #[cfg(feature = "code-search")]
            {
                return super::helpers_code::run_search_code(&self.state, p).await;
            }
            #[cfg(not(feature = "code-search"))]
            {
                let _ = p;
                return not_enabled("code-search");
            }
            // Unreachable when `code-search` is compiled in (the cfg block above returns); kept so
            // the async block has a tail expression under either feature configuration.
            #[allow(unreachable_code)]
            not_enabled("code-search")
        }
        .await;
        record_call(&self.state, "search_code", &__params_json, __started, &__result);
        __result
    }

    #[tool(
        description = "Fetch one code chunk's source body by `path` (from a `search_code` hit). \
        Disambiguate within a file with `chunk_id` or `byte_start`; both may be omitted when the \
        file has a single chunk. Returns the chunk text plus its symbol, signature, doc, and \
        line/byte range. The fetch half of the `search_code` → `get_chunk` two-call pattern. \
        Needs --features code-search.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn get_chunk(
        &self,
        Parameters(Lenient(p)): Parameters<Lenient<GetChunkParams>>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = async {
            #[cfg(feature = "code-search")]
            {
                return super::helpers_code::run_get_chunk(&self.state, p).await;
            }
            #[cfg(not(feature = "code-search"))]
            {
                let _ = p;
                return not_enabled("code-search");
            }
            // Unreachable when `code-search` is compiled in (the cfg block above returns); kept so
            // the async block has a tail expression under either feature configuration.
            #[allow(unreachable_code)]
            not_enabled("code-search")
        }
        .await;
        record_call(&self.state, "get_chunk", &__params_json, __started, &__result);
        __result
    }
}
