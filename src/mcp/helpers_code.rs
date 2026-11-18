//! Body of the `code_search` verb tool, which `basemind code search` also runs. The other code
//! tools are engine operations (`src/engine/ops/`).

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::ServerState;
use super::types_tools::CodeSearchParams;

/// `code_search`: semantic / hybrid search returning pointers.
pub(super) async fn code_search(state: &ServerState, p: CodeSearchParams) -> Result<CallToolResult, McpError> {
    #[cfg(feature = "code-search")]
    {
        use super::helpers_page::{RANKED_FETCH, RANKED_LIMIT_DEFAULT, page_ranked, ranked_offset, ranked_retrieval};

        let generation = state.shared.index_generation.load(std::sync::atomic::Ordering::Relaxed);
        let offset = ranked_offset("code_search", p.cursor.as_ref(), generation)?;
        let limit = p.limit.unwrap_or(RANKED_LIMIT_DEFAULT);
        let ranking = serde_json::json!({ "query": &p.query, "lane": &p.lane });
        let search = super::types_code::SearchCodeParams {
            query: p.query,
            limit: Some(RANKED_FETCH),
            max_tokens: None,
            // The page is cut out of the structured response, which only the JSON encoding has, so
            // `[documents.output] format = "toon"` does not apply to this tool.
            format: Some("json".to_string()),
            mode: p.lane,
            reranker_enabled: None,
            reranker_preset: None,
            reranker_top_k: None,
        };
        let cache = &state.shared.ranked;
        let result = ranked_retrieval(cache, "code_search", &ranking, "hits", generation, || {
            super::helpers_code_search::run_search_code(state, search)
        })
        .await?;
        page_ranked(
            result,
            "hits",
            offset,
            limit,
            super::tool_contract::budget(p.max_tokens),
            generation,
        )
    }
    #[cfg(not(feature = "code-search"))]
    {
        let _ = (state, p);
        Err(super::tool_contract::feature_missing("code_search", "code-search"))
    }
}
