use std::fs::{File, OpenOptions};
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

/// Cache layout: where the global cache root is, how a worktree root maps to a workspace cache
/// dir, and the `workspace.json` marker recording that mapping. Lives in its own module (like
/// `store_lock.rs`) to keep this file under the module size cap; re-exported here so callers keep
/// importing every name from `crate::store`.
#[cfg(feature = "intelligence")]
pub use crate::store_layout::LANCE_DIR;
#[cfg(any(feature = "test-support", test))]
pub use crate::store_layout::init_isolated_cache;
pub use crate::store_layout::{
    BLOBS_DIR, CACHE_DIR, DATA_HOME_ENV, INDEX_FILE, LOCK_FILE, LOCK_META_FILE, VIEW_STAGED, VIEW_WORKING, VIEWS_DIR,
    WORKSPACE_MARKER_FILE, WORKSPACES_DIR, WorkspaceMarker, cache_root, ensure_workspace_marker, global_blobs_dir,
    read_workspace_marker, view_name_for_rev, workspace_cache_dir, workspace_key,
};

/// Which basemind command is taking the exclusive store lock. Threaded from the caller
/// (`scan` / `rescan` / `watch` / `serve`) into [`Store::open_with_holder`] so a lock
/// contention error can name the *actual* holder rather than a hardcoded guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockHolder {
    /// `basemind serve` — the long-running MCP server (the common holder; an editor plugin).
    Serve,
    /// `basemind watch` — the filesystem watcher / incremental re-indexer.
    Watch,
    /// `basemind scan` — a one-shot full index.
    Scan,
    /// `basemind rescan` — an incremental re-index.
    Rescan,
    /// GC / cache maintenance or any caller that did not specify a more precise identity.
    Maintenance,
}

impl LockHolder {
    /// The exact CLI command a user would run for this holder, used verbatim in the error
    /// message so the guidance is actionable ("stop `basemind serve`").
    pub fn command(self) -> &'static str {
        match self {
            LockHolder::Serve => "basemind serve",
            LockHolder::Watch => "basemind watch",
            LockHolder::Scan => "basemind scan",
            LockHolder::Rescan => "basemind rescan",
            LockHolder::Maintenance => "a basemind cache/maintenance task",
        }
    }
}

/// On-disk sidecar describing who currently holds the store lock. Written atomically when
/// the exclusive lock is acquired and read on contention. Additive, non-load-bearing: a
/// missing or corrupt sidecar simply falls back to the generic lock message, so it never
/// trips schema wipe-on-mismatch (it lives outside the versioned index/blob stores).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockMeta {
    /// The CLI command of the holder (`basemind serve`, etc.).
    pub command: String,
    /// OS process id of the holder.
    pub pid: u32,
    /// Unix-epoch seconds when the lock was acquired.
    pub acquired_unix: i64,
}

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
    #[error("corrupt filemap blob at {path}: malformed frame header")]
    CorruptBlob { path: PathBuf },
    #[error("filemap L1 tier exceeds the 4 GiB frame limit")]
    BlobTooLarge,
    #[error("{}", lock_contention_message(.path, .holder))]
    Locked {
        /// The `.lock` path whose acquisition failed.
        path: PathBuf,
        /// The live holder read from the `.lock.meta` sidecar, when present. `None` when the
        /// sidecar is missing/corrupt — the message then falls back to a generic guess.
        holder: Option<LockMeta>,
    },
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

/// Render the lock-contention message, naming the live holder from the `.lock.meta` sidecar
/// when it is available and falling back to the generic guess otherwise. Kept as a free fn so
/// the `thiserror` `#[error(...)]` attribute can call it for [`StoreError::Locked`].
fn lock_contention_message(path: &Path, holder: &Option<LockMeta>) -> String {
    match holder {
        Some(meta) => format!(
            "another basemind process holds the lock on {} (`{}`, pid {})",
            path.display(),
            meta.command,
            meta.pid
        ),
        None => format!(
            "another basemind process holds the lock on {} (usually the `basemind serve` MCP \
             server from your editor plugin, or `basemind watch`)",
            path.display()
        ),
    }
}

