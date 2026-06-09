//! Helper bodies for the 6 memory + document-search MCP tools.

use std::sync::Arc;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::ServerState;
use super::helpers::json_result;
#[cfg(feature = "documents")]
use super::types::{DocumentSearchHit, SearchDocumentsParams, SearchDocumentsResponse};
#[cfg(feature = "memory")]
use super::types::{
    MemoryDeleteParams, MemoryDeleteResponse, MemoryEntry, MemoryGetParams, MemoryListParams,
    MemoryListResponse, MemoryPutParams, MemoryPutResponse, MemoryRecord, MemorySearchHit,
    MemorySearchParams, MemorySearchResponse,
};

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
            crate::lance::LanceStore::open(&lance_dir, dim, &model)
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

const MEMORY_PREVIEW_CHARS: usize = 200;

#[cfg(feature = "memory")]
pub(super) async fn run_memory_list(
    state: &ServerState,
    params: MemoryListParams,
) -> Result<CallToolResult, McpError> {
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
    let key_prefix_filter = params.prefix.as_deref().unwrap_or("");
    let tag_filter = params.tag.as_deref();
    let mut entries: Vec<MemoryEntry> = Vec::with_capacity(limit.min(64));
    let mut total: usize = 0;
    for guard in idx.memory_by_key.prefix(scope_prefix) {
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
        }
    }
    json_result(&MemoryListResponse {
        total,
        truncated: total > limit,
        entries,
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
    let hits = hits_raw
        .into_iter()
        .map(|h| DocumentSearchHit {
            path: h.path,
            chunk_idx: h.chunk_idx,
            text: h.text,
            mime_type: h.mime_type,
            byte_start: h.byte_start,
            byte_end: h.byte_end,
            distance: h.distance,
        })
        .collect();
    json_result(&SearchDocumentsResponse {
        query: params.query,
        hits,
    })
}
