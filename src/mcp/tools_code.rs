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
        description = "Search indexed source-code chunks. `mode` picks the strategy: \"hybrid\" \
        (default) fuses three lanes via Reciprocal Rank Fusion — vector KNN (semantic), native BM25 \
        (keyword), and an exact symbol lane that resolves an identifier-shaped query to the chunks \
        defining that symbol; it degrades gracefully, dropping any lane that is unavailable. When the \
        index was built without embeddings (`[code_search] embed=false`, the default), hybrid runs \
        keyword+exact only and \"semantic\" mode returns an error — use \"keyword\" or \"hybrid\" \
        there. \"semantic\" is vector-only (hits carry L2 `distance`, lower = closer); \"keyword\" is \
        BM25-only (hits carry a `score`, higher = better; needs no embeddings). Set `rerank:true` for \
        an optional cross-encoder rerank over the fused hits \
        (first call downloads an ONNX model; off by default). In hybrid mode each hit carries \
        why-matched provenance: `matched_lanes` (which lanes produced it, in fixed order \
        exact→vector→keyword) plus the 1-based `exact_rank` / `vector_rank` / `keyword_rank` it held \
        in each contributing lane. \
        Returns POINTERS (path + line/byte \
        range + symbol + kind), NOT bodies — call `get_chunk` to fetch a chunk's source. Default 10, \
        max 100. `max_tokens` budgets the hits (best-first, sets `budgeted`). `format:\"toon\"` for \
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
            #[allow(unreachable_code)]
            not_enabled("code-search")
        }
        .await;
        record_call(&self.state, "get_chunk", &__params_json, __started, &__result);
        __result
    }
}