/// Actionable guidance printed when a CLI writer (`scan` / `rescan`) can't acquire the
/// store lock because another basemind process is holding it. Kept as a constant so the
/// scan and rescan paths emit identical wording and a test can assert the contract.
pub const LOCK_CONTENTION_HELP: &str = "the basemind index is locked by another process \
(likely the MCP server). If an editor/plugin is serving this repo, use its `rescan` tool \
to refresh the index, or stop that server before running `basemind scan`.";

pub use crate::store_lock::{WriterProbe, probe_writer_lock};
pub(crate) use crate::store_lock::{acquire_lock, acquire_lock_as, writer_lock_is_held};

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
}

impl Index {
    pub fn empty() -> Self {
        Self {
            schema_ver: SCHEMA_VER,
            files: AHashMap::new(),
            doc_files: AHashMap::new(),
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
    /// Always `true` since the blob store went global: a standalone Store references only ONE
    /// workspace's index, so it can never enumerate the full live blob set across all workspaces.
    /// Auto-GC (boot + background) is therefore disabled here — reference-counted GC that spans
    /// every workspace is the daemon's job. Retained (rather than removed) so the auto-GC skip in
    /// `mcp::background::run_background_gc` and the boot path keep compiling unchanged.
    pub blobs_shared: bool,
    pub view_dir: PathBuf,
    pub view: String,
    pub index: Index,
    /// Fjall-backed inverted index over symbols / calls / imports. Lives under
    /// `view_dir/index.fjall/`. Reads + writes from any caller go through `IndexDb`,
    /// which is cheap to clone (internally Arc'd). `None` in read-only mode when the
    /// directory doesn't exist yet — callers must handle the absence.
    pub index_db: Option<IndexDb>,
    /// LanceDB-backed vector store. Lazy-opened on first document insert so a
    /// vanilla code-only scan doesn't pay the LanceDB startup cost.
    #[cfg(feature = "intelligence")]
    pub lance: Option<LanceStore>,
    _lock: Option<File>,
}

impl Store {
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
        let blobs_dir = global_blobs_dir();
        ensure_dir(&blobs_dir)?;
        // Blobs are global (shared by every workspace), so a standalone Store can never see the
        // full set of live references — auto-GC is disabled here (`blobs_shared = true`); the
        // daemon performs reference-counted GC across all workspaces.
        let blobs_shared = true;
        ensure_dir(&basemind_dir.join(VIEWS_DIR))?;
        migrate_legacy_index_into_views(&basemind_dir)?;

        let view_dir = basemind_dir.join(VIEWS_DIR).join(view);
        ensure_dir(&view_dir)?;
        let lock = acquire_lock_as(&basemind_dir, holder)?;
        // Record (or self-heal) the root marker under the lock, so the orphan reaper can later tell
        // whether this workspace's worktree still exists. See [`WORKSPACE_MARKER_FILE`].
        ensure_workspace_marker(&basemind_dir, root);
        let index = match read_index(&view_dir) {
            Ok(Some(idx)) => idx,
            Ok(None) => Index::empty(),
            Err(StoreError::SchemaMismatch { found, expected }) => {
                tracing::info!(
                    found,
                    expected,
                    view,
                    "cache schema bumped; refreshing view in place (re-extract + GC reclaims orphans)"
                );
                wipe_view(&view_dir)?;
                Index::empty()
            }
            Err(e) => return Err(e),
        };
        let index_db = Some(open_index_with_retry(&view_dir)?);
        Ok(Self {
            root: root.to_path_buf(),
            basemind_dir,
            blobs_dir,
            blobs_shared,
            view_dir,
            view: view.to_string(),
            index,
            index_db,
            #[cfg(feature = "intelligence")]
            lance: None,
            _lock: Some(lock),
        })
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
            // Self-heal the root marker for workspaces that predate it (a read-only open never
            // creates the dir, so this only ever touches an existing workspace). Idempotent: a
            // marker already naming this root is left alone.
            ensure_workspace_marker(&basemind_dir, root);
        }
        let blobs_dir = global_blobs_dir();
        // See `open_with_holder`: blobs are global, so auto-GC is disabled in a standalone Store.
        let blobs_shared = true;
        let view_dir = basemind_dir.join(VIEWS_DIR).join(view);
        if view != VIEW_WORKING && !view_dir.join(INDEX_FILE).exists() {
            return Err(StoreError::ViewNotScanned { view: view.to_string() });
        }
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
            Err(e) => return Err(e),
        };
        let index_db = if allow_index_db && schema_ok && view_dir.exists() && !writer_lock_is_held(&basemind_dir) {
            match IndexDb::open(&view_dir) {
                Ok(db) => Some(db),
                Err(IndexError::Fjall(fjall::Error::Locked)) => None,
                Err(error) => {
                    tracing::warn!(%error, "read-only index open failed; degrading to blob-only reads");
                    None
                }
            }
        } else {
            None
        };
        Ok(Self {
            root: root.to_path_buf(),
            basemind_dir,
            blobs_dir,
            blobs_shared,
            view_dir,
            view: view.to_string(),
            index,
            index_db,
            #[cfg(feature = "intelligence")]
            lance: None,
            _lock: None,
        })
    }

    /// Lazy-open the LanceDB store at `.basemind/lance/`. Subsequent calls return
    /// the cached handle; the first call pays the connection + table-init cost.
    ///
    /// A mismatch between the stored `(dim, embedding_model)` and the values
    /// passed here wipes the whole `.basemind/lance/` directory and rebuilds —
    /// the standard schema-bump migration story for the vector store.
    #[cfg(feature = "intelligence")]
    pub fn lance_or_open(&mut self, dim: u16, embedding_model: &str) -> Result<&LanceStore, anyhow::Error> {
        if self.lance.is_none() {
            let dir = self.basemind_dir.join(LANCE_DIR);
            let store = LanceStore::open(&dir, dim, embedding_model)?;
            self.lance = Some(store);
        }
        // SAFETY of unwrap: we just populated it on the line above when None.
        Ok(self.lance.as_ref().expect("lance store just populated"))
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
        self.index.files.insert(rel.into(), entry);
    }

    pub fn remove(&mut self, rel: impl AsRef<[u8]>) {
        self.index.files.remove(bstr::BStr::new(rel.as_ref()));
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

    /// Atomically rewrite the index file (tmp + rename).
    pub fn flush(&self) -> Result<(), StoreError> {
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
        Ok(())
    }
}

