//! The memory notes of a repo group: `memory_save`, `memory_search` and `basemind memory delete`.
//!
//! The Fjall `memory_by_key` record is the truth for a note ([`ops`]); its vector row, which
//! `memory_search` by meaning reads, is derived from it ([`vectors`]). A search no model can embed
//! matches the notes by words ([`lexical`]).

pub(crate) mod lexical;
pub(crate) mod ops;
#[cfg(test)]
pub(crate) mod test_support;
pub(crate) mod vectors;

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use self::ops::WireMemoryEntry;
use super::EngineError;
use super::cursor::Cursor;
use super::timing::elapsed_us;
use super::types::memory::{
    MemoryClearResponse, MemoryDeleteParams, MemoryDeleteResponse, MemoryEntry, MemoryGetParams, MemoryListParams,
    MemoryListResponse, MemoryPutParams, MemoryPutResponse, Visibility,
};
use super::workspace::Workspace;

/// Per-`(scope, key)` write serialization for `memory_put`.
///
/// `memory_put` is a read-modify-write across two stores (Fjall + LanceDB).
/// Without serialization, two concurrent puts for the same key both read "no
/// existing record" and stamp different `created_at` values, and their two-phase
/// (Fjall, then Lance) writes can interleave so the stores disagree.
///
/// We serialize per key — unrelated keys still write in parallel — by handing
/// out a per-key `Mutex` from a process-global registry, itself guarded
/// by a short-lived `Mutex` (held only to clone an `Arc`).
///
/// The registry is an `LruCache` bounded at [`MEMORY_PUT_LOCK_CAP`] so it cannot grow
/// without limit as distinct `(scope, key)` pairs are written. Eviction is safe for
/// correctness: a call already holding the `Arc` keeps its mutex alive. The single
/// rare-eviction caveat is that if a key's lock is evicted while one put holds
/// it and a *second* put for the same key arrives, the second put mints a fresh
/// mutex and the two no longer serialize — a window only reachable when more
/// than [`MEMORY_PUT_LOCK_CAP`] distinct keys are written between two racing
/// puts on the same key.
const MEMORY_PUT_LOCK_CAP: std::num::NonZeroUsize = std::num::NonZeroUsize::new(4096).unwrap();

type MemoryPutLockKey = (String, u8, String, String);

type MemoryPutLockRegistry = Mutex<lru::LruCache<MemoryPutLockKey, Arc<Mutex<()>>>>;

fn memory_put_lock(scope: &str, vis_byte: u8, owner: &str, key: &str) -> Arc<Mutex<()>> {
    use std::sync::OnceLock;
    static LOCKS: OnceLock<MemoryPutLockRegistry> = OnceLock::new();
    let registry = LOCKS.get_or_init(|| Mutex::new(lru::LruCache::new(MEMORY_PUT_LOCK_CAP)));
    let mut guard = registry.lock().unwrap_or_else(PoisonError::into_inner);
    let registry_key = (scope.to_string(), vis_byte, owner.to_string(), key.to_string());
    Arc::clone(guard.get_or_insert(registry_key, || Arc::new(Mutex::new(()))))
}

/// String form of a [`Visibility`] for the LanceDB `visibility` column. Matches the serde
/// `rename_all = "lowercase"` discriminants so the column reads back as the same enum.
pub(crate) fn lance_visibility(visibility: Visibility) -> &'static str {
    match visibility {
        Visibility::Group => "group",
        Visibility::Individual => "individual",
    }
}

/// What `memory_save` says about the vector row of a note that is already saved:
/// `(embedded, notice)`. `None` means embedding was not asked for.
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

/// The refusal for a memory call on a session that holds no index. A workspace nothing has scanned
/// has no index to hold, so it is told to scan; otherwise another process holds the workspace's
/// index lock and no daemon serves it, which names the fix and is busy so the CLI exits `3`.
pub(crate) fn index_unavailable(root: &Path) -> EngineError {
    index_refusal(
        crate::store_layout::group_index_dir(root)
            .join(crate::index::INDEX_DIR)
            .is_dir(),
    )
}

/// [`index_unavailable`] given whether the workspace's group index exists on disk.
pub(crate) fn index_refusal(index_exists: bool) -> EngineError {
    if !index_exists {
        return EngineError::index_unavailable(
            "memory: this workspace has no index yet. Run `basemind scan` first, then retry.",
        );
    }
    EngineError::busy(
        "memory: the memory_by_key index not available here: another basemind process holds this \
         workspace's index. Start the daemon (`basemind comms start`), which then serves memory to every \
         session, or stop the process that holds the lock, then retry.",
    )
}

/// Map a wire memory entry onto the [`MemoryEntry`] response shape.
fn wire_to_entry(w: WireMemoryEntry) -> MemoryEntry {
    MemoryEntry {
        key: w.key,
        value: w.value,
        tags: w.tags,
        created_at: w.created_at,
        updated_at: w.updated_at,
    }
}

