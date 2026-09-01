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
pub mod keys_governance;
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
/// `code_bm25_by_path` partitions for the code-search BM25 keyword lane (`search_code mode=keyword`);
/// `+5` the per-keyspace [`KEYSPACE_MEMTABLE_BYTES`] sizing — fjall persists `max_memtable_size`
/// into its meta keyspace at *create* time and recovers it on reopen, ignoring the create options
/// for a keyspace that already exists, so an index built by an earlier build would keep the 64 MiB
/// default forever unless the schema mismatch forces it to be recreated.
const INDEX_PARTITION_REVISION: u32 = 5;

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

/// Per-keyspace memtable sizes, in bytes. Three tiers, assigned by how much one scan writes to the
/// keyspace: [`MEMTABLE_HOT_BYTES`] for the BM25 posting list (the densest writer by far — one
/// entry per `(term, chunk)` pair), [`MEMTABLE_WARM_BYTES`] for the secondary indexes the scanner
/// rewrites once per file, [`MEMTABLE_COLD_BYTES`] for the keyspaces a scan barely touches.
///
/// This table is the single source of truth: [`open_keyspace`] looks every keyspace up here, and
/// `keyspace_memtable_table_covers_every_partition` fails if the two ever drift apart.
const KEYSPACE_MEMTABLE_BYTES: &[(&str, u64)] = &[
    ("meta", MEMTABLE_COLD_BYTES),
    ("symbols_by_path", MEMTABLE_WARM_BYTES),
    ("symbols_by_name", MEMTABLE_WARM_BYTES),
    ("calls_by_path", MEMTABLE_WARM_BYTES),
    ("calls_by_callee", MEMTABLE_WARM_BYTES),
    ("imports_by_module", MEMTABLE_COLD_BYTES),
    ("imports_by_path", MEMTABLE_COLD_BYTES),
    ("implementations_by_trait", MEMTABLE_COLD_BYTES),
    ("implementations_by_path", MEMTABLE_COLD_BYTES),
    ("refs_by_def", MEMTABLE_WARM_BYTES),
    ("refs_by_path", MEMTABLE_WARM_BYTES),
    ("code_bm25_postings", MEMTABLE_HOT_BYTES),
    ("code_bm25_by_path", MEMTABLE_WARM_BYTES),
    ("embeddings", MEMTABLE_COLD_BYTES),
    ("memory_by_key", MEMTABLE_COLD_BYTES),
    ("memory_archive", MEMTABLE_COLD_BYTES),
    ("proposals", MEMTABLE_COLD_BYTES),
];

/// Memtable size for the BM25 posting keyspace — the one partition that measurably benefits from
/// headroom (it absorbs the large majority of a scan's index entries), so it gets the largest
/// share of the ceiling while still sitting well inside fjall's recommended 8-64 MiB band.
const MEMTABLE_HOT_BYTES: u64 = 16 * 1_024 * 1_024;

/// Memtable size for the keyspaces the scanner rewrites once per file. 8 MiB is the floor of
/// fjall's recommended band: small enough to matter for the ceiling, large enough that an ordinary
/// repo's per-keyspace working set still lands in one or two flushes.
const MEMTABLE_WARM_BYTES: u64 = 8 * 1_024 * 1_024;

/// Memtable size for keyspaces a scan barely writes (`meta`, the reserved companions, the memory
/// and proposal tiers). Deliberately below fjall's recommended band: none of them approaches even
/// 4 MiB in practice, so the value never changes their flush cadence — it only stops each from
/// contributing a 64 MiB slice to the worst case below.
const MEMTABLE_COLD_BYTES: u64 = 4 * 1_024 * 1_024;

/// Sealed (rotated, not yet flushed) memtables fjall lets one keyspace queue before it throttles
/// the writer — `Keyspace::local_backpressure` in fjall 3.1.10 sleeps at 4+. So a keyspace's
/// worst-case resident memtable memory is `max_memtable_size * (1 active + 4 sealed)`.
const MEMTABLE_SEALED_LIMIT: u64 = 4;

