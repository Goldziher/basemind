//! Helper bodies for the 6 memory + document-search MCP tools.

use std::sync::Arc;

#[cfg(feature = "memory")]
use rmcp::ErrorData as McpError;
#[cfg(feature = "memory")]
use rmcp::model::CallToolResult;

use super::ServerState;
#[cfg(feature = "memory")]
use super::helpers::elapsed_us;
#[cfg(feature = "memory")]
use super::helpers::json_result;
#[cfg(feature = "memory")]
use super::types_memory::{
    MemoryDeleteParams, MemoryDeleteResponse, MemoryEntry, MemoryGetParams, MemoryListParams, MemoryListResponse,
    MemoryPutParams, MemoryPutResponse, MemorySearchParams, Visibility,
};

/// The shared embedding engine ([`Workspace::shared_embedder`](crate::engine::Workspace)), loaded
/// on a blocking thread.
#[cfg(any(feature = "memory", feature = "documents"))]
pub(super) async fn shared_embedder(state: &ServerState) -> Result<Arc<crate::embeddings::SharedEmbedder>, String> {
    let ws = Arc::clone(&state.shared);
    super::adapter::blocking(move || ws.shared_embedder()).await
}

/// Embed a search query, with the preset's query prefix (see `SharedEmbedder::embed_query`).
#[cfg(feature = "memory")]
pub(super) async fn embed_query(state: &ServerState, text: &str) -> Result<Vec<f32>, McpError> {
    let (ws, text) = (Arc::clone(&state.shared), text.to_string());
    Ok(super::adapter::blocking(move || ws.embed_query(&text)).await?)
}

/// Embed a passage to be found later (a memory note): no query prefix.
#[cfg(feature = "memory")]
pub(super) async fn embed_passage(state: &ServerState, text: &str) -> Result<Vec<f32>, McpError> {
    let (ws, text) = (Arc::clone(&state.shared), text.to_string());
    Ok(super::adapter::blocking(move || ws.embed_passage(&text)).await?)
}

/// Per-`(scope, key)` write serialization for `memory_put`.
///
/// `memory_put` is a read-modify-write across two stores (Fjall + LanceDB).
/// Without serialization, two concurrent puts for the same key both read "no
/// existing record" and stamp different `created_at` values, and their two-phase
/// (Fjall, then async Lance) writes can interleave so the stores disagree.
///
/// We serialize per key — unrelated keys still write in parallel — by handing
/// out a per-key `tokio::sync::Mutex` from a process-global registry, itself guarded
/// by a short-lived `std::sync::Mutex` (held only to clone an `Arc`, never across an `.await`).
///
/// The registry is an `LruCache` bounded at [`MEMORY_PUT_LOCK_CAP`] so it cannot grow
/// without limit as distinct `(scope, key)` pairs are written. Eviction is safe for
/// correctness: a task already holding the `Arc` keeps its mutex alive. The single
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
pub(super) fn memory_namespace(state: &ServerState, visibility: Visibility) -> (u8, &str) {
    let owner: &str = match visibility {
        Visibility::Individual => &state.shared.memory_owner,
        Visibility::Group => "",
    };
    (visibility.vis_byte(), owner)
}

#[cfg(feature = "memory")]
pub(super) async fn run_memory_put(state: &ServerState, params: MemoryPutParams) -> Result<CallToolResult, McpError> {
    let (vis_byte, owner) = memory_namespace(state, params.visibility);
    let tags = params.tags.clone().unwrap_or_default();
    super::memory_ops::check_note(&params.key, &params.value, &tags)?;

    let (created_at, updated_at) = memory_put_fjall(state, vis_byte, owner, &params.key, &params.value, &tags).await?;

    // The note is durable from here on. A failure to embed it is reported on a successful result,
    // never as an error: an error would read as "not saved" and invite a retry of a write that
    // already happened.
    let embedded = if params.embed && !params.value.is_empty() {
        Some(super::memory_vectors::embed_note(state, &params, owner, tags, created_at, updated_at).await)
    } else {
        None
    };
    if matches!(embedded, Some(Ok(()))) {
        super::memory_vectors::owe_rows_elsewhere(state).await;
    } else {
        // The note has no row of this write. One of an earlier write still holds the OLD text's
        // vector; the join refuses it, and removing it keeps it from taking a slot in every later
        // search. The namespace is no longer level: the next search by meaning embeds the note.
        super::memory_vectors::delete_vector_row(state, params.visibility, owner, &params.key).await;
        super::memory_vectors::not_level(state, vis_byte, owner);
        let root = state.shared.root.clone();
        let _ = tokio::task::spawn_blocking(move || super::memory_vectors::owe_rows(&root)).await;
    }
    let (embedded, notice) = embed_report(embedded);
    json_result(&MemoryPutResponse {
        key: params.key,
        created_at,
        updated_at,
        embedded,
        notice,
    })
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
                 ({error}); `memory_search` with `query` will not find it until a later search embeds it"
            )),
        ),
        None => (false, None),
    }
}

