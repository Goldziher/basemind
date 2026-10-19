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

pub mod fault;
pub mod grep_bloom;
pub mod keys;
pub mod keys_governance;
pub mod keys_probe;
pub(crate) mod memory_store;
pub(crate) mod name_dict;
pub mod view;
#[cfg(test)]
mod view_tests;
pub mod writer;

use std::path::{Path, PathBuf};

use fjall::config::CompressionPolicy;
use fjall::{CompressionType, Database, Keyspace, KeyspaceCreateOptions};
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
/// default forever unless the schema mismatch forces it to be recreated; `+6` the LZ4-at-every-level
/// block compression ([`index_keyspace_options`]), persisted at create time for the same reason;
/// `+7` the `grep_bloom` re-key from path to content hash, whose old rows would read as foreign hashes.
const INDEX_PARTITION_REVISION: u32 = 7;

/// Bumped whenever the on-disk key layout changes — the sum of `RELEASE_MINOR` and the
/// [`INDEX_PARTITION_REVISION`] offset, monotonic across both. When `RELEASE_MINOR` next bumps,
/// both move together. Decoupled from blob schema ([`crate::extract::SCHEMA_VER`]) which stays tied
/// to `RELEASE_MINOR` — blobs remain valid across a pure index revision; only the secondary index
/// rebuilds on next open via the wipe-on-mismatch flow in [`IndexDb::open`].
pub const INDEX_SCHEMA_VER: u32 = crate::version::RELEASE_MINOR as u32 + INDEX_PARTITION_REVISION;

const META_SCHEMA_VER: &[u8] = b"schema_ver";

/// `meta` row stamping that `import_targets` / `importers_by_probe` describe EVERY importer, i.e. a
/// wholesale resolve pass completed since they were created. An index that predates the two
/// keyspaces (or a wholesale pass that never finished) lacks it, and the incremental pass then runs
/// the wholesale one once instead of trusting an empty probe index.
const META_IMPORT_PROBE_VER: &[u8] = b"import_probe_ver";

/// Layout revision of the probe keyspaces; bump to force one wholesale resolve pass.
const IMPORT_PROBE_VER: u8 = 1;

/// `meta` rows carrying the corpus-global BM25 stats — recomputed at the end of each scan by
/// [`IndexDb::recompute_bm25_stats`] and read at query time. `N` = number of indexed code chunks;
/// `total_len` = sum of their token lengths (`avgdl = total_len / N`).
const META_BM25_DOC_COUNT: &[u8] = b"code_bm25_n";
const META_BM25_TOTAL_LEN: &[u8] = b"code_bm25_total_len";

pub(crate) const INDEX_DIR: &str = "index.fjall";

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
    ("import_targets", MEMTABLE_WARM_BYTES),
    ("importers_by_probe", MEMTABLE_WARM_BYTES),
    ("code_bm25_postings", MEMTABLE_HOT_BYTES),
    ("code_bm25_by_path", MEMTABLE_WARM_BYTES),
    ("grep_bloom", MEMTABLE_COLD_BYTES),
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
    Ok(db.keyspace(name, move || index_keyspace_options(bytes))?)
}

