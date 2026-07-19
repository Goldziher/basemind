//! Helper bodies for the 6 memory + document-search MCP tools.

use std::sync::Arc;

use rmcp::ErrorData as McpError;
#[cfg(any(feature = "memory", feature = "documents"))]
use rmcp::model::CallToolResult;

use super::ServerState;
#[cfg(any(feature = "memory", feature = "documents"))]
use super::helpers::elapsed_us;
#[cfg(feature = "documents")]
use super::helpers::format_response;
#[cfg(feature = "memory")]
use super::helpers::json_result;
#[cfg(feature = "documents")]
use super::types::{DocumentSearchHit, SearchDocumentsParams, SearchDocumentsResponse};
#[cfg(feature = "memory")]
use super::types_memory::{
    MemoryDeleteParams, MemoryDeleteResponse, MemoryEntry, MemoryGetParams, MemoryListParams, MemoryListResponse,
    MemoryPutParams, MemoryPutResponse, MemorySearchHit, MemorySearchParams, MemorySearchResponse, Visibility,
};
#[cfg(feature = "documents")]
use crate::extract::doc::{DocEntity, DocKeyword, DocSummary};

#[cfg(feature = "intelligence")]
pub(super) async fn embed_query(state: &ServerState, text: &str) -> Result<Vec<f32>, McpError> {
    let preset = state.config.documents.embedding_preset.clone();
    let max_embed_threads = state
        .config
        .resources
        .effective_embed_threads(state.config.documents.embed_max_threads);
    let embed_batch_size = state.config.resources.embed_batch_size;
    let embedder = state
        .embedder
        .get_or_try_init(|| async {
            crate::embeddings::SharedEmbedder::load(&preset, max_embed_threads, embed_batch_size)
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

#[cfg(any(feature = "memory", feature = "documents", feature = "code-search"))]
pub(super) async fn lance_store(state: &ServerState) -> Result<Arc<crate::lance::LanceStore>, McpError> {
    let preset = state.config.documents.embedding_preset.clone();
    let max_embed_threads = state
        .config
        .resources
        .effective_embed_threads(state.config.documents.embed_max_threads);
    let embed_batch_size = state.config.resources.embed_batch_size;
    state
        .lance
        .get_or_try_init(|| async {
            let embedder = state
                .embedder
                .get_or_try_init(|| async {
                    crate::embeddings::SharedEmbedder::load(&preset, max_embed_threads, embed_batch_size)
                        .map(Arc::new)
                        .map_err(|e| format!("load embedder: {e}"))
                })
                .await
                .map_err(|e| format!("embedder init: {e}"))?;
            let dim = embedder.dim();
            let model = embedder.model().to_string();
            let lance_dir = state.store.read().await.basemind_dir.join(crate::store::LANCE_DIR);
            let model_for_open = model.clone();
            tokio::task::spawn_blocking(move || crate::lance::LanceStore::open(&lance_dir, dim, &model_for_open))
                .await
                .map_err(|e| format!("lance open join: {e}"))?
                .map(Arc::new)
                .map_err(|e| format!("open LanceStore: {e}"))
        })
        .await
        .cloned()
        .map_err(|e| McpError::internal_error(e.clone(), None))
}

/// Per-`(scope, key)` write serialization for `memory_put`.
///
/// `memory_put` is a read-modify-write across two stores (Fjall + LanceDB).
/// Without serialization, two concurrent puts for the same key both read "no
/// existing record" and stamp different `created_at` values, and their two-phase
/// (Fjall, then async Lance) writes can interleave so the stores disagree.
///
/// We serialize per key — unrelated keys still write in parallel — by handing
/// out a per-key `tokio::sync::Mutex` from a process-global registry. The
/// registry itself is guarded by a short-lived `std::sync::Mutex` (held only to
/// clone an `Arc`, never across an `.await`).
///
/// The registry is an `LruCache` bounded at [`MEMORY_PUT_LOCK_CAP`] so it cannot
/// grow without limit as distinct `(scope, key)` pairs are written over the
/// process lifetime. The cap is generous enough that realistic key counts never
/// evict. Eviction is safe for correctness: any task already holding the `Arc`
/// keeps its mutex alive after the entry is dropped from the cache. The single
/// rare-eviction caveat is that if a key's lock is evicted while one put holds
/// it and a *second* put for the same key arrives, the second put mints a fresh
/// mutex and the two no longer serialize — a window only reachable when more
/// than [`MEMORY_PUT_LOCK_CAP`] distinct keys are written between two racing
/// puts on the same key.
#[cfg(feature = "memory")]
const MEMORY_PUT_LOCK_CAP: std::num::NonZeroUsize = std::num::NonZeroUsize::new(4096).unwrap();

#[cfg(feature = "memory")]
type MemoryPutLockKey = (String, u8, String, String);

#[cfg(feature = "memory")]
type MemoryPutLockRegistry = std::sync::Mutex<lru::LruCache<MemoryPutLockKey, Arc<tokio::sync::Mutex<()>>>>;

#[cfg(feature = "memory")]
fn memory_put_lock(scope: &str, vis_byte: u8, owner: &str, key: &str) -> Arc<tokio::sync::Mutex<()>> {
    use std::sync::OnceLock;
    static LOCKS: OnceLock<MemoryPutLockRegistry> = OnceLock::new();
    let registry = LOCKS.get_or_init(|| std::sync::Mutex::new(lru::LruCache::new(MEMORY_PUT_LOCK_CAP)));
    let mut guard = registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let registry_key = (scope.to_string(), vis_byte, owner.to_string(), key.to_string());
    Arc::clone(guard.get_or_insert(registry_key, || Arc::new(tokio::sync::Mutex::new(()))))
}

/// String form of a [`Visibility`] for the LanceDB `visibility` column. Matches the serde
/// `rename_all = "lowercase"` discriminants so the column reads back as the same enum.
#[cfg(feature = "memory")]
fn lance_visibility(visibility: Visibility) -> &'static str {
    match visibility {
        Visibility::Group => "group",
        Visibility::Individual => "individual",
    }
}

/// Resolve the `(vis_byte, owner)` namespace coordinates for a memory call.
///
/// `group` → `owner = ""` (shared tier, today's behavior). `individual` → `owner` is the
/// server's resolved `agent_id`, which was validated through `AgentId` at boot and is
/// therefore NUL-free — safe to embed in the length-prefixed Fjall key segment.
#[cfg(feature = "memory")]
fn namespace(state: &ServerState, visibility: Visibility) -> (u8, &str) {
    let owner: &str = match visibility {
        Visibility::Individual => &state.agent_id,
        Visibility::Group => "",
    };
    (visibility.vis_byte(), owner)
}

#[cfg(feature = "memory")]
pub(super) async fn run_memory_put(state: &ServerState, params: MemoryPutParams) -> Result<CallToolResult, McpError> {
    let (vis_byte, owner) = namespace(state, params.visibility);
    let tags = params.tags.clone().unwrap_or_default();

    // Fjall RMW: forwarded to the daemon under `daemon_writer` (the sole fjall writer serializes
    // same-workspace ops, so no per-key lock is needed there); otherwise done locally under the
    // per-key `memory_put_lock`. Both paths return the resolved `(created_at, updated_at)`.
    let (created_at, updated_at) = memory_put_fjall(state, vis_byte, owner, &params.key, &params.value, &tags).await?;

    if params.embed {
        let embedding = embed_query(state, &params.value).await?;
        let lance = lance_store(state).await?;
        let row = crate::lance::MemoryRow {
            scope: state.scope.clone(),
            key: params.key.clone(),
            value: params.value.clone(),
            tags,
            visibility: lance_visibility(params.visibility).to_string(),
            agent_id: owner.to_string(),
            embedding,
            created_at,
            updated_at,
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
        updated_at,
    })
}

/// Run the fjall half of `memory_put`, returning the resolved `(created_at, updated_at)`.
///
/// `daemon_writer` builds a [`MemoryOp::Put`] and forwards it (the daemon serializes same-workspace
/// writes, so the RMW is atomic without a per-key lock). Every other build takes the per-key
/// [`memory_put_lock`] and calls [`put_core`](super::memory_ops::put_core) against the local index.
#[cfg(feature = "memory")]
async fn memory_put_fjall(
    state: &ServerState,
    vis_byte: u8,
    owner: &str,
    key: &str,
    value: &str,
    tags: &[String],
) -> Result<(i64, i64), McpError> {
    #[cfg(all(feature = "comms", any(unix, windows)))]
    if state.daemon_writer {
        use super::helpers_comms::{comms_err, resolve_comms_client};
        use crate::comms::memory_proto::{MemoryOp, MemoryOutcome};

        let op = MemoryOp::Put {
            vis_byte,
            owner: owner.to_string(),
            key: key.to_string(),
            value: value.to_string(),
            tags: tags.to_vec(),
        };
        let client = resolve_comms_client(state, None).await?;
        let mut guard = client.lock().await;
        let outcome = guard
            .memory_op(state.root.clone(), state.scope.clone(), op)
            .await
            .map_err(comms_err)?;
        return match outcome {
            MemoryOutcome::Put { created_at, updated_at } => Ok((created_at, updated_at)),
            other => Err(McpError::internal_error(
                format!("memory_put: unexpected daemon outcome {other:?}"),
                None,
            )),
        };
    }

    let key_lock = memory_put_lock(&state.scope, vis_byte, owner, key);
    let _put_guard = key_lock.lock().await;
    let store = state.store.read().await;
    let idx = store
        .index_db
        .as_ref()
        .ok_or_else(|| McpError::internal_error("memory_by_key index not available", None))?;
    Ok(super::memory_ops::put_core(
        idx,
        &state.scope,
        vis_byte,
        owner,
        key,
        value,
        tags,
    )?)
}

#[cfg(feature = "memory")]
pub(super) async fn run_memory_get(state: &ServerState, params: MemoryGetParams) -> Result<CallToolResult, McpError> {
    let (vis_byte, owner) = namespace(state, params.visibility);

    #[cfg(all(feature = "comms", any(unix, windows)))]
    if state.daemon_writer {
        use super::helpers_comms::{comms_err, resolve_comms_client};
        use crate::comms::memory_proto::{MemoryOp, MemoryOutcome};

        let op = MemoryOp::Get {
            vis_byte,
            owner: owner.to_string(),
            key: params.key.clone(),
        };
        let client = resolve_comms_client(state, None).await?;
        let mut guard = client.lock().await;
        let outcome = guard
            .memory_op(state.root.clone(), state.scope.clone(), op)
            .await
            .map_err(comms_err)?;
        let entry = match outcome {
            MemoryOutcome::Got(entry) => entry.map(wire_to_entry),
            other => {
                return Err(McpError::internal_error(
                    format!("memory_get: unexpected daemon outcome {other:?}"),
                    None,
                ));
            }
        };
        return json_result(&entry);
    }

    let store = state.store.read().await;
    let idx = store
        .index_db
        .as_ref()
        .ok_or_else(|| McpError::internal_error("memory_by_key index not available", None))?;
    let entry: Option<MemoryEntry> =
        super::memory_ops::get_core(idx, &state.scope, vis_byte, owner, &params.key)?.map(wire_to_entry);
    json_result(&entry)
}

/// Map a wire memory entry onto the MCP [`MemoryEntry`] response shape.
#[cfg(feature = "memory")]
fn wire_to_entry(w: super::memory_ops::WireMemoryEntry) -> MemoryEntry {
    MemoryEntry {
        key: w.key,
        value: w.value,
        tags: w.tags,
        created_at: w.created_at,
        updated_at: w.updated_at,
    }
}

#[cfg(feature = "memory")]
pub(super) async fn run_memory_list(state: &ServerState, params: MemoryListParams) -> Result<CallToolResult, McpError> {
    use super::cursor::Cursor;

    let __body = std::time::Instant::now();
    let limit = params
        .limit
        .unwrap_or(super::helpers::SEARCH_LIMIT_DEFAULT)
        .min(super::helpers::SEARCH_LIMIT_MAX) as usize;
    let (vis_byte, owner) = namespace(state, params.visibility);
    let key_prefix_filter = params.prefix.clone().unwrap_or_default();
    let cursor_bytes = params.cursor.as_ref().map(|c| c.decode_fjall()).transpose()?;

    #[cfg(all(feature = "comms", any(unix, windows)))]
    if state.daemon_writer {
        use super::helpers_comms::{comms_err, resolve_comms_client};
        use crate::comms::memory_proto::{MemoryOp, MemoryOutcome};

        let op = MemoryOp::List {
            vis_byte,
            owner: owner.to_string(),
            prefix: key_prefix_filter,
            tag: params.tag.clone(),
            limit: limit as u32,
            cursor: cursor_bytes,
        };
        let client = resolve_comms_client(state, None).await?;
        let mut guard = client.lock().await;
        let outcome = guard
            .memory_op(state.root.clone(), state.scope.clone(), op)
            .await
            .map_err(comms_err)?;
        return match outcome {
            MemoryOutcome::Listed {
                entries,
                total,
                truncated,
                next_cursor,
            } => json_result(&MemoryListResponse {
                total: total as usize,
                truncated,
                entries: entries.into_iter().map(wire_to_entry).collect(),
                next_cursor: next_cursor.as_deref().map(Cursor::encode_fjall),
                elapsed_us: elapsed_us(__body),
            }),
            other => Err(McpError::internal_error(
                format!("memory_list: unexpected daemon outcome {other:?}"),
                None,
            )),
        };
    }

    let store = state.store.read().await;
    let idx = store
        .index_db
        .as_ref()
        .ok_or_else(|| McpError::internal_error("memory_by_key index not available", None))?;
    let result = super::memory_ops::list_core(
        idx,
        &state.scope,
        &super::memory_ops::ListQuery {
            vis_byte,
            owner,
            key_prefix: &key_prefix_filter,
            tag: params.tag.as_deref(),
            limit,
            cursor: cursor_bytes.as_deref(),
        },
    )?;
    json_result(&MemoryListResponse {
        total: result.total as usize,
        truncated: result.truncated,
        entries: result.entries.into_iter().map(wire_to_entry).collect(),
        next_cursor: result.next_cursor.as_deref().map(Cursor::encode_fjall),
        elapsed_us: elapsed_us(__body),
    })
}

#[cfg(feature = "memory")]
pub(super) async fn run_memory_search(
    state: &ServerState,
    params: MemorySearchParams,
) -> Result<CallToolResult, McpError> {
    let __body = std::time::Instant::now();
    let limit = params.limit.unwrap_or(10).min(100) as usize;
    let (_, owner) = namespace(state, params.visibility);
    let visibility = lance_visibility(params.visibility).to_string();
    let agent_id = owner.to_string();
    let embedding = embed_query(state, &params.query).await?;
    let lance = lance_store(state).await?;
    let scope = state.scope.clone();
    let tag = params.tag.clone();
    let hits_raw = tokio::task::spawn_blocking(move || {
        lance.search_memory(&scope, &visibility, &agent_id, embedding, limit, tag.as_deref())
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
        elapsed_us: elapsed_us(__body),
    })
}

#[cfg(feature = "memory")]
pub(super) async fn run_memory_delete(
    state: &ServerState,
    params: MemoryDeleteParams,
) -> Result<CallToolResult, McpError> {
    let (vis_byte, owner) = namespace(state, params.visibility);
    let deleted_fjall = memory_delete_fjall(state, vis_byte, owner, &params.key).await?;
    if let Some(lance) = state.lance.get() {
        let lance = Arc::clone(lance);
        let scope = state.scope.clone();
        let key = params.key.clone();
        let visibility = lance_visibility(params.visibility).to_string();
        let agent_id = owner.to_string();
        match tokio::task::spawn_blocking(move || lance.delete_memory(&scope, &visibility, &agent_id, &key)).await {
            Ok(Ok(_rows_deleted)) => {}
            Ok(Err(error)) => {
                tracing::warn!(
                    key = %params.key,
                    ?error,
                    "lance delete_memory failed; embedding may be stale relative to Fjall"
                );
            }
            Err(join_error) => {
                tracing::warn!(
                    key = %params.key,
                    ?join_error,
                    "lance delete_memory task panicked; embedding may be stale relative to Fjall"
                );
            }
        }
    }
    json_result(&MemoryDeleteResponse { deleted: deleted_fjall })
}

/// Run the fjall half of `memory_delete`, returning whether a record existed.
///
/// `daemon_writer` forwards a [`MemoryOp::Delete`]; every other build calls
/// [`delete_core`](super::memory_ops::delete_core) against the local index.
#[cfg(feature = "memory")]
async fn memory_delete_fjall(state: &ServerState, vis_byte: u8, owner: &str, key: &str) -> Result<bool, McpError> {
    #[cfg(all(feature = "comms", any(unix, windows)))]
    if state.daemon_writer {
        use super::helpers_comms::{comms_err, resolve_comms_client};
        use crate::comms::memory_proto::{MemoryOp, MemoryOutcome};

        let op = MemoryOp::Delete {
            vis_byte,
            owner: owner.to_string(),
            key: key.to_string(),
        };
        let client = resolve_comms_client(state, None).await?;
        let mut guard = client.lock().await;
        let outcome = guard
            .memory_op(state.root.clone(), state.scope.clone(), op)
            .await
            .map_err(comms_err)?;
        return match outcome {
            MemoryOutcome::Deleted { deleted } => Ok(deleted),
            other => Err(McpError::internal_error(
                format!("memory_delete: unexpected daemon outcome {other:?}"),
                None,
            )),
        };
    }

    let store = state.store.read().await;
    let idx = store
        .index_db
        .as_ref()
        .ok_or_else(|| McpError::internal_error("memory_by_key index not available", None))?;
    Ok(super::memory_ops::delete_core(idx, &state.scope, vis_byte, owner, key)?)
}

/// Pick the ingestion scope to search: the caller's if they named one, else this repo's.
///
/// Documents are partitioned by ingestion scope, and the lanes do not agree on one: the scanner
/// writes under the repo scope while `web_scrape` / `web_crawl` write under `web:<host>`. Reading
/// only `state.scope`, with no way to ask for another, made every scraped page permanently
/// invisible — the write landed, LanceDB grew on disk, and the query filtered it out and returned
/// an empty, error-free, 14 ms answer.
#[cfg(feature = "documents")]
fn resolve_doc_scope(requested: Option<&str>, repo_scope: &str) -> String {
    requested.unwrap_or(repo_scope).to_string()
}

#[cfg(feature = "documents")]
pub(super) async fn run_search_documents(
    state: &ServerState,
    params: SearchDocumentsParams,
) -> Result<CallToolResult, McpError> {
    let __body = std::time::Instant::now();
    let (output_format, reranker_enabled, reranker_preset, reranker_top_k) = if params.overrides.any() {
        let mut effective = (*state.config).clone();
        crate::config::layered::apply_documents_overrides(
            &mut effective,
            &params.overrides,
            crate::config::ConfigSource::Mcp,
            None,
        );
        let r = &effective.documents.reranker;
        (effective.documents.output.format, r.enabled, r.preset.clone(), r.top_k)
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
    let scope = resolve_doc_scope(params.scope.as_deref(), &state.scope);
    let mime = params.mime_type.clone();
    let hits_raw =
        tokio::task::spawn_blocking(move || lance.search_documents(&scope, embedding, limit, mime.as_deref()))
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
            summary: None,
        })
        .collect();

    attach_doc_metadata(
        state,
        &mut hits,
        params.entity_category.as_deref(),
        params.keywords_contains.as_deref(),
    )
    .await?;

    if reranker_enabled && !hits.is_empty() {
        if xberg::get_reranker_preset(&reranker_preset).is_none() {
            return Err(McpError::invalid_params(
                format!("unknown reranker preset: {reranker_preset:?}"),
                None,
            ));
        }
        let krz_config = xberg::core::config::RerankerConfig {
            model: xberg::core::config::RerankerModelType::Preset { name: reranker_preset },
            top_k: Some(reranker_top_k),
            ..Default::default()
        };
        let documents: Vec<String> = hits.iter().map(|h| h.text.clone()).collect();
        let reranked = xberg::rerank_async(params.query.clone(), documents, &krz_config)
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

    let output_format = match params.format.as_deref().map(str::trim) {
        Some(f) if f.eq_ignore_ascii_case("toon") => crate::config::OutputFormat::Toon,
        Some(f) if f.eq_ignore_ascii_case("json") => crate::config::OutputFormat::Json,
        _ => output_format,
    };

    let budget = super::budget::apply_budget(hits, params.max_tokens);
    format_response(
        &SearchDocumentsResponse {
            query: params.query,
            budgeted: budget.budgeted,
            hits: budget.items,
            elapsed_us: elapsed_us(__body),
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
    let mut unique_paths: Vec<String> = Vec::with_capacity(hits.len());
    {
        let mut seen: ahash::AHashSet<&str> = ahash::AHashSet::new();
        for h in hits.iter() {
            if seen.insert(h.path.as_str()) {
                unique_paths.push(h.path.clone());
            }
        }
    }

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
    };

    type DocMeta = (Vec<DocKeyword>, Vec<DocEntity>, Option<DocSummary>);
    let meta: ahash::AHashMap<String, Arc<DocMeta>> = tokio::task::spawn_blocking(move || {
        let mut out: ahash::AHashMap<String, Arc<DocMeta>> = ahash::AHashMap::with_capacity(pairs.len());
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
                    out.insert(path, Arc::new((doc.keywords, doc.entities, doc.summary)));
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

    let cat_needle = entity_category.map(|s| s.to_lowercase());
    let kw_needle = keywords_contains.map(|s| s.to_lowercase());

    hits.retain_mut(|hit| {
        let meta_arc = meta.get(&hit.path).cloned();

        if let Some(needle) = cat_needle.as_deref() {
            let ents = meta_arc.as_ref().map(|m| m.1.as_slice()).unwrap_or(&[]);
            if !ents.iter().any(|e| e.category.to_lowercase().contains(needle)) {
                return false;
            }
        }
        if let Some(needle) = kw_needle.as_deref() {
            let kws = meta_arc.as_ref().map(|m| m.0.as_slice()).unwrap_or(&[]);
            if !kws.iter().any(|k| k.text.to_lowercase().contains(needle)) {
                return false;
            }
        }

        if let Some(m) = meta_arc {
            hit.keywords = m.0.clone();
            hit.entities = m.1.clone();
            hit.summary = m.2.clone();
        }
        true
    });
    Ok(())
}

#[cfg(all(test, feature = "documents"))]
mod scope_tests {
    use super::resolve_doc_scope;
    use crate::web::ingest::default_scope;

    #[test]
    fn defaults_to_the_repo_scope_when_the_caller_names_none() {
        assert_eq!(resolve_doc_scope(None, "repo-abc123"), "repo-abc123");
    }

    #[test]
    fn a_scraped_page_is_reachable_only_by_naming_its_web_scope() {
        // The bug, stated as a test: web_scrape writes here...
        let url = crate::url::Url::parse("https://example.com/page").unwrap();
        let write_scope = default_scope(&url);
        assert_eq!(write_scope, "web:example.com");

        // ...and the default read looks somewhere else entirely, which is why the page came back
        // as 0 hits however it was queried.
        let repo_scope = "repo-abc123";
        assert_ne!(resolve_doc_scope(None, repo_scope), write_scope);

        // Naming the scope the ingest tool echoed back is what closes the loop.
        assert_eq!(resolve_doc_scope(Some(&write_scope), repo_scope), write_scope);
    }
}