impl Workspace {
    /// Resolve the `(vis_byte, owner)` namespace coordinates for a memory call.
    ///
    /// `group` → `owner = ""` (shared tier). `individual` → `owner` is the workspace's durable
    /// owner, resolved when it was opened ([`OpenOptions::memory_owner`]): a principal validated
    /// at boot and therefore NUL-free — safe to embed in the length-prefixed Fjall key segment.
    ///
    /// [`OpenOptions::memory_owner`]: super::workspace::OpenOptions::memory_owner
    pub(crate) fn memory_namespace(&self, visibility: Visibility) -> (u8, &str) {
        let owner: &str = match visibility {
            Visibility::Individual => &self.memory_owner,
            Visibility::Group => "",
        };
        (visibility.vis_byte(), owner)
    }

    /// Save a note in the Fjall truth, then (when asked) embed it into its vector row.
    pub(crate) fn memory_put(&self, params: MemoryPutParams) -> Result<MemoryPutResponse, EngineError> {
        let (vis_byte, owner) = self.memory_namespace(params.visibility);
        let tags = params.tags.clone().unwrap_or_default();
        ops::check_note(&params.key, &params.value, &tags)?;

        let (created_at, updated_at) = self.memory_put_fjall(vis_byte, owner, &params.key, &params.value, &tags)?;

        // The note is durable from here on. A failure to embed it is reported on a successful result,
        // never as an error: an error would read as "not saved" and invite a retry of a write that
        // already happened.
        let embedded = if params.embed && !params.value.is_empty() {
            Some(vectors::embed_note(self, &params, owner, tags, created_at, updated_at))
        } else {
            None
        };
        if matches!(embedded, Some(Ok(()))) {
            vectors::owe_rows_elsewhere(self);
        } else {
            // The note has no row of this write. One of an earlier write still holds the OLD text's
            // vector; the join refuses it, and removing it keeps it from taking a slot in every later
            // search. The namespace is no longer level: the next search by meaning embeds the note.
            vectors::delete_vector_row(self, params.visibility, owner, &params.key);
            vectors::not_level(self, vis_byte, owner);
            vectors::owe_rows(&self.root);
        }
        let (embedded, notice) = embed_report(embedded);
        Ok(MemoryPutResponse {
            key: params.key,
            created_at,
            updated_at,
            embedded,
            notice,
        })
    }

    /// Run the fjall half of `memory_put`, returning the resolved `(created_at, updated_at)`.
    ///
    /// Takes the per-key [`memory_put_lock`], process-wide so every session the daemon hosts on one
    /// workspace shares it, and calls [`put_core`](ops::put_core) against the index.
    fn memory_put_fjall(
        &self,
        vis_byte: u8,
        owner: &str,
        key: &str,
        value: &str,
        tags: &[String],
    ) -> Result<(i64, i64), EngineError> {
        let key_lock = memory_put_lock(&self.scope, vis_byte, owner, key);
        let _put_guard = key_lock.lock().unwrap_or_else(PoisonError::into_inner);
        let store = self.store_read();
        let idx = store.index_handle().ok_or_else(|| index_unavailable(&self.root))?;
        Ok(ops::put_core(&idx, &self.scope, vis_byte, owner, key, value, tags)?)
    }

    /// Read one note by key, or `None`.
    pub(crate) fn memory_get(&self, params: MemoryGetParams) -> Result<Option<MemoryEntry>, EngineError> {
        let (vis_byte, owner) = self.memory_namespace(params.visibility);
        Ok(self.memory_get_wire(vis_byte, owner, &params.key)?.map(wire_to_entry))
    }

    /// Read one note from the Fjall truth.
    pub(crate) fn memory_get_wire(
        &self,
        vis_byte: u8,
        owner: &str,
        key: &str,
    ) -> Result<Option<WireMemoryEntry>, EngineError> {
        let store = self.store_read();
        let idx = store.index_handle().ok_or_else(|| index_unavailable(&self.root))?;
        Ok(ops::get_core(&idx, &self.scope, vis_byte, owner, key)?)
    }

    /// Read several notes of one namespace from the Fjall truth, one slot per key in order. However
    /// many keys, a request carries at most [`GET_MANY_MAX_KEYS`](ops::GET_MANY_MAX_KEYS) of them,
    /// and a reply answers those whose notes fit in [`GET_MANY_MAX_BYTES`](ops::GET_MANY_MAX_BYTES):
    /// the next request starts at the first key left unanswered.
    pub(crate) fn memory_get_many_wire(
        &self,
        vis_byte: u8,
        owner: &str,
        keys: Vec<String>,
    ) -> Result<Vec<Option<WireMemoryEntry>>, EngineError> {
        let mut entries = Vec::with_capacity(keys.len());
        while entries.len() < keys.len() {
            let rest = &keys[entries.len()..];
            let chunk = &rest[..rest.len().min(ops::GET_MANY_MAX_KEYS)];
            let store = self.store_read();
            let idx = store.index_handle().ok_or_else(|| index_unavailable(&self.root))?;
            entries.extend(ops::get_many_core(&idx, &self.scope, vis_byte, owner, chunk)?);
        }
        Ok(entries)
    }