/// Create options shared by every index keyspace: the tiered memtable size plus LZ4 on data and
/// index blocks at *every* level. fjall's default policy is `[None, None, Lz4]`, which leaves L0 and
/// L1 — where most of a freshly bulk-built index sits — uncompressed. Blocks record their own
/// compression in the table metadata, so tables written under the old policy still read; the policy
/// itself is persisted at keyspace creation, which is why this ships with an
/// [`INDEX_PARTITION_REVISION`] bump.
fn index_keyspace_options(memtable_bytes: u64) -> KeyspaceCreateOptions {
    KeyspaceCreateOptions::default()
        .max_memtable_size(memtable_bytes)
        .data_block_compression_policy(CompressionPolicy::all(CompressionType::Lz4))
        .index_block_compression_policy(CompressionPolicy::all(CompressionType::Lz4))
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

/// `path` followed by each of its ancestor directories, innermost first.
fn probe_candidates(path: &str) -> impl Iterator<Item = &str> {
    std::iter::once(path).chain(path.rmatch_indices('/').map(move |(i, _)| &path[..i]))
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

/// One `(keyspace, key, value)` row, as returned by [`IndexDb::resolution_rows`].
pub type ResolutionRow = (&'static str, Vec<u8>, Vec<u8>);

/// Handle to every keyspace we read or write. Cloned cheaply (each `Keyspace` is `Arc`'d
/// internally by Fjall), so callers can pass it around freely.
#[derive(Clone)]
pub struct IndexDb {
    pub(crate) db: Database,
    /// Carries the `schema_ver` row read + stamped in [`IndexDb::open`]; also read and written
    /// by the BM25 corpus-stats recompute.
    pub(in crate::index) meta: Keyspace,
    pub(in crate::index) symbols_by_path: Keyspace,
    /// Reserved fast-path partition: written on every upsert so that future name-based
    /// symbol search can skip the in-RAM linear scan. Not yet read by any MCP query path;
    /// kept to avoid a schema migration when the read path lands.
    pub(in crate::index) symbols_by_name: Keyspace,
    pub(in crate::index) calls_by_path: Keyspace,
    pub(in crate::index) calls_by_callee: Keyspace,
    /// Distinct callee names of `calls_by_callee`; lets the substring name scan range-scan only the
    /// matching names instead of walking the partition.
    pub(in crate::index) callee_names: std::sync::Arc<name_dict::NameDict>,
    /// Reserved fast-path partition: written on every upsert so that future
    /// `dependents`-by-module queries can use a prefix scan instead of iterating the
    /// full import set. Not yet read by any MCP query path; kept to avoid a schema
    /// migration when the read path lands.
    pub(in crate::index) imports_by_module: Keyspace,
    pub(in crate::index) imports_by_path: Keyspace,
    /// `implementations_by_trait`: prefix scans on trait name — backs `find_implementations`.
    pub(in crate::index) implementations_by_trait: Keyspace,
    /// Distinct trait names of `implementations_by_trait`, as [`Self::callee_names`] is for callees.
    pub(in crate::index) trait_names: std::sync::Arc<name_dict::NameDict>,
    /// `implementations_by_path`: companion to keep the per-file delete on upsert O(prefix).
    pub(in crate::index) implementations_by_path: Keyspace,
    /// `refs_by_def`: scope/import-resolved reference edges keyed by defining site — backs the
    /// resolved `find_references` / `find_callers`. Written by the scanner's resolve pass (B3).
    pub(in crate::index) refs_by_def: Keyspace,
    /// `refs_by_path`: companion keyed by the use file — O(prefix) delete on re-resolve and the
    /// forward lookup behind `goto_definition`. Written by the scanner's resolve pass (B3).
    pub(in crate::index) refs_by_path: Keyspace,
    /// `import_targets`: importer + import-binding offset -> the definition the stitch bound it to,
    /// plus one reserved probe-manifest row per importer (see [`keys_probe`]).
    pub(in crate::index) import_targets: Keyspace,
    /// `importers_by_probe`: probed path hash + importer. Every path the resolver consulted for an
    /// importer's imports, so the incremental re-stitch finds exactly the importers a changed,
    /// added or deleted path can affect.
    pub(in crate::index) importers_by_probe: Keyspace,
    /// `code_bm25_postings`: term → chunks (with inlined tf + doclen) for the code-search BM25
    /// keyword lane. Always created for DB stability; read + written only under the `code-search`
    /// feature (hence `dead_code`-allowed on a default build, mirroring `embeddings`).
    #[allow(dead_code)]
    pub(in crate::index) code_bm25_postings: Keyspace,
    /// `code_bm25_by_path`: forward companion keyed by file → its chunks' `(chunk_id, doclen,
    /// terms)`, so a re-scan deletes the previous postings in O(prefix). Always created.
    pub(in crate::index) code_bm25_by_path: Keyspace,
    /// `grep_bloom`: hex content hash -> trigram bloom filter of that content (see [`grep_bloom`]),
    /// shared by every path holding the bytes. A candidate PREFILTER for `code grep` only; a missing
    /// row means "candidate", which is why this partition was added without a schema bump and
    /// backfills on rescan.
    pub(in crate::index) grep_bloom: Keyspace,
    #[allow(dead_code)]
    pub(crate) embeddings: Keyspace,
    /// `memory_by_key`: scope + key → msgpack `MemoryRecord`. Lives in the per-repo-group
    /// [`memory_store`], not in this view's database.
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
    /// First fatal commit error (disk full, poisoned handle) seen through any clone of this handle;
    /// shared so the writers record it where it happens and the scan polls one place.
    pub(crate) fault: std::sync::Arc<fault::IndexFault>,
    /// Keeps the shared group memory database open for as long as any `IndexDb` clone uses its
    /// keyspaces above.
    _memory: std::sync::Arc<memory_store::MemoryDb>,
    /// True when the group memory store was held by another process at open and the three memory
    /// keyspaces above are an empty private placeholder: the code index is fully usable, but memory
    /// reads and writes must go through the process that owns the store (the daemon). See
    /// [`IndexDb::open_tolerating_held_memory`].
    memory_detached: bool,
    /// True when this open found an index written under another [`INDEX_SCHEMA_VER`] and discarded
    /// it, so the database is empty while the file map that fed it may still be populated.
    wiped_on_open: bool,
}

/// Flush/compaction threads per index database. fjall defaults to `min(cores, 4)` per `Database`, and
/// the daemon holds one per hot workspace, so the default multiplies into dozens of mostly idle threads.
const FJALL_WORKER_THREADS: usize = 2;

/// Journal ceiling per index database (fjall's minimum; its default is 512 MiB). Recovery replays every
/// unflushed journal into memtables on open, so an oversized journal is a multi-GB allocation on every
/// workspace open; a 2.5 GiB single journal was observed on a scanned worktree.
const FJALL_MAX_JOURNAL_BYTES: u64 = 64 * 1_024 * 1_024;

/// Journal ceiling for a database opened empty, i.e. about to take a cold bulk build. At the 64 MiB
/// steady-state cap a cold scan of a large repo logged hundreds of `Rotating ... to reduce journal
/// size` events, each flushing a small table and queueing extra compaction. fjall fixes the cap per
/// `Database` at build time, so it cannot be lowered mid-run; the next normal open uses
/// [`FJALL_MAX_JOURNAL_BYTES`] again, and the flush-on-open below keeps a large leftover journal
/// from being replayed twice.
const FJALL_BULK_JOURNAL_BYTES: u64 = JOURNAL_FLUSH_ON_OPEN_BYTES;

/// Journal size past which `open` flushes every keyspace, so the next open does not replay it again.
const JOURNAL_FLUSH_ON_OPEN_BYTES: u64 = 256 * 1_024 * 1_024;

/// True when `dir` has no entries, i.e. fjall is about to create the database from scratch.
fn is_empty_dir(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_none())
}

/// Total bytes of fjall journal files under `view_dir`'s index database, WITHOUT opening it (opening
/// replays every journal into memtables, which is the cost being measured). Zero when absent.
pub fn journal_bytes_on_disk(view_dir: &Path) -> u64 {
    // fjall keeps its journals as `<n>.jnl` files directly under the database directory.
    std::fs::read_dir(view_dir.join(INDEX_DIR))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "jnl"))
        .filter_map(|entry| entry.metadata().ok())
        .filter(std::fs::Metadata::is_file)
        .map(|meta| meta.len())
        .sum()
}

