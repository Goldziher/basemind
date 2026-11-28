use std::path::{Path, PathBuf};

use ahash::AHashMap;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::extract::SCHEMA_VER;
use crate::index::{IndexDb, IndexError};
#[cfg(feature = "intelligence")]
use crate::lance::LanceStore;
use crate::path::RelPath;
use crate::store_hash_refs::HashRefs;
use crate::store_key_order::KeyOrder;
#[path = "store_durability.rs"]
mod durability;
#[path = "store_file_map.rs"]
mod file_map;
#[path = "store_file_map_log.rs"]
mod file_map_log;
pub use durability::RESCAN_OWED;
pub(crate) use durability::open_index_for_writer;
#[cfg(test)]
pub(crate) use file_map_log::LOG_FILE as FILE_MAP_LOG;
pub(crate) use file_map_log::ViewStamp;
#[path = "store_group.rs"]
mod group;
use file_map::migrate_legacy_index_into_views;
#[cfg(test)]
pub(crate) use file_map::read_index_calls;
pub(crate) use file_map::{
    ViewOnOpen, check_schema, read_index, sets_aside, view_on_open, wipe_view, written_by_newer,
};

/// Cache layout: where the global cache root is, how a worktree root maps to a workspace cache
/// dir, and the `workspace.json` marker recording that mapping. Lives in its own module (like
/// `store_lock.rs`) to keep this file under the module size cap; re-exported here so callers keep
/// importing every name from `crate::store`.
#[cfg(feature = "intelligence")]
pub use crate::store_layout::LANCE_DIR;
#[cfg(any(feature = "test-support", test))]
pub use crate::store_layout::init_isolated_cache;
pub use crate::store_layout::{
    BLOBS_DIR, CACHE_DIR, DATA_HOME_ENV, INDEX_FILE, LOCK_FILE, LOCK_META_FILE, MEMORY_STORE_DIR, PartialScan,
    REPOS_DIR, STATUS_SIDECAR_FILE, STATUS_SIDECAR_SCHEMA_VER, ScanHealth, StatusSidecar, VIEW_STAGED, VIEW_WORKING,
    VIEWS_DIR, WORKSPACE_MARKER_FILE, WORKSPACES_DIR, WorkspaceMarker, count_fm_blobs, ensure_workspace_marker,
    global_blobs_dir, memory_store_dir, read_status_sidecar, read_workspace_marker, repo_group_dir, repo_group_key,
    status_sidecar_path, update_status_health, view_name_for_rev, workspace_cache_dir, workspace_key,
    write_status_sidecar,
};

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("msgpack encode error: {0}")]
    Encode(#[from] rmp_serde::encode::Error),
    #[error("msgpack decode error: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
    #[error("schema version mismatch: stored {found}, current {expected}")]
    SchemaMismatch { found: u16, expected: u16 },
    /// The on-disk schema is newer than this binary's. Never treated as rebuildable: the data
    /// belongs to a newer basemind sharing this cache.
    #[error(
        "index written by a newer basemind (schema {stored}); this binary understands {current} \
         — upgrade basemind"
    )]
    NewerSchema { stored: u16, current: u16 },
    /// The file map was written for an earlier layout of the blob store, so the blobs it names are
    /// not where this binary looks. Rebuildable, like an older schema.
    #[error("file map written for blob store layout {found}; this binary uses layout {expected}")]
    StaleBlobLayout { found: u8, expected: u8 },
    #[error("corrupt blob at {path}: malformed or unsupported envelope")]
    CorruptBlob { path: PathBuf },
    #[error("blob payload exceeds the supported envelope limits")]
    BlobTooLarge,
    #[error("zstd blob compression failed: {0}")]
    Compression(#[source] std::io::Error),
    #[error("zstd blob decompression failed at {path}: {source}")]
    Decompression {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{}", lock_contention_message(.path, .holder))]
    Locked {
        /// The `.lock` path whose acquisition failed.
        path: PathBuf,
        /// The live holder read from the `.lock.meta` sidecar, when present. `None` when the
        /// sidecar is missing/corrupt — the message then falls back to a generic guess.
        holder: Option<LockMeta>,
    },
    /// The file map's version fields cannot be read, so which basemind wrote it is unknown. A
    /// writer open sets it aside and rebuilds; nothing deletes it.
    #[error("file map {path} is unreadable; `basemind scan` sets it aside and rebuilds the view")]
    UnreadableFileMap { path: PathBuf },
    /// The file map's base was replaced while it was read, on every attempt: a compaction may have
    /// removed the delta log the read applied, so the map read could miss durable updates. Nothing
    /// is damaged; the read is retried later.
    #[error("file map {path} was replaced while it was read, on each of {attempts} attempts; retry")]
    FileMapUnsettled { path: PathBuf, attempts: u32 },
    #[error("inverted index error: {0}")]
    Index(#[from] IndexError),
    #[error(
        "view {view:?} has not been scanned; run `basemind scan --view {view}` \
         (or omit --view to use the working view)"
    )]
    ViewNotScanned { view: String },
}

impl StoreError {
    /// True when this error is lock contention from another live basemind process,
    /// not a corrupt store or a logic bug. Two distinct holders surface here:
    ///
    /// - [`StoreError::Locked`]: our own `fs2` advisory lock on `.basemind/.lock`,
    ///   taken by every writer (`scan` / `rescan` / `watch` / `serve`).
    /// - [`StoreError::Index`] wrapping [`fjall::Error::Locked`]: Fjall's *own*
    ///   exclusive lock taken when it opens the `index.fjall/` database. A reader can
    ///   slip past our advisory lock yet still trip this one, so the CLI must treat
    ///   both as the same "index is busy" condition and surface the same guidance.
    pub fn is_lock_contention(&self) -> bool {
        matches!(
            self,
            StoreError::Locked { .. } | StoreError::Index(IndexError::Fjall(fjall::Error::Locked))
        )
    }
}