/// Worst-case resident memtable bytes across every keyspace of one open [`IndexDb`], each holding a
/// full active memtable plus the maximum sealed queue at the same moment.
///
/// This is the bound [`KEYSPACE_MEMTABLE_BYTES`] exists to set. At fjall's unconfigured 64 MiB
/// default the same expression is `17 * 5 * 64 MiB` = 5.3 GiB — headroom fjall would grant *before
/// stalling a single write*, which is how an unbounded scan reached 43.8 GiB RSS without fjall ever
/// pushing back.
pub const INDEX_MEMTABLE_CEILING_BYTES: u64 = memtable_ceiling_bytes();

const fn memtable_ceiling_bytes() -> u64 {
    let mut total = 0;
    let mut i = 0;
    while i < KEYSPACE_MEMTABLE_BYTES.len() {
        total += KEYSPACE_MEMTABLE_BYTES[i].1 * (1 + MEMTABLE_SEALED_LIMIT);
        i += 1;
    }
    total
}

/// Open (creating on first use) one keyspace at its [`KEYSPACE_MEMTABLE_BYTES`] size.
///
/// fjall reads `max_memtable_size` from the create options only when the keyspace does not yet
/// exist; every later open recovers the value it persisted then. That is what couples this sizing
/// to [`INDEX_PARTITION_REVISION`] — without the bump, an index created by an earlier build would
/// never pick the new sizes up.
fn open_keyspace(db: &Database, name: &str) -> Result<Keyspace, IndexError> {
    let bytes = memtable_bytes_for(name);
    Ok(db.keyspace(name, move || KeyspaceCreateOptions::default().max_memtable_size(bytes))?)
}

fn memtable_bytes_for(name: &str) -> u64 {
    KEYSPACE_MEMTABLE_BYTES
        .iter()
        .find(|(known, _)| *known == name)
        .map_or(MEMTABLE_COLD_BYTES, |(_, bytes)| *bytes)
}

/// Floor for the Fjall block cache, in bytes. Matches the size fjall itself defaults to when
/// left unconfigured (32 MiB — see `fjall::db_config::Config::new`), so a small or freshly
/// created index is never worse off than before this module started sizing the cache
/// deliberately.
const INDEX_CACHE_FLOOR_BYTES: u64 = 32 * 1_024 * 1_024;

/// Ceiling for the Fjall block cache, in bytes, **per opened `IndexDb`**. The daemon's
/// `WorkspacePool` (see `src/comms/daemon.rs`) holds up to 16 workspaces at once under LRU
/// eviction, each owning one `IndexDb`. This ceiling is the load-bearing memory bound: in the
/// pathological worst case where all 16 pooled slots simultaneously hold a huge, cache-
/// saturating monorepo index (this repo's own `armis` hardening fixture: 82 k files, a 3.1 GB
/// on-disk index), total block-cache memory across the whole pool is bounded at
/// `16 * INDEX_CACHE_CEILING_BYTES` = 4 GiB. That is a deliberately conservative, explicit bound
/// (not unbounded, unlike the previous zero-configuration state) — the common case is far below
/// it, because [`index_cache_bytes`] scales the cache down for small/medium indexes instead of
/// defaulting every pooled slot to the ceiling.
const INDEX_CACHE_CEILING_BYTES: u64 = 256 * 1_024 * 1_024;

/// Fraction of the on-disk `index.fjall` size to budget for the block cache before clamping to
/// the floor/ceiling above. 20% mirrors fjall's own single-tenant guidance ("configure the block
/// cache capacity to be ~20-25% of the available memory — or more if the data set fully fits in
/// memory" — `fjall::Builder::cache_size` doc), applied to the dataset size rather than total
/// system RAM, since basemind opens many of these concurrently (see the ceiling above) and the
/// per-DB share of RAM is not a stable per-DB quantity the way the on-disk index size is.
const INDEX_CACHE_DISK_FRACTION: f64 = 0.20;

/// Overrides the computed Fjall block cache size outright (bytes), bypassing the floor/fraction/
/// ceiling heuristic in [`index_cache_bytes`]. Mirrors the `BASEMIND_GIT_CACHE_LOG_MAX_BYTES` /
/// `BASEMIND_BLAME_MAX_BYTES` env-var escape hatches used elsewhere for cache/budget tuning.
const INDEX_CACHE_BYTES_ENV: &str = "BASEMIND_INDEX_CACHE_BYTES";

