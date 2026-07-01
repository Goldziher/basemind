//! Fjall-backed inverted index over the msgpack content-addressed blob store.
//!
//! The blob store (`.basemind/blobs/<hash>.fm.msgpack`) stays canonical — it holds the per-
//! file extracted maps (L1 outline + L2 calls) in their full shape. This module adds a
//! *secondary* index on top:
//! six Fjall keyspaces that let MCP tools answer "who calls `foo`?" or "what imports
//! `requests`?" via prefix range scans instead of linear sweeps over the in-RAM map.
//!
//! ## Layout
//!
//! `.basemind/views/<view>/index.fjall/` — Fjall manages its own directory shape.
//!
//! ## Schema versioning
//!
//! The `meta` keyspace carries a `schema_ver` row. Mismatch on open drops the whole
//! `index.fjall/` directory and the caller is expected to repopulate it from the existing
//! msgpack blobs. This is fast (no parsing — just decode each L1, push to secondary
//! indexes) and keeps the on-disk format free to evolve.

pub mod keys;
pub mod writer;

use std::path::{Path, PathBuf};

use fjall::{Database, Keyspace, KeyspaceCreateOptions};
use thiserror::Error;

/// Bumped whenever the on-disk key layout changes. Offset by +2 from the release minor:
/// +1 was the `imports_by_path` companion partition added ahead of the next minor cut;
/// +2 is this revision, which adds `implementations_by_trait` + `implementations_by_path`
/// for the iteration-3 `find_implementations` query path. The offset is monotonic:
/// `RELEASE_MINOR = 0` → `INDEX_SCHEMA_VER = 2`. When RELEASE_MINOR next bumps, both move
/// together. Decoupled from blob schema ([`crate::extract::SCHEMA_VER`]) which stays tied
/// to `RELEASE_MINOR` — blobs remain valid across this index revision; only the secondary
/// index rebuilds on next open via the wipe-on-mismatch flow in [`IndexDb::open`].
pub const INDEX_SCHEMA_VER: u32 = crate::version::RELEASE_MINOR as u32 + 2;

const META_SCHEMA_VER: &[u8] = b"schema_ver";

const INDEX_DIR: &str = "index.fjall";

