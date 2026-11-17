//! Helper bodies for the 6 memory + document-search MCP tools.

use std::sync::Arc;

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

/// Embed a search query, with the preset's query prefix (see `SharedEmbedder::embed_query`).
#[cfg(any(feature = "memory", feature = "code-search"))]
pub(super) async fn embed_query(state: &ServerState, text: &str) -> Result<Vec<f32>, McpError> {
    embed_with(state, text, true).await
}

/// Embed a passage to be found later (a memory note): no query prefix.
#[cfg(feature = "memory")]
pub(super) async fn embed_passage(state: &ServerState, text: &str) -> Result<Vec<f32>, McpError> {
    embed_with(state, text, false).await
}

#[cfg(any(feature = "memory", feature = "code-search"))]
async fn embed_with(state: &ServerState, text: &str, query: bool) -> Result<Vec<f32>, McpError> {
    let embedder = shared_embedder(state)
        .await
        .map_err(|e| McpError::internal_error(e, None))?;
    let text = text.to_string();
    tokio::task::spawn_blocking(move || {
        if query {
            embedder.embed_query(&text)
        } else {
            embedder.embed(&text)
        }
    })
    .await
    .map_err(|e| McpError::internal_error(format!("spawn_blocking: {e}"), None))?
    .map_err(|e| McpError::internal_error(format!("embed: {e}"), None))
}

/// Open the vector store at `dir` for `embedder`'s dimension and model, off the async runtime.
#[cfg(feature = "code-search")]
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

#[cfg(feature = "code-search")]
pub(super) async fn lance_store(
    state: &ServerState,
) -> Result<Arc<crate::lance::LanceStore>, super::lance_cell::Unavailable> {
    state
        .shared
        .lance
        .get_or_open(|| async {
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
pub(super) fn memory_namespace(state: &ServerState, visibility: Visibility) -> (u8, &str) {
    let owner: &str = match visibility {
        Visibility::Individual => &state.agent_id,
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
    // The put ends in an fsync: off the reactor, like every other blocking store call.
    Ok(super::index_route::blocking(|| {
        super::memory_ops::put_core(idx, &state.shared.scope, vis_byte, owner, key, value, tags)
    })?)
}

#[cfg(feature = "memory")]
pub(super) async fn run_memory_get(state: &ServerState, params: MemoryGetParams) -> Result<CallToolResult, McpError> {
    let (vis_byte, owner) = memory_namespace(state, params.visibility);

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
    let framed = state.shared.memory_is_framed();
    let mut entries = Vec::with_capacity(keys.len());
    while entries.len() < keys.len() {
        let rest = &keys[entries.len()..];
        let chunk = super::memory_ops::request_chunk(rest, super::memory_ops::GET_MANY_MAX_KEYS, framed)?;
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
    #[cfg(all(feature = "comms", any(unix, windows)))]
    if state.shared.daemon_writer {
        use crate::comms::memory_proto::{MemoryOp, MemoryOutcome};

        let wanted = keys.len();
        let op = MemoryOp::GetMany {
            vis_byte,
            owner: owner.to_string(),
            keys: keys.to_vec(),
        };
        return match super::helpers_comms::dispatch_memory_op(state, op).await? {
            MemoryOutcome::GotMany(entries) if (1..=wanted).contains(&entries.len()) => Ok(entries),
            other => Err(McpError::internal_error(
                format!("memory_get_many: unexpected daemon outcome {other:?}"),
                None,
            )),
        };
    }

    let store = state.shared.store.read().await;
    let idx = store
        .index_db
        .as_ref()
        .ok_or_else(|| McpError::internal_error("memory_by_key index not available", None))?;
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
    let framed = state.shared.memory_is_framed();
    let mut stamps = Vec::with_capacity(keys.len());
    while stamps.len() < keys.len() {
        let rest = &keys[stamps.len()..];
        let chunk = super::memory_ops::request_chunk(rest, super::memory_ops::STAMPS_MAX_KEYS, framed)?;
        #[cfg(all(feature = "comms", any(unix, windows)))]
        if state.shared.daemon_writer {
            use crate::comms::memory_proto::{MemoryOp, MemoryOutcome};

            let op = MemoryOp::Stamps {
                vis_byte,
                owner: owner.to_string(),
                keys: chunk.to_vec(),
            };
            match super::helpers_comms::dispatch_memory_op(state, op).await? {
                MemoryOutcome::Stamps(read) if read.len() == chunk.len() => stamps.extend(read),
                other => {
                    return Err(McpError::internal_error(
                        format!("memory_stamps: unexpected daemon outcome {other:?}"),
                        None,
                    ));
                }
            }
            continue;
        }
        let store = state.shared.store.read().await;
        let idx = store
            .index_db
            .as_ref()
            .ok_or_else(|| McpError::internal_error("memory_by_key index not available", None))?;
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
    Ok(super::index_route::blocking(|| {
        super::memory_ops::delete_core(idx, &state.shared.scope, vis_byte, owner, key)
    })?)
}

#[cfg(feature = "code-search")]
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

#[cfg(feature = "code-search")]
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

#[cfg(all(test, feature = "code-search"))]
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