/// Best-effort recursive sum of file sizes under `dir`. Errors (directory not yet created,
/// permission denied, a race with Fjall's own background compaction renaming/removing files
/// mid-walk) are swallowed — this only feeds the cache-size heuristic below, not correctness, so
/// an undercount just means a smaller (still-floored) cache rather than a failed `open`.
fn dir_size_bytes(dir: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                stack.push(entry.path());
            } else if file_type.is_file()
                && let Ok(metadata) = entry.metadata()
            {
                total += metadata.len();
            }
        }
    }
    total
}

/// Computes the Fjall block cache size (bytes) for opening the index at `dir` — the
/// `index.fjall` directory itself, whether it already exists or is about to be created fresh.
///
/// Workload-driven, not a magic constant: sizes the cache as [`INDEX_CACHE_DISK_FRACTION`] of the
/// *current* on-disk index size, clamped between [`INDEX_CACHE_FLOOR_BYTES`] and
/// [`INDEX_CACHE_CEILING_BYTES`]. A fresh/tiny repo's empty-or-small `index.fjall` clamps to the
/// floor (unchanged from fjall's own unconfigured default); a huge monorepo clamps to the
/// ceiling instead of trying to cache gigabytes. `INDEX_CACHE_BYTES_ENV` overrides this entirely
/// for operators who want to tune past the heuristic.
fn index_cache_bytes(dir: &Path) -> u64 {
    if let Some(bytes) = std::env::var(INDEX_CACHE_BYTES_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        return bytes;
    }
    let disk_bytes = dir_size_bytes(dir);
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let scaled_bytes = (disk_bytes as f64 * INDEX_CACHE_DISK_FRACTION) as u64;
    scaled_bytes.clamp(INDEX_CACHE_FLOOR_BYTES, INDEX_CACHE_CEILING_BYTES)
}

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
    /// Carries the `schema_ver` row read + stamped in [`IndexDb::open`]; also read and written
    /// by the BM25 corpus-stats recompute.
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
        let cache_bytes = index_cache_bytes(&dir);
        let mut db = Database::builder(&dir).cache_size(cache_bytes).open()?;
        let mut meta = open_keyspace(&db, "meta")?;
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
            db = Database::builder(&dir).cache_size(cache_bytes).open()?;
            meta = open_keyspace(&db, "meta")?;
        }
        let symbols_by_path = open_keyspace(&db, "symbols_by_path")?;
        let symbols_by_name = open_keyspace(&db, "symbols_by_name")?;
        let calls_by_path = open_keyspace(&db, "calls_by_path")?;
        let calls_by_callee = open_keyspace(&db, "calls_by_callee")?;
        let imports_by_module = open_keyspace(&db, "imports_by_module")?;
        let imports_by_path = open_keyspace(&db, "imports_by_path")?;
        let implementations_by_trait = open_keyspace(&db, "implementations_by_trait")?;
        let implementations_by_path = open_keyspace(&db, "implementations_by_path")?;
        let refs_by_def = open_keyspace(&db, "refs_by_def")?;
        let refs_by_path = open_keyspace(&db, "refs_by_path")?;
        let code_bm25_postings = open_keyspace(&db, "code_bm25_postings")?;
        let code_bm25_by_path = open_keyspace(&db, "code_bm25_by_path")?;
        let embeddings = open_keyspace(&db, "embeddings")?;
        let memory_by_key = open_keyspace(&db, "memory_by_key")?;
        let memory_archive = open_keyspace(&db, "memory_archive")?;
        let proposals = open_keyspace(&db, "proposals")?;

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
    /// Cross-file definitions take precedence over the local import binding that an imported use
    /// also resolves through. `None` when the position isn't a resolved reference.
    pub fn definition_of(
        &self,
        use_path: &crate::path::RelPath,
        use_start: u32,
    ) -> Option<(crate::path::RelPath, u32)> {
        let prefix = keys::refs_by_use_prefix(use_path, use_start);
        let mut local = None;
        for guard in self.refs_by_path.prefix(prefix) {
            if let Ok((k, _)) = guard.into_inner()
                && let Some((_use_path, _use_start, def_path, def_start)) = keys::parse_ref_by_path(&k)
            {
                if def_path != *use_path {
                    return Some((def_path, def_start));
                }
                local = Some((def_path, def_start));
            }
        }
        local
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Bytes written into one keyspace to provoke a rotation. Above every tier in
    /// [`KEYSPACE_MEMTABLE_BYTES`], and far below fjall's unconfigured 64 MiB default — so the
    /// flush this triggers happens *because* the keyspace was sized, and would not happen at the
    /// default. Also well below fjall's 512 MiB `max_journaling_size`, which is the only other
    /// thing that could rotate a memtable here.
    const ROTATION_PROBE_BYTES: usize = 24 * 1_024 * 1_024;

    fn write_until_rotation_probe(keyspace: &Keyspace) {
        let value = vec![0u8; 4_096];
        for i in 0..(ROTATION_PROBE_BYTES / value.len()) {
            keyspace
                .insert(i.to_be_bytes(), value.as_slice())
                .expect("probe insert");
        }
    }

    /// True once fjall has flushed at least one memtable of `keyspace` to disk. Rotation is
    /// requested synchronously on insert but performed by a background worker, so this polls.
    fn flushed_within(keyspace: &Keyspace, timeout: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if keyspace.disk_space() > 0 {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        keyspace.disk_space() > 0
    }

    #[test]
    fn keyspace_memtable_table_covers_every_partition() {
        let dir = tempfile::tempdir().unwrap();
        let db = IndexDb::open(dir.path()).unwrap();
        let mut opened: Vec<String> = db.db.list_keyspace_names().iter().map(ToString::to_string).collect();
        opened.sort();
        let mut sized: Vec<String> = KEYSPACE_MEMTABLE_BYTES
            .iter()
            .map(|(name, _)| (*name).to_string())
            .collect();
        sized.sort();
        assert_eq!(
            opened, sized,
            "every opened keyspace must be sized by the table, and the table must name no keyspace \
             that is never opened — an unlisted name silently falls back to the cold tier"
        );
    }

    #[test]
    fn memtable_ceiling_is_bounded() {
        let partitions = u64::try_from(KEYSPACE_MEMTABLE_BYTES.len()).expect("keyspace count fits a u64");
        let unsized_ceiling = partitions * (1 + MEMTABLE_SEALED_LIMIT) * 64 * 1_024 * 1_024;
        // A const block, because both sides are constants: this then fails the build rather than a
        // test run, which is the right moment to learn that a new keyspace blew the ceiling. ~keep
        const { assert!(INDEX_MEMTABLE_CEILING_BYTES <= 1_024 * 1_024 * 1_024) };
        assert!(
            INDEX_MEMTABLE_CEILING_BYTES * 4 <= unsized_ceiling,
            "sizing must buy at least a 4x cut against fjall's 64 MiB default"
        );
    }

    /// The load-bearing half of the sizing: fjall persists `max_memtable_size` when the keyspace is
    /// *created* and recovers it on every later open, ignoring the create options for a keyspace
    /// that already exists. A test that only checked a freshly created DB would pass even if the
    /// value never survived a restart — which is the state every long-lived index is actually in.
    ///
    /// `max_memtable_size` has no public getter, so this observes the size behaviourally: write
    /// more than the configured size (but far less than fjall's 64 MiB default) into a cold-tier
    /// keyspace *after* reopening, and require that fjall rotated and flushed it.
    #[test]
    fn configured_memtable_size_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        {
            let db = IndexDb::open(dir.path()).unwrap();
            assert_eq!(db.proposals.disk_space(), 0, "nothing written yet");
        }
        let db = IndexDb::open(dir.path()).unwrap();
        write_until_rotation_probe(&db.proposals);
        assert!(
            flushed_within(&db.proposals, std::time::Duration::from_secs(30)),
            "a reopened keyspace must still rotate at its configured {MEMTABLE_COLD_BYTES}-byte \
             memtable size; no flush means fjall recovered the 64 MiB default instead"
        );
    }
}