    /// The `updated_at` of each key's note, in order, `None` where no note exists, without the
    /// notes' text: one request per [`STAMPS_MAX_KEYS`](ops::STAMPS_MAX_KEYS) keys.
    pub(crate) fn memory_stamps_wire(
        &self,
        vis_byte: u8,
        owner: &str,
        keys: &[String],
    ) -> Result<Vec<Option<i64>>, EngineError> {
        let mut stamps = Vec::with_capacity(keys.len());
        while stamps.len() < keys.len() {
            let rest = &keys[stamps.len()..];
            let chunk = &rest[..rest.len().min(ops::STAMPS_MAX_KEYS)];
            let store = self.store_read();
            let idx = store.index_handle().ok_or_else(|| index_unavailable(&self.root))?;
            stamps.extend(ops::stamps_core(&idx, &self.scope, vis_byte, owner, chunk)?);
        }
        Ok(stamps)
    }

    /// One page of a namespace's notes, values preview-truncated.
    pub(crate) fn memory_list(&self, params: MemoryListParams) -> Result<MemoryListResponse, EngineError> {
        let started = std::time::Instant::now();
        let limit = params
            .limit
            .unwrap_or(super::ops::SEARCH_LIMIT_DEFAULT)
            .min(super::ops::SEARCH_LIMIT_MAX) as usize;
        let (vis_byte, owner) = self.memory_namespace(params.visibility);
        let key_prefix_filter = params.prefix.clone().unwrap_or_default();
        let cursor_bytes = params.cursor.as_ref().map(Cursor::decode_fjall).transpose()?;

        let page = self.memory_list_page(vis_byte, owner, &key_prefix_filter, &params.tags, limit, cursor_bytes)?;
        // ~keep Derived from the count rather than the wire flag, which also reports a `limit` cut.
        let truncated = page.total as usize > ops::list_scan_cap(limit);
        Ok(MemoryListResponse {
            total: page.total as usize,
            truncated,
            truncated_reason: truncated.then_some("scan_cap"),
            entries: page.entries.into_iter().map(wire_to_entry).collect(),
            next_cursor: page.next_cursor.as_deref().map(Cursor::encode_fjall),
            elapsed_us: elapsed_us(started),
        })
    }

    /// One page of a namespace's notes from the Fjall truth. Values are preview-truncated, as
    /// `memory_list` returns them.
    pub(crate) fn memory_list_page(
        &self,
        vis_byte: u8,
        owner: &str,
        prefix: &str,
        tags: &[String],
        limit: usize,
        cursor: Option<Vec<u8>>,
    ) -> Result<ops::ListResult, EngineError> {
        let store = self.store_read();
        let idx = store.index_handle().ok_or_else(|| index_unavailable(&self.root))?;
        Ok(ops::list_core(
            &idx,
            &self.scope,
            &ops::ListQuery {
                vis_byte,
                owner,
                key_prefix: prefix,
                tags,
                limit,
                cursor: cursor.as_deref(),
            },
        )?)
    }

    /// Remove every note of the repository group and, best effort, every row of every model's
    /// vector store.
    pub(crate) fn memory_clear_group(&self) -> Result<MemoryClearResponse, EngineError> {
        let notes_removed = {
            let store = self.store_read();
            let idx = store.index_handle().ok_or_else(|| index_unavailable(&self.root))?;
            ops::clear_core(&idx)?
        };
        let purge = vectors::purge_every_vector_row(self);
        vectors::forget_backfill_marks(self);
        Ok(MemoryClearResponse {
            notes_removed,
            vector_stores_purged: purge.purged,
            vector_stores_failed: purge.failed,
        })
    }

    /// Delete one note and, best effort, its vector rows in every model's store.
    pub(crate) fn memory_delete_note(&self, params: MemoryDeleteParams) -> Result<MemoryDeleteResponse, EngineError> {
        let (vis_byte, owner) = self.memory_namespace(params.visibility);
        let deleted = {
            let store = self.store_read();
            let idx = store.index_handle().ok_or_else(|| index_unavailable(&self.root))?;
            ops::delete_core(&idx, &self.scope, vis_byte, owner, &params.key)?
        };
        let purge = vectors::purge_vector_rows(self, params.visibility, owner, &params.key);
        Ok(MemoryDeleteResponse {
            deleted,
            vector_stores_failed: purge.failed,
        })
    }
}

#[cfg(test)]
#[path = "clear_tests.rs"]
mod clear_tests;

#[cfg(test)]
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