/// Whether the index database under `view_dir` carries a journal big enough that opening it is a
/// memory spike (the threshold at which `open` itself flushes).
pub fn journal_exceeds_open_budget(view_dir: &Path) -> bool {
    journal_bytes_on_disk(view_dir) > JOURNAL_FLUSH_ON_OPEN_BYTES
}

/// Read the fjall `meta.schema_ver` under `view_dir` WITHOUT the wipe-on-mismatch that
/// [`IndexDb::open`] performs. `Ok(None)` when the database carries no version. Used by the
/// worktree view seeder to vet a cloned view before it is promoted into place.
pub fn peek_index_schema_ver(view_dir: &Path) -> Result<Option<u32>, IndexError> {
    let dir = view_dir.join(INDEX_DIR);
    let db = Database::builder(&dir)
        .cache_size(8 * 1_024 * 1_024)
        .worker_threads(FJALL_WORKER_THREADS)
        .max_journaling_size(FJALL_MAX_JOURNAL_BYTES)
        .open()?;
    let meta = open_keyspace(&db, "meta")?;
    let ver = meta
        .get(META_SCHEMA_VER)?
        .and_then(|bytes| <[u8; 4]>::try_from(&bytes[..]).ok())
        .map(u32::from_be_bytes);
    Ok(ver)
}

impl IndexDb {
    /// Open (or create) the index DB under `view_dir`. On schema-version mismatch the
    /// existing `index.fjall/` directory is dropped and a fresh one is created — the
    /// caller is responsible for repopulating it via `IndexWriter`.
    ///
    /// The user-memory keyspaces live in a sibling `memory.fjall` under `view_dir`. Production opens
    /// go through [`IndexDb::open_with_memory`] with the per-repo-group store instead.
    pub fn open(view_dir: &Path) -> Result<Self, IndexError> {
        Self::open_with_memory(view_dir, &view_dir.join(crate::store_layout::MEMORY_STORE_DIR))
    }

