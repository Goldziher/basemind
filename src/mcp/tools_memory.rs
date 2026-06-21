//! Memory + document-search tool shims for `BasemindServer`.
//!
//! Kept in a separate file so `tools.rs` stays under the 1000-line cap.
//! Each shim delegates to `memory::run_*` helpers and returns a graceful
//! MCP error when the gating feature is not enabled.

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::tool;
use serde_json::Value;

use super::BasemindServer;
use super::helpers::record_call;
use super::lenient::Lenient;
use super::types::SearchDocumentsParams;
use super::types_memory::{
    MemoryDeleteParams, MemoryGetParams, MemoryListParams, MemoryPutParams, MemorySearchParams,
};

fn not_enabled(feature: &'static str) -> Result<CallToolResult, McpError> {
    Err(McpError::invalid_request(
        format!("{feature} feature not enabled — rebuild with --features {feature}"),
        None,
    ))
}

#[rmcp::tool_router(vis = "pub(super)", router = "tool_router_memory")]
impl BasemindServer {
    #[tool(
        description = "Persist a key-value in scoped memory (scope = git remote URL). Upsert \
        semantics. `embed=true` also stores in LanceDB for `memory_search`. Needs --features \
        memory."
    )]
    pub(crate) async fn memory_put(
        &self,
        Parameters(p): Parameters<MemoryPutParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = async {
            #[cfg(feature = "memory")]
            {
                return super::memory::run_memory_put(&self.state, p).await;
            }
            #[cfg(not(feature = "memory"))]
            {
                let _ = p;
                return not_enabled("memory");
            }
            #[allow(unreachable_code)]
            not_enabled("memory")
        }
        .await;
        record_call(
            &self.state,
            "memory_put",
            &__params_json,
            __started,
            &__result,
        );
        __result
    }

    #[tool(description = "Exact-key lookup in scoped memory. Returns entry \
        (key,value,tags,timestamps) or null. Fjall only, no vector touch. \
        Needs --features memory.")]
    pub(crate) async fn memory_get(
        &self,
        Parameters(p): Parameters<MemoryGetParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = async {
            #[cfg(feature = "memory")]
            {
                return super::memory::run_memory_get(&self.state, p).await;
            }
            #[cfg(not(feature = "memory"))]
            {
                let _ = p;
                return not_enabled("memory");
            }
            #[allow(unreachable_code)]
            not_enabled("memory")
        }
        .await;
        record_call(
            &self.state,
            "memory_get",
            &__params_json,
            __started,
            &__result,
        );
        __result
    }

    #[tool(
        description = "List scoped memory entries. `prefix` is a key-prefix filter (not \
        substring); `tag` is exact. Values truncated ~200 chars. Default 100, max 1000. \
        `cursor` pages results. Needs --features memory."
    )]
    pub(crate) async fn memory_list(
        &self,
        Parameters(p): Parameters<MemoryListParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = async {
            #[cfg(feature = "memory")]
            {
                return super::memory::run_memory_list(&self.state, p).await;
            }
            #[cfg(not(feature = "memory"))]
            {
                let _ = p;
                return not_enabled("memory");
            }
            #[allow(unreachable_code)]
            not_enabled("memory")
        }
        .await;
        record_call(
            &self.state,
            "memory_list",
            &__params_json,
            __started,
            &__result,
        );
        __result
    }

    #[tool(
        description = "Vector KNN over stored memory. Embeds `query`, KNN in the scope-filtered \
        LanceDB memory table; `tag` is a post-KNN exact filter. Default 10, max 100 by L2 \
        distance. Needs --features memory."
    )]
    pub(crate) async fn memory_search(
        &self,
        Parameters(p): Parameters<MemorySearchParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = async {
            #[cfg(feature = "memory")]
            {
                return super::memory::run_memory_search(&self.state, p).await;
            }
            #[cfg(not(feature = "memory"))]
            {
                let _ = p;
                return not_enabled("memory");
            }
            #[allow(unreachable_code)]
            not_enabled("memory")
        }
        .await;
        record_call(
            &self.state,
            "memory_search",
            &__params_json,
            __started,
            &__result,
        );
        __result
    }

    #[tool(
        description = "Delete memory entry by exact key from Fjall and LanceDB. \
        Returns {deleted:true} when found. Needs --features memory."
    )]
    pub(crate) async fn memory_delete(
        &self,
        Parameters(p): Parameters<MemoryDeleteParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = async {
            #[cfg(feature = "memory")]
            {
                return super::memory::run_memory_delete(&self.state, p).await;
            }
            #[cfg(not(feature = "memory"))]
            {
                let _ = p;
                return not_enabled("memory");
            }
            #[allow(unreachable_code)]
            not_enabled("memory")
        }
        .await;
        record_call(
            &self.state,
            "memory_delete",
            &__params_json,
            __started,
            &__result,
        );
        __result
    }

    #[tool(
        description = "Semantic search over indexed document chunks (PDF/Office/HTML). Embeds \
        `query`, KNN in the scope-filtered LanceDB documents table; `mime_type` is an exact \
        filter. Default 10, max 100. `max_tokens` budgets the hits (best-first, sets `budgeted`; \
        no cursor — raise it for more). `format:\"toon\"` for compact rows (overrides config). \
        Needs --features documents."
    )]
    pub(crate) async fn search_documents(
        &self,
        Parameters(Lenient(p)): Parameters<Lenient<SearchDocumentsParams>>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = async {
            #[cfg(feature = "documents")]
            {
                return super::memory::run_search_documents(&self.state, p).await;
            }
            #[cfg(not(feature = "documents"))]
            {
                let _ = p;
                return not_enabled("documents");
            }
            #[allow(unreachable_code)]
            not_enabled("documents")
        }
        .await;
        record_call(
            &self.state,
            "search_documents",
            &__params_json,
            __started,
            &__result,
        );
        __result
    }
}
