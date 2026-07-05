//! Helper bodies for the semantic code-search tools (`search_code`, `get_chunk`).
//!
//! Gated on `feature = "code-search"`. `run_search_code` is the vector channel (Phase 1): embed
//! the query, KNN over the LanceDB `code_chunks` table, budget + format the pointer hits.
//! `run_get_chunk` is the offline fetch half — it reads the file's content-addressed `.chunk`
//! sidecar and returns one chunk's body, no LanceDB round-trip.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::ServerState;
use super::helpers::json_result;
use super::memory::{embed_query, lance_store};
use super::types_code::{CodeSearchHit, GetChunkParams, GetChunkResponse, SearchCodeParams, SearchCodeResponse};

/// Serialize a code-search response honoring the requested wire format. TOON is only available
/// when the `documents` feature (which links `serde_toon`) is also compiled in; a `toon` request
/// on a code-search-only build silently falls back to JSON.
fn format_code_response<T: serde::Serialize>(value: &T, want_toon: bool) -> Result<CallToolResult, McpError> {
    #[cfg(feature = "documents")]
    if want_toon {
        return super::helpers::format_response(value, crate::config::OutputFormat::Toon);
    }
    let _ = want_toon;
    json_result(value)
}

/// Resolve whether the caller wants TOON output: an explicit `format` param wins; otherwise fall
/// back to the `[documents.output] format` config knob.
fn wants_toon(state: &ServerState, format: Option<&str>) -> bool {
    match format.map(str::trim) {
        Some(f) if f.eq_ignore_ascii_case("toon") => true,
        Some(f) if f.eq_ignore_ascii_case("json") => false,
        _ => matches!(state.config.documents.output.format, crate::config::OutputFormat::Toon),
    }
}

pub(super) async fn run_search_code(state: &ServerState, params: SearchCodeParams) -> Result<CallToolResult, McpError> {
    let limit = params.limit.unwrap_or(10).min(100) as usize;
    let want_toon = wants_toon(state, params.format.as_deref());

    // Retrieval lane: "semantic" (default) embeds the query and runs vector KNN; "keyword" runs the
    // native BM25 index over each chunk's lexical text. Any other value is an actionable error.
    let mode = params.mode.as_deref().map(str::trim).unwrap_or("semantic");
    let hits: Vec<CodeSearchHit> = if mode.is_empty() || mode.eq_ignore_ascii_case("semantic") {
        semantic_hits(state, &params.query, limit).await?
    } else if mode.eq_ignore_ascii_case("keyword") {
        keyword_hits(state, &params.query, limit).await
    } else {
        return Err(McpError::invalid_request(
            format!("search_code: unknown mode {mode:?}; valid modes are \"semantic\" or \"keyword\""),
            None,
        ));
    };

    // Token budget bounds the returned hits (best-first). No cursor — raise `max_tokens` for more.
    let budget = super::budget::apply_budget(hits, params.max_tokens);
    format_code_response(
        &SearchCodeResponse {
            query: params.query,
            budgeted: budget.budgeted,
            hits: budget.items,
        },
        want_toon,
    )
}

/// Semantic lane: embed the query and run vector KNN over the scope-filtered LanceDB `code_chunks`
/// table. Each hit carries an L2 `distance` (lower = closer) and no BM25 `score`.
async fn semantic_hits(state: &ServerState, query: &str, limit: usize) -> Result<Vec<CodeSearchHit>, McpError> {
    let embedding = embed_query(state, query).await?;
    let lance = lance_store(state).await?;
    let scope = state.scope.clone();
    let hits_raw = tokio::task::spawn_blocking(move || lance.search_code_chunks(&scope, embedding, limit))
        .await
        .map_err(|e| McpError::internal_error(format!("spawn_blocking: {e}"), None))?
        .map_err(|e| McpError::internal_error(format!("search_code_chunks: {e}"), None))?;

    Ok(hits_raw
        .into_iter()
        .map(|h| CodeSearchHit {
            path: h.path,
            chunk_id: h.chunk_id,
            symbol: h.symbol,
            kind: h.kind,
            lang: h.lang,
            line_start: h.line_start,
            line_end: h.line_end,
            byte_start: h.byte_start,
            byte_end: h.byte_end,
            distance: Some(h.distance),
            score: None,
        })
        .collect())
}

