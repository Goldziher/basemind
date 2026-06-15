//! Helper bodies for the 6 memory + document-search MCP tools.

use std::sync::Arc;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::ServerState;
#[cfg(feature = "documents")]
use super::helpers::format_response;
use super::helpers::json_result;
#[cfg(feature = "documents")]
use super::types::{DocumentSearchHit, SearchDocumentsParams, SearchDocumentsResponse};
#[cfg(feature = "memory")]
use super::types::{
    MemoryDeleteParams, MemoryDeleteResponse, MemoryEntry, MemoryGetParams, MemoryListParams,
    MemoryListResponse, MemoryPutParams, MemoryPutResponse, MemoryRecord, MemorySearchHit,
    MemorySearchParams, MemorySearchResponse,
};
#[cfg(feature = "documents")]
use crate::extract::doc::{DocEntity, DocKeyword};

#[cfg(feature = "intelligence")]
pub(super) async fn embed_query(state: &ServerState, text: &str) -> Result<Vec<f32>, McpError> {
    let embedder = state
        .embedder
        .get_or_try_init(|| async {
            crate::embeddings::SharedEmbedder::load("balanced")
                .map(Arc::new)
                .map_err(|e| format!("load embedder: {e}"))
        })
        .await
        .map_err(|e| McpError::internal_error(e.clone(), None))?;
    let embedder = Arc::clone(embedder);
    let text = text.to_string();
    tokio::task::spawn_blocking(move || embedder.embed(&text))
        .await
        .map_err(|e| McpError::internal_error(format!("spawn_blocking: {e}"), None))?
        .map_err(|e| McpError::internal_error(format!("embed: {e}"), None))
}

#[cfg(any(feature = "memory", feature = "documents"))]
pub(super) async fn lance_store(
    state: &ServerState,
) -> Result<Arc<crate::lance::LanceStore>, McpError> {
    state
        .lance
        .get_or_try_init(|| async {
            let embedder = state
                .embedder
                .get_or_try_init(|| async {
                    crate::embeddings::SharedEmbedder::load("balanced")
                        .map(Arc::new)
                        .map_err(|e| format!("load embedder: {e}"))
                })
                .await
                .map_err(|e| format!("embedder init: {e}"))?;
            let dim = embedder.dim();
            let model = embedder.model().to_string();
            let lance_dir = state.store.read().await.basemind_dir.join("lance");
            // LanceStore::open builds its own current-thread tokio runtime and
            // calls `block_on`, which panics when invoked from inside the live
            // server runtime. Offload to a blocking thread so the inner runtime
            // owns its own thread.
            let model_for_open = model.clone();
            tokio::task::spawn_blocking(move || {
                crate::lance::LanceStore::open(&lance_dir, dim, &model_for_open)
            })
            .await
            .map_err(|e| format!("lance open join: {e}"))?
            .map(Arc::new)
            .map_err(|e| format!("open LanceStore: {e}"))
        })
        .await
        .cloned()
        .map_err(|e| McpError::internal_error(e.clone(), None))
}

#[cfg(feature = "memory")]
fn read_memory_record(idx: &crate::index::IndexDb, scope: &str, key: &str) -> Option<MemoryRecord> {
    let raw_key = crate::index::keys::memory_by_key(scope, key);
    let bytes = idx.memory_by_key.get(raw_key).ok().flatten()?;
    rmp_serde::from_slice(&bytes).ok()
}

#[cfg(feature = "memory")]
fn write_memory_record(
    idx: &crate::index::IndexDb,
    scope: &str,
    key: &str,
    record: &MemoryRecord,
) -> Result<(), McpError> {
    let raw_key = crate::index::keys::memory_by_key(scope, key);
    let bytes = rmp_serde::to_vec_named(record)
        .map_err(|e| McpError::internal_error(format!("serialize memory record: {e}"), None))?;
    idx.memory_by_key
        .insert(raw_key, bytes)
        .map_err(|e| McpError::internal_error(format!("fjall insert: {e}"), None))
}

