use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use ahash::AHashMap;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::extract::{FileMapL1, FileMapL2, SCHEMA_VER};
use crate::hashing::{self, Hash};
use crate::index::{IndexDb, IndexError};
#[cfg(feature = "intelligence")]
use crate::lance::LanceStore;
use crate::path::RelPath;
use crate::store_blob::{
    frame_filemap, parse_filemap_l1, parse_filemap_l2, peek_filemap_schema, read_if_exists, write_blob,
    write_bytes_atomic,
};

pub const INDEX_FILE: &str = "index.msgpack";
pub const BLOBS_DIR: &str = "blobs";
pub const LOCK_FILE: &str = ".lock";
/// Sidecar JSON written next to `.lock` naming the live lock holder (command + pid +
/// timestamp). Read on contention so the error can name the *actual* holder instead of a
/// hardcoded guess. Best-effort: a missing/corrupt sidecar degrades to a generic message.
pub const LOCK_META_FILE: &str = ".lock.meta";
pub const VIEWS_DIR: &str = "views";
/// Lazy-opened LanceDB store directory under `.basemind/`. Created on first use.
#[cfg(feature = "intelligence")]
pub const LANCE_DIR: &str = "lance";

/// View name used for the working-tree index. Also the default for `basemind serve`.
pub const VIEW_WORKING: &str = "working";
/// View name used when scanning the staging index.
pub const VIEW_STAGED: &str = "staged";

/// Build the view name used for an arbitrary rev. Slash-free so it's a single directory.
pub fn view_name_for_rev(short_sha: &str) -> String {
    format!("rev-{short_sha}")
}

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

// The lock *behavior* lives in `store_lock.rs` (extracted to keep this file under the module-size
// cap); the lock *types* above stay here. Re-export the entry points so callers keep importing
// them from `crate::store`.
pub use crate::store_lock::{WriterProbe, probe_writer_lock};
pub(crate) use crate::store_lock::{acquire_lock, acquire_lock_as, writer_lock_is_held};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Index {
    pub schema_ver: u16,
    /// Relative path → FileEntry. Keyed by `RelPath` so paths with non-UTF-8 bytes
    /// round-trip losslessly through the msgpack store; valid UTF-8 paths serialize as
    /// plain strings (zero wire-format churn for the common case).
    pub files: AHashMap<RelPath, FileEntry>,
}