/// The refusal for a memory call on a session that holds no index: another process holds the
/// workspace's index lock and no daemon serves it. It names the fix, and
/// is marked busy so the CLI exits `3`.
#[cfg(feature = "memory")]
pub(super) fn index_unavailable() -> McpError {
    McpError::invalid_request(
        "memory: the memory_by_key index not available here: another basemind process holds this \
         workspace's index. Start the daemon (`basemind comms start`), which then serves memory to every \
         session, or stop the process that holds the lock, then retry.",
        Some(serde_json::json!({ super::BUSY_DATA: true })),
    )
}

/// Run the fjall half of `memory_put`, returning the resolved `(created_at, updated_at)`.
///
/// Takes the per-key [`memory_put_lock`], process-wide so every session the daemon hosts on one
/// workspace shares it, and calls [`put_core`](super::memory_ops::put_core) against the index.
#[cfg(feature = "memory")]
async fn memory_put_fjall(
    state: &ServerState,
    vis_byte: u8,
    owner: &str,
    key: &str,
    value: &str,
    tags: &[String],
) -> Result<(i64, i64), McpError> {
    let key_lock = memory_put_lock(&state.shared.scope, vis_byte, owner, key);
    let _put_guard = key_lock.lock().await;
    let store = state.store_read().await;
    let idx = &store.index_handle().ok_or_else(index_unavailable)?;
    // The put ends in an fsync: off the reactor, like every other blocking store call.
    Ok(super::blocking::blocking(|| {
        super::memory_ops::put_core(idx, &state.shared.scope, vis_byte, owner, key, value, tags)
    })?)
}

#[cfg(feature = "memory")]
pub(super) async fn run_memory_get(state: &ServerState, params: MemoryGetParams) -> Result<CallToolResult, McpError> {
    let (vis_byte, owner) = memory_namespace(state, params.visibility);

    let entry = memory_get_wire(state, vis_byte, owner, &params.key).await?;
    json_result(&entry.map(wire_to_entry))
}

/// Read one note from the Fjall truth.
#[cfg(feature = "memory")]
pub(super) async fn memory_get_wire(
    state: &ServerState,
    vis_byte: u8,
    owner: &str,
    key: &str,
) -> Result<Option<super::memory_ops::WireMemoryEntry>, McpError> {
    let store = state.store_read().await;
    let idx = &store.index_handle().ok_or_else(index_unavailable)?;
    Ok(super::memory_ops::get_core(
        idx,
        &state.shared.scope,
        vis_byte,
        owner,
        key,
    )?)
}

/// Read several notes of one namespace from the Fjall truth, one slot per key in order. However
/// many keys, a request carries at most [`GET_MANY_MAX_KEYS`](super::memory_ops::GET_MANY_MAX_KEYS)
/// of them, and a reply answers those whose notes fit in
/// [`GET_MANY_MAX_BYTES`](super::memory_ops::GET_MANY_MAX_BYTES): the next request starts at the
/// first key left unanswered.
#[cfg(feature = "memory")]
pub(super) async fn memory_get_many_wire(
    state: &ServerState,
    vis_byte: u8,
    owner: &str,
    keys: Vec<String>,
) -> Result<Vec<Option<super::memory_ops::WireMemoryEntry>>, McpError> {
    let mut entries = Vec::with_capacity(keys.len());
    while entries.len() < keys.len() {
        let rest = &keys[entries.len()..];
        let chunk = &rest[..rest.len().min(super::memory_ops::GET_MANY_MAX_KEYS)];
        entries.extend(memory_get_chunk_wire(state, vis_byte, owner, chunk).await?);
    }
    Ok(entries)
}

