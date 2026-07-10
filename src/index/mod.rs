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

/// The index-layout revision, added to `RELEASE_MINOR` to form [`INDEX_SCHEMA_VER`]. Bump this
/// (per the `index-keyspace-evolution` skill) whenever the on-disk keyspace layout changes
/// independently of a release: `+1` was the `imports_by_path` companion partition; `+2` the
/// `implementations_by_trait` / `implementations_by_path` partitions for `find_implementations`;
/// `+3` the `refs_by_def` / `refs_by_path` partitions for the code-intelligence tier's
/// scope/import-resolved `find_references` / `goto_definition`; `+4` the `code_bm25_postings` /
/// `code_bm25_by_path` partitions for the code-search BM25 keyword lane (`search_code mode=keyword`).
const INDEX_PARTITION_REVISION: u32 = 4;

/// Bumped whenever the on-disk key layout changes — the sum of `RELEASE_MINOR` and the
/// [`INDEX_PARTITION_REVISION`] offset, monotonic across both. When `RELEASE_MINOR` next bumps,
/// both move together. Decoupled from blob schema ([`crate::extract::SCHEMA_VER`]) which stays tied
/// to `RELEASE_MINOR` — blobs remain valid across a pure index revision; only the secondary index
/// rebuilds on next open via the wipe-on-mismatch flow in [`IndexDb::open`].
pub const INDEX_SCHEMA_VER: u32 = crate::version::RELEASE_MINOR as u32 + INDEX_PARTITION_REVISION;

const META_SCHEMA_VER: &[u8] = b"schema_ver";