pub use crate::store_group_live::{LivenessUnknown, doc_key};
pub use crate::store_lock::{LOCK_CONTENTION_HELP, LockHolder, LockMeta, WriterProbe, probe_writer_lock};
pub(crate) use crate::store_lock::{
    acquire_lock, acquire_lock_as, lock_contention_message, try_acquire_lock_as, writer_lock_is_held,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Index {
    pub schema_ver: u16,
    /// Relative path → FileEntry. Keyed by `RelPath` so paths with non-UTF-8 bytes
    /// round-trip losslessly through the msgpack store; valid UTF-8 paths serialize as
    /// plain strings (zero wire-format churn for the common case).
    pub files: AHashMap<RelPath, FileEntry>,
    /// Relative path → [`DocEntry`] for document-tier files (the xberg/LanceDB path — NOT code).
    /// Kept separate from `files` so code-only consumers (`list_files`, MapCache, corpus stats)
    /// stay unchanged. Populated only under the `documents` feature; `#[serde(default)]` so older
    /// `index.msgpack` blobs (no `doc_files` key) still deserialize — additive, no schema bump.
    /// Purpose: (1) skip re-extracting + re-embedding unchanged docs on rescan, and (2) mark
    /// `.doc.msgpack` blobs as GC-referenced so the blob GC stops reaping the doc cache.
    #[serde(default)]
    pub doc_files: AHashMap<RelPath, DocEntry>,
    /// [`crate::config::rules::embed_policy_digest`] of the config the last complete full scan ran
    /// under. A different value means the embed on/off switches or embed globs changed, so the next
    /// full scan re-flushes eligible files and purges vector / keyword rows of ineligible ones.
    /// `#[serde(default)]` keeps older `index.msgpack` files readable (additive, no schema bump).
    #[serde(default)]
    pub embed_policy: String,
    /// Relative path → [`crate::store_doc_tombstone::DocTombstone`] for documents whose extraction
    /// timed out, so an unchanged file is not re-extracted (and does not strand another abandoned
    /// thread) on every scan. `#[serde(default)]` keeps older `index.msgpack` files readable
    /// (additive, no schema bump).
    #[serde(default)]
    pub doc_tombstones: AHashMap<RelPath, crate::store_doc_tombstone::DocTombstone>,
    /// Relative path -> [`crate::config::list_only::ListedEntry`] for list-only files (`[scan]
    /// list_only`): known to exist, contents never read. Kept apart from `files` so no code consumer
    /// sees them; `find_files` and `index_status` read this map. Replaced whole by each full
    /// working-tree scan. `#[serde(default)]` keeps older `index.msgpack` files readable (additive,
    /// no schema bump).
    #[serde(default)]
    pub listed: AHashMap<RelPath, crate::config::list_only::ListedEntry>,
    /// The scan generation [`Store::flush`] sealed into the Fjall index before writing this file map
    /// (see `crate::index::durability`). An index stamped below it lost writes this map vouches
    /// for and is rebuilt before it is sealed again. `0` (also what a file map from before the field
    /// decodes to, via `#[serde(default)]`: additive, no schema bump) makes no claim about the index.
    #[serde(default)]
    pub generation: u64,
    /// Set when `files` was emptied so a full scan rebuilds the index, cleared by the full scan that
    /// does. Survives a process that died in between. Additive, no schema bump.
    #[serde(default)]
    pub rebuild_pending: bool,
    /// Content hashes `files` held when it was emptied for the rebuild: the blob sweep counts them
    /// as referenced until the rebuild is done, since the rebuild re-indexes from those blobs.
    #[serde(default)]
    pub rebuild_pins: Vec<String>,
    /// The layout of the blob store the hashes above are found in
    /// ([`crate::store_layout::BLOB_LAYOUT`]). A map without the field was written when blobs lay
    /// flat in `cache/blobs/` and decodes to `0`: `read_index` refuses it as stale, whatever the
    /// schema it carries, so its files are not called unchanged over blobs that are elsewhere.
    #[serde(default)]
    pub blob_layout: u8,
    /// The incarnation of the group index this map was written against (ADR-0028); `0` for a view
    /// with an index database of its own. A map naming another incarnation is emptied on open.
    #[serde(default)]
    pub group_incarnation: u64,
    /// The id the group index gave this view; `0` for a view with a database of its own.
    #[serde(default)]
    pub view_id: u32,
}

impl Index {
    pub fn empty() -> Self {
        Self {
            schema_ver: SCHEMA_VER,
            files: AHashMap::new(),
            doc_files: AHashMap::new(),
            embed_policy: String::new(),
            doc_tombstones: AHashMap::new(),
            listed: AHashMap::new(),
            generation: 0,
            rebuild_pending: false,
            rebuild_pins: Vec::new(),
            blob_layout: crate::store_layout::BLOB_LAYOUT,
            group_incarnation: 0,
            view_id: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileEntry {
    pub hash_hex: String,
    pub language: String,
    pub size_bytes: u64,
    /// File mtime in NANOSECONDS since the epoch (0 = unknown / git source). Compared with
    /// `size_bytes` as the mtime+size fast-path in `process_file` — an unchanged file skips the
    /// read + blake3 hash. Nanosecond resolution keeps that fast-path effectively race-free. Only
    /// ever compared against a stored value, never displayed, so the unit is internal.
    pub mtime: i64,
    /// The [`crate::extract::EXTRACT_EPOCH`] the entry's blob was extracted under; `0` for entries
    /// written before the field existed. An entry below the current epoch is never short-circuited
    /// as unchanged, so the scan re-extracts it (see `process_file`).
    #[serde(default)]
    pub extract_epoch: u16,
    /// The [`crate::index::keys::ContentKey`] the file's symbol and call rows were written under
    /// (ADR-0028 rule 3): its view reads those rows by this key, never by one recomputed from the
    /// other fields, which `Store::extract_again` changes while the rows stay. `None` for an entry
    /// that names no rows; a scan never takes such an entry for unchanged. Additive within the
    /// minor: a map without the field decodes to `None` and its files are extracted again.
    #[serde(
        default,
        with = "crate::index::keys_content::opt_content_key",
        skip_serializing_if = "Option::is_none"
    )]
    pub content_key: Option<crate::index::keys::ContentKey>,
    /// Lines in the file as scanned: every `\n` ends one, and a final unterminated line counts.
    /// Additive within the minor: a map without the field decodes to `0` until the file is
    /// extracted again; 0.34 rescans every workspace (ADR-0028), so no released map lacks it.
    #[serde(default)]
    pub lines: u32,
}

/// Per-document index entry — the doc-tier analogue of [`FileEntry`]. Records the content hash of
/// the source bytes (the key into the `.doc.msgpack` blob that already carries chunks + embeddings)
/// and the embedding preset the vectors were produced under, so a rescan can (a) skip re-extraction
/// when the content hash is unchanged and (b) recompute when the preset changed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DocEntry {
    pub hash_hex: String,
    pub embedding_preset: String,
    pub size_bytes: u64,
    /// File mtime in nanoseconds since the epoch (0 = unknown, or read too close to an edit to be
    /// trusted). With [`Self::size_bytes`] it lets the scan skip reading an unchanged document.
    pub mtime: i64,
    /// Whether the pass that recorded this entry left the doc's embedding requirement satisfied —
    /// vectors present, doc chunkless, or the pass didn't ask to embed. The unchanged fast path
    /// consults this (alongside [`Self::embed_attempted`]) so a tracked-but-vectorless doc that has
    /// *never* been embed-attempted still short-circuits into a re-embed — the quiescent half of the
    /// issue-#44 re-embed loop. Additive msgpack field: entries written before it existed deserialize
    /// as `false`, costing exactly one healing re-process on the next embedding pass (whose result now
    /// persists, see `Store::write_doc`).
    #[serde(default)]
    pub embedded: bool,
    /// Whether a prior `Inline` pass actually ran an embedding attempt on this exact content under
    /// this exact preset — regardless of whether xberg returned any vectors. Distinct from
    /// [`Self::embedded`] (true only when vectors are present or none were needed): a doc whose
    /// embedding *failed* (xberg yielded zero vectors — missing ONNX runtime, an unembeddable body)
    /// has `embedded == false` but `embed_attempted == true`. The unchanged fast path treats an
    /// already-attempted doc as settled, so a deterministically-unembeddable file is not re-extracted
    /// and re-embedded on every scan (issue #44 follow-up: the "no backoff" gap). A content-hash or
    /// preset change independently re-opens the attempt (both break the fast path on their own); a
    /// pure environment fix — installing ONNX without touching the file or preset — heals on the next
    /// full re-index. Additive msgpack field: pre-existing entries deserialize as `false`, costing
    /// exactly one healing re-process.
    #[serde(default)]
    pub embed_attempted: bool,
    /// [`crate::config::rules::doc_digest`] of the settings the doc was processed under. The
    /// unchanged fast path compares it with the current config, so changing a chunk-size / keyword /
    /// OCR knob re-processes the doc. Empty on entries that predate the field, which costs one
    /// cheap re-check (the cached blob is adopted and stamped, not re-extracted).
    #[serde(default)]
    pub config_digest: String,
}

pub struct Store {
    pub root: PathBuf,
    pub basemind_dir: PathBuf,
    /// Directory holding the content-addressed blob cache (`<hash>.{fm,doc,rref,chunk}.msgpack`, flat
    /// or in the bucket named for the first two digits of its hash, by the namespace's layout:
    /// `store_blob_layout`).
    /// The GLOBAL blob store at `<data home>/cache/blobs/`, shared by every workspace on the
    /// machine, so byte-identical files across repositories/worktrees are extracted + embedded
    /// exactly once. Per-workspace state (views, LanceDB, lock) lives under [`Store::basemind_dir`].
    /// See [`global_blobs_dir`].
    pub blobs_dir: PathBuf,
    /// The data home this store was opened under: where the other workspaces of its group live.
    pub data_root: crate::data_root::DataRoot,
    /// Always `true` since the blob store went global: a standalone Store references only ONE
    /// workspace's index, so it can never enumerate the full live blob set across all workspaces.
    /// Auto-GC (boot + background) is therefore disabled here — reference-counted GC that spans
    /// every workspace is the daemon's job. Retained (rather than removed) so the auto-GC skip in
    /// `mcp::background::run_background_gc` and the boot path keep compiling unchanged.
    pub blobs_shared: bool,
    pub view_dir: PathBuf,
    pub view: String,
    pub index: Index,
    /// Which content hashes `index.files` holds, kept in step by [`Store::upsert`] / [`Store::remove`]
    /// so "does any file still hold this content" is a lookup, not a pass over the manifest. Mutate
    /// `index.files` through those two methods only.
    hash_refs: HashRefs,
    /// The keys of `index.files`, `doc_files` and `doc_tombstones` in sorted order, kept in step by
    /// the mutators below so the entries under a directory are a range lookup. Mutate those maps
    /// through the store's methods only.
    pub(crate) key_order: KeyOrder,
    /// What `index` changed since its last flush, so a flush appends the changed entries instead of
    /// rewriting the whole file map (see [`file_map_log`]).
    file_map: file_map_log::FileMapState,
    /// This view's handle on the group index (`cache/groups/<group>/index.fjall/`, ADR-0028). Every
    /// read and write goes through `IndexDb`, which is cheap to clone (internally Arc'd).
    /// `None` for a read-only open in a process that holds no handle on the group (ADR-0036):
    /// callers must handle the absence.
    pub index_db: Option<IndexDb>,
    /// Where a daemon-hosted read store borrows the index for each read (ADR-0042): the pool
    /// writer's current handle, looked up per call. The store keeps no handle of its own, so a
    /// reopen of a poisoned database has only in-flight calls to wait for. `None` everywhere else.
    pub index_source: Option<IndexSource>,
    /// LanceDB-backed vector store. Lazy-opened on first document insert so a
    /// vanilla code-only scan doesn't pay the LanceDB startup cost.
    #[cfg(feature = "intelligence")]
    pub lance: Option<LanceStore>,
    /// Why the vector store could not be opened in this scan pass. The pass's later opens fail at
    /// once instead of each waiting out another holder of the store's open lock again.
    #[cfg(feature = "intelligence")]
    pub(crate) lance_unavailable: Option<String>,
    /// True when this open discarded a view index written under another schema (a release-minor
    /// bump): every blob that index referenced is now unreferenced, so the first scan of the
    /// rebuilt view should be followed by a blob sweep (see `Store::displaced_blobs_pending`).
    pub reset_stale_schema: bool,
    /// True when the fjall index was empty or just wiped (a partition-revision bump) while the file
    /// map was populated: symbol / call / reference reads would answer empty until a full scan
    /// rebuilds it. A writer open also empties the file map, in memory and on disk, so that scan
    /// re-extracts every file (blobs are reused by hash) instead of skipping them as unchanged over
    /// an empty index. A read-only open sets it for an index of an older schema, which it leaves
    /// untouched and unopened (`index_db` is `None`) for the scan to replace.
    pub index_rebuild_needed: bool,
    /// The view's per-view index of a release up to 0.33 is still on disk, its notes already in the
    /// group's store: [`Store::reap_legacy_index`] deletes it once a scan's free-space check passes.
    legacy_index_adopted: bool,
    /// Paths whose vector rows are not written yet (see [`crate::store_pending`]).
    pub pending_vectors: crate::store_pending::PendingVectors,
    /// Document keys ([`doc_key`]) the file map stopped naming since the last document sweep, by
    /// [`Store::upsert_doc`] and [`Store::remove_doc`]: an incremental pass checks only these.
    pub(crate) dropped_doc_keys: Vec<String>,
    /// Declared last: fields drop in order, and the lock must outlive the index handle above.
    _lock: Option<crate::store_lock::LockGuard>,
}

/// How a read store borrows another store's index handle per call ([`Store::index_source`]).
pub type IndexSource = std::sync::Arc<dyn Fn() -> Option<IndexDb> + Send + Sync>;

impl Store {
    /// The index handle a read uses, owned for the call: this store's own, or the one its
    /// [`index_source`](Self::index_source) lends. `None` when the session holds no index.
    pub(crate) fn index_handle(&self) -> Option<IndexDb> {
        match &self.index_source {
            Some(source) => source(),
            None => self.index_db.clone(),
        }
    }

    /// Own the effective read snapshot so indexed queries can release the store lock.
    pub fn read_index_view(&self) -> Option<std::sync::Arc<dyn crate::index::view::IndexView>> {
        self.index_handle()
            .map(|db| std::sync::Arc::new(crate::index::view::StandaloneView(db)) as _)
    }

    /// A read view over the fjall index, or `None` when this session holds none. Every index read
    /// goes through it (see [`crate::index::view`]).
    pub fn index_view(&self) -> Option<crate::index::view::StandaloneView<&IndexDb>> {
        self.index_db.as_ref().map(IndexDb::view)
    }

    /// Open the store for a specific view. View names are flat strings: `"working"`,
    /// `"staged"`, `"rev-<sha7>"`. Each view has its own `index.msgpack` under
    /// `.basemind/views/<view>/`; blobs are shared in `.basemind/blobs/`.
    pub fn open(root: &Path, view: &str) -> Result<Self, StoreError> {
        Self::open_with_holder(root, view, LockHolder::Maintenance)
    }

    /// Like [`Store::open`] but records which command (`serve` / `watch` / `scan` / `rescan`)
    /// is taking the lock, so a concurrent acquirer's contention error names the live holder.
    pub fn open_with_holder(root: &Path, view: &str, holder: LockHolder) -> Result<Self, StoreError> {
        let data_root = crate::data_root::DataRoot::current();
        let basemind_dir = data_root.workspace_cache_dir(root);
        ensure_dir(&basemind_dir)?;
        // Lock before creating any of the rebuildable subdirectories below: the cache-budget
        // eviction sweep (`store_gc_budget::evict_workspace`) takes this same lock before it
        // `remove_dir_all`s `views/` et al., and skips (`locked_skipped`) when it can't get it.
        // Locking first closes the window where it could see `views/`/`view_dir` freshly created
        // but still unlocked, delete them, and leave `flush()` writing into a directory that no
        // longer exists. ~keep
        let lock = acquire_lock_as(&basemind_dir, holder)?;
        let blobs_dir = data_root.global_blobs_dir();
        ensure_dir(&blobs_dir)?;
        let blobs_shared = true;
        ensure_dir(&basemind_dir.join(VIEWS_DIR))?;
        migrate_legacy_index_into_views(&basemind_dir)?;

        let view_dir = basemind_dir.join(VIEWS_DIR).join(view);
        ensure_dir(&view_dir)?;
        ensure_workspace_marker(&basemind_dir, root);
        let mut reset_stale_schema = false;
        let mut index = match file_map::view_on_open(&view_dir) {
            ViewOnOpen::Keep(idx) => *idx,
            ViewOnOpen::Absent => Index::empty(),
            ViewOnOpen::OlderSchema { found } => {
                reset_stale_schema = true;
                tracing::info!(
                    found,
                    expected = SCHEMA_VER,
                    view,
                    "cache schema bumped; refreshing view in place (re-extract + GC reclaims orphans)"
                );
                wipe_view(&view_dir)?;
                Index::empty()
            }
            ViewOnOpen::Discard(error) => {
                reset_stale_schema = true;
                tracing::info!(view, %error, "refreshing view in place (every file is extracted again)");
                durability::discard_view_of_a_stale_layout(&view_dir, &error)?;
                Index::empty()
            }
            ViewOnOpen::Refuse(e) => return Err(e),
        };
        let (index_db, index_rebuild_needed, legacy_index_adopted) =
            open_index_for_writer(root, view, &view_dir, &mut index, holder)?;
        let pending_vectors = crate::store_pending::PendingVectors::load(&view_dir);
        let mut store = Self {
            root: root.to_path_buf(),
            basemind_dir,
            blobs_dir,
            data_root,
            blobs_shared,
            view_dir,
            view: view.to_string(),
            hash_refs: HashRefs::of(&index.files),
            key_order: KeyOrder::of(&index),
            index,
            index_db: Some(index_db),
            #[cfg(feature = "intelligence")]
            lance: None,
            #[cfg(feature = "intelligence")]
            lance_unavailable: None,
            reset_stale_schema,
            index_rebuild_needed,
            legacy_index_adopted,
            pending_vectors,
            dropped_doc_keys: Vec::new(),
            file_map: file_map_log::FileMapState::new(),
            index_source: None,
            _lock: Some(lock),
        };
        store.persist_file_map_emptied_on_open()?;
        store.publish_live_keys();
        Ok(store)
    }

    /// Materialize this store's on-disk directories (`basemind_dir`, `blobs_dir`, `views/`,
    /// `view_dir`) if they don't already exist.
    ///
    /// A store opened via [`Store::open_read_only`] never creates these — that constructor
    /// deliberately does no filesystem writes so a read-only consumer never contends for the
    /// exclusive lock. But `admin rescan` / `basemind scan` upgrade a read-only-opened store to a
    /// writer in place (`Workspace::rescan` takes the in-process write lock and scans directly into
    /// it, never re-opening through [`Store::open_with_holder`]), so on a workspace's first-ever
    /// scan `view_dir` does not exist on disk yet and `flush()`'s `OpenOptions::open` on
    /// `index.msgpack.tmp` fails with ENOENT. Call this before scanning a store that may have been
    /// opened read-only. Cheap and idempotent when the directories already exist; does not touch
    /// the lock file. ~keep
    pub fn ensure_dirs(&self) -> Result<(), StoreError> {
        ensure_dir(&self.basemind_dir)?;
        ensure_dir(&self.blobs_dir)?;
        ensure_dir(&self.basemind_dir.join(VIEWS_DIR))?;
        ensure_dir(&self.view_dir)?;
        Ok(())
    }

    /// Open without taking the exclusive lock. Use for read-only consumers (CLI query, MCP).
    ///
    /// Reads the group index only through a handle this process already holds (ADR-0036 rule 3);
    /// otherwise, or when the index is held or unreadable, serves from blobs.
    pub fn open_read_only(root: &Path, view: &str) -> Result<Self, StoreError> {
        Self::open_read_only_inner(root, view, Some(group::ReadAccess::HeldHandle))
    }

    /// [`Store::open_read_only`] for one CLI command run with no daemon: when this process holds no
    /// handle on the group index, it takes a free group lock without waiting and opens the index
    /// for reading only, until the store is dropped. A held lock serves from blobs. Never use it for
    /// a long-lived reader: every writer of the group waits on the lock while the store lives.
    pub fn open_for_query(root: &Path, view: &str) -> Result<Self, StoreError> {
        Self::open_read_only_inner(root, view, Some(group::ReadAccess::BriefLock))
    }

    /// Open read-only WITHOUT ever touching the Fjall index (`index_db` is always `None`, so reads
    /// come purely from the shared blobs / in-RAM `MapCache`).
    ///
    /// Fjall's directory lock is exclusive even for a read-only open, so a process that opened the
    /// index would steal the lock its machine daemon (the sole writer) needs. Skipping the index
    /// entirely leaves the daemon free to hold it; the daemon's own hosted read store attaches the
    /// writer's handle to a store opened this way.
    pub fn open_read_only_no_index(root: &Path, view: &str) -> Result<Self, StoreError> {
        Self::open_read_only_inner(root, view, None)
    }

    /// Shared body for the read-only opens. `index_access` says how the group index may be reached,
    /// `None` for never (see [`Store::open_read_only`] vs [`Store::open_read_only_no_index`]).
    fn open_read_only_inner(
        root: &Path,
        view: &str,
        index_access: Option<group::ReadAccess>,
    ) -> Result<Self, StoreError> {
        let data_root = crate::data_root::DataRoot::current();
        let basemind_dir = data_root.workspace_cache_dir(root);
        if basemind_dir.exists() {
            let _ = migrate_legacy_index_into_views(&basemind_dir);
            ensure_workspace_marker(&basemind_dir, root);
        }
        let blobs_dir = data_root.global_blobs_dir();
        let blobs_shared = true;
        let view_dir = basemind_dir.join(VIEWS_DIR).join(view);
        if view != VIEW_WORKING && !view_dir.join(INDEX_FILE).exists() {
            return Err(StoreError::ViewNotScanned { view: view.to_string() });
        }
        let mut stale_layout = false;
        let (index, schema_ok) = match read_index(&view_dir) {
            Ok(Some(idx)) => (idx, true),
            Ok(None) => (Index::empty(), true),
            Err(StoreError::SchemaMismatch { found, expected }) => {
                tracing::warn!(
                    found,
                    expected,
                    "cache schema mismatch; index reads empty until `basemind scan` refreshes it"
                );
                (Index::empty(), false)
            }
            Err(error @ StoreError::StaleBlobLayout { .. }) => {
                tracing::warn!(%error, "index reads empty until `basemind scan` refreshes it");
                stale_layout = true;
                (Index::empty(), false)
            }
            Err(e) => return Err(e),
        };
        // A stale layout leaves an index of the current schema over a map that is gone: the scan
        // that takes the lock replaces both ([`Store::open_index_owed_to_scan`]). ~keep
        let mut index_rebuild_needed = stale_layout;
        if matches!(index_access, Some(group::ReadAccess::BriefLock)) {
            wait_for_embed_worker(&basemind_dir);
        }
        let index_db = if let Some(access) = index_access.filter(|_| schema_ok && !writer_lock_is_held(&basemind_dir)) {
            let (db, rebuild) = group::open_index_read_only(root, view, &index, access)?;
            index_rebuild_needed |= rebuild;
            db
        } else {
            None
        };
        // The journal lost the tail a fsynced file map vouches for: serve, but say a rescan is owed. ~keep
        if let Some(db) = index_db.as_ref()
            && db.scan_generation().is_ok_and(|sealed| index.generation > sealed)
        {
            index_rebuild_needed = true;
        }
        let pending_vectors = crate::store_pending::PendingVectors::load(&view_dir);
        let store = Self {
            root: root.to_path_buf(),
            basemind_dir,
            blobs_dir,
            data_root,
            blobs_shared,
            view_dir,
            view: view.to_string(),
            hash_refs: HashRefs::of(&index.files),
            key_order: KeyOrder::of(&index),
            index,
            index_db,
            #[cfg(feature = "intelligence")]
            lance: None,
            #[cfg(feature = "intelligence")]
            lance_unavailable: None,
            reset_stale_schema: false,
            index_rebuild_needed,
            legacy_index_adopted: false,
            pending_vectors,
            dropped_doc_keys: Vec::new(),
            file_map: file_map_log::FileMapState::new(),
            index_source: None,
            _lock: None,
        };
        store.publish_live_keys_if_unset();
        Ok(store)
    }

    /// Lazy-open the repository group's code-chunk vector store for `embedding_model` at `dim`
    /// ([`crate::lance::code_store_dir`]). Subsequent calls return the cached handle; the first call
    /// pays the connection + table-init cost. The store is shared by every worktree of the group, and
    /// the open removes the per-workspace `lance/` directory an earlier build kept it in: its rows
    /// were keyed by path, and the next scan writes this view's from the cached chunk blobs without
    /// embedding again.
    ///
    /// A mismatch between the stored `(dim, embedding_model)` and the values passed here wipes the
    /// whole store directory and rebuilds it, the standard schema-bump migration story for the vector
    /// store.
    ///
    /// An open that fails is not tried again until [`Self::retry_vector_store`]: the error is
    /// remembered and returned at once.
    #[cfg(feature = "intelligence")]
    pub fn lance_or_open(&mut self, dim: u16, embedding_model: &str) -> Result<&LanceStore, anyhow::Error> {
        if self.lance.is_none() {
            if let Some(reason) = &self.lance_unavailable {
                return Err(anyhow::anyhow!("{reason} (in an earlier open of this scan pass)"));
            }
            let dir = crate::lance::code_store_dir(&self.root, embedding_model, dim);
            let store = LanceStore::open(&dir, dim, embedding_model)
                .inspect_err(|error| self.lance_unavailable = Some(format!("{error:#}")))?;
            self.lance = Some(store);
            let legacy = self.basemind_dir.join(LANCE_DIR);
            if legacy.is_dir()
                && let Err(error) = std::fs::remove_dir_all(&legacy)
            {
                tracing::debug!(%error, dir = %legacy.display(), "removing the workspace's old vector store failed");
            }
        }
        // SAFETY of unwrap: we just populated it on the line above when None.
        Ok(self.lance.as_ref().expect("lance store just populated"))
    }

    /// Begin a scan pass: a vector store an earlier pass could not open is tried again.
    pub fn retry_vector_store(&mut self) {
        #[cfg(feature = "intelligence")]
        {
            self.lance_unavailable = None;
        }
    }

    /// Whether the group's code-chunk vector stores directory already exists on disk. Lets callers
    /// avoid lazily *creating* a store (via [`Self::lance_or_open`]) just to issue a delete that would
    /// target a table that was never built (e.g. a stale-file purge on a repo that never enabled
    /// code-search embeddings).
    #[cfg(feature = "intelligence")]
    pub fn code_stores_exist(&self) -> bool {
        crate::lance::code_stores_dir(&self.root).exists()
    }

    pub fn upsert(&mut self, rel: impl Into<RelPath>, entry: FileEntry) {
        self.hash_refs.acquire(&entry.hash_hex);
        let rel = rel.into();
        self.key_order.insert(&rel);
        let prior = self.index.files.insert(rel.clone(), entry);
        self.file_map.file_changed(rel, prior.is_none());
        if let Some(prior) = prior {
            self.hash_refs.release(&prior.hash_hex);
        }
    }

    pub fn remove(&mut self, rel: impl AsRef<[u8]>) {
        if let Some((rel, prior)) = self.index.files.remove_entry(bstr::BStr::new(rel.as_ref())) {
            self.hash_refs.release(&prior.hash_hex);
            self.key_order.forget(&rel, &self.index);
            self.file_map.file_dropped(rel);
        }
    }

    /// Have the next pass extract `rel` again although its content is unchanged: its entry's
    /// extraction epoch is reset, which no pass takes for current. Its [`FileEntry::content_key`]
    /// stays, so the view keeps reading the rows it names until that pass replaces the entry.
    #[cfg(feature = "code-search")]
    pub(crate) fn extract_again(&mut self, rel: &str) {
        if let Some(entry) = self.index.files.get_mut(bstr::BStr::new(rel.as_bytes())) {
            entry.extract_epoch = 0;
            self.file_map.file_edited(RelPath::from(rel));
        }
    }

    /// True when some indexed file holds the content `hash_hex`.
    pub fn hash_is_held(&self, hash_hex: &str) -> bool {
        self.hash_refs.is_held(hash_hex)
    }

    /// The sorted keys of every indexed file, document and tombstone.
    pub(crate) fn key_order(&self) -> &KeyOrder {
        &self.key_order
    }

    /// Look a file up by its repository-relative path. Accepts any byte source —
    /// `&str`, `&RelPath`, `&[u8]` — so call sites that already hold a String can keep
    /// working without an explicit conversion.
    pub fn lookup(&self, rel: impl AsRef<[u8]>) -> Option<&FileEntry> {
        self.index.files.get(bstr::BStr::new(rel.as_ref()))
    }

    /// Insert / replace the document-tier index entry for `rel`. The doc-tier analogue of
    /// [`Store::upsert`].
    pub fn upsert_doc(&mut self, rel: impl Into<RelPath>, entry: DocEntry) {
        let rel = rel.into();
        let replaced = self
            .index
            .doc_files
            .get(&rel)
            .filter(|old| old.hash_hex != entry.hash_hex || old.config_digest != entry.config_digest)
            .map(|old| doc_key(&old.hash_hex, &old.config_digest));
        self.key_order.insert(&rel);
        let prior = self.index.doc_files.insert(rel.clone(), entry);
        self.file_map.doc_changed(rel, prior.is_none());
        self.dropped_doc_keys.extend(replaced);
    }

    /// Drop the document-tier index entry for `rel`.
    pub fn remove_doc(&mut self, rel: impl AsRef<[u8]>) {
        if let Some((rel, old)) = self.index.doc_files.remove_entry(bstr::BStr::new(rel.as_ref())) {
            self.dropped_doc_keys.push(doc_key(&old.hash_hex, &old.config_digest));
            self.key_order.forget(&rel, &self.index);
            self.file_map.doc_dropped(rel);
        }
    }

    /// Look up a document-tier entry by repository-relative path.
    pub fn lookup_doc(&self, rel: impl AsRef<[u8]>) -> Option<&DocEntry> {
        self.index.doc_files.get(bstr::BStr::new(rel.as_ref()))
    }

    /// Atomically rewrite the index file (tmp + rename), after making the Fjall index durable.
    ///
    /// The file map is what lets the next scan skip a file as unchanged, so the rows it vouches for
    /// must be on disk before it is: the index seals a new generation (one journal fsync) first, and
    /// a failed or refused seal fails the flush with the previous file map left in place. A store
    /// with no index handle writes no rows, so its file map keeps the generation it carries. The
    /// paths owed vector rows are written first: the map must not claim a file whose debt is lost.
    pub fn flush(&mut self) -> Result<(), StoreError> {
        self.flush_sealing(false, false)
    }

    /// [`Self::flush`] for an incremental pass: when only entries this store changed differ from
    /// what it last wrote, appends them to the file map's delta log instead of rewriting the file
    /// map (see [`file_map_log`]).
    pub(crate) fn flush_delta(&mut self) -> Result<(), StoreError> {
        self.flush_sealing(false, true)
    }

    /// [`Self::flush`] for the flush that ends a pass which wrote rows and was not cancelled: its
    /// seal records the pass ended ([`IndexDb::seal_scan_ending_rows_pass`]).
    pub(crate) fn flush_ending_rows_pass(&mut self) -> Result<(), StoreError> {
        self.flush_sealing(true, false)
    }

    /// [`Self::flush_ending_rows_pass`] writing only the changed entries, as [`Self::flush_delta`].
    pub(crate) fn flush_ending_rows_pass_delta(&mut self) -> Result<(), StoreError> {
        self.flush_sealing(true, true)
    }

    fn flush_sealing(&mut self, ends_rows_pass: bool, delta: bool) -> Result<(), StoreError> {
        self.pending_vectors.persist(&self.view_dir)?;
        if let Some(db) = self.index_db.as_ref() {
            self.index.generation = if ends_rows_pass {
                db.seal_scan_ending_rows_pass(self.index.generation)?
            } else {
                db.seal_scan(self.index.generation)?
            };
        }
        self.file_map.write(&self.view_dir, &self.index, delta)?;
        self.publish_live_keys();
        Ok(())
    }

    /// Bytes this store's flushes wrote to the file map so far (the base file and its delta log).
    pub fn file_map_bytes_written(&self) -> u64 {
        self.file_map.bytes_written()
    }

    /// Refresh the cheap `status.json` sidecar for a shell statusline (see [`StatusSidecar`]).
    ///
    /// A no-op for non-working views: the statusline only ever reflects the working view, and writing
    /// from a `staged`/`rev-*` scan would clobber it with the wrong counts. Best-effort — a failed
    /// write is swallowed by [`write_status_sidecar`], so it never affects a scan's success.
    ///
    /// Keeps the completeness fields the last full scan recorded: an incremental update refreshes
    /// counts but must not mark a partial or unresolved index healthy.
    pub fn write_status_sidecar(&self) {
        let health = read_status_sidecar(&self.basemind_dir).map_or_else(ScanHealth::complete, |s| s.health());
        self.write_status_sidecar_with(&health);
    }

    /// [`Self::write_status_sidecar`] recording `health` — what a full scan states about itself.
    pub fn write_status_sidecar_with(&self, health: &ScanHealth) {
        if self.view != VIEW_WORKING {
            return;
        }
        let file_count = self.index.files.len();
        // This workspace's distinct contents, not a directory listing of the machine-wide blob
        // namespace, which every watcher batch paid for and which counted every workspace's blobs. ~keep
        let blob_count = self.hash_refs.distinct();
        write_status_sidecar(&self.basemind_dir, file_count, blob_count, health);
    }

    /// Drop and reopen the Fjall index, replacing a poisoned handle with a fresh one. Our handle is
    /// released first. While any other handle on the group database stays open in this process (a
    /// clone, or another view of the group) the reopen would only hand that same database back, so
    /// it waits briefly for them to drop and then fails with [`IndexError::StillOpen`]; on any
    /// failure `index_db` stays `None`, which the caller must treat as a dead store.
    pub fn reopen_index_db(&mut self) -> Result<(), IndexError> {
        if self.index_db.take().is_some() {
            self.index_db = Some(group::reopen_view(&self.root, &self.view)?);
            self.publish_written_live_keys_if_unset();
        }
        Ok(())
    }

    /// Record `health` while leaving the sidecar's counts as the last durable flush wrote them. For
    /// an aborted scan, whose in-memory file map is ahead of the unflushed `index.msgpack`.
    pub fn mark_status_health(&self, health: &ScanHealth) {
        if self.view == VIEW_WORKING {
            update_status_health(&self.basemind_dir, health);
        }
    }
}

fn ensure_dir(p: &Path) -> Result<(), StoreError> {
    std::fs::create_dir_all(p).map_err(|source| StoreError::Io {
        path: p.to_path_buf(),
        source,
    })
}

/// Empty an explicit blob directory (keeping the directory itself). Used by
/// `store_gc::clear_component` for an explicit `Blobs` component clear (the CLI / MCP admin
/// surface): production passes the GLOBAL blob store ([`global_blobs_dir`]), unit tests pass a
/// per-test temp dir so a `Blobs` clear never wipes the machine-global store nor races sibling
/// content-addressed-blob tests.
///
/// The blob store is machine-global now, so a production `Blobs` clear reaps blobs for EVERY
/// workspace — the daemon (Track E) owns per-workspace-safe reference-counted GC. NOT called on a
/// schema bump: `Store::open` refreshes blobs durably in place (re-extract overwrites stale blobs;
/// orphans are reclaimed by `store_gc::run_gc`) rather than destroying the cache.
pub(crate) fn wipe_blobs_in(blobs_dir: &Path) -> Result<(), StoreError> {
    if blobs_dir.exists() {
        std::fs::remove_dir_all(blobs_dir).map_err(|source| StoreError::Io {
            path: blobs_dir.to_path_buf(),
            source,
        })?;
        std::fs::create_dir_all(blobs_dir).map_err(|source| StoreError::Io {
            path: blobs_dir.to_path_buf(),
            source,
        })?;
    }
    Ok(())
}

/// Retries a writer performs when opening the group index hits a *transient* fjall `Locked`: it
/// already holds the group lock, so fjall's directory lock can only be held by a handle shutting
/// down. ~10 × 50 ms tracks the fs2 `.lock` retry budget (`acquire_lock_as`).
const INDEX_OPEN_RETRIES: u32 = 10;
const INDEX_OPEN_BACKOFF: std::time::Duration = std::time::Duration::from_millis(50);

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "store_key_set_tests.rs"]
mod key_set_tests;

#[cfg(test)]
#[path = "store_vector_tests.rs"]
mod vector_tests;

/// A one-shot query finding the background embedding worker on the workspace lock asks it to give
/// the lock up and waits for it ([`store_lock::embed_yield`]), so the query reads the index rather
/// than only the blobs. Any other holder is left alone.
fn wait_for_embed_worker(basemind_dir: &Path) {
    let held_by_worker = matches!(
        probe_writer_lock(basemind_dir),
        WriterProbe::Held { holder: Some(meta) } if crate::store_lock::embed_yield::held_by_worker(&meta.command)
    );
    if held_by_worker {
        let _ = crate::store_lock::embed_yield::wait_for_worker(basemind_dir, || {
            if writer_lock_is_held(basemind_dir) {
                Err(())
            } else {
                Ok(())
            }
        });
    }
}
