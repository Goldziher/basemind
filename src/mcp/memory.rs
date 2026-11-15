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
    MemoryPutParams, MemoryPutResponse, MemorySearchParams, MemorySearchResponse, Visibility,
};
#[cfg(feature = "documents")]
use crate::extract::doc::{DocEntity, DocKeyword, DocSummary};

/// The shared embedding engine, loaded on first use. Loading reads the model from disk (or
/// downloads it), so it runs on a blocking thread and never stalls the async runtime.
#[cfg(feature = "intelligence")]
pub(super) async fn shared_embedder(state: &ServerState) -> Result<Arc<crate::embeddings::SharedEmbedder>, String> {
    let preset = state.shared.config.documents.embedding_preset.clone();
    let max_embed_threads = state
        .shared
        .config
        .resources
        .effective_embed_threads(state.shared.config.documents.embed_max_threads);
    let embed_batch_size = state.shared.config.resources.embed_batch_size;
    let provider = state.shared.config.resources.onnx_provider;
    state
        .shared
        .embedder
        .get_or_try_init(|| async {
            tokio::task::spawn_blocking(move || {
                crate::embeddings::SharedEmbedder::load_with_provider(
                    &preset,
                    max_embed_threads,
                    embed_batch_size,
                    provider,
                )
            })
            .await
            .map_err(|e| format!("load embedder join: {e}"))?
            .map(Arc::new)
            .map_err(|e| format!("load embedder: {e}"))
        })
        .await
        .cloned()
}

#[cfg(feature = "intelligence")]
pub(super) async fn embed_query(state: &ServerState, text: &str) -> Result<Vec<f32>, McpError> {
    let embedder = shared_embedder(state)
        .await
        .map_err(|e| McpError::internal_error(e, None))?;
    let text = text.to_string();
    tokio::task::spawn_blocking(move || embedder.embed(&text))
        .await
        .map_err(|e| McpError::internal_error(format!("spawn_blocking: {e}"), None))?
        .map_err(|e| McpError::internal_error(format!("embed: {e}"), None))
}

/// Open the vector store at `dir` for `embedder`'s dimension and model, off the async runtime.
#[cfg(feature = "intelligence")]
async fn open_lance(
    dir: std::path::PathBuf,
    embedder: &crate::embeddings::SharedEmbedder,
) -> Result<Arc<crate::lance::LanceStore>, String> {
    let (dim, model) = (embedder.dim(), embedder.model().to_string());
    tokio::task::spawn_blocking(move || crate::lance::LanceStore::open(&dir, dim, &model))
        .await
        .map_err(|e| format!("lance open join: {e}"))?
        .map(Arc::new)
        .map_err(|e| format!("open LanceStore: {e}"))
}

#[cfg(any(feature = "memory", feature = "documents", feature = "code-search"))]
pub(super) async fn lance_store(state: &ServerState) -> Result<Arc<crate::lance::LanceStore>, McpError> {
    state
        .shared
        .lance
        .get_or_try_init(|| async {
            let embedder = shared_embedder(state)
                .await
                .map_err(|e| format!("embedder init: {e}"))?;
            let lance_dir = state
                .shared
                .store
                .read()
                .await
                .basemind_dir
                .join(crate::store::LANCE_DIR);
            open_lance(lance_dir, &embedder).await
        })
        .await
        .cloned()
        .map_err(|e| McpError::internal_error(e.clone(), None))
}