/// `meta` rows carrying the corpus-global BM25 stats — recomputed at the end of each scan by
/// [`IndexDb::recompute_bm25_stats`] and read at query time. `N` = number of indexed code chunks;
/// `total_len` = sum of their token lengths (`avgdl = total_len / N`).
const META_BM25_DOC_COUNT: &[u8] = b"code_bm25_n";
const META_BM25_TOTAL_LEN: &[u8] = b"code_bm25_total_len";

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
    /// `refs_by_def`: scope/import-resolved reference edges keyed by defining site — backs the
    /// resolved `find_references` / `find_callers`. Written by the scanner's resolve pass (B3).
    pub(crate) refs_by_def: Keyspace,
    /// `refs_by_path`: companion keyed by the use file — O(prefix) delete on re-resolve and the
    /// forward lookup behind `goto_definition`. Written by the scanner's resolve pass (B3).
    pub(crate) refs_by_path: Keyspace,
    /// `code_bm25_postings`: term → chunks (with inlined tf + doclen) for the code-search BM25
    /// keyword lane. Always created for DB stability; read + written only under the `code-search`
    /// feature (hence `dead_code`-allowed on a default build, mirroring `embeddings`).
    #[allow(dead_code)]
    pub(crate) code_bm25_postings: Keyspace,
    /// `code_bm25_by_path`: forward companion keyed by file → its chunks' `(chunk_id, doclen,
    /// terms)`, so a re-scan deletes the previous postings in O(prefix). Always created.
    pub(crate) code_bm25_by_path: Keyspace,
    #[allow(dead_code)]
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
        let mut db = Database::builder(&dir).open()?;
        let mut meta = db.keyspace("meta", KeyspaceCreateOptions::default)?;
        let on_disk_ver = meta
            .get(META_SCHEMA_VER)?
            .and_then(|bytes| <[u8; 4]>::try_from(&bytes[..]).ok())
            .map(u32::from_be_bytes);
        if matches!(on_disk_ver, Some(ver) if ver != INDEX_SCHEMA_VER) {
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
        let implementations_by_trait = db.keyspace("implementations_by_trait", KeyspaceCreateOptions::default)?;
        let implementations_by_path = db.keyspace("implementations_by_path", KeyspaceCreateOptions::default)?;
        let refs_by_def = db.keyspace("refs_by_def", KeyspaceCreateOptions::default)?;
        let refs_by_path = db.keyspace("refs_by_path", KeyspaceCreateOptions::default)?;
        let code_bm25_postings = db.keyspace("code_bm25_postings", KeyspaceCreateOptions::default)?;
        let code_bm25_by_path = db.keyspace("code_bm25_by_path", KeyspaceCreateOptions::default)?;
        let embeddings = db.keyspace("embeddings", KeyspaceCreateOptions::default)?;
        let memory_by_key = db.keyspace("memory_by_key", KeyspaceCreateOptions::default)?;
        let memory_archive = db.keyspace("memory_archive", KeyspaceCreateOptions::default)?;
        let proposals = db.keyspace("proposals", KeyspaceCreateOptions::default)?;

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
            refs_by_def,
            refs_by_path,
            code_bm25_postings,
            code_bm25_by_path,
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

    /// Resolved references to the definition at `(def_path, def_start)` — the scope/import-resolved
    /// backing for `find_references`. Returns each binding `(use_path, use_start)`; empty when the
    /// definition has no resolved uses (or resolution never ran for its file).
    pub fn references_to(&self, def_path: &crate::path::RelPath, def_start: u32) -> Vec<(crate::path::RelPath, u32)> {
        let prefix = keys::refs_by_def_prefix(def_path, def_start);
        let mut out = Vec::new();
        for guard in self.refs_by_def.prefix(prefix) {
            if let Ok((k, _)) = guard.into_inner()
                && let Some((_def_path, _def_start, use_path, use_start)) = keys::parse_ref_by_def(&k)
            {
                out.push((use_path, use_start));
            }
        }
        out
    }

    /// The definition the use at `(use_path, use_start)` binds to — backs `goto_definition`.
    /// `None` when the position isn't a resolved reference.
    pub fn definition_of(
        &self,
        use_path: &crate::path::RelPath,
        use_start: u32,
    ) -> Option<(crate::path::RelPath, u32)> {
        let prefix = keys::refs_by_use_prefix(use_path, use_start);
        for guard in self.refs_by_path.prefix(prefix) {
            if let Ok((k, _)) = guard.into_inner()
                && let Some((_use_path, _use_start, def_path, def_start)) = keys::parse_ref_by_path(&k)
            {
                return Some((def_path, def_start));
            }
        }
        None
    }

    /// Symbols whose name starts with `name`, from the `symbols_by_name` keyspace — an index-backed
    /// prefix scan (length-prefixed keys isolate `Foo` from `Foobar`). Returns `(name, kind, path,
    /// start_byte)` for each match, capped at `cap` entries. Backs the code-search **exact lane**:
    /// an identifier-shaped query resolves to the symbols that define it, which then map to their
    /// owning chunks. The returned `name` lets the caller rank exact-name matches ahead of longer
    /// prefix matches. `start_byte` is the L1 `Symbol.start_byte` (node start), which falls inside
    /// the symbol's owning chunk span.
    pub fn symbols_by_name_lookup(
        &self,
        name: &str,
        cap: usize,
    ) -> Vec<(String, crate::extract::SymbolKind, crate::path::RelPath, u32)> {
        let prefix = keys::symbols_by_name_prefix(name);
        let mut out = Vec::new();
        for guard in self.symbols_by_name.prefix(prefix) {
            if out.len() >= cap {
                break;
            }
            if let Ok((k, _)) = guard.into_inner()
                && let Some((matched, kind, rel, start_byte)) = keys::parse_symbol_by_name(&k)
            {
                out.push((matched, kind, rel, start_byte));
            }
        }
        out
    }

    /// Corpus-global BM25 stats for the code-search keyword lane: `(N, total_len)` where `N` is the
    /// number of indexed chunks and `total_len` the sum of their token lengths (so `avgdl =
    /// total_len / N`). Read from the `meta` keyspace at query time. `None` (or `N == 0`) means the
    /// BM25 index is empty — no chunks were indexed, or [`recompute_bm25_stats`] never ran.
    ///
    /// [`recompute_bm25_stats`]: Self::recompute_bm25_stats
    pub fn bm25_stats(&self) -> Option<(u64, u64)> {
        let n = self
            .meta
            .get(META_BM25_DOC_COUNT)
            .ok()
            .flatten()
            .and_then(|b| <[u8; 8]>::try_from(&b[..]).ok())
            .map(u64::from_be_bytes)?;
        let total_len = self
            .meta
            .get(META_BM25_TOTAL_LEN)
            .ok()
            .flatten()
            .and_then(|b| <[u8; 8]>::try_from(&b[..]).ok())
            .map(u64::from_be_bytes)
            .unwrap_or(0);
        Some((n, total_len))
    }

    /// Recompute the corpus-global BM25 stats by sweeping the `code_bm25_by_path` forward keyspace
    /// once (one entry per chunk; only its 4-byte `doclen` prefix is decoded — the term list is not
    /// touched) and stamping `(N, total_len)` into `meta`. Runs single-threaded in the scanner's
    /// serial apply pass, after every per-file batch has committed, so there is no cross-thread
    /// counter contention — the per-file workers only ever append postings.
    ///
    /// The full sweep is exact regardless of what changed this scan. On a huge repo an incremental
    /// rescan still pays a full (cheap — small-value) sweep; a delta-update path is the obvious
    /// optimization if it ever shows up in the harden timings.
    pub fn recompute_bm25_stats(&self) -> Result<(), IndexError> {
        let mut n: u64 = 0;
        let mut total_len: u64 = 0;
        for guard in self.code_bm25_by_path.iter() {
            let (_k, v) = guard.into_inner()?;
            if v.len() >= 4 {
                total_len += u64::from(u32::from_be_bytes([v[0], v[1], v[2], v[3]]));
            }
            n += 1;
        }
        self.meta.insert(META_BM25_DOC_COUNT, n.to_be_bytes())?;
        self.meta.insert(META_BM25_TOTAL_LEN, total_len.to_be_bytes())?;
        Ok(())
    }
}
