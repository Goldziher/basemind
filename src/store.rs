use std::fs::OpenOptions;
use std::io::Write;
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
#[path = "store_base_open.rs"]
mod base_open;
#[path = "store_durability.rs"]
mod durability;
pub use durability::RESCAN_OWED;

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
    VIEWS_DIR, WORKSPACE_MARKER_FILE, WORKSPACES_DIR, WorkspaceMarker, cache_root, count_fm_blobs,
    ensure_workspace_marker, global_blobs_dir, memory_store_dir, read_status_sidecar, read_workspace_marker,
    repo_group_dir, repo_group_key, status_sidecar_path, update_status_health, view_name_for_rev, workspace_cache_dir,
    workspace_key, write_status_sidecar,
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
    #[error("working view migration recovery failed: {0}")]
    Migration(#[source] anyhow::Error),
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

pub use crate::store_lock::{LOCK_CONTENTION_HELP, LockHolder, LockMeta, WriterProbe, probe_writer_lock};
pub(crate) use crate::store_lock::{acquire_lock, acquire_lock_as, lock_contention_message, writer_lock_is_held};

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
    /// The scan generation [`Store::flush`] sealed into the Fjall index before writing this file map
    /// (see [`crate::index::durability`]). An index stamped below it lost writes this map vouches
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
    /// flat in `cache/blobs/` and decodes to `0`: [`read_index`] refuses it as stale, whatever the
    /// schema it carries, so its files are not called unchanged over blobs that are elsewhere.
    #[serde(default)]
    pub blob_layout: u8,
}

