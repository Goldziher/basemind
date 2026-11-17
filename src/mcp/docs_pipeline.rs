//! `docs_search`: the MCP adapter of the document search engine (`crate::lance::docs_rank`,
//! `crate::lance::docs_page`). It gathers what the engine works over from the server state (the
//! view's document map, the store, the query embedder, the reranker setting), runs the engine off
//! the async runtime, keeps the ranking for the pages that follow and maps the engine's errors to
//! tool errors.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::ServerState;
use super::types_documents::DocsSearchResponse;
use super::types_tools::DocsSearchParams;
use crate::lance::docs::DocStore;
use crate::lance::docs_rank::{DocsRequest, DocsSearchError, Filters, LiveDocs, RankInputs, RerankSettings};
use crate::lance::docs_types::DocsRanking;

fn tool_error(error: DocsSearchError) -> McpError {
    let message = format!("`docs_search`: {error}");
    match error {
        DocsSearchError::InvalidArgument(_) => McpError::invalid_params(message, None),
        DocsSearchError::Unsupported(_) | DocsSearchError::NoLane(_) => McpError::invalid_request(message, None),
        DocsSearchError::Store(_) => McpError::internal_error(message, None),
    }
}

fn join_error(error: tokio::task::JoinError) -> McpError {
    McpError::internal_error(format!("spawn_blocking: {error}"), None)
}

/// The view's document store for the configured preset, or `None` when no scan has written one.
/// Opened without the embedding model: the lexical lane needs none.
async fn docs_store(state: &ServerState) -> Result<Option<DocStore>, McpError> {
    let preset = state.shared.config.documents.embedding_preset.clone();
    let dim = crate::scanner_docs::preset_dim(&preset)
        .map_err(|error| McpError::invalid_request(format!("`docs_search`: {error}"), None))?;
    let parent = state.shared.store.read().await.view_dir.clone();
    tokio::task::spawn_blocking(move || {
        DocStore::shared_existing(&crate::lance::docs::store_dir(&parent, &preset, dim), dim, &preset)
    })
    .await
    .map_err(join_error)?
    .map_err(|error| {
        McpError::invalid_request(
            format!("`docs_search`: the document store did not open: {error:#}"),
            None,
        )
    })
}

/// Rank the query: gather the inputs here, run the engine on a blocking thread.
async fn rank(
    state: &ServerState,
    request: DocsRequest,
    filters: Filters,
    store: Option<DocStore>,
) -> Result<DocsRanking, McpError> {
    let (live, owed) = {
        let index = state.shared.store.read().await;
        (
            LiveDocs::of_store(&index, filters.path.as_ref()),
            index.pending_vectors.docs().len(),
        )
    };
    let config = &state.shared.config;
    // The embedder is loaded on the async side; embedding a query runs on the blocking thread.
    let embedder = if config.documents.embed {
        super::memory::shared_embedder(state)
            .await
            .map_err(|error| format!("the embedding model could not load: {error}"))
    } else {
        Err("embeddings are off (`[documents] embed = false`)".to_string())
    };
    let reranker = &config.documents.reranker;
    let rerank = reranker.enabled.then(|| RerankSettings {
        preset: reranker.preset.clone(),
        candidates: reranker.top_k,
        acceleration: crate::embeddings::acceleration(config.resources.onnx_provider),
    });
    tokio::task::spawn_blocking(move || {
        let embed = |text: &str| -> Result<Vec<f32>, String> {
            match &embedder {
                Ok(embedder) => embedder.embed_query(text).map_err(|error| error.to_string()),
                Err(reason) => Err(reason.clone()),
            }
        };
        let inputs = RankInputs {
            store: store.as_ref(),
            live,
            owed,
            embed: match &embedder {
                Ok(_) => Ok(&embed),
                Err(reason) => Err(reason.clone()),
            },
            rerank,
        };
        crate::lance::docs_rank::rank(&request.query, &filters, inputs)
    })
    .await
    .map_err(join_error)?
    .map_err(tool_error)
}

/// `docs_search`: the ranking of the query (computed once per index generation and kept), and the
/// page of it the cursor points at.
pub(super) async fn docs_search(state: &ServerState, p: DocsSearchParams) -> Result<CallToolResult, McpError> {
    use super::helpers_page::{RANKED_LIMIT_DEFAULT, ranked_offset};

    let started = std::time::Instant::now();
    let generation = state.shared.index_generation.load(std::sync::atomic::Ordering::Relaxed);
    let offset = ranked_offset("docs_search", p.cursor.as_ref(), generation)?;
    let limit = p.limit.unwrap_or(RANKED_LIMIT_DEFAULT);
    let request = DocsRequest {
        query: p.query,
        mime_type: p.mime_type,
        kind: p.kind,
        entity: p.entity,
        keywords: p.keywords,
        path: p.path,
    };
    let filters =
        Filters::parse(&request, crate::extract::doc_tiers::entity_filter_unavailable()).map_err(tool_error)?;
    let key = super::ranked_cache::RankedCache::key(
        "docs_search",
        &serde_json::json!({
            "query": &request.query,
            "mime_type": &filters.mime_type,
            "kind": &filters.kind,
            "entity": &filters.entity,
            "keywords": &filters.keywords,
            "path": &request.path,
        }),
    );
    let store = docs_store(state).await?;
    let cache = &state.shared.ranked;
    let ranking = match cache
        .get(&key, generation)
        .and_then(|body| serde_json::from_value::<DocsRanking>(body).ok())
    {
        Some(ranking) => ranking,
        None => {
            let ranking = rank(state, request, filters, store.clone()).await?;
            if crate::lance::docs_rank::is_cacheable(&ranking)
                && let Ok(body) = serde_json::to_value(&ranking)
            {
                cache.put(key, generation, &body);
            }
            ranking
        }
    };
    let max_tokens = super::tool_contract::budget(p.max_tokens);
    let page = tokio::task::spawn_blocking(move || {
        crate::lance::docs_page::page(store.as_ref(), ranking, offset, limit, max_tokens)
    })
    .await
    .map_err(join_error)?;
    let next_cursor = page
        .next_offset
        .map(|next| super::cursor::Cursor::encode_in_memory(u64::from(next), generation));
    super::helpers::json_result(&DocsSearchResponse {
        page,
        next_cursor,
        elapsed_us: super::helpers::elapsed_us(started),
    })
}
