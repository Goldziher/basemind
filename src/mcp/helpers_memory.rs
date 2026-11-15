//! Bodies of `memory_save`, `memory_search` and `docs_search`.
//!
//! The three tools are advertised in every build and gate in the body: an agent looking for
//! "remember this" or "search the PDFs" is better served a tool that answers "rebuild with
//! `--features memory`" than a tool that is not there. [`feature_missing`] is the only answer a
//! gated-off tool gives, never an empty result an agent would read as "nothing stored".

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::ServerState;
#[cfg(not(all(feature = "memory", feature = "documents")))]
use super::tool_contract::feature_missing;
use super::types_memory::MemoryDeleteParams;
use super::types_tools::{DocsSearchParams, MemorySaveParams, MemorySearchToolParams};

/// The argument shapes `memory_search` accepts, quoted in every shape error.
#[cfg(feature = "memory")]
const SEARCH_SHAPES: &str = "pass at most one of `query` (search by meaning), `key` (read one note) or `prefix` (list \
     notes by key prefix); with none of them every note is listed. `tag` filters a search or a listing; `cursor` \
     pages a listing";

/// `memory_save`: write a durable note.
#[cfg(feature = "memory")]
pub(super) async fn memory_save(state: &ServerState, p: MemorySaveParams) -> Result<CallToolResult, McpError> {
    super::memory::run_memory_put(
        state,
        super::types_memory::MemoryPutParams {
            key: p.key,
            value: p.value,
            tags: p.tags,
            embed: true,
            visibility: p.visibility.unwrap_or_default(),
        },
    )
    .await
}

/// `memory_save` in a build without the memory store.
#[cfg(not(feature = "memory"))]
pub(super) async fn memory_save(_state: &ServerState, _p: MemorySaveParams) -> Result<CallToolResult, McpError> {
    Err(feature_missing("memory_save", "memory"))
}

/// `memory_search`: read one note, list notes, or search them by meaning.
#[cfg(feature = "memory")]
pub(super) async fn memory_search(state: &ServerState, p: MemorySearchToolParams) -> Result<CallToolResult, McpError> {
    use super::types_memory::{MemoryGetParams, MemoryListParams, MemorySearchParams};

    let shape_error =
        |problem: &str| McpError::invalid_params(format!("`memory_search`: {problem}; {SEARCH_SHAPES}"), None);
    let visibility = p.visibility.unwrap_or_default();
    match (p.query, p.key, p.prefix) {
        (Some(query), None, None) => {
            if p.cursor.is_some() {
                return Err(shape_error(
                    "a `query` search is ranked and takes no `cursor`; raise `limit`",
                ));
            }
            super::memory::run_memory_search(
                state,
                MemorySearchParams {
                    query,
                    limit: p.limit,
                    tag: p.tag,
                    visibility,
                },
            )
            .await
        }
        (None, Some(key), None) => {
            if p.tag.is_some() || p.limit.is_some() || p.cursor.is_some() {
                return Err(shape_error(
                    "a `key` read returns one note and takes no `tag`, `limit` or `cursor`",
                ));
            }
            super::memory::run_memory_get(state, MemoryGetParams { key, visibility }).await
        }
        (None, None, prefix) => {
            super::memory::run_memory_list(
                state,
                MemoryListParams {
                    prefix,
                    tag: p.tag,
                    limit: p.limit,
                    cursor: p.cursor,
                    visibility,
                },
            )
            .await
        }
        _ => Err(shape_error("`query`, `key` and `prefix` are alternatives")),
    }
}

/// `memory_search` in a build without the memory store.
#[cfg(not(feature = "memory"))]
pub(super) async fn memory_search(
    _state: &ServerState,
    _p: MemorySearchToolParams,
) -> Result<CallToolResult, McpError> {
    Err(feature_missing("memory_search", "memory"))
}

/// Delete one note. No MCP tool: `basemind memory delete`.
#[cfg(feature = "memory")]
pub(super) async fn memory_delete(state: &ServerState, p: MemoryDeleteParams) -> Result<CallToolResult, McpError> {
    super::memory::run_memory_delete(state, p).await
}

/// `basemind memory delete` in a build without the memory store.
#[cfg(not(feature = "memory"))]
pub(super) async fn memory_delete(_state: &ServerState, _p: MemoryDeleteParams) -> Result<CallToolResult, McpError> {
    Err(feature_missing("memory delete", "memory"))
}

/// `docs_search`: semantic search over indexed documents.
#[cfg(feature = "documents")]
pub(super) async fn docs_search(state: &ServerState, p: DocsSearchParams) -> Result<CallToolResult, McpError> {
    use super::helpers_page::{RANKED_DEPTH, RANKED_LIMIT_DEFAULT, page_ranked, ranked_offset};

    let offset = ranked_offset(p.cursor.as_ref())?;
    let limit = p.limit.unwrap_or(RANKED_LIMIT_DEFAULT);
    let result = super::memory::run_search_documents(
        state,
        super::types::SearchDocumentsParams {
            query: p.query,
            limit: Some(RANKED_DEPTH),
            max_tokens: None,
            // The page is cut out of the structured response, which only the JSON encoding has, so
            // `[documents.output] format = "toon"` does not apply to this tool.
            format: Some("json".to_string()),
            mime_type: p.mime_type,
            scope: p.scope,
            entity_category: p.entity,
            keywords_contains: p.keywords,
        },
    )
    .await?;
    page_ranked(
        result,
        "hits",
        offset,
        limit,
        super::tool_contract::budget(p.max_tokens),
    )
}

/// `docs_search` in a build without the document tier.
#[cfg(not(feature = "documents"))]
pub(super) async fn docs_search(_state: &ServerState, _p: DocsSearchParams) -> Result<CallToolResult, McpError> {
    Err(feature_missing("docs_search", "documents"))
}