/// Keyword lane: native BM25 over the Fjall index, hydrating each hit's `chunk_id` back into a
/// pointer via the content-addressed chunk sidecar (the same read `run_get_chunk` uses). Each hit
/// carries a BM25 `score` (higher = better) and no `distance`. Hits whose sidecar or ordinal cannot
/// be resolved are skipped (logged at debug), never fatal. Returns an empty vec when the index is
/// read-only (no `IndexDb` handle) — there is no keyword lane on a reader session.
async fn keyword_hits(state: &ServerState, query: &str, limit: usize) -> Vec<CodeSearchHit> {
    let store = state.store.read().await;
    let Some(db) = store.index_db.as_ref() else {
        return Vec::new();
    };
    let raw = crate::search::bm25::bm25_search(db, query, limit);
    let mut hits = Vec::with_capacity(raw.len());
    for hit in raw {
        // chunk_id is `<source-hash-hex>:<ordinal>` — split on the LAST ':' (hex never contains one).
        let Some((hash_hex, ordinal)) = hit.chunk_id.rsplit_once(':') else {
            tracing::debug!(chunk_id = %hit.chunk_id, "keyword hit: malformed chunk_id, skipping");
            continue;
        };
        let Ok(ordinal) = ordinal.parse::<usize>() else {
            tracing::debug!(chunk_id = %hit.chunk_id, "keyword hit: non-numeric ordinal, skipping");
            continue;
        };
        let blob = match store.read_chunks_by_hex(hash_hex) {
            Ok(Some(blob)) => blob,
            Ok(None) => {
                tracing::debug!(hash = %hash_hex, "keyword hit: no chunk sidecar, skipping");
                continue;
            }
            Err(error) => {
                tracing::debug!(hash = %hash_hex, %error, "keyword hit: chunk sidecar read failed, skipping");
                continue;
            }
        };
        let Some(chunk) = blob.chunks.get(ordinal) else {
            tracing::debug!(chunk_id = %hit.chunk_id, "keyword hit: ordinal out of range, skipping");
            continue;
        };
        hits.push(CodeSearchHit {
            path: chunk.path.clone(),
            chunk_id: hit.chunk_id.clone(),
            symbol: chunk.symbol.clone().unwrap_or_default(),
            kind: chunk.kind.clone().unwrap_or_default(),
            lang: chunk.lang.clone(),
            line_start: chunk.line_start,
            line_end: chunk.line_end,
            byte_start: chunk.byte_start,
            byte_end: chunk.byte_end,
            distance: None,
            score: Some(hit.score),
        });
    }
    hits
}

pub(super) async fn run_get_chunk(state: &ServerState, params: GetChunkParams) -> Result<CallToolResult, McpError> {
    // Resolve the file's content hash, then read its chunk sidecar — offline, no LanceDB.
    let blob = {
        let store = state.store.read().await;
        let entry = store
            .lookup(&params.path)
            .ok_or_else(|| McpError::invalid_params(format!("get_chunk: file not indexed: {}", params.path), None))?;
        let hash_hex = entry.hash_hex.clone();
        store
            .read_chunks_by_hex(&hash_hex)
            .map_err(|e| McpError::internal_error(format!("get_chunk: read chunk blob: {e}"), None))?
            .ok_or_else(|| {
                McpError::invalid_params(
                    format!(
                        "get_chunk: no code chunks indexed for {} (scan with --features code-search)",
                        params.path
                    ),
                    None,
                )
            })?
    };

    let chunks = &blob.chunks;
    if chunks.is_empty() {
        return Err(McpError::invalid_params(
            format!("get_chunk: {} has no chunks", params.path),
            None,
        ));
    }

    // Selection: chunk_id > byte_start > single-chunk default. Ambiguity is an actionable error.
    let chunk = if let Some(id) = params.chunk_id.as_deref() {
        chunks.iter().find(|c| c.chunk_id == id).ok_or_else(|| {
            McpError::invalid_params(format!("get_chunk: chunk_id {id:?} not found in {}", params.path), None)
        })?
    } else if let Some(bs) = params.byte_start {
        chunks.iter().find(|c| c.byte_start == bs).ok_or_else(|| {
            McpError::invalid_params(
                format!("get_chunk: no chunk at byte_start {bs} in {}", params.path),
                None,
            )
        })?
    } else if chunks.len() == 1 {
        &chunks[0]
    } else {
        let ids: Vec<&str> = chunks.iter().map(|c| c.chunk_id.as_str()).collect();
        return Err(McpError::invalid_params(
            format!(
                "get_chunk: {} has {} chunks; pass `chunk_id` or `byte_start` to disambiguate: {}",
                params.path,
                chunks.len(),
                ids.join(", ")
            ),
            None,
        ));
    };

    json_result(&GetChunkResponse {
        path: chunk.path.clone(),
        chunk_id: chunk.chunk_id.clone(),
        symbol: chunk.symbol.clone(),
        kind: chunk.kind.clone(),
        lang: chunk.lang.clone(),
        signature: chunk.signature.clone(),
        doc: chunk.doc.clone(),
        line_start: chunk.line_start,
        line_end: chunk.line_end,
        byte_start: chunk.byte_start,
        byte_end: chunk.byte_end,
        text: chunk.text.clone(),
    })
}