    /// [`IndexDb::open`] with the memory keyspaces (`memory_by_key`, `memory_archive`, `proposals`)
    /// backed by the shared Fjall database at `memory_dir` — one per repo group, so they survive
    /// eviction of any view and are shared by every worktree of the clone. Rows an older build left
    /// in this view are merged into that store (last writer wins by timestamp) and removed from the
    /// view before any schema-mismatch wipe can destroy them.
    pub fn open_with_memory(view_dir: &Path, memory_dir: &Path) -> Result<Self, IndexError> {
        Self::open_inner(view_dir, memory_dir, false)
    }

    /// [`IndexDb::open_with_memory`] for read-only consumers: when another process holds the group
    /// memory store, the view index still opens and [`IndexDb::memory_detached`] reports `true`
    /// instead of the whole open failing with `Locked`. Only a lock on the view itself fails.
    pub fn open_tolerating_held_memory(view_dir: &Path, memory_dir: &Path) -> Result<Self, IndexError> {
        Self::open_inner(view_dir, memory_dir, true)
    }

    /// Whether the memory keyspaces are an empty placeholder because another process owns the group
    /// store. Memory operations on a detached handle must be refused or forwarded, never served.
    pub fn memory_detached(&self) -> bool {
        self.memory_detached
    }

    /// True when this open discarded an index written under another schema revision; the caller
    /// must rebuild it from a full scan.
    pub fn wiped_on_open(&self) -> bool {
        self.wiped_on_open
    }