/// The repo group's memory vector store, shared by every worktree of the clone like the notes
/// themselves (see [`super::memory_vectors::memory_lance_dir`]).
#[cfg(feature = "memory")]
pub(super) async fn memory_lance_store(state: &ServerState) -> Result<Arc<crate::lance::LanceStore>, McpError> {
    state
        .shared
        .memory_lance
        .get_or_try_init(|| async {
            let embedder = shared_embedder(state)
                .await
                .map_err(|e| format!("embedder init: {e}"))?;
            let root = state.shared.root.clone();
            let dir = tokio::task::spawn_blocking(move || super::memory_vectors::memory_lance_dir(&root))
                .await
                .map_err(|e| format!("memory lance dir join: {e}"))?;
            open_lance(dir, &embedder).await
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
pub(super) fn lance_visibility(visibility: Visibility) -> &'static str {
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

    let (created_at, updated_at) = memory_put_fjall(state, vis_byte, owner, &params.key, &params.value, &tags).await?;

    // The note is durable from here on. A failure to embed it is reported on a successful result,
    // never as an error: an error would read as "not saved" and invite a retry of a write that
    // already happened.
    let embedded = if params.embed {
        let outcome = embed_note(state, &params, owner, tags, created_at, updated_at).await;
        // The previous vector row still holds the OLD text; left in place, a search by meaning
        // would keep serving it for this key. The store is opened first because the embedder can
        // fail before anything in this process has touched it.
        if outcome.is_err() && memory_lance_store(state).await.is_ok() {
            delete_vector_row(state, params.visibility, owner, &params.key).await;
        }
        Some(outcome)
    } else {
        None
    };
    let (embedded, notice) = embed_report(embedded);
    json_result(&MemoryPutResponse {
        key: params.key,
        created_at,
        updated_at,
        embedded,
        notice,
    })
}

/// Embed a saved note and upsert its vector row.
#[cfg(feature = "memory")]
async fn embed_note(
    state: &ServerState,
    params: &MemoryPutParams,
    owner: &str,
    tags: Vec<String>,
    created_at: i64,
    updated_at: i64,
) -> Result<(), String> {
    let embedding = embed_query(state, &params.value)
        .await
        .map_err(|error| error.message.into_owned())?;
    let lance = memory_lance_store(state)
        .await
        .map_err(|error| error.message.into_owned())?;
    let row = crate::lance::MemoryRow {
        scope: state.shared.scope.clone(),
        key: params.key.clone(),
        value: params.value.clone(),
        tags,
        visibility: lance_visibility(params.visibility).to_string(),
        agent_id: owner.to_string(),
        embedding,
        created_at,
        updated_at,
    };
    tokio::task::spawn_blocking(move || lance.upsert_memory(row))
        .await
        .map_err(|error| format!("spawn_blocking: {error}"))?
        .map_err(|error| format!("upsert_memory: {error}"))
}

/// What `memory_save` says about the vector row of a note that is already saved:
/// `(embedded, notice)`. `None` means embedding was not asked for.
#[cfg(feature = "memory")]
fn embed_report(outcome: Option<Result<(), String>>) -> (bool, Option<String>) {
    match outcome {
        Some(Ok(())) => (true, None),
        Some(Err(error)) => (
            false,
            Some(format!(
                "the note is saved and can be read by `key` or listed by `prefix`, but it could not be embedded \
                 ({error}), so `memory_search` with `query` will not find it until it is saved again"
            )),
        ),
        None => (false, None),
    }
}

/// Best-effort removal of one note's vector row. The fjall record is the source of truth; a row
/// that cannot be removed is logged, never an error.
#[cfg(feature = "memory")]
async fn delete_vector_row(state: &ServerState, visibility: Visibility, owner: &str, key: &str) {
    let Some(lance) = state.shared.memory_lance.get() else {
        return;
    };
    let lance = Arc::clone(lance);
    let scope = state.shared.scope.clone();
    let row_key = key.to_string();
    let visibility = lance_visibility(visibility).to_string();
    let agent_id = owner.to_string();
    match tokio::task::spawn_blocking(move || lance.delete_memory(&scope, &visibility, &agent_id, &row_key)).await {
        Ok(Ok(_rows_deleted)) => {}
        Ok(Err(error)) => {
            tracing::warn!(%key, ?error, "lance delete_memory failed; embedding may be stale relative to Fjall");
        }
        Err(join_error) => {
            tracing::warn!(
                %key,
                ?join_error,
                "lance delete_memory task panicked; embedding may be stale relative to Fjall"
            );
        }
    }
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
    if state.shared.daemon_writer {
        use crate::comms::memory_proto::{MemoryOp, MemoryOutcome};

        let op = MemoryOp::Put {
            vis_byte,
            owner: owner.to_string(),
            key: key.to_string(),
            value: value.to_string(),
            tags: tags.to_vec(),
        };
        let outcome = super::helpers_comms::dispatch_memory_op(state, op).await?;
        return match outcome {
            MemoryOutcome::Put { created_at, updated_at } => Ok((created_at, updated_at)),
            other => Err(McpError::internal_error(
                format!("memory_put: unexpected daemon outcome {other:?}"),
                None,
            )),
        };
    }

    let key_lock = memory_put_lock(&state.shared.scope, vis_byte, owner, key);
    let _put_guard = key_lock.lock().await;
    let store = state.shared.store.read().await;
    let idx = store
        .index_db
        .as_ref()
        .ok_or_else(|| McpError::internal_error("memory_by_key index not available", None))?;
    Ok(super::memory_ops::put_core(
        idx,
        &state.shared.scope,
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

    let entry = memory_get_wire(state, vis_byte, owner, &params.key).await?;
    json_result(&entry.map(wire_to_entry))
}

/// Read one note from the Fjall truth, through the daemon when it owns the store.
#[cfg(feature = "memory")]
pub(super) async fn memory_get_wire(
    state: &ServerState,
    vis_byte: u8,
    owner: &str,
    key: &str,
) -> Result<Option<super::memory_ops::WireMemoryEntry>, McpError> {
    #[cfg(all(feature = "comms", any(unix, windows)))]
    if state.shared.daemon_writer {
        use crate::comms::memory_proto::{MemoryOp, MemoryOutcome};

        let op = MemoryOp::Get {
            vis_byte,
            owner: owner.to_string(),
            key: key.to_string(),
        };
        return match super::helpers_comms::dispatch_memory_op(state, op).await? {
            MemoryOutcome::Got(entry) => Ok(entry),
            other => Err(McpError::internal_error(
                format!("memory_get: unexpected daemon outcome {other:?}"),
                None,
            )),
        };
    }

    let store = state.shared.store.read().await;
    let idx = store
        .index_db
        .as_ref()
        .ok_or_else(|| McpError::internal_error("memory_by_key index not available", None))?;
    Ok(super::memory_ops::get_core(
        idx,
        &state.shared.scope,
        vis_byte,
        owner,
        key,
    )?)
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

    let page = memory_list_page(
        state,
        vis_byte,
        owner,
        &key_prefix_filter,
        params.tag.as_deref(),
        limit,
        cursor_bytes,
    )
    .await?;
    json_result(&MemoryListResponse {
        total: page.total as usize,
        truncated: page.truncated,
        entries: page.entries.into_iter().map(wire_to_entry).collect(),
        next_cursor: page.next_cursor.as_deref().map(Cursor::encode_fjall),
        elapsed_us: elapsed_us(__body),
    })
}

/// One page of a namespace's notes from the Fjall truth, through the daemon when it owns the
/// store. Values are preview-truncated, as `memory_list` returns them.
#[cfg(feature = "memory")]
pub(super) async fn memory_list_page(
    state: &ServerState,
    vis_byte: u8,
    owner: &str,
    prefix: &str,
    tag: Option<&str>,
    limit: usize,
    cursor: Option<Vec<u8>>,
) -> Result<super::memory_ops::ListResult, McpError> {
    #[cfg(all(feature = "comms", any(unix, windows)))]
    if state.shared.daemon_writer {
        use crate::comms::memory_proto::{MemoryOp, MemoryOutcome};

        let op = MemoryOp::List {
            vis_byte,
            owner: owner.to_string(),
            prefix: prefix.to_string(),
            tag: tag.map(str::to_string),
            limit: limit as u32,
            cursor,
        };
        return match super::helpers_comms::dispatch_memory_op(state, op).await? {
            MemoryOutcome::Listed {
                entries,
                total,
                truncated,
                next_cursor,
            } => Ok(super::memory_ops::ListResult {
                entries,
                total,
                truncated,
                next_cursor,
            }),
            other => Err(McpError::internal_error(
                format!("memory_list: unexpected daemon outcome {other:?}"),
                None,
            )),
        };
    }

    let store = state.shared.store.read().await;
    let idx = store
        .index_db
        .as_ref()
        .ok_or_else(|| McpError::internal_error("memory_by_key index not available", None))?;
    Ok(super::memory_ops::list_core(
        idx,
        &state.shared.scope,
        &super::memory_ops::ListQuery {
            vis_byte,
            owner,
            key_prefix: prefix,
            tag,
            limit,
            cursor: cursor.as_deref(),
        },
    )?)
}

#[cfg(feature = "memory")]
pub(super) async fn run_memory_search(
    state: &ServerState,
    params: MemorySearchParams,
) -> Result<CallToolResult, McpError> {
    let __body = std::time::Instant::now();
    let limit = params.limit.unwrap_or(10).min(100) as usize;
    let (vis_byte, owner) = namespace(state, params.visibility);
    let visibility = lance_visibility(params.visibility).to_string();
    let agent_id = owner.to_string();
    let embedding = embed_query(state, &params.query).await?;
    let lance = memory_lance_store(state).await?;
    super::memory_vectors::backfill_once(state, params.visibility, vis_byte, owner).await;
    let scope = state.shared.scope.clone();
    let tag = params.tag.clone();
    // Rows the join drops must not shrink the answer, so the vector search over-fetches.
    let fetch = limit.saturating_mul(2);
    let hits_raw = tokio::task::spawn_blocking(move || {
        lance.search_memory(&scope, &visibility, &agent_id, embedding, fetch, tag.as_deref())
    })
    .await
    .map_err(|e| McpError::internal_error(format!("spawn_blocking: {e}"), None))?
    .map_err(|e| McpError::internal_error(format!("search_memory: {e}"), None))?;
    let mut notes = Vec::with_capacity(hits_raw.len());
    for hit in &hits_raw {
        notes.push(memory_get_wire(state, vis_byte, owner, &hit.key).await?);
    }
    let hits = super::memory_vectors::truth_join(hits_raw, notes, limit);
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
    delete_vector_row(state, params.visibility, owner, &params.key).await;
    json_result(&MemoryDeleteResponse { deleted: deleted_fjall })
}

/// Run the fjall half of `memory_delete`, returning whether a record existed.
///
/// `daemon_writer` forwards a [`MemoryOp::Delete`]; every other build calls
/// [`delete_core`](super::memory_ops::delete_core) against the local index.
#[cfg(feature = "memory")]
async fn memory_delete_fjall(state: &ServerState, vis_byte: u8, owner: &str, key: &str) -> Result<bool, McpError> {
    #[cfg(all(feature = "comms", any(unix, windows)))]
    if state.shared.daemon_writer {
        use crate::comms::memory_proto::{MemoryOp, MemoryOutcome};

        let op = MemoryOp::Delete {
            vis_byte,
            owner: owner.to_string(),
            key: key.to_string(),
        };
        let outcome = super::helpers_comms::dispatch_memory_op(state, op).await?;
        return match outcome {
            MemoryOutcome::Deleted { deleted } => Ok(deleted),
            other => Err(McpError::internal_error(
                format!("memory_delete: unexpected daemon outcome {other:?}"),
                None,
            )),
        };
    }

    let store = state.shared.store.read().await;
    let idx = store
        .index_db
        .as_ref()
        .ok_or_else(|| McpError::internal_error("memory_by_key index not available", None))?;
    Ok(super::memory_ops::delete_core(
        idx,
        &state.shared.scope,
        vis_byte,
        owner,
        key,
    )?)
}

/// Pick the ingestion scope to search: the caller's if they named one, else this repo's.
///
/// Documents are partitioned by ingestion scope. The scanner writes under the repo scope; rows an
/// older version ingested from the web sit under `web:<host>` and are no longer refreshed, so
/// naming that scope is the only way to reach them.
#[cfg(feature = "documents")]
fn resolve_doc_scope(requested: Option<&str>, repo_scope: &str) -> String {
    requested.unwrap_or(repo_scope).to_string()
}

#[cfg(any(feature = "documents", feature = "code-search"))]
pub(super) async fn shared_lance_route(
    state: &ServerState,
    lane: crate::lance::shared_store::SharedLane,
) -> Option<(std::path::PathBuf, String)> {
    if !crate::lance::shared_snapshot::enabled() {
        return None;
    }
    let store = state.shared.store.read().await;
    let generation = crate::lance::shared_snapshot::generation(&store.index, &state.shared.config, lane).ok()?;
    Some((crate::store::repo_group_dir(&store.root), generation))
}

#[cfg(any(feature = "documents", feature = "code-search"))]
pub(super) fn observe_semantic_route<T>(
    tier: &str,
    requested_shared: bool,
    shared: Option<T>,
    fallback: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    if let Some(hits) = shared {
        tracing::debug!(tier, route = "layered", degraded = false, "semantic search route");
        return Ok(hits);
    }
    let hits = fallback()?;
    tracing::debug!(
        tier,
        route = "standalone",
        degraded = requested_shared,
        "semantic search route"
    );
    Ok(hits)
}

#[cfg(feature = "documents")]
pub(super) async fn run_search_documents(
    state: &ServerState,
    params: SearchDocumentsParams,
) -> Result<CallToolResult, McpError> {
    let __body = std::time::Instant::now();
    if params.entity_category.is_some()
        && let Some(reason) = crate::extract::doc_tiers::entity_filter_unavailable()
    {
        return Err(McpError::invalid_request(reason, None));
    }
    let (output_format, reranker_enabled, reranker_preset, reranker_top_k) = {
        let r = &state.shared.config.documents.reranker;
        (
            state.shared.config.documents.output.format,
            r.enabled,
            r.preset.clone(),
            r.top_k,
        )
    };

    let limit = params.limit.unwrap_or(10).min(100) as usize;
    let embedding = embed_query(state, &params.query).await?;
    let lance = lance_store(state).await?;
    let scope = resolve_doc_scope(params.scope.as_deref(), &state.shared.scope);
    let mime = params.mime_type.clone();
    let lane = crate::lance::shared_store::SharedLane::Documents;
    let routing = shared_lance_route(state, lane).await;
    let hits_raw = tokio::task::spawn_blocking(move || {
        let requested_shared = crate::lance::shared_snapshot::enabled();
        let shared = routing.and_then(|(group, generation)| {
            crate::lance::shared_snapshot::read(&lance, &group, lane, &scope, &generation, |shared, paths| {
                shared.search_documents(&scope, embedding.clone(), mime.as_deref(), paths, limit)
            })
        });
        observe_semantic_route("documents", requested_shared, shared, || {
            lance.search_documents(&scope, embedding, limit, mime.as_deref())
        })
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
            acceleration: crate::embeddings::acceleration(state.shared.config.resources.onnx_provider),
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
        let store = state.shared.store.read().await;
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

#[cfg(all(test, feature = "memory"))]
mod embed_report_tests {
    use super::embed_report;

    #[test]
    fn an_embedded_note_carries_no_notice() {
        assert_eq!(embed_report(Some(Ok(()))), (true, None));
        assert_eq!(embed_report(None), (false, None));
    }

    #[test]
    fn a_failed_embedding_is_a_notice_on_a_saved_note() {
        let (embedded, notice) = embed_report(Some(Err("load embedder: offline".to_string())));
        assert!(!embedded);
        let notice = notice.expect("notice");
        assert!(notice.contains("the note is saved"), "{notice}");
        assert!(notice.contains("load embedder: offline"), "{notice}");
        assert!(notice.contains("`query`"), "{notice}");
    }
}

#[cfg(all(test, feature = "documents"))]
mod scope_tests {
    use super::resolve_doc_scope;

    #[test]
    fn defaults_to_the_repo_scope_when_the_caller_names_none() {
        assert_eq!(resolve_doc_scope(None, "repo-abc123"), "repo-abc123");
    }

    #[test]
    fn a_legacy_web_scope_is_reachable_only_by_naming_it() {
        let legacy_scope = "web:example.com";
        let repo_scope = "repo-abc123";
        assert_ne!(resolve_doc_scope(None, repo_scope), legacy_scope);

        assert_eq!(resolve_doc_scope(Some(legacy_scope), repo_scope), legacy_scope);
    }
}

#[cfg(all(test, any(feature = "documents", feature = "code-search")))]
#[test]
fn successful_semantic_routes_emit_observed_provenance() {
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let output = Capture::default();
    let writer = output.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_ansi(false)
        .without_time()
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        assert_eq!(
            observe_semantic_route("code", true, Some(7), || panic!("fallback ran")).unwrap(),
            7
        );
        assert_eq!(observe_semantic_route("documents", true, None, || Ok(8)).unwrap(), 8);
        assert_eq!(observe_semantic_route("code", false, None, || Ok(9)).unwrap(), 9);
        assert!(observe_semantic_route::<u8>("documents", true, None, || anyhow::bail!("failed")).is_err());
    });
    let bytes = output.0.lock().unwrap();
    let events = std::str::from_utf8(&bytes).unwrap().lines().collect::<Vec<_>>();
    assert_eq!(events.len(), 3);
    for (event, expected) in events.iter().zip([
        "tier=\"code\" route=\"layered\" degraded=false",
        "tier=\"documents\" route=\"standalone\" degraded=true",
        "tier=\"code\" route=\"standalone\" degraded=false",
    ]) {
        assert!(event.contains(expected), "{event}");
    }
}