#[cfg(feature = "memory")]
fn delete_memory_record(
    idx: &crate::index::IndexDb,
    scope: &str,
    key: &str,
) -> Result<bool, McpError> {
    let raw_key = crate::index::keys::memory_by_key(scope, key);
    let existed = idx
        .memory_by_key
        .get(raw_key.clone())
        .map_err(|e| McpError::internal_error(format!("fjall get: {e}"), None))?
        .is_some();
    if existed {
        idx.memory_by_key
            .remove(raw_key)
            .map_err(|e| McpError::internal_error(format!("fjall remove: {e}"), None))?;
    }
    Ok(existed)
}

#[cfg(feature = "memory")]
pub(super) async fn run_memory_put(
    state: &ServerState,
    params: MemoryPutParams,
) -> Result<CallToolResult, McpError> {
    let now = crate::lance::now_micros();
    let store = state.store.read().await;
    let idx = store
        .index_db
        .as_ref()
        .ok_or_else(|| McpError::internal_error("memory_by_key index not available", None))?;
    let existing = read_memory_record(idx, &state.scope, &params.key);
    let created_at = existing.map(|r| r.created_at).unwrap_or(now);
    let tags = params.tags.clone().unwrap_or_default();
    let record = MemoryRecord {
        value: params.value.clone(),
        tags: tags.clone(),
        created_at,
        updated_at: now,
    };
    write_memory_record(idx, &state.scope, &params.key, &record)?;
    drop(store);
    if params.embed {
        let embedding = embed_query(state, &params.value).await?;
        let lance = lance_store(state).await?;
        let row = crate::lance::MemoryRow {
            scope: state.scope.clone(),
            key: params.key.clone(),
            value: params.value.clone(),
            tags,
            embedding,
            created_at,
            updated_at: now,
        };
        let lance_clone = Arc::clone(&lance);
        tokio::task::spawn_blocking(move || lance_clone.upsert_memory(row))
            .await
            .map_err(|e| McpError::internal_error(format!("spawn_blocking: {e}"), None))?
            .map_err(|e| McpError::internal_error(format!("upsert_memory: {e}"), None))?;
    }
    json_result(&MemoryPutResponse {
        key: params.key,
        created_at,
        updated_at: now,
    })
}

#[cfg(feature = "memory")]
pub(super) async fn run_memory_get(
    state: &ServerState,
    params: MemoryGetParams,
) -> Result<CallToolResult, McpError> {
    let store = state.store.read().await;
    let idx = store
        .index_db
        .as_ref()
        .ok_or_else(|| McpError::internal_error("memory_by_key index not available", None))?;
    let entry: Option<MemoryEntry> =
        read_memory_record(idx, &state.scope, &params.key).map(|r| MemoryEntry {
            key: params.key.clone(),
            value: r.value,
            tags: r.tags,
            created_at: r.created_at,
            updated_at: r.updated_at,
        });
    json_result(&entry)
}

#[cfg(feature = "memory")]
const MEMORY_PREVIEW_CHARS: usize = 200;