#[derive(Debug, Error)]
pub enum IndexError {
    #[error("fjall error: {0}")]
    Fjall(#[from] fjall::Error),
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
}

/// Handle to every keyspace we read or write. Cloned cheaply (each `Keyspace` is `Arc`'d
/// internally by Fjall), so callers can pass it around freely.
#[derive(Clone)]
pub struct IndexDb {
    pub(crate) db: Database,
    /// Carries the `schema_ver` row read + stamped in [`IndexDb::open`]; kept on the handle for
    /// future meta writes.
    #[allow(dead_code)]
    pub(crate) meta: Keyspace,
    pub(crate) symbols_by_path: Keyspace,
    /// Reserved fast-path partition: written on every upsert so that future name-based
    /// symbol search can skip the in-RAM linear scan. Not yet read by any MCP query path;
    /// kept to avoid a schema migration when the read path lands.
    pub(crate) symbols_by_name: Keyspace,
    pub(crate) calls_by_path: Keyspace,
    pub(crate) calls_by_callee: Keyspace,
    /// Reserved fast-path partition: written on every upsert so that future
    /// `dependents`-by-module queries can use a prefix scan instead of iterating the
    /// full import set. Not yet read by any MCP query path; kept to avoid a schema
    /// migration when the read path lands.
    pub(crate) imports_by_module: Keyspace,
    pub(crate) imports_by_path: Keyspace,
    /// `implementations_by_trait`: prefix scans on trait name — backs `find_implementations`.
    pub(crate) implementations_by_trait: Keyspace,
    /// `implementations_by_path`: companion to keep the per-file delete on upsert O(prefix).
    pub(crate) implementations_by_path: Keyspace,
    #[allow(dead_code)] // reserved for the future vector iteration
    pub(crate) embeddings: Keyspace,
    /// `memory_by_key`: scope + key → msgpack `MemoryRecord`.
    /// Always created for DB stability; used by `memory` feature tools.
    #[allow(dead_code)]
    pub(crate) memory_by_key: Keyspace,
    /// `memory_archive`: same key shape as `memory_by_key` — holds memories the W10 audit
    /// auto-archived after going stale > 90 days. Recoverable; never read on the hot path.
    /// Always created for DB stability; used by `memory` feature governance tools.
    #[allow(dead_code)]
    pub(crate) memory_archive: Keyspace,
    /// `proposals`: scope + kind + content-addressed id → msgpack proposal record. Backs the
    /// W11 propose-don't-commit skill-mining surface. Always created for DB stability.
    #[allow(dead_code)]
    pub(crate) proposals: Keyspace,
}

impl IndexDb {
    /// Open (or create) the index DB under `view_dir`. On schema-version mismatch the
    /// existing `index.fjall/` directory is dropped and a fresh one is created — the
    /// caller is responsible for repopulating it via `IndexWriter`.
    pub fn open(view_dir: &Path) -> Result<Self, IndexError> {
        let dir = view_dir.join(INDEX_DIR);
        std::fs::create_dir_all(&dir).map_err(|source| IndexError::Io {
            path: dir.clone(),
            source,
        })?;
        // Open the Fjall database ONCE and read the persisted schema version through the same
        // handle we intend to keep. The previous code peeked the version via a throwaway
        // `Database::open` before this real open — two exclusive fjall-lock acquisitions per
        // `IndexDb::open`, which doubled the window in which a concurrent reader's transient open
        // could collide with a rightful writer and force it to downgrade to read-only (the
        // multi-session writer-downgrade race). One open closes that extra window. Only the rare
        // schema-mismatch path below pays a second open.
        let mut db = Database::builder(&dir).open()?;
        let mut meta = db.keyspace("meta", KeyspaceCreateOptions::default)?;
        let on_disk_ver = meta
            .get(META_SCHEMA_VER)?
            .and_then(|bytes| <[u8; 4]>::try_from(&bytes[..]).ok())
            .map(u32::from_be_bytes);
        if matches!(on_disk_ver, Some(ver) if ver != INDEX_SCHEMA_VER) {
            // Schema drifted: drop every handle first so Fjall releases the directory lock and its
            // file handles, then wipe and reopen fresh. The caller repopulates from the msgpack
            // blobs via `IndexWriter`. Mirrors the old wipe-on-mismatch flow, minus the extra open
            // on the common (matching / brand-new) path.
            drop(meta);
            drop(db);
            std::fs::remove_dir_all(&dir).map_err(|source| IndexError::Io {
                path: dir.clone(),
                source,
            })?;
            std::fs::create_dir_all(&dir).map_err(|source| IndexError::Io {
                path: dir.clone(),
                source,
            })?;
            db = Database::builder(&dir).open()?;
            meta = db.keyspace("meta", KeyspaceCreateOptions::default)?;
        }
        let symbols_by_path = db.keyspace("symbols_by_path", KeyspaceCreateOptions::default)?;
        let symbols_by_name = db.keyspace("symbols_by_name", KeyspaceCreateOptions::default)?;
        let calls_by_path = db.keyspace("calls_by_path", KeyspaceCreateOptions::default)?;
        let calls_by_callee = db.keyspace("calls_by_callee", KeyspaceCreateOptions::default)?;
        let imports_by_module = db.keyspace("imports_by_module", KeyspaceCreateOptions::default)?;
        let imports_by_path = db.keyspace("imports_by_path", KeyspaceCreateOptions::default)?;
        let implementations_by_trait =
            db.keyspace("implementations_by_trait", KeyspaceCreateOptions::default)?;
        let implementations_by_path =
            db.keyspace("implementations_by_path", KeyspaceCreateOptions::default)?;
        let embeddings = db.keyspace("embeddings", KeyspaceCreateOptions::default)?;
        let memory_by_key = db.keyspace("memory_by_key", KeyspaceCreateOptions::default)?;
        let memory_archive = db.keyspace("memory_archive", KeyspaceCreateOptions::default)?;
        let proposals = db.keyspace("proposals", KeyspaceCreateOptions::default)?;

        // Stamp the version on a fresh open. We do this every time because rewriting one
        // 4-byte row is essentially free and saves us from a "was it really empty?" race.
        meta.insert(META_SCHEMA_VER, INDEX_SCHEMA_VER.to_be_bytes())?;

        Ok(Self {
            db,
            meta,
            symbols_by_path,
            symbols_by_name,
            calls_by_path,
            calls_by_callee,
            imports_by_module,
            imports_by_path,
            implementations_by_trait,
            implementations_by_path,
            embeddings,
            memory_by_key,
            memory_archive,
            proposals,
        })
    }

    /// Open a new batched writer scoped to this DB. Multiple writers can coexist — Fjall
    /// handles internal serialization. Used by the scanner's per-file worker tasks.
    pub fn writer(&self) -> writer::IndexWriter {
        writer::IndexWriter::new(self.clone())
    }

    /// True when the secondary index holds no per-file symbol entries. Cheap — peeks at
    /// the first key of `symbols_by_path` rather than counting.
    ///
    /// Used by the MCP startup auto-scan to detect a present-but-empty Fjall index (e.g. a
    /// `views/<view>/index.fjall/` that was wiped or removed out-of-band while the msgpack
    /// `index.msgpack` survived). In that state the in-RAM map cache looks populated but the
    /// Fjall-backed tools (`find_references` / `search_symbols`) would silently return nothing,
    /// so a rescan is warranted even though the RAM cache is non-empty.
    pub fn symbols_index_is_empty(&self) -> bool {
        self.symbols_by_path.iter().next().is_none()
    }
}