    fn open_inner(view_dir: &Path, memory_dir: &Path, tolerate_held_memory: bool) -> Result<Self, IndexError> {
        let dir = view_dir.join(INDEX_DIR);
        std::fs::create_dir_all(&dir).map_err(|source| IndexError::Io {
            path: dir.clone(),
            source,
        })?;
        let cache_bytes = index_cache_bytes(&dir);
        let journal_bytes = if is_empty_dir(&dir) {
            FJALL_BULK_JOURNAL_BYTES
        } else {
            FJALL_MAX_JOURNAL_BYTES
        };
        let mut db = Database::builder(&dir)
            .cache_size(cache_bytes)
            .worker_threads(FJALL_WORKER_THREADS)
            .max_journaling_size(journal_bytes)
            .open()?;
        let (memory, memory_detached) = match memory_store::open_shared(memory_dir) {
            Ok(memory) => (memory, false),
            Err(IndexError::Fjall(fjall::Error::Locked)) if tolerate_held_memory => {
                tracing::debug!(
                    memory_dir = %memory_dir.display(),
                    "group memory store is held by another process; memory is unavailable locally"
                );
                (memory_store::open_detached(view_dir)?, true)
            }
            Err(error) => return Err(error),
        };
        if !memory_detached {
            memory_store::migrate_legacy(&db, &memory)?;
        }
        let mut meta = open_keyspace(&db, "meta")?;
        let on_disk_ver = meta
            .get(META_SCHEMA_VER)?
            .and_then(|bytes| <[u8; 4]>::try_from(&bytes[..]).ok())
            .map(u32::from_be_bytes);
        let wiped_on_open = matches!(on_disk_ver, Some(ver) if ver != INDEX_SCHEMA_VER);
        if wiped_on_open {
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
            db = Database::builder(&dir)
                .cache_size(cache_bytes)
                .worker_threads(FJALL_WORKER_THREADS)
                .max_journaling_size(FJALL_BULK_JOURNAL_BYTES)
                .open()?;
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
        let import_targets = open_keyspace(&db, "import_targets")?;
        let importers_by_probe = open_keyspace(&db, "importers_by_probe")?;
        let code_bm25_postings = open_keyspace(&db, "code_bm25_postings")?;
        let code_bm25_by_path = open_keyspace(&db, "code_bm25_by_path")?;
        let grep_bloom = open_keyspace(&db, "grep_bloom")?;
        let embeddings = open_keyspace(&db, "embeddings")?;
        let memory_by_key = memory.memory_by_key.clone();
        let memory_archive = memory.memory_archive.clone();
        let proposals = memory.proposals.clone();

        meta.insert(META_SCHEMA_VER, INDEX_SCHEMA_VER.to_be_bytes())?;

        if db.journal_disk_space().unwrap_or(0) > JOURNAL_FLUSH_ON_OPEN_BYTES {
            for keyspace in [
                &symbols_by_path,
                &symbols_by_name,
                &calls_by_path,
                &calls_by_callee,
                &imports_by_module,
                &imports_by_path,
                &implementations_by_trait,
                &implementations_by_path,
                &refs_by_def,
                &refs_by_path,
                &import_targets,
                &importers_by_probe,
                &code_bm25_postings,
                &code_bm25_by_path,
                &grep_bloom,
                &embeddings,
                &meta,
            ] {
                // Best-effort: the flush only spares the NEXT open a replay. On a full or read-only disk
                // it fails, and that must not make the workspace unopenable.
                if let Err(error) = keyspace.rotate_memtable_and_wait() {
                    tracing::warn!(%error, "index journal flush on open failed; continuing with the replayed journal");
                    break;
                }
            }
        }

        Ok(Self {
            db,
            meta,
            symbols_by_path,
            symbols_by_name,
            calls_by_path,
            calls_by_callee,
            callee_names: std::sync::Arc::default(),
            imports_by_module,
            imports_by_path,
            implementations_by_trait,
            trait_names: std::sync::Arc::default(),
            implementations_by_path,
            refs_by_def,
            refs_by_path,
            import_targets,
            importers_by_probe,
            code_bm25_postings,
            code_bm25_by_path,
            grep_bloom,
            embeddings,
            memory_by_key,
            memory_archive,
            proposals,
            fault: std::sync::Arc::default(),
            _memory: memory,
            memory_detached,
            wiped_on_open,
        })
    }

    /// The first fatal commit failure (`StorageFull`, `Poisoned`) any writer on this handle hit, or
    /// `None` while it is healthy. A scan that sees `Some` must abort: every later commit fails too.
    pub fn fatal_fault(&self) -> Option<String> {
        self.fault.reason()
    }

    /// Allocation-free form of `fatal_fault().is_some()` for per-file polling.
    pub fn has_fatal_fault(&self) -> bool {
        self.fault.is_tripped()
    }

    /// Whether the handle is poisoned, which only dropping and reopening the database clears. A
    /// full disk is fatal to a scan but leaves the handle usable once space is freed.
    pub fn is_poisoned(&self) -> bool {
        self.fault.is_poisoned()
    }

    /// Drop a latched failure that freeing disk space cures, so a long-lived handle (the daemon's)
    /// is not refused forever after one full-disk scan. A poisoned handle stays poisoned.
    pub fn clear_recoverable_fault(&self) {
        self.fault.clear_recoverable();
    }

    /// Latch `error` if it is fatal. Called by [`writer::IndexWriter::commit`]; public so a commit
    /// path that bypasses the writer can report through the same latch.
    pub fn record_commit_error(&self, error: &IndexError) {
        self.fault.record(error);
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

    /// Whether the probe keyspaces cover every importer (a wholesale resolve pass completed since
    /// they were created). When false, the incremental pass must run the wholesale one first.
    pub fn import_probes_complete(&self) -> bool {
        self.meta
            .get(META_IMPORT_PROBE_VER)
            .ok()
            .flatten()
            .is_some_and(|v| v.first() == Some(&IMPORT_PROBE_VER))
    }

    /// Mark the probe keyspaces complete (`true`) or in flux (`false`). The wholesale pass clears
    /// the stamp before it rewrites any importer and sets it only after the stitch committed.
    pub fn set_import_probes_complete(&self, complete: bool) -> Result<(), IndexError> {
        if complete {
            self.meta.insert(META_IMPORT_PROBE_VER, [IMPORT_PROBE_VER])?;
        } else {
            self.meta.remove(META_IMPORT_PROBE_VER)?;
        }
        Ok(())
    }

    /// Every importer whose resolution consulted `path` (repo-relative, forward-slashed) or one of
    /// its ancestor directories: a prefix scan of `importers_by_probe` per candidate, O(importers).
    ///
    /// The ancestors matter because the resolver does not descend into a directory it found
    /// missing. An importer that probed `util` and found nothing never probed `util/index.ts`, so
    /// adding `util/index.ts` must reach it through the directory. (A directory that existed when
    /// the importer resolved is never recorded, so a path under an existing directory costs only
    /// empty scans.)
    pub fn importers_probing(&self, path: &str) -> Vec<crate::path::RelPath> {
        probe_candidates(path)
            .flat_map(|candidate| {
                let prefix = keys::importers_by_probe_prefix(&keys::probe_hash(candidate));
                self.importers_by_probe
                    .prefix(prefix)
                    .filter_map(|guard| guard.into_inner().ok())
                    .filter_map(|(k, _)| keys::parse_importer_by_probe(&k))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// Whether any importer consulted `path` or an ancestor directory of it (see
    /// [`Self::importers_probing`]). Cheap enough to gate a rescan on a non-indexed file
    /// (`tsconfig.json`, `package.json`) the watcher saw change.
    pub fn is_probed(&self, path: &str) -> bool {
        probe_candidates(path).any(|candidate| {
            let prefix = keys::importers_by_probe_prefix(&keys::probe_hash(candidate));
            self.importers_by_probe.prefix(prefix).next().is_some()
        })
    }

    /// Every row of the four resolution keyspaces (`refs_by_def`, `refs_by_path`, `import_targets`,
    /// `importers_by_probe`) as `(keyspace, key, value)`, in keyspace then key order. Lets an
    /// equivalence test compare an incremental rescan with a full one row for row.
    #[doc(hidden)]
    pub fn resolution_rows(&self) -> Result<Vec<ResolutionRow>, IndexError> {
        let mut rows = Vec::new();
        for (name, keyspace) in [
            ("refs_by_def", &self.refs_by_def),
            ("refs_by_path", &self.refs_by_path),
            ("import_targets", &self.import_targets),
            ("importers_by_probe", &self.importers_by_probe),
        ] {
            for guard in keyspace.iter() {
                let (k, v) = guard.into_inner()?;
                rows.push((name, k.to_vec(), v.to_vec()));
            }
        }
        Ok(rows)
    }

    /// The stitched `(local_start, def_path, def_start)` bindings of `importer`.
    pub fn import_targets_of(&self, importer: &crate::path::RelPath) -> Vec<(u32, crate::path::RelPath, u32)> {
        self.import_targets
            .prefix(keys::import_targets_prefix(importer))
            .filter_map(|guard| guard.into_inner().ok())
            .filter_map(|(k, v)| keys::parse_import_target(&k, &v))
            .map(|(_, local_start, def_path, def_start)| (local_start, def_path, def_start))
            .collect()
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

    /// fjall has no getter for the compression policy, so observe it on disk: highly compressible
    /// rows flushed to L0 (which the default policy leaves raw) must occupy far less than their
    /// raw size, and must still read back after a reopen.
    #[test]
    fn index_keyspaces_compress_level_zero_and_round_trip() {
        const ROWS: usize = 512;
        const VALUE_LEN: usize = 4_096;
        let dir = tempfile::tempdir().unwrap();
        {
            let db = IndexDb::open(dir.path()).unwrap();
            for ks in [&db.code_bm25_postings, &db.refs_by_def, &db.proposals] {
                for i in 0..ROWS {
                    ks.insert(i.to_be_bytes(), vec![7u8; VALUE_LEN]).unwrap();
                }
                ks.rotate_memtable_and_wait().unwrap();
                let raw = (ROWS * VALUE_LEN) as u64;
                assert!(
                    ks.disk_space() < raw / 4,
                    "L0 table of {raw} raw bytes occupies {} on disk; LZ4 is not applied",
                    ks.disk_space()
                );
            }
        }
        let db = IndexDb::open(dir.path()).unwrap();
        let row = db
            .code_bm25_postings
            .get(7usize.to_be_bytes())
            .unwrap()
            .expect("row survives reopen");
        assert_eq!(&*row, vec![7u8; VALUE_LEN].as_slice());
    }

    #[test]
    fn probe_candidates_walk_up_the_ancestor_directories() {
        assert_eq!(
            probe_candidates("a/b/c.ts").collect::<Vec<_>>(),
            vec!["a/b/c.ts", "a/b", "a"]
        );
        assert_eq!(
            probe_candidates("tsconfig.json").collect::<Vec<_>>(),
            vec!["tsconfig.json"]
        );
    }

    #[test]
    fn keyspace_memtable_table_covers_every_partition() {
        let dir = tempfile::tempdir().unwrap();
        let db = IndexDb::open(dir.path()).unwrap();
        let mut opened: Vec<String> = db.db.list_keyspace_names().iter().map(ToString::to_string).collect();
        opened.extend(db._memory.keyspace_names());
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