#[cfg(feature = "memory")]
pub(super) async fn run_memory_list(
    state: &ServerState,
    params: MemoryListParams,
) -> Result<CallToolResult, McpError> {
    use std::ops::Bound;

    use super::cursor::{Cursor, prefix_upper_bound};
    let limit = params
        .limit
        .unwrap_or(super::helpers::SEARCH_LIMIT_DEFAULT)
        .min(super::helpers::SEARCH_LIMIT_MAX) as usize;
    let store = state.store.read().await;
    let idx = store
        .index_db
        .as_ref()
        .ok_or_else(|| McpError::internal_error("memory_by_key index not available", None))?;
    let scope_prefix = crate::index::keys::memory_by_key_scope_prefix(&state.scope);
    let upper = prefix_upper_bound(&scope_prefix);
    let cursor_bytes = params
        .cursor
        .as_ref()
        .map(|c| c.decode_fjall())
        .transpose()?;
    let lower: Bound<Vec<u8>> = match cursor_bytes.as_deref() {
        Some(k) => Bound::Excluded(k.to_vec()),
        None => Bound::Included(scope_prefix.clone()),
    };
    let upper_bound: Bound<Vec<u8>> = match upper {
        Some(b) => Bound::Excluded(b),
        None => Bound::Unbounded,
    };
    let key_prefix_filter = params.prefix.as_deref().unwrap_or("");
    let tag_filter = params.tag.as_deref();
    let mut entries: Vec<MemoryEntry> = Vec::with_capacity(limit.min(64));
    let mut total: usize = 0;
    let mut last_emitted_key: Option<Vec<u8>> = None;
    let mut has_more = false;
    for guard in idx.memory_by_key.range::<Vec<u8>, _>((lower, upper_bound)) {
        let (raw_key, raw_val) = guard
            .into_inner()
            .map_err(|e| McpError::internal_error(format!("index iter: {e}"), None))?;
        let Some((_, key)) = crate::index::keys::parse_memory_by_key(&raw_key) else {
            continue;
        };
        if !key.starts_with(key_prefix_filter) {
            continue;
        }
        let Ok(record): Result<MemoryRecord, _> = rmp_serde::from_slice(&raw_val) else {
            continue;
        };
        if let Some(tag) = tag_filter
            && !record.tags.iter().any(|t| t == tag)
        {
            continue;
        }
        total += 1;
        if entries.len() < limit {
            let value = if record.value.len() > MEMORY_PREVIEW_CHARS {
                format!(
                    "{}…",
                    record
                        .value
                        .char_indices()
                        .nth(MEMORY_PREVIEW_CHARS)
                        .map(|(i, _)| &record.value[..i])
                        .unwrap_or(&record.value)
                )
            } else {
                record.value.clone()
            };
            entries.push(MemoryEntry {
                key,
                value,
                tags: record.tags,
                created_at: record.created_at,
                updated_at: record.updated_at,
            });
            last_emitted_key = Some(raw_key.to_vec());
        } else {
            has_more = true;
        }
    }
    let next_cursor = if has_more {
        last_emitted_key.as_deref().map(Cursor::encode_fjall)
    } else {
        None
    };
    json_result(&MemoryListResponse {
        total,
        truncated: total > limit,
        entries,
        next_cursor,
    })
}

#[cfg(feature = "memory")]
pub(super) async fn run_memory_search(
    state: &ServerState,
    params: MemorySearchParams,
) -> Result<CallToolResult, McpError> {
    let limit = params.limit.unwrap_or(10).min(100) as usize;
    let embedding = embed_query(state, &params.query).await?;
    let lance = lance_store(state).await?;
    let scope = state.scope.clone();
    let tag = params.tag.clone();
    let hits_raw = tokio::task::spawn_blocking(move || {
        lance.search_memory(&scope, embedding, limit, tag.as_deref())
    })
    .await
    .map_err(|e| McpError::internal_error(format!("spawn_blocking: {e}"), None))?
    .map_err(|e| McpError::internal_error(format!("search_memory: {e}"), None))?;
    let hits = hits_raw
        .into_iter()
        .map(|h| MemorySearchHit {
            key: h.key,
            value: h.value,
            tags: h.tags,
            distance: h.distance,
        })
        .collect();
    json_result(&MemorySearchResponse {
        query: params.query,
        hits,
    })
}

#[cfg(feature = "memory")]
pub(super) async fn run_memory_delete(
    state: &ServerState,
    params: MemoryDeleteParams,
) -> Result<CallToolResult, McpError> {
    let store = state.store.read().await;
    let idx = store
        .index_db
        .as_ref()
        .ok_or_else(|| McpError::internal_error("memory_by_key index not available", None))?;
    let deleted_fjall = delete_memory_record(idx, &state.scope, &params.key)?;
    drop(store);
    if let Some(lance) = state.lance.get() {
        let lance = Arc::clone(lance);
        let scope = state.scope.clone();
        let key = params.key.clone();
        tokio::task::spawn_blocking(move || lance.delete_memory(&scope, &key))
            .await
            .ok();
    }
    json_result(&MemoryDeleteResponse {
        deleted: deleted_fjall,
    })
}

