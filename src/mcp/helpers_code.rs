//! Helper bodies for the code-search tools (`search_code`, `get_chunk`).
//!
//! Gated on `feature = "code-search"`. `run_search_code` dispatches by `mode`: `hybrid` (default)
//! fuses the vector, keyword, and exact-symbol lanes via RRF ([`hybrid_hits`]); `semantic`
//! ([`semantic_hits`]) is vector KNN over the LanceDB `code_chunks` table; `keyword`
//! ([`keyword_hits`]) is native BM25 over the Fjall index. An optional cross-encoder [`rerank_hits`]
//! pass reorders the result. `run_get_chunk` is the offline fetch half — it reads the file's
//! content-addressed `.chunk` sidecar and returns one chunk's body, no LanceDB round-trip.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::ServerState;
use super::helpers::json_result;
use super::memory::{embed_query, lance_store};
use super::types_code::{CodeSearchHit, GetChunkParams, GetChunkResponse, SearchCodeParams, SearchCodeResponse};
use crate::search::bm25::bm25_search;
use crate::search::exact::exact_lane_chunk_ids;
use crate::search::rrf::{DEFAULT_RRF_K, FusionLane, WEIGHT_EXACT, WEIGHT_KEYWORD, WEIGHT_VECTOR, rrf_fuse};
use crate::store::Store;

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

    // Rerank config: per-query override wins over the `[code_search.reranker]` knob.
    let rr = &state.config.code_search.reranker;
    let rerank_enabled = params.reranker_enabled.unwrap_or(rr.enabled);
    let rerank_preset = params.reranker_preset.clone().unwrap_or_else(|| rr.preset.clone());
    let rerank_top_k = params.reranker_top_k.unwrap_or(rr.top_k);

    // When reranking, fetch at least `rerank_top_k` candidates so the cross-encoder has material.
    let fetch_n = if rerank_enabled { limit.max(rerank_top_k) } else { limit };

    // Retrieval lane: "hybrid" (default) fuses vector + keyword + exact via RRF; "semantic" is
    // vector KNN only; "keyword" is BM25 only. Any other value is an actionable error.
    let mode = params.mode.as_deref().map(str::trim).unwrap_or("hybrid");
    let hits: Vec<CodeSearchHit> = if mode.is_empty() || mode.eq_ignore_ascii_case("hybrid") {
        hybrid_hits(state, &params.query, fetch_n).await
    } else if mode.eq_ignore_ascii_case("semantic") {
        semantic_hits(state, &params.query, fetch_n).await?
    } else if mode.eq_ignore_ascii_case("keyword") {
        keyword_hits(state, &params.query, fetch_n).await
    } else {
        return Err(McpError::invalid_request(
            format!("search_code: unknown mode {mode:?}; valid modes are \"hybrid\", \"semantic\", or \"keyword\""),
            None,
        ));
    };

    // Optional cross-encoder rerank over the produced hits (reorders + truncates to `rerank_top_k`).
    let hits = if rerank_enabled {
        rerank_hits(state, &params.query, hits, &rerank_preset, rerank_top_k).await?
    } else {
        hits
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

/// Hybrid lane: run the vector, keyword, and exact lanes best-effort and fuse their rankings via RRF
/// on the shared `chunk_id` key. Each lane is independent — a lane that is unavailable (no embedder,
/// read-only index) or that a non-identifier query doesn't trigger simply contributes nothing; the
/// query never fails on a single lane. The returned hits carry the fused RRF score in `score`.
async fn hybrid_hits(state: &ServerState, query: &str, limit: usize) -> Vec<CodeSearchHit> {
    // Each lane fetches an expanded candidate set so fusion has material below rank-1.
    let fuse_limit = (limit * 4).clamp(limit, 200);

    // Vector lane (best-effort): only when embeddings are configured; any failure drops the lane.
    let vector_ids: Vec<String> = if state.config.code_search.embed {
        match semantic_hits(state, query, fuse_limit).await {
            Ok(hits) => hits.into_iter().map(|h| h.chunk_id).collect(),
            Err(error) => {
                tracing::debug!(%error, "hybrid: vector lane unavailable — fusing keyword + exact only");
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    // Keyword + exact lanes + hydration under a single store read lock.
    let store = state.store.read().await;
    let (keyword_ids, exact_ids): (Vec<String>, Vec<String>) = match store.index_db.as_ref() {
        Some(db) => (
            bm25_search(db, query, fuse_limit)
                .into_iter()
                .map(|h| h.chunk_id)
                .collect(),
            exact_lane_chunk_ids(&store, db, query, fuse_limit),
        ),
        None => (Vec::new(), Vec::new()),
    };

    let fused = rrf_fuse(
        &[
            FusionLane::new(&exact_ids, WEIGHT_EXACT),
            FusionLane::new(&vector_ids, WEIGHT_VECTOR),
            FusionLane::new(&keyword_ids, WEIGHT_KEYWORD),
        ],
        DEFAULT_RRF_K,
    );

    let mut hits = Vec::with_capacity(fused.len().min(limit));
    for (chunk_id, score) in fused.into_iter().take(limit) {
        if let Some((mut hit, _text)) = hydrate_one(&store, &chunk_id) {
            hit.score = Some(score);
            hits.push(hit);
        }
    }
    hits
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
            rerank_score: None,
        })
        .collect())
}

/// Keyword lane: native BM25 over the Fjall index, hydrating each ranked `chunk_id` into a pointer.
/// Each hit carries a BM25 `score` (higher = better) and no `distance`. Returns an empty vec when the
/// index is read-only (no `IndexDb` handle) — there is no keyword lane on a reader session.
async fn keyword_hits(state: &ServerState, query: &str, limit: usize) -> Vec<CodeSearchHit> {
    let store = state.store.read().await;
    let Some(db) = store.index_db.as_ref() else {
        return Vec::new();
    };
    let raw = bm25_search(db, query, limit);
    let mut hits = Vec::with_capacity(raw.len());
    for hit in raw {
        if let Some((mut ch, _text)) = hydrate_one(&store, &hit.chunk_id) {
            ch.score = Some(hit.score);
            hits.push(ch);
        }
    }
    hits
}

/// Hydrate a ranked `chunk_id` (`<hash>:<ordinal>`) into a base `CodeSearchHit` (all score fields
/// `None`) plus the chunk's body text (for the optional rerank pass), via the content-addressed
/// sidecar. `None` when the sidecar is missing or the ordinal is out of range — the caller skips it.
fn hydrate_one(store: &Store, chunk_id: &str) -> Option<(CodeSearchHit, String)> {
    // chunk_id is `<source-hash-hex>:<ordinal>` — split on the LAST ':' (hex never contains one).
    let (hash_hex, ordinal) = chunk_id.rsplit_once(':')?;
    let ordinal: usize = ordinal.parse().ok()?;
    let blob = store.read_chunks_by_hex(hash_hex).ok()??;
    let chunk = blob.chunks.get(ordinal)?;
    let hit = CodeSearchHit {
        path: chunk.path.clone(),
        chunk_id: chunk_id.to_string(),
        symbol: chunk.symbol.clone().unwrap_or_default(),
        kind: chunk.kind.clone().unwrap_or_default(),
        lang: chunk.lang.clone(),
        line_start: chunk.line_start,
        line_end: chunk.line_end,
        byte_start: chunk.byte_start,
        byte_end: chunk.byte_end,
        distance: None,
        score: None,
        rerank_score: None,
    };
    Some((hit, chunk.text.clone()))
}

/// Optional cross-encoder rerank of `hits`, reusing the same xberg reranker as the documents tier.
/// Reads each hit's chunk body as the candidate text, scores against `query`, and returns the hits
/// reordered best-first (truncated to `top_k`) with `rerank_score` set. Off-path when `hits` is
/// empty. Errors on an unknown preset (before any model download) or an out-of-range rerank index.
async fn rerank_hits(
    state: &ServerState,
    query: &str,
    hits: Vec<CodeSearchHit>,
    preset: &str,
    top_k: usize,
) -> Result<Vec<CodeSearchHit>, McpError> {
    if hits.is_empty() {
        return Ok(hits);
    }
    // Fail fast on an unknown preset before constructing the config / triggering a wrong download.
    if xberg::get_reranker_preset(preset).is_none() {
        return Err(McpError::invalid_params(
            format!("unknown reranker preset: {preset:?}"),
            None,
        ));
    }
    // Candidate texts aligned 1:1 with `hits` (empty when a sidecar read fails — the reranker
    // tolerates it and just scores it low).
    let texts: Vec<String> = {
        let store = state.store.read().await;
        hits.iter()
            .map(|h| {
                hydrate_one(&store, &h.chunk_id)
                    .map(|(_, text)| text)
                    .unwrap_or_default()
            })
            .collect()
    };
    let krz_config = xberg::core::config::RerankerConfig {
        model: xberg::core::config::RerankerModelType::Preset {
            name: preset.to_string(),
        },
        top_k: Some(top_k),
        ..Default::default()
    };
    let reranked = xberg::rerank_async(query.to_string(), texts, &krz_config)
        .await
        .map_err(|e| {
            let msg = e.to_string();
            let kind = if msg.contains("download") || msg.contains("HuggingFace") || msg.contains("model") {
                "rerank model load"
            } else {
                "rerank inference"
            };
            McpError::internal_error(format!("{kind}: {msg}"), None)
        })?;
    // Bounds-check every external index — a buggy ONNX run must not panic the server.
    let original = hits;
    reranked
        .into_iter()
        .map(|r| {
            original
                .get(r.index)
                .cloned()
                .map(|mut hit| {
                    hit.rerank_score = Some(r.score);
                    hit
                })
                .ok_or_else(|| {
                    McpError::internal_error(
                        format!(
                            "reranker returned out-of-range index {} (got {} hits)",
                            r.index,
                            original.len()
                        ),
                        None,
                    )
                })
        })
        .collect()
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