/// One request of [`memory_get_many_wire`]: a slot for each of the leading keys, at least one.
#[cfg(feature = "memory")]
async fn memory_get_chunk_wire(
    state: &ServerState,
    vis_byte: u8,
    owner: &str,
    keys: &[String],
) -> Result<Vec<Option<super::memory_ops::WireMemoryEntry>>, McpError> {
    let store = state.store_read().await;
    let idx = &store.index_handle().ok_or_else(index_unavailable)?;
    Ok(super::memory_ops::get_many_core(
        idx,
        &state.shared.scope,
        vis_byte,
        owner,
        keys,
    )?)
}

/// The `updated_at` of each key's note, in order, `None` where no note exists, without the notes'
/// text: one request per [`STAMPS_MAX_KEYS`](super::memory_ops::STAMPS_MAX_KEYS) keys.
#[cfg(feature = "memory")]
pub(super) async fn memory_stamps_wire(
    state: &ServerState,
    vis_byte: u8,
    owner: &str,
    keys: &[String],
) -> Result<Vec<Option<i64>>, McpError> {
    let mut stamps = Vec::with_capacity(keys.len());
    while stamps.len() < keys.len() {
        let rest = &keys[stamps.len()..];
        let chunk = &rest[..rest.len().min(super::memory_ops::STAMPS_MAX_KEYS)];
        let store = state.store_read().await;
        let idx = &store.index_handle().ok_or_else(index_unavailable)?;
        stamps.extend(super::memory_ops::stamps_core(
            idx,
            &state.shared.scope,
            vis_byte,
            owner,
            chunk,
        )?);
    }
    Ok(stamps)
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
    let (vis_byte, owner) = memory_namespace(state, params.visibility);
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
    // ~keep Derived from the count rather than the wire flag, which also reports a `limit` cut.
    let truncated = page.total as usize > super::memory_ops::list_scan_cap(limit);
    json_result(&MemoryListResponse {
        total: page.total as usize,
        truncated,
        truncated_reason: truncated.then_some("scan_cap"),
        entries: page.entries.into_iter().map(wire_to_entry).collect(),
        next_cursor: page.next_cursor.as_deref().map(Cursor::encode_fjall),
        elapsed_us: elapsed_us(__body),
    })
}

/// One page of a namespace's notes from the Fjall truth. Values are preview-truncated, as `memory_list` returns them.
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
    let store = state.store_read().await;
    let idx = &store.index_handle().ok_or_else(index_unavailable)?;
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
    super::memory_vectors::search(state, params).await
}

#[cfg(feature = "memory")]
pub(super) async fn run_memory_delete(
    state: &ServerState,
    params: MemoryDeleteParams,
) -> Result<CallToolResult, McpError> {
    let (vis_byte, owner) = memory_namespace(state, params.visibility);
    let deleted_fjall = memory_delete_fjall(state, vis_byte, owner, &params.key).await?;
    super::memory_vectors::delete_vector_row(state, params.visibility, owner, &params.key).await;
    json_result(&MemoryDeleteResponse { deleted: deleted_fjall })
}

/// Run the fjall half of `memory_delete`, returning whether a record existed.
///
/// Calls [`delete_core`](super::memory_ops::delete_core) against the index.
#[cfg(feature = "memory")]
async fn memory_delete_fjall(state: &ServerState, vis_byte: u8, owner: &str, key: &str) -> Result<bool, McpError> {
    let store = state.store_read().await;
    let idx = &store.index_handle().ok_or_else(index_unavailable)?;
    Ok(super::blocking::blocking(|| {
        super::memory_ops::delete_core(idx, &state.shared.scope, vis_byte, owner, key)
    })?)
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