#[cfg(feature = "documents")]
pub(super) async fn run_search_documents(
    state: &ServerState,
    params: SearchDocumentsParams,
) -> Result<CallToolResult, McpError> {
    // Resolve effective config. The common case has no overrides — skip the
    // `ConfigV1` deep clone (`BTreeMap<String, LanguageConfig>` + several `Vec<String>`)
    // entirely. Only pay the clone when overrides actually need to be layered.
    // We capture both the output format AND the reranker config in one pass so we
    // only clone once even when the reranker is enabled via TOML (no overrides).
    let (output_format, reranker_enabled, reranker_preset, reranker_top_k) =
        if params.overrides.any() {
            let mut effective = (*state.config).clone();
            crate::config::layered::apply_documents_overrides(
                &mut effective,
                &params.overrides,
                crate::config::ConfigSource::Mcp,
                None,
            );
            let r = &effective.documents.reranker;
            (
                effective.documents.output.format,
                r.enabled,
                r.preset.clone(),
                r.top_k,
            )
        } else {
            let r = &state.config.documents.reranker;
            (
                state.config.documents.output.format,
                r.enabled,
                r.preset.clone(),
                r.top_k,
            )
        };

    let limit = params.limit.unwrap_or(10).min(100) as usize;
    let embedding = embed_query(state, &params.query).await?;
    let lance = lance_store(state).await?;
    let scope = state.scope.clone();
    let mime = params.mime_type.clone();
    let hits_raw = tokio::task::spawn_blocking(move || {
        lance.search_documents(&scope, embedding, limit, mime.as_deref())
    })
    .await
    .map_err(|e| McpError::internal_error(format!("spawn_blocking: {e}"), None))?
    .map_err(|e| McpError::internal_error(format!("search_documents: {e}"), None))?;
    let mut hits: Vec<DocumentSearchHit> = hits_raw
        .into_iter()
        .map(|h| DocumentSearchHit {
            path: h.path,
            chunk_idx: h.chunk_idx,
            text: h.text,
            mime_type: h.mime_type,
            byte_start: h.byte_start,
            byte_end: h.byte_end,
            distance: h.distance,
            rerank_score: None,
            keywords: Vec::new(),
            entities: Vec::new(),
        })
        .collect();

    // Attach keywords + entities from each hit's parent doc blob (if extraction
    // ran them at scan time) and optionally post-filter by `entity_category` /
    // `keywords_contains`. We dedupe by `path` first so we only read each blob
    // once even when several chunks of the same doc landed in the result set.
    attach_doc_metadata(
        state,
        &mut hits,
        params.entity_category.as_deref(),
        params.keywords_contains.as_deref(),
    )
    .await?;

    // Reranker post-step: cross-encoder rescores and reorders the candidate hits.
    // Default OFF — first call downloads ONNX weights (~100 MB) into
    // `~/.cache/kreuzberg/rerankers/`. Enable via `[documents.reranker] enabled = true`
    // in TOML or `reranker_enabled = true` as a per-query override.
    if reranker_enabled && !hits.is_empty() {
        // Fail-fast on unknown preset names before constructing `RerankerConfig` so we
        // don't trigger an opaque ONNX error or a wrong-model download.
        if kreuzberg::get_reranker_preset(&reranker_preset).is_none() {
            return Err(McpError::invalid_params(
                format!("unknown reranker preset: {reranker_preset:?}"),
                None,
            ));
        }
        let krz_config = kreuzberg::core::config::RerankerConfig {
            model: kreuzberg::core::config::RerankerModelType::Preset {
                name: reranker_preset,
            },
            top_k: Some(reranker_top_k),
            ..Default::default()
        };
        let documents: Vec<String> = hits.iter().map(|h| h.text.clone()).collect();
        let reranked = kreuzberg::rerank_async(params.query.clone(), documents, &krz_config)
            .await
            .map_err(|e| {
                // Best-effort split between model-load and inference failures. The
                // kreuzberg error variants don't expose a kind discriminator, so we
                // substring-match the Display impl — better than the opaque `rerank: {e}`
                // we had before, even if it occasionally misclassifies.
                let msg = e.to_string();
                let kind = if msg.contains("download")
                    || msg.contains("HuggingFace")
                    || msg.contains("model")
                {
                    "rerank model load"
                } else {
                    "rerank inference"
                };
                McpError::internal_error(format!("{kind}: {msg}"), None)
            })?;
        // Bounds-check every external index — the reranker is a third-party ONNX model
        // and a buggy run must not panic the server.
        let original_hits = std::mem::take(&mut hits);
        hits = reranked
            .into_iter()
            .map(|r| {
                original_hits
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
                                original_hits.len()
                            ),
                            None,
                        )
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
    }

    format_response(
        &SearchDocumentsResponse {
            query: params.query,
            hits,
        },
        output_format,
    )
}