fn ensure_dir(p: &Path) -> Result<(), StoreError> {
    std::fs::create_dir_all(p).map_err(|source| StoreError::Io {
        path: p.to_path_buf(),
        source,
    })
}

/// Delete the index file in a single view's directory.
fn wipe_view(view_dir: &Path) -> Result<(), StoreError> {
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
/// `Err(StoreError::SchemaMismatch)` on a version mismatch. Reused by `store_gc` to
/// enumerate the live blob hashes referenced by every view.
pub(crate) fn read_index(view_dir: &Path) -> Result<Option<Index>, StoreError> {
    let path = view_dir.join(INDEX_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(&path).map_err(|source| StoreError::Io {
        path: path.clone(),
        source,
    })?;
    let index: Index = rmp_serde::from_slice(&bytes)?;
    check_schema(index.schema_ver)?;
    Ok(Some(index))
}

/// Acquire the store's advisory `.lock` (exclusive flock, with bounded retry).
/// Reused by `store_gc::run_gc` so the mark+sweep races neither a concurrent scan
/// nor a `basemind watch`.
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
pub(crate) fn open_index_with_retry(view_dir: &Path) -> Result<IndexDb, IndexError> {
    let mut attempt = 0;
    loop {
        match IndexDb::open(view_dir) {
            Ok(db) => return Ok(db),
            Err(IndexError::Fjall(fjall::Error::Locked)) if attempt < INDEX_OPEN_RETRIES => {
                attempt += 1;
                std::thread::sleep(INDEX_OPEN_BACKOFF);
            }
            Err(other) => return Err(other),
        }
    }
}

/// Guard every blob / index read against a stale on-disk schema. `pub(crate)` because the blob
/// accessors that call it live in [`crate::store_blob`].
pub(crate) fn check_schema(found: u16) -> Result<(), StoreError> {
    if found == SCHEMA_VER {
        Ok(())
    } else {
        Err(StoreError::SchemaMismatch {
            found,
            expected: SCHEMA_VER,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locked_display_names_the_serve_holder() {
        let err = StoreError::Locked {
            path: PathBuf::from("/repo/.basemind/.lock"),
            holder: None,
        };
        let msg = err.to_string();
        assert!(
            msg.contains("serve"),
            "Locked message should name the `serve` holder, got: {msg}"
        );
        assert!(
            msg.contains("watch"),
            "Locked message should still mention `watch`, got: {msg}"
        );
    }

    #[test]
    fn locked_message_names_actual_holder_from_sidecar() {
        let err = StoreError::Locked {
            path: PathBuf::from("/repo/.basemind/.lock"),
            holder: Some(LockMeta {
                command: "basemind scan".to_string(),
                pid: 4321,
                acquired_unix: 1_700_000_000,
            }),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("basemind scan"),
            "message should name the actual holder command, got: {msg}"
        );
        assert!(msg.contains("4321"), "message should name the holder pid, got: {msg}");
    }

    #[test]
    fn second_acquisition_names_first_holders_command() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let basemind_dir = tmp.path().join(".basemind");
        std::fs::create_dir_all(&basemind_dir).expect("mkdir");

        let _held = acquire_lock_as(&basemind_dir, LockHolder::Scan).expect("first lock");
        let err = acquire_lock_as(&basemind_dir, LockHolder::Serve)
            .expect_err("second acquisition must fail while the first holds the lock");
        assert!(err.is_lock_contention(), "must be a contention error");
        let msg = err.to_string();
        assert!(
            msg.contains("basemind scan"),
            "second error should name the FIRST holder (scan), got: {msg}"
        );
    }

    #[test]
    fn open_read_only_errors_on_never_scanned_named_view() {
        init_isolated_cache();
        let tmp = tempfile::tempdir().expect("tempdir");
        let err = match Store::open_read_only(tmp.path(), "rev-deadbee") {
            Ok(_) => panic!("named unscanned view must error, not silently open empty"),
            Err(e) => e,
        };
        assert!(
            matches!(&err, StoreError::ViewNotScanned { view } if view == "rev-deadbee"),
            "expected ViewNotScanned, got: {err:?}"
        );
        assert!(
            err.to_string().contains("rev-deadbee"),
            "error names the view, got: {err}"
        );
    }

    #[test]
    fn open_read_only_allows_unscanned_working_view() {
        init_isolated_cache();
        let tmp = tempfile::tempdir().expect("tempdir");
        let store =
            Store::open_read_only(tmp.path(), VIEW_WORKING).expect("working view opens even when never scanned");
        assert!(store.index.files.is_empty(), "empty working index");
    }

    #[test]
    fn open_writer_creates_named_view_for_first_scan() {
        init_isolated_cache();
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = Store::open(tmp.path(), "rev-cafe000").expect("writer creates a named view on first scan");
        assert!(store.view_dir.exists(), "named view dir created by writer");
    }

    #[test]
    fn fs2_advisory_lock_is_lock_contention() {
        let err = StoreError::Locked {
            path: PathBuf::from("/repo/.basemind/.lock"),
            holder: None,
        };
        assert!(err.is_lock_contention());
    }

    #[test]
    fn fjall_internal_lock_is_lock_contention() {
        let err = StoreError::Index(IndexError::Fjall(fjall::Error::Locked));
        assert!(err.is_lock_contention());
    }

    #[test]
    fn schema_mismatch_is_not_lock_contention() {
        let err = StoreError::SchemaMismatch { found: 1, expected: 2 };
        assert!(!err.is_lock_contention());
    }
}