impl Index {
    pub fn empty() -> Self {
        Self {
            schema_ver: SCHEMA_VER,
            files: AHashMap::new(),
            doc_files: AHashMap::new(),
            embed_policy: String::new(),
            doc_tombstones: AHashMap::new(),
            generation: 0,
            rebuild_pending: false,
            rebuild_pins: Vec::new(),
            blob_layout: crate::store_layout::BLOB_LAYOUT,
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
    /// File mtime in nanoseconds since the epoch (0 = unknown). Recorded for symmetry with
    /// [`FileEntry`]; the doc unchanged-skip currently keys on the content hash, not mtime.
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
    /// Directory holding the content-addressed blob cache (`<hash>.{fm,doc,rref,chunk}.msgpack`).
    /// The GLOBAL blob store at `cache_root()/cache/blobs/`, shared by every workspace on the
    /// machine, so byte-identical files across repositories/worktrees are extracted + embedded
    /// exactly once. Per-workspace state (views, LanceDB, lock) lives under [`Store::basemind_dir`].
    /// See [`global_blobs_dir`].
    pub blobs_dir: PathBuf,
    pub(crate) fallback_blobs_dir: Option<PathBuf>,
    pub(crate) layered_read: Option<std::sync::Arc<dyn crate::index::view::IndexView>>,
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
    /// Fjall-backed inverted index over symbols / calls / imports. Lives under
    /// `view_dir/index.fjall/`. Reads + writes from any caller go through `IndexDb`,
    /// which is cheap to clone (internally Arc'd). `None` in read-only mode when the
    /// directory doesn't exist yet — callers must handle the absence.
    pub index_db: Option<IndexDb>,
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
    /// Paths whose vector rows are not written yet (see [`crate::store_pending`]).
    pub pending_vectors: crate::store_pending::PendingVectors,
    /// Declared last: fields drop in order, and the lock must outlive the index handle above.
    _lock: Option<crate::store_lock::LockGuard>,
}

impl Store {
    pub(crate) fn install_layered_manifest(
        &mut self,
        index: Index,
        blobs: PathBuf,
        fallback: PathBuf,
        view: std::sync::Arc<dyn crate::index::view::IndexView>,
    ) {
        self.hash_refs = HashRefs::of(&index.files);
        // The generation vouches for this store's own index, not the shared manifest's. ~keep
        let generation = self.index.generation;
        self.index = index;
        self.index.generation = generation;
        self.blobs_dir = blobs;
        self.fallback_blobs_dir = Some(fallback);
        self.layered_read = Some(view);
    }

    /// Own the effective read snapshot so indexed queries can release the store lock.
    pub(crate) fn read_index_view(&self) -> Option<std::sync::Arc<dyn crate::index::view::IndexView>> {
        self.layered_read.clone().or_else(|| {
            self.index_db
                .as_ref()
                .map(|db| std::sync::Arc::new(crate::index::view::StandaloneView(db.clone())) as _)
        })
    }

    /// Reconstruct a reclaimed working view under the normal writer lock.
    #[cfg(unix)]
    pub fn rebuild_migrated_working_view(
        root: &Path,
        config: &crate::config::Config,
        cancel: &crate::scanner::ScanCancel,
    ) -> anyhow::Result<bool> {
        let workspace = workspace_cache_dir(root);
        let views = workspace.join(VIEWS_DIR);
        if !views.join("working.migration").try_exists()? {
            return Ok(false);
        }
        let _writer = acquire_lock_as(&workspace, LockHolder::Scan)?;
        let migration = crate::store_migration::Migration::lock(&views)?;
        migration.rebuild(|stage| {
            anyhow::ensure!(
                !config.documents.enabled
                    && !config.documents.embed
                    && !config.code_search.enabled
                    && !config.code_search.embed,
                "standalone migration rebuild requires documents and code search disabled"
            );
            let mut store = Self::open_base_scratch(root, stage)?;
            store.blobs_dir = global_blobs_dir();
            ensure_dir(&store.blobs_dir)?;
            store.blobs_shared = true;
            let report = crate::scanner::scan_with_cancel(
                root,
                &mut store,
                config,
                crate::scanner::ScanSource::WorkingTree,
                crate::scanner::EmbedMode::Deferred,
                cancel,
            )?;
            anyhow::ensure!(
                !report.cancelled
                    && report.stats.read_failed == 0
                    && report.stats.extract_failed == 0
                    && store.index_db.as_ref().is_some_and(|db| !db.is_poisoned()),
                "incomplete standalone migration rebuild"
            );
            store.flush()?;
            Ok(())
        })
    }

    /// Retire the standalone working view after comparing it to a leased clean generation.
    #[cfg(unix)]
    pub fn retire_verified_working_view(
        &mut self,
        config: &crate::config::Config,
        reclaim: bool,
    ) -> anyhow::Result<bool> {
        if self.view != VIEW_WORKING || self._lock.is_none() {
            return Ok(false);
        }
        let root = self.root.clone();
        let open_candidate = || -> anyhow::Result<_> {
            let mut candidate = Self::open_read_only_no_index(&root, VIEW_WORKING)?;
            candidate.index = Index::empty();
            crate::store_base::hydrate_cold_store(&mut candidate, config)?;
            Ok(candidate)
        };
        let candidate_store = open_candidate()?;
        let Some(layered) = candidate_store.layered_read.as_ref() else {
            return Ok(false);
        };
        let dirty = candidate_store.fallback_blobs_dir.as_ref() != Some(&candidate_store.blobs_dir);
        if dirty && reclaim {
            return Ok(false);
        }
        let Some(standalone) = self.index_view() else {
            return Ok(false);
        };
        crate::index::verify_shared_base(layered.as_ref(), &standalone)?;
        let directory = candidate_store
            .fallback_blobs_dir
            .as_ref()
            .and_then(|path| path.parent())
            .ok_or_else(|| anyhow::anyhow!("missing generation directory"))?;
        let candidate = &candidate_store.index;
        if self.index.files.len() != candidate.files.len()
            || self.index.files.iter().any(|(path, entry)| {
                candidate.files.get(path).is_none_or(|other| {
                    other.hash_hex != entry.hash_hex
                        || other.language != entry.language
                        || other.extract_epoch != entry.extract_epoch
                })
            })
            || !self.index.doc_files.is_empty()
        {
            return Ok(false);
        }
        let generation: u64 = directory
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix("gen-"))
            .ok_or_else(|| anyhow::anyhow!("invalid generation path"))?
            .parse()?;
        if dirty {
            let pin = repo_group_dir(&root)
                .join("overlay-pins")
                .join(format!("{}-{generation}", crate::store_layout::workspace_key(&root)));
            anyhow::ensure!(
                std::fs::read(pin)? == generation.to_be_bytes(),
                "invalid overlay generation pin"
            );
        }
        let views = self.basemind_dir.join(VIEWS_DIR);
        let migration = crate::store_migration::Migration::lock(&views)?;
        drop(self.index_db.take());
        let result = migration.retire(generation, || Ok(())).and_then(|()| {
            migration.commit(generation, || {
                let current = open_candidate()?;
                anyhow::ensure!(
                    current.layered_read.is_some()
                        && current.blobs_dir == candidate_store.blobs_dir
                        && current.fallback_blobs_dir == candidate_store.fallback_blobs_dir,
                    "worktree no longer matches the verified generation and overlay"
                );
                Ok(())
            })
        });
        if let Err(error) = result {
            migration.recover(|_| false)?;
            return Err(error);
        }
        if reclaim {
            migration.reclaim()?;
        }
        Ok(true)
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
        let basemind_dir = workspace_cache_dir(root);
        ensure_dir(&basemind_dir)?;
        // Lock before creating any of the rebuildable subdirectories below: the cache-budget
        // eviction sweep (`store_gc_budget::evict_workspace`) takes this same lock before it
        // `remove_dir_all`s `views/` et al., and skips (`locked_skipped`) when it can't get it.
        // Locking first closes the window where it could see `views/`/`view_dir` freshly created
        // but still unlocked, delete them, and leave `flush()` writing into a directory that no
        // longer exists. ~keep
        let lock = acquire_lock_as(&basemind_dir, holder)?;
        let blobs_dir = global_blobs_dir();
        ensure_dir(&blobs_dir)?;
        let blobs_shared = true;
        ensure_dir(&basemind_dir.join(VIEWS_DIR))?;
        migrate_legacy_index_into_views(&basemind_dir)?;

        let view_dir = basemind_dir.join(VIEWS_DIR).join(view);
        // A fresh linked worktree clones its working view from a sibling checkout (COW, atomic
        // rename) BEFORE fjall opens, so the first scan skips content-identical files. ~keep
        if view == VIEW_WORKING {
            crate::store_migration::Migration::recover_for_writer(&basemind_dir.join(VIEWS_DIR))
                .map_err(StoreError::Migration)?;
            crate::store_seed::seed_working_view(root, &basemind_dir, &view_dir);
        }
        ensure_dir(&view_dir)?;
        ensure_workspace_marker(&basemind_dir, root);
        let mut reset_stale_schema = false;
        let mut index = match read_index(&view_dir) {
            Ok(Some(idx)) => idx,
            Ok(None) => Index::empty(),
            Err(StoreError::SchemaMismatch { found, expected }) => {
                reset_stale_schema = true;
                tracing::info!(
                    found,
                    expected,
                    view,
                    "cache schema bumped; refreshing view in place (re-extract + GC reclaims orphans)"
                );
                wipe_view(&view_dir)?;
                Index::empty()
            }
            // A map that does not decode is rebuildable cache; refusing it left the workspace unopenable
            // until someone cleared the cache. Its index goes too: no map lists the files of its rows. ~keep
            Err(error @ (StoreError::StaleBlobLayout { .. } | StoreError::Decode(_))) => {
                reset_stale_schema = true;
                tracing::info!(view, %error, "refreshing view in place (every file is extracted again)");
                durability::discard_view_of_a_stale_layout(&view_dir)?;
                Index::empty()
            }
            Err(e) => return Err(e),
        };
        let (index_db, index_rebuild_needed) = durability::open_index_for_writer(root, view, &view_dir, &mut index)?;
        let pending_vectors = crate::store_pending::PendingVectors::load(&view_dir);
        let mut store = Self {
            root: root.to_path_buf(),
            basemind_dir,
            blobs_dir,
            blobs_shared,
            fallback_blobs_dir: None,
            layered_read: None,
            view_dir,
            view: view.to_string(),
            hash_refs: HashRefs::of(&index.files),
            index,
            index_db: Some(index_db),
            #[cfg(feature = "intelligence")]
            lance: None,
            #[cfg(feature = "intelligence")]
            lance_unavailable: None,
            reset_stale_schema,
            index_rebuild_needed,
            pending_vectors,
            _lock: Some(lock),
        };
        store.persist_file_map_emptied_on_open()?;
        Ok(store)
    }

    /// Materialize this store's on-disk directories (`basemind_dir`, `blobs_dir`, `views/`,
    /// `view_dir`) if they don't already exist.
    ///
    /// A store opened via [`Store::open_read_only`] never creates these — that constructor
    /// deliberately does no filesystem writes so a read-only consumer never contends for the
    /// exclusive lock. But `admin rescan` / `basemind scan` upgrade a read-only-opened store to a
    /// writer in place (`scan_and_refresh` takes the in-process write lock and scans directly into
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
    /// Opens the Fjall index for reads when the writer lock is free; falls back to blob-only reads
    /// when the index is held or unreadable.
    pub fn open_read_only(root: &Path, view: &str) -> Result<Self, StoreError> {
        Self::open_read_only_inner(root, view, true)
    }

    /// Open read-only WITHOUT ever touching the Fjall index (`index_db` is always `None`, so reads
    /// come purely from the shared blobs / in-RAM `MapCache`).
    ///
    /// This is the `daemon_writer` serve path: Fjall's directory lock is exclusive even for a
    /// read-only open, so a serve that opened the index would steal the lock its own machine daemon
    /// (the sole writer) needs. Skipping the index entirely leaves the daemon free to hold it.
    pub fn open_read_only_no_index(root: &Path, view: &str) -> Result<Self, StoreError> {
        Self::open_read_only_inner(root, view, false)
    }

    /// Shared body for the read-only opens. `allow_index_db` gates whether the Fjall index is opened
    /// (see [`Store::open_read_only`] vs [`Store::open_read_only_no_index`]).
    fn open_read_only_inner(root: &Path, view: &str, allow_index_db: bool) -> Result<Self, StoreError> {
        let basemind_dir = workspace_cache_dir(root);
        if basemind_dir.exists() {
            let _ = migrate_legacy_index_into_views(&basemind_dir);
            ensure_workspace_marker(&basemind_dir, root);
        }
        let blobs_dir = global_blobs_dir();
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
        let index_db = if allow_index_db && schema_ok && view_dir.exists() && !writer_lock_is_held(&basemind_dir) {
            match IndexDb::open_tolerating_held_memory(&view_dir, &memory_store_dir(root)) {
                Ok(db) => Some(db),
                Err(IndexError::Fjall(fjall::Error::Locked)) => None,
                Err(error @ IndexError::NewerSchema { .. }) => return Err(error.into()),
                Err(error @ IndexError::NeedsRescan { .. }) => {
                    tracing::warn!(%error, "serving from blobs until a scan rebuilds the index");
                    index_rebuild_needed = true;
                    None
                }
                Err(error) => {
                    tracing::warn!(%error, "read-only index open failed; degrading to blob-only reads");
                    None
                }
            }
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
        Ok(Self {
            root: root.to_path_buf(),
            basemind_dir,
            blobs_dir,
            blobs_shared,
            fallback_blobs_dir: None,
            layered_read: None,
            view_dir,
            view: view.to_string(),
            hash_refs: HashRefs::of(&index.files),
            index,
            index_db,
            #[cfg(feature = "intelligence")]
            lance: None,
            #[cfg(feature = "intelligence")]
            lance_unavailable: None,
            reset_stale_schema: false,
            index_rebuild_needed,
            pending_vectors,
            _lock: None,
        })
    }

    /// Lazy-open the LanceDB store at `.basemind/lance/`. Subsequent calls return
    /// the cached handle; the first call pays the connection + table-init cost.
    ///
    /// A mismatch between the stored `(dim, embedding_model)` and the values
    /// passed here wipes the whole `.basemind/lance/` directory and rebuilds —
    /// the standard schema-bump migration story for the vector store.
    ///
    /// An open that fails is not tried again until [`Self::retry_vector_store`]: the error is
    /// remembered and returned at once.
    #[cfg(feature = "intelligence")]
    pub fn lance_or_open(&mut self, dim: u16, embedding_model: &str) -> Result<&LanceStore, anyhow::Error> {
        if self.lance.is_none() {
            if let Some(reason) = &self.lance_unavailable {
                return Err(anyhow::anyhow!("{reason} (in an earlier open of this scan pass)"));
            }
            let dir = self.basemind_dir.join(LANCE_DIR);
            let store = LanceStore::open(&dir, dim, embedding_model)
                .inspect_err(|error| self.lance_unavailable = Some(format!("{error:#}")))?;
            self.lance = Some(store);
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

    /// Whether the LanceDB vector-store directory already exists on disk. Lets callers avoid
    /// lazily *creating* the store (via [`Self::lance_or_open`]) just to issue a delete that would
    /// target a table that was never built (e.g. a stale-file purge on a repo that never enabled
    /// code-search embeddings).
    #[cfg(feature = "intelligence")]
    pub fn lance_dir_exists(&self) -> bool {
        self.basemind_dir.join(LANCE_DIR).exists()
    }

    pub fn upsert(&mut self, rel: impl Into<RelPath>, entry: FileEntry) {
        self.hash_refs.acquire(&entry.hash_hex);
        if let Some(prior) = self.index.files.insert(rel.into(), entry) {
            self.hash_refs.release(&prior.hash_hex);
        }
    }

    pub fn remove(&mut self, rel: impl AsRef<[u8]>) {
        if let Some(prior) = self.index.files.remove(bstr::BStr::new(rel.as_ref())) {
            self.hash_refs.release(&prior.hash_hex);
        }
    }

    /// True when some indexed file holds the content `hash_hex`.
    pub fn hash_is_held(&self, hash_hex: &str) -> bool {
        self.hash_refs.is_held(hash_hex)
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
        self.index.doc_files.insert(rel.into(), entry);
    }

    /// Drop the document-tier index entry for `rel`.
    pub fn remove_doc(&mut self, rel: impl AsRef<[u8]>) {
        self.index.doc_files.remove(bstr::BStr::new(rel.as_ref()));
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
        self.pending_vectors.persist(&self.view_dir)?;
        if let Some(db) = self.index_db.as_ref() {
            self.index.generation = db.seal_scan(self.index.generation)?;
        }
        let final_path = self.view_dir.join(INDEX_FILE);
        let tmp_path = self.view_dir.join(format!("{INDEX_FILE}.tmp"));
        let bytes = rmp_serde::to_vec_named(&self.index)?;
        {
            let mut f = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp_path)
                .map_err(|source| StoreError::Io {
                    path: tmp_path.clone(),
                    source,
                })?;
            f.write_all(&bytes).map_err(|source| StoreError::Io {
                path: tmp_path.clone(),
                source,
            })?;
            f.sync_all().map_err(|source| StoreError::Io {
                path: tmp_path.clone(),
                source,
            })?;
        }
        std::fs::rename(&tmp_path, &final_path).map_err(|source| StoreError::Io {
            path: final_path,
            source,
        })?;
        // The rename is durable only once the directory entry is. Not fatal: the new file map is
        // already in place, and some filesystems refuse to fsync a directory.
        if let Err(error) = crate::store_base::sync_directory(&self.view_dir) {
            tracing::warn!(%error, "could not fsync the view directory after writing the file map");
        }
        Ok(())
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
    /// released first so the directory lock can be retaken; if a clone elsewhere still holds the old
    /// database the reopen fails and `index_db` stays `None`, which the caller must treat as a dead
    /// store.
    pub fn reopen_index_db(&mut self) -> Result<(), IndexError> {
        if self.index_db.take().is_some() {
            self.index_db = Some(open_index_with_retry(&self.root, &self.view_dir)?);
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

/// Delete the index file in a single view's directory.
pub(crate) fn wipe_view(view_dir: &Path) -> Result<(), StoreError> {
    let index_path = view_dir.join(INDEX_FILE);
    if index_path.exists() {
        std::fs::remove_file(&index_path).map_err(|source| StoreError::Io {
            path: index_path,
            source,
        })?;
    }
    Ok(())
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

/// Pre-views installs kept `index.msgpack` at the top of `.basemind/`. After the upgrade,
/// each view lives under `.basemind/views/<view>/`. If we detect the legacy file AND no
/// working-view file exists yet, move it in place. Idempotent: re-runs are no-ops.
fn migrate_legacy_index_into_views(basemind_dir: &Path) -> Result<(), StoreError> {
    let legacy = basemind_dir.join(INDEX_FILE);
    if !legacy.exists() {
        return Ok(());
    }
    let working_dir = basemind_dir.join(VIEWS_DIR).join(VIEW_WORKING);
    let working_index = working_dir.join(INDEX_FILE);
    if working_index.exists() {
        let _ = std::fs::remove_file(&legacy);
        return Ok(());
    }
    ensure_dir(&working_dir)?;
    std::fs::rename(&legacy, &working_index).map_err(|source| StoreError::Io {
        path: working_index,
        source,
    })?;
    tracing::info!("migrated .basemind/index.msgpack → .basemind/views/{VIEW_WORKING}/index.msgpack");
    Ok(())
}

/// Read and deserialize a view's `index.msgpack`. `Ok(None)` when the file is absent;
/// `Err(StoreError::SchemaMismatch)` on an older version, `Err(StoreError::NewerSchema)` on a newer
/// one, `Err(StoreError::StaleBlobLayout)` on a map of an earlier blob store layout. Reused by
/// `store_gc` to enumerate the live blob hashes referenced by every view.
pub(crate) fn read_index(view_dir: &Path) -> Result<Option<Index>, StoreError> {
    /// The one field every layout of the file map has had.
    #[derive(Deserialize)]
    struct Version {
        schema_ver: u16,
    }
    let path = view_dir.join(INDEX_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(&path).map_err(|source| StoreError::Io {
        path: path.clone(),
        source,
    })?;
    // The version is read on its own first: a newer map need not decode as this one, and a decode
    // failure must not read as a damaged map that may be removed. ~keep
    if let Ok(Version { schema_ver }) = rmp_serde::from_slice(&bytes) {
        check_schema(schema_ver)?;
    }
    let index: Index = rmp_serde::from_slice(&bytes)?;
    check_schema(index.schema_ver)?;
    if index.blob_layout > crate::store_layout::BLOB_LAYOUT {
        return Err(StoreError::NewerSchema {
            stored: u16::from(index.blob_layout),
            current: u16::from(crate::store_layout::BLOB_LAYOUT),
        });
    }
    if index.blob_layout < crate::store_layout::BLOB_LAYOUT {
        return Err(StoreError::StaleBlobLayout {
            found: index.blob_layout,
            expected: crate::store_layout::BLOB_LAYOUT,
        });
    }
    Ok(Some(index))
}

/// Retries a rightful writer performs when opening the Fjall index hits a *transient* fjall
/// `Locked`. The caller already holds the `.basemind/.lock` advisory lock, so it is the sole
/// legitimate writer — any fjall contention here is a short-lived reader open (a CLI `query` /
/// `outline`, or another serve's read-only fallback briefly probing the index) that releases
/// within sub-ms to low-ms. Retrying lets the rightful writer win instead of misfiring the
/// read-only downgrade — the multi-session writer-downgrade race. ~10 × 50 ms tracks the fs2
/// `.lock` retry budget (`acquire_lock_as`).
const INDEX_OPEN_RETRIES: u32 = 10;
const INDEX_OPEN_BACKOFF: std::time::Duration = std::time::Duration::from_millis(50);

/// Open the Fjall [`IndexDb`], retrying a transient fjall `Locked` (see [`INDEX_OPEN_RETRIES`]).
/// ONLY correct for a caller that already holds `.basemind/.lock`; such a caller is the sole
/// rightful writer, so any `Locked` is transient and clears. Every other [`IndexError`] — and a
/// lock that never clears within the budget — propagates.
pub(crate) fn open_index_with_retry(root: &Path, view_dir: &Path) -> Result<IndexDb, IndexError> {
    let memory_dir = memory_store_dir(root);
    let mut attempt = 0;
    loop {
        match IndexDb::open_with_memory(view_dir, &memory_dir) {
            Ok(db) => return Ok(db),
            Err(IndexError::Fjall(fjall::Error::Locked)) if attempt < INDEX_OPEN_RETRIES => {
                attempt += 1;
                std::thread::sleep(INDEX_OPEN_BACKOFF);
            }
            Err(other) => return Err(other),
        }
    }
}

/// Guard every blob / index read against a foreign on-disk schema: an older one is a rebuildable
/// [`StoreError::SchemaMismatch`], a newer one a [`StoreError::NewerSchema`] no caller may wipe.
/// `pub(crate)` because the blob accessors that call it live in [`crate::store_blob`].
pub(crate) fn check_schema(found: u16) -> Result<(), StoreError> {
    match found.cmp(&SCHEMA_VER) {
        std::cmp::Ordering::Equal => Ok(()),
        std::cmp::Ordering::Less => Err(StoreError::SchemaMismatch {
            found,
            expected: SCHEMA_VER,
        }),
        std::cmp::Ordering::Greater => Err(StoreError::NewerSchema {
            stored: found,
            current: SCHEMA_VER,
        }),
    }
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