/// Load the parent doc blob for each unique hit path, copy its keywords +
/// entities onto every hit pointing at that path, and (optionally) drop hits
/// whose parent fails an `entity_category` / `keywords_contains` filter.
///
/// We deliberately do this BEFORE the reranker so the cross-encoder only
/// rescores survivors. Per-blob cost is bounded by the result-set size (at
/// most `limit` distinct paths); blobs are skipped silently when the path is
/// no longer in the working-tree index (e.g. file deleted between scan and
/// query) — a missing blob is not an error.
#[cfg(feature = "documents")]
async fn attach_doc_metadata(
    state: &ServerState,
    hits: &mut Vec<DocumentSearchHit>,
    entity_category: Option<&str>,
    keywords_contains: Option<&str>,
) -> Result<(), McpError> {
    if hits.is_empty() {
        return Ok(());
    }
    // Collect distinct paths once so we don't re-read the same blob for every chunk.
    let mut unique_paths: Vec<String> = Vec::with_capacity(hits.len());
    {
        let mut seen: ahash::AHashSet<&str> = ahash::AHashSet::new();
        for h in hits.iter() {
            if seen.insert(h.path.as_str()) {
                unique_paths.push(h.path.clone());
            }
        }
    }

    // Phase 1: under the read guard, resolve path → blob filesystem path. We do
    // NOT touch the filesystem here so the lock window stays trivially short.
    let pairs: Vec<(String, std::path::PathBuf)> = {
        let store = state.store.read().await;
        unique_paths
            .iter()
            .filter_map(|p| {
                store
                    .lookup(p.as_str())
                    .map(|entry| (p.clone(), store.blob_path_doc_hex(&entry.hash_hex)))
            })
            .collect()
    }; // guard dropped here

    // Phase 2: read blobs off the async runtime. Each blob is a synchronous
    // `std::fs::read` + msgpack decode — exactly the work `spawn_blocking` is
    // for. The async path keeps making progress while we crunch metadata.
    let meta: ahash::AHashMap<String, (Vec<DocKeyword>, Vec<DocEntity>)> =
        tokio::task::spawn_blocking(move || {
            let mut out: ahash::AHashMap<String, (Vec<DocKeyword>, Vec<DocEntity>)> =
                ahash::AHashMap::with_capacity(pairs.len());
            for (path, blob_path) in pairs {
                if !blob_path.exists() {
                    continue;
                }
                let bytes = match std::fs::read(&blob_path) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(path = %path, error = %e, "read doc blob for metadata attach failed");
                        continue;
                    }
                };
                match rmp_serde::from_slice::<crate::extract::doc::FileMapDoc>(&bytes) {
                    Ok(doc) => {
                        out.insert(path, (doc.keywords, doc.entities));
                    }
                    Err(e) => {
                        tracing::warn!(path = %path, error = %e, "decode doc blob for metadata attach failed");
                    }
                }
            }
            out
        })
        .await
        .unwrap_or_default();

    // Pre-lowercase filter substrings once.
    let cat_needle = entity_category.map(|s| s.to_lowercase());
    let kw_needle = keywords_contains.map(|s| s.to_lowercase());

    hits.retain_mut(|hit| {
        let (kws, ents) = meta
            .get(&hit.path)
            .cloned()
            .unwrap_or_else(|| (Vec::new(), Vec::new()));

        // Apply filters first; if a filter is set and the parent doc has no
        // matching metadata, drop the hit before paying the clone cost.
        if let Some(needle) = cat_needle.as_deref()
            && !ents
                .iter()
                .any(|e| e.category.to_lowercase().contains(needle))
        {
            return false;
        }
        if let Some(needle) = kw_needle.as_deref()
            && !kws.iter().any(|k| k.text.to_lowercase().contains(needle))
        {
            return false;
        }

        hit.keywords = kws;
        hit.entities = ents;
        true
    });
    Ok(())
}