impl Index {
    pub fn empty() -> Self {
        Self {
            schema_ver: SCHEMA_VER,
            files: AHashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileEntry {
    pub hash_hex: String,
    pub language: String,
    pub size_bytes: u64,
    /// File mtime in seconds since the epoch. Cheap pre-filter before hashing.
    pub mtime: i64,
}

pub struct Store {
    pub root: PathBuf,
    pub basemind_dir: PathBuf,
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
        let basemind_dir = root.join(crate::config::BASEMIND_DIR);
        ensure_dir(&basemind_dir)?;
        ensure_gitignore(&basemind_dir)?;
        ensure_dir(&basemind_dir.join(BLOBS_DIR))?;
        ensure_dir(&basemind_dir.join(VIEWS_DIR))?;
        migrate_legacy_index_into_views(&basemind_dir)?;

        let view_dir = basemind_dir.join(VIEWS_DIR).join(view);
        ensure_dir(&view_dir)?;
        let lock = acquire_lock_as(&basemind_dir, holder)?;
        let index = match read_index(&view_dir) {
            Ok(Some(idx)) => idx,
            Ok(None) => Index::empty(),
            Err(StoreError::SchemaMismatch { found, expected }) => {
                // Durable refresh: reset only THIS view's `index.msgpack` (so `read_index`
                // → None → `Index::empty()` and the next scan treats every file as new and
                // re-extracts it). The shared content-addressed blob store is left in place;
                // re-extraction overwrites each stale-schema blob at its (content-hash) path
                // via `write_blob`'s schema-aware guard, and any blob no longer referenced
                // becomes an orphan reclaimed later by `store_gc::run_gc`. No destructive
                // `rm -rf blobs/`, so there is no window where the expensive cache is gone.
                //
                // Stale-blob-read safety: after this reset the in-RAM index is empty and the
                // view's `index.msgpack` is gone, so every read path that resolves a blob by
                // a `FileEntry.hash_hex` (`query::file_outline` / `file_outline_l2`, the
                // MapCache build) finds no entry and returns "not found". The rescan that
                // follows `Store::open` is what repopulates the index, and it re-extracts
                // before it records the entry — so no consumer ever deserializes a
                // pre-refresh (stale-schema) blob through the current code.
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
        // We hold `.basemind/.lock` (acquired above), so we are the sole rightful writer; retry a
        // transient fjall `Locked` from a concurrent reader instead of letting it propagate — a
        // bare `?` here would surface as lock contention and downgrade this writer to read-only,
        // the multi-session writer-downgrade race.
        let index_db = Some(open_index_with_retry(&view_dir)?);
        Ok(Self {
            root: root.to_path_buf(),
            basemind_dir,
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
    pub fn open_read_only(root: &Path, view: &str) -> Result<Self, StoreError> {
        let basemind_dir = root.join(crate::config::BASEMIND_DIR);
        // Idempotent migration so a read-only consumer sees the same shape a fresh writer would.
        if basemind_dir.exists() {
            let _ = migrate_legacy_index_into_views(&basemind_dir);
        }
        let view_dir = basemind_dir.join(VIEWS_DIR).join(view);
        // An explicitly-named view (e.g. `rev-<sha>`, `staged`) that has never been scanned
        // has no `index.msgpack`. Reading it would silently return an empty, working-like
        // index — masking a typo'd `--view` or an unscanned rev as "0 results" (bug #18).
        // Error instead so the caller knows to scan it. The default working view is exempt:
        // it legitimately reads empty before the first scan, and `serve` auto-scans it.
        if view != VIEW_WORKING && !view_dir.join(INDEX_FILE).exists() {
            return Err(StoreError::ViewNotScanned { view: view.to_string() });
        }
        // A read-only consumer cannot wipe + rebuild like `Store::open` does, so a schema
        // bump must degrade gracefully rather than propagate a hard error: an out-of-date
        // index reads as empty until the next `basemind scan` / `serve` refreshes it in
        // place. `schema_ok == false` also means we must NOT open the Fjall index — its
        // own open path wipes on a schema mismatch, which would corrupt a concurrently
        // running `serve`'s live index. Skip it; the CLI degrades to "no indexed files".
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
        // Fjall is a SINGLE-holder store: only one process can open `index.fjall/` at a time. If a
        // writer (a lock-holding `serve` / `scan`) is live, this read-only consumer cannot open the
        // index anyway — and merely attempting it can transiently hold the fjall lock long enough to
        // force the rightful writer into read-only (the multi-session writer-downgrade race). So we
        // only open Fjall when NO writer holds `.basemind/.lock`; otherwise we serve from the
        // concurrently-readable blobs (`index_db = None`, the in-RAM MapCache fallback covers
        // find_references/callers/impls/call_graph). A single open with no retry — a reader never
        // holds the lock, and any race with a starting writer just degrades to `None`. A transient
        // `Locked` is the expected silent degradation; any OTHER error (corrupt WAL, truncated SST,
        // IO) is logged so a genuinely broken index isn't indistinguishable from "writer holds it".
        let index_db = if schema_ok && view_dir.exists() && !writer_lock_is_held(&basemind_dir) {
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

    pub fn blob_path_fm(&self, hash: &Hash) -> PathBuf {
        let buf = hashing::hex_buf(hash);
        self.blob_path_fm_hex(hashing::hex_str(&buf))
    }

    /// Build the combined-filemap blob path from an already-hex-encoded hash. One blob per
    /// source file holds both the L1 outline and (when extracted) the L2 calls, framed as
    /// `[l1_len: u32 LE][l1 msgpack][l2 msgpack | empty]`. Skips the encode round-trip when
    /// the caller starts from a `FileEntry::hash_hex`.
    pub fn blob_path_fm_hex(&self, hash_hex: &str) -> PathBuf {
        self.basemind_dir.join(BLOBS_DIR).join(format!("{hash_hex}.fm.msgpack"))
    }

    #[cfg(feature = "documents")]
    pub fn blob_path_doc(&self, hash: &Hash) -> PathBuf {
        let buf = hashing::hex_buf(hash);
        self.blob_path_doc_hex(hashing::hex_str(&buf))
    }

    #[cfg(feature = "documents")]
    pub fn blob_path_doc_hex(&self, hash_hex: &str) -> PathBuf {
        self.basemind_dir
            .join(BLOBS_DIR)
            .join(format!("{hash_hex}.doc.msgpack"))
    }

    /// Read the L1 outline from the combined-filemap blob. Deserializes only the L1 slice of
    /// the frame — the trailing L2 bytes are read off disk but never decoded, so the common
    /// outline-only read path (`MapCache` build, `search_symbols`) pays no L2 decode cost.
    pub fn read_l1_by_hex(&self, hash_hex: &str) -> Result<Option<FileMapL1>, StoreError> {
        let path = self.blob_path_fm_hex(hash_hex);
        let Some(bytes) = read_if_exists(&path)? else {
            return Ok(None);
        };
        let map = parse_filemap_l1(&path, &bytes)?;
        check_schema(map.schema_ver)?;
        Ok(Some(map))
    }

    /// Read the L2 calls from the combined-filemap blob. Returns `Ok(None)` both when the blob
    /// is absent and when it carries no L2 tier (the file was scanned with `eager_l2 = false`
    /// or L2 extraction failed) — callers escalate via `query::file_outline_l2`.
    pub fn read_l2_by_hex(&self, hash_hex: &str) -> Result<Option<FileMapL2>, StoreError> {
        let path = self.blob_path_fm_hex(hash_hex);
        let Some(bytes) = read_if_exists(&path)? else {
            return Ok(None);
        };
        match parse_filemap_l2(&path, &bytes)? {
            Some(map) => {
                check_schema(map.schema_ver)?;
                Ok(Some(map))
            }
            None => Ok(None),
        }
    }

    /// Write the combined-filemap blob for a file. Holds both tiers in one content-addressed
    /// blob (`[l1_len][l1][l2|empty]`), so the default eager-L2 scan does one `open` + `write`
    /// + atomic `rename` per file instead of two. `l2 = None` writes an L1-only frame.
    pub fn write_filemap_hex(&self, hash_hex: &str, l1: &FileMapL1, l2: Option<&FileMapL2>) -> Result<(), StoreError> {
        let path = self.blob_path_fm_hex(hash_hex);
        // Content-addressed skip: an existing frame at this path holds the extraction of
        // identical source bytes. Only short-circuit when its schema already matches — a
        // schema bump must overwrite the stale frame in place (see `write_blob`).
        if path.exists() && peek_filemap_schema(&path) == Some(SCHEMA_VER) {
            return Ok(());
        }
        let bytes = frame_filemap(l1, l2)?;
        write_bytes_atomic(path, &bytes)
    }

    #[cfg(feature = "documents")]
    pub fn write_doc(&self, hash: &Hash, map: &crate::extract::doc::FileMapDoc) -> Result<(), StoreError> {
        write_blob(self.blob_path_doc(hash), map)
    }

    #[cfg(feature = "documents")]
    pub fn read_doc_by_hex(&self, hash_hex: &str) -> Result<Option<crate::extract::doc::FileMapDoc>, StoreError> {
        let path = self.blob_path_doc_hex(hash_hex);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = std::fs::read(&path).map_err(|source| StoreError::Io {
            path: path.clone(),
            source,
        })?;
        let map: crate::extract::doc::FileMapDoc = rmp_serde::from_slice(&bytes)?;
        check_schema(map.schema_ver)?;
        Ok(Some(map))
    }

    /// Path of a file's resolution blob (`<hash>.rref.msgpack`) — the per-file code-intelligence
    /// facts (intra-file resolved edges + import/export list). A sibling of the `.fm`/`.doc`
    /// blobs, content-addressed by source hash. Unframed single-map msgpack, like the doc tier.
    pub fn blob_path_rref_hex(&self, hash_hex: &str) -> PathBuf {
        self.basemind_dir
            .join(BLOBS_DIR)
            .join(format!("{hash_hex}.rref.msgpack"))
    }

    /// Write a file's resolution facts. Content-addressed skip on matching schema (identical
    /// source bytes already analyzed), else serialize + atomic write — mirrors `write_doc`.
    pub fn write_resolved_hex(
        &self,
        hash_hex: &str,
        refs: &crate::intel::model::FileResolvedRefs,
    ) -> Result<(), StoreError> {
        write_blob(self.blob_path_rref_hex(hash_hex), refs)
    }

    /// Read a file's resolution facts. `Ok(None)` when the file has no resolution blob (never
    /// analyzed, or produced no facts). A schema mismatch surfaces as an error so the second pass
    /// recomputes rather than trusting a stale blob.
    pub fn read_resolved_by_hex(
        &self,
        hash_hex: &str,
    ) -> Result<Option<crate::intel::model::FileResolvedRefs>, StoreError> {
        let path = self.blob_path_rref_hex(hash_hex);
        let Some(bytes) = read_if_exists(&path)? else {
            return Ok(None);
        };
        let refs: crate::intel::model::FileResolvedRefs = rmp_serde::from_slice(&bytes)?;
        check_schema(refs.schema_ver)?;
        Ok(Some(refs))
    }

    /// Path of a file's code-chunk sidecar (`<hash>.chunk.msgpack`) — the per-file chunk list +
    /// embeddings that back the semantic code-search tier. A sibling of the `.fm`/`.doc`/`.rref`
    /// blobs, content-addressed by source hash. Unframed single-map msgpack, like the doc tier.
    #[cfg(feature = "code-search")]
    pub fn blob_path_chunk_hex(&self, hash_hex: &str) -> PathBuf {
        self.basemind_dir
            .join(BLOBS_DIR)
            .join(format!("{hash_hex}.chunk.msgpack"))
    }

    /// Write a file's code-chunk sidecar. Content-addressed skip on matching schema (identical
    /// source bytes already chunked + embedded) — this is what lets an unchanged file skip
    /// re-embedding. Otherwise serialize + atomic write; mirrors `write_doc`.
    #[cfg(feature = "code-search")]
    pub fn write_chunks_hex(&self, hash_hex: &str, blob: &crate::chunk::CodeChunkBlob) -> Result<(), StoreError> {
        write_blob(self.blob_path_chunk_hex(hash_hex), blob)
    }

    /// Read a file's code-chunk sidecar. `Ok(None)` when the file has no chunk blob (never
    /// chunked, or produced no chunks). A schema mismatch surfaces as an error so the scanner
    /// re-chunks rather than trusting a stale blob.
    #[cfg(feature = "code-search")]
    pub fn read_chunks_by_hex(&self, hash_hex: &str) -> Result<Option<crate::chunk::CodeChunkBlob>, StoreError> {
        let path = self.blob_path_chunk_hex(hash_hex);
        let Some(bytes) = read_if_exists(&path)? else {
            return Ok(None);
        };
        let blob: crate::chunk::CodeChunkBlob = rmp_serde::from_slice(&bytes)?;
        check_schema(blob.schema_ver)?;
        Ok(Some(blob))
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

/// Drop a `.gitignore` inside `.basemind/` the first time the store is created.
/// A bare `*` makes git ignore the whole directory (this file included), so a
/// user's repository never accidentally commits the machine-local index. Written
/// once and never overwritten — a deliberate user edit is respected.
fn ensure_gitignore(basemind_dir: &Path) -> Result<(), StoreError> {
    let gitignore = basemind_dir.join(".gitignore");
    if gitignore.exists() {
        return Ok(());
    }
    std::fs::write(
        &gitignore,
        "# basemind's machine-local index — not version-controlled.\n*\n",
    )
    .map_err(|source| StoreError::Io {
        path: gitignore,
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

/// Wipe the shared blobs directory. Used by `store_gc::clear_component` for an explicit
/// `Blobs` component clear (the CLI / MCP admin surface). NOT called on a schema bump:
/// `Store::open` refreshes blobs durably in place (re-extract overwrites stale blobs;
/// orphans are reclaimed by `store_gc::run_gc`) rather than destroying the cache.
pub(crate) fn wipe_blobs(basemind_dir: &Path) -> Result<(), StoreError> {
    let blobs_dir = basemind_dir.join(BLOBS_DIR);
    if blobs_dir.exists() {
        std::fs::remove_dir_all(&blobs_dir).map_err(|source| StoreError::Io {
            path: blobs_dir.clone(),
            source,
        })?;
        std::fs::create_dir_all(&blobs_dir).map_err(|source| StoreError::Io {
            path: blobs_dir,
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
        // Both present — `legacy` is the duplicate. Remove it so we don't migrate twice.
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

fn check_schema(found: u16) -> Result<(), StoreError> {
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

    fn sample_l1() -> FileMapL1 {
        FileMapL1 {
            schema_ver: SCHEMA_VER,
            language: "rust".to_string(),
            size_bytes: 42,
            had_errors: false,
            error_count: 0,
            symbols: Vec::new(),
            imports: Vec::new(),
            implementations: Vec::new(),
        }
    }

    fn sample_l2() -> FileMapL2 {
        FileMapL2 {
            schema_ver: SCHEMA_VER,
            language: "rust".to_string(),
            calls: Vec::new(),
            docs: Vec::new(),
        }
    }

    #[test]
    fn filemap_frame_round_trips_both_tiers() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path(), VIEW_WORKING).expect("open store");
        let hash_hex = "a".repeat(64);

        store
            .write_filemap_hex(&hash_hex, &sample_l1(), Some(&sample_l2()))
            .expect("write combined frame");

        let l1 = store.read_l1_by_hex(&hash_hex).expect("read l1");
        assert_eq!(l1.map(|m| m.size_bytes), Some(42), "L1 slice round-trips");
        let l2 = store.read_l2_by_hex(&hash_hex).expect("read l2");
        assert_eq!(l2.map(|m| m.language), Some("rust".to_string()), "L2 present");
    }

    #[test]
    fn filemap_frame_l1_only_reads_back_no_l2() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path(), VIEW_WORKING).expect("open store");
        let hash_hex = "b".repeat(64);

        store
            .write_filemap_hex(&hash_hex, &sample_l1(), None)
            .expect("write L1-only frame");

        assert!(
            store.read_l1_by_hex(&hash_hex).expect("read l1").is_some(),
            "L1 present in an L1-only frame"
        );
        assert!(
            store.read_l2_by_hex(&hash_hex).expect("read l2").is_none(),
            "L2 absent in an L1-only frame (escalation will extract on demand)"
        );
    }

    #[test]
    fn resolved_blob_round_trips_and_missing_reads_none() {
        use crate::intel::model::{ExportEdge, FileResolvedRefs, ImportEdge, ResolvedEdge};
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path(), VIEW_WORKING).expect("open store");
        let hash_hex = "d".repeat(64);

        let mut refs = FileResolvedRefs::new("typescript");
        refs.intra.push(ResolvedEdge {
            use_start: 40,
            use_end: 43,
            def_start: 4,
            def_end: 7,
        });
        refs.imports.push(ImportEdge {
            local: "foo".to_string(),
            specifier: "./bar".to_string(),
            imported: Some("baz".to_string()),
            is_type: false,
            local_start: 9,
        });
        refs.exports.push(ExportEdge {
            name: "alpha".to_string(),
            name_start: 20,
        });

        store.write_resolved_hex(&hash_hex, &refs).expect("write resolved blob");
        let read = store.read_resolved_by_hex(&hash_hex).expect("read resolved blob");
        assert_eq!(read.as_ref(), Some(&refs), "resolution blob round-trips exactly");

        let missing = store.read_resolved_by_hex(&"e".repeat(64)).expect("read missing");
        assert_eq!(missing, None, "absent resolution blob reads back as None");
    }

    #[test]
    fn locked_display_names_the_serve_holder() {
        // With no sidecar, the generic fallback still names both serve and watch.
        let err = StoreError::Locked {
            path: PathBuf::from("/repo/.basemind/.lock"),
            holder: None,
        };
        let msg = err.to_string();
        // The common holder is the editor plugin's `serve` process; the message must
        // name it so the user knows what to stop (W8: the old text said only `watch`).
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
        // When the sidecar is present, the message names the ACTUAL holder command + pid,
        // not the hardcoded serve/watch guess (bug #11).
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
        // Simulate two acquisitions of the same store lock: the first wins as `scan`, the
        // second must surface a Locked error naming `basemind scan` (bug #11 contract).
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
        // Opening an explicitly-named view that was never scanned must error rather than
        // silently return an empty working-like index (bug #18).
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
        // The default working view legitimately reads empty before the first scan — it must
        // NOT error (would break `serve` auto-scan-on-empty + first-run query).
        let tmp = tempfile::tempdir().expect("tempdir");
        let store =
            Store::open_read_only(tmp.path(), VIEW_WORKING).expect("working view opens even when never scanned");
        assert!(store.index.files.is_empty(), "empty working index");
    }

    #[test]
    fn open_writer_creates_named_view_for_first_scan() {
        // The writer path (scan/serve) must still be able to create a named view fresh —
        // that is how `basemind scan --rev <sha>` first materializes `rev-<sha>`.
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
        // A reader can slip past the fs2 advisory lock yet still trip Fjall's own
        // exclusive open lock; both must route to the same actionable guidance.
        let err = StoreError::Index(IndexError::Fjall(fjall::Error::Locked));
        assert!(err.is_lock_contention());
    }

    #[test]
    fn schema_mismatch_is_not_lock_contention() {
        let err = StoreError::SchemaMismatch { found: 1, expected: 2 };
        assert!(!err.is_lock_contention());
    }
}
