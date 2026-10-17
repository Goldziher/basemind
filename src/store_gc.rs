//! Cache garbage-collection + cleanup for the `.basemind/` directory.
//!
//! Two responsibilities:
//!
//! 1. **Mark-and-sweep GC of the shared blob store** ([`run_gc`]). Blobs under
//!    `.basemind/blobs/` are content-addressed and shared across every view; a blob is
//!    *live* iff some view's `index.msgpack` still references its content hash. Re-scans
//!    and branch switches leave behind blobs no view points at anymore — this reclaims them.
//! 2. **Whole-component cleanup** ([`clear_component`]) and **introspection**
//!    ([`cache_stats`]) for the CLI / MCP admin surface (wired up by separate workstreams).
//!
//! ## Why a single content hash addresses both blob suffixes
//!
//! Each [`crate::store::FileEntry`] carries exactly one `hash_hex` — the content hash of the
//! source file. The scanner writes up to two blobs for that file, both keyed by the *same*
//! hash with different suffixes: `<hash>.fm.msgpack` (the combined L1 + L2 filemap) and
//! (documents build) `<hash>.doc.msgpack`. So the set of live blob stems is exactly the set
//! of `hash_hex` values across all entries of all views — there is no separate `fm_hash` /
//! `doc_hash` to union.

use std::path::Path;
use std::time::{Duration, SystemTime};

use serde::Serialize;
use thiserror::Error;

use crate::store::{
    CACHE_DIR, INDEX_FILE, StoreError, VIEWS_DIR, WORKSPACES_DIR, cache_root, global_blobs_dir, read_index,
};
pub use crate::store_gc_live::LiveBlobs;
use crate::store_gc_live::{SweepLock, collect_workspace_hashes_healing_stale_views};

/// The blob filename suffixes the scanner emits today, all keyed by one content hash.
/// Used to strip the suffix off a blob filename to recover its hex stem. The four suffixes are
/// `.fm.msgpack` (combined L1 + L2 filemap), `.doc.msgpack` (documents tier), `.chunk.msgpack`
/// (code-search tier), and `.rref.msgpack` (code-intel resolved-references tier). All share the
/// same source-hash stem as the `.fm` blob, so they are reclaimed together when the source file
/// changes or is deleted (its stem drops out of the live set).
const BLOB_SUFFIXES: [&str; 4] = [".fm.msgpack", ".doc.msgpack", ".chunk.msgpack", ".rref.msgpack"];

/// Pre-0.9 split-tier blob suffixes (`<hash>.l1.msgpack` / `<hash>.l2.msgpack`), superseded by
/// the combined `.fm.msgpack` frame. No current code writes or reads these, so any left on disk
/// after a schema-bump refresh are dead format — the sweep deletes them on sight regardless of
/// whether their stem is still referenced (the live `.fm` blob shares that stem).
const LEGACY_BLOB_SUFFIXES: [&str; 2] = [".l1.msgpack", ".l2.msgpack"];

/// Grace window the blob sweep grants young blobs: an unreferenced blob whose mtime is younger
/// than this is kept. Content-addressed blobs are legitimately *entry-less* for a while — a
/// Deferred pass writes doc blobs before any `DocEntry` exists, and a NoCache rename's remove
/// half drops the tracking entry moments before the create half re-references the same hash
/// (issue #44). Reaping inside that window forces a full re-extract + re-embed on the next
/// encounter; the grace costs only delayed reclamation.
const BLOB_GC_GRACE: Duration = Duration::from_secs(6 * 60 * 60);

/// Suffix of the temp files blob writers stage before an atomic rename
/// (`<stem>.rref.msgpack.<pid>.ThreadId(N).tmp`). A writer that died leaves its file behind.
const TMP_SUFFIX: &str = ".tmp";

/// Env override for [`BLOB_GC_GRACE`], in whole seconds (`0` reclaims every unreferenced blob at
/// once). Meant for operators clearing a known-dead cache and for the upgrade regression tests; the
/// default protects entry-less blobs that a concurrent scan is about to reference.
pub const BLOB_GC_GRACE_ENV: &str = "BASEMIND_BLOB_GC_GRACE_SECS";

/// The grace the destructive sweeps apply: [`BLOB_GC_GRACE_ENV`] when set to a number, else
/// [`BLOB_GC_GRACE`].
fn blob_gc_grace() -> Duration {
    grace_from(std::env::var(BLOB_GC_GRACE_ENV).ok().as_deref())
}

fn grace_from(raw: Option<&str>) -> Duration {
    raw.and_then(|raw| raw.trim().parse::<u64>().ok())
        .map_or(BLOB_GC_GRACE, Duration::from_secs)
}

/// The orphaned-workspace reaper — the other half of keeping the machine-global cache bounded.
/// Lives in its own module (like `store_lock.rs`) to keep this file under the module size cap;
/// re-exported here so callers see one GC surface.
pub use crate::store_gc_workspace::{ReapReport, reap_orphaned_workspaces};

/// Cache size-budget enforcement + persisted last-GC state — the third leg of keeping the
/// machine-global cache bounded (and observable). Re-exported so callers see one GC surface.
pub use crate::store_gc_budget::{
    GcState, GcStatus, cache_budget_bytes, enforce_cache_budget, persist_gc_error, persist_gc_state, read_gc_state,
};

/// Whole-component cleanup + cache introspection — responsibility (2) in the module doc above.
/// Lives in its own module to keep this file under the module size cap; re-exported here so
/// callers keep importing the whole cache surface from `crate::store_gc`.
pub(crate) use crate::store_cache_admin::dir_size;
pub use crate::store_cache_admin::{CacheComponent, CacheStats, cache_stats, clear_component, clear_single_view};

/// Errors raised by the cache GC + cleanup layer. Wraps [`StoreError`] for the shared
/// blob/index machinery and adds a thin I/O variant for the directory walks this module
/// performs directly.
#[derive(Debug, Error)]
pub enum GcError {
    /// An underlying store operation failed (index read, blob wipe, lock acquisition).
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A filesystem operation in the GC walk failed, annotated with the offending path.
    #[error("io error on {path}: {source}")]
    Io {
        /// The path the failing operation targeted.
        path: std::path::PathBuf,
        /// The underlying OS error.
        #[source]
        source: std::io::Error,
    },
    /// The blocking GC task panicked or was cancelled before returning a report.
    #[error("blob GC task failed to join: {0}")]
    Join(String),
    /// The sweep could not acquire the blob-GC write lock within its bound — a rescan held the
    /// store the whole time. The cycle is skipped (and retried on the next tick) instead of
    /// parking the maintenance task forever, which is how the starved-GC incident stayed
    /// invisible while the cache grew unbounded.
    #[error("blob GC starved: a rescan held the store lock beyond {0:?}; skipping this cycle")]
    Starved(Duration),
}

/// Result of a blob garbage-collection sweep.
#[derive(Debug, Clone, Default, Serialize)]
pub struct GcReport {
    /// Total blob files inspected.
    pub scanned: usize,
    /// Orphan blob files removed.
    pub removed: usize,
    /// Bytes reclaimed by the removals (stat'd before deletion).
    pub bytes_freed: u64,
    /// Orphaned workspace cache dirs reaped by the same sweep (see [`reap_orphaned_workspaces`]).
    /// Additive field: `0` on every path that only sweeps blobs ([`gc_blobs`], [`gc_report_only`]).
    #[serde(default)]
    pub workspaces_reaped: usize,
    /// Bytes reclaimed by reaping those workspace dirs (their `views/` trees). Disjoint from
    /// [`Self::bytes_freed`], which counts only global-blob bytes.
    #[serde(default)]
    pub workspace_bytes_freed: u64,
    /// Cold workspace dirs evicted by cache-budget enforcement (see
    /// [`crate::store_gc_budget::enforce_cache_budget`]). `0` on every path that does not
    /// enforce the budget.
    #[serde(default)]
    pub workspaces_evicted: usize,
    /// Bytes reclaimed by those evictions. Disjoint from the other byte counters.
    #[serde(default)]
    pub evicted_bytes_freed: u64,
    /// Cache budget applied by the daemon maintenance sweep.
    #[serde(default)]
    pub cache_budget_bytes: Option<u64>,
    /// Measured cache footprint after budget enforcement.
    #[serde(default)]
    pub cache_bytes_after: Option<u64>,
    /// Hot workspaces evicted after cold candidates were exhausted.
    #[serde(default)]
    pub hot_workspaces_evicted: usize,
    /// Workspaces skipped by budget enforcement because they were locked.
    #[serde(default)]
    pub locked_workspaces_skipped: usize,
    /// Workspaces budget enforcement refused to evict (locked, most recently used, or inside the
    /// post-build grace window) while the cache stayed over budget.
    #[serde(default)]
    pub protected_workspaces: Vec<crate::store_gc_protect::ProtectedWorkspace>,
}

/// Enumerate every view's `index.msgpack` and union the hex content hashes it references.
///
/// A blob is live iff *any* view points at its content hash, so the union across all views
/// is the complete live set; the returned stems compare directly against on-disk blob
/// filenames (which are `<hex-stem>.{l1,l2,doc}.msgpack`).
///
/// ## Safety of the unreadable-view case
///
/// A view directory that simply has no `index.msgpack` yet (`read_index` returns
/// `Ok(None)`) contributes nothing and is skipped — it genuinely references no blobs.
///
/// Any *other* read failure (corrupt msgpack, schema mismatch, I/O error) is treated as a
/// hard error and propagated. Silently skipping such a view would drop its live hashes from
/// the union and cause the subsequent sweep to delete blobs that are in fact still
/// referenced — orphaning the entire store. Refusing to sweep when the live set might be
/// incomplete is the safe failure mode: the caller surfaces the error and the operator can
/// re-scan to rebuild the offending view's index before retrying GC.
pub fn collect_referenced_hashes(basemind_dir: &Path) -> Result<LiveBlobs, GcError> {
    let mut referenced = LiveBlobs::default();
    let views_dir = basemind_dir.join(VIEWS_DIR);
    if !views_dir.exists() {
        return Ok(referenced);
    }
    for entry in read_dir(&views_dir)? {
        let entry = entry.map_err(|source| GcError::Io {
            path: views_dir.clone(),
            source,
        })?;
        let view_dir = entry.path();
        if !view_dir.is_dir() {
            continue;
        }
        if !view_dir.join(INDEX_FILE).exists() {
            tracing::warn!(view = %view_dir.display(), "view has no index.msgpack; skipping");
            continue;
        }
        let index = match read_index(&view_dir) {
            Ok(Some(idx)) => idx,
            Ok(None) => continue,
            Err(e) => return Err(GcError::Store(e)),
        };
        for entry in index.files.values() {
            referenced.code.insert(entry.hash_hex.clone());
        }
        for entry in index.doc_files.values() {
            referenced.docs.insert(entry.hash_hex.clone());
        }
    }
    Ok(referenced)
}

/// Sweep the GLOBAL blob store, deleting every blob whose hex stem is not in `referenced`.
///
/// Files that do not match a known blob suffix are inspected (counted in `scanned`) but
/// never deleted — a conservative choice so a stray file under `blobs/` is never reaped.
///
/// NOTE: the blob store is machine-global now, so `referenced` MUST be the union across every
/// workspace that could reference a blob. A single-workspace reference set would orphan (and
/// delete) blobs other workspaces still need — which is why the standalone in-process auto-GC is
/// disabled (`Store::blobs_shared == true`) and cross-workspace reference-counted GC is deferred
/// to the daemon.
pub fn gc_blobs(referenced: &LiveBlobs) -> Result<GcReport, GcError> {
    gc_blobs_in(&global_blobs_dir(), referenced, blob_gc_grace())
}

/// Sweep an explicit blob directory. The seam production reaches via [`gc_blobs`] (passing the
/// global store and [`BLOB_GC_GRACE`]) and unit tests reach with a per-test temp dir (usually
/// `Duration::ZERO` grace), so tests never touch — nor race on — the machine-global blob store
/// or each other. Unreferenced blobs younger than `grace` are kept (see [`BLOB_GC_GRACE`]);
/// legacy split-tier blobs are dead format and reaped regardless of age.
fn gc_blobs_in(blobs_dir: &Path, referenced: &LiveBlobs, grace: Duration) -> Result<GcReport, GcError> {
    let now = SystemTime::now();
    let mut report = GcReport::default();
    if !blobs_dir.exists() {
        return Ok(report);
    }
    for entry in read_dir(blobs_dir)? {
        let entry = entry.map_err(|source| GcError::Io {
            path: blobs_dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if file_name.ends_with(TMP_SUFFIX) {
            report.scanned += 1;
            sweep_stray_tmp(&path, now, grace, &mut report)?;
            continue;
        }
        let is_legacy = LEGACY_BLOB_SUFFIXES.iter().any(|suffix| file_name.ends_with(suffix));
        let Some(_) = blob_stem(file_name) else {
            report.scanned += 1;
            if is_legacy {
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                std::fs::remove_file(&path).map_err(|source| GcError::Io {
                    path: path.clone(),
                    source,
                })?;
                report.removed += 1;
                report.bytes_freed += size;
            }
            continue;
        };
        report.scanned += 1;
        if referenced.keeps(file_name) {
            continue;
        }
        let meta = std::fs::metadata(&path).map_err(|source| GcError::Io {
            path: path.clone(),
            source,
        })?;
        let age = meta
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .unwrap_or(Duration::ZERO);
        if age < grace {
            continue;
        }
        let size = meta.len();
        std::fs::remove_file(&path).map_err(|source| GcError::Io {
            path: path.clone(),
            source,
        })?;
        report.removed += 1;
        report.bytes_freed += size;
    }
    Ok(report)
}

/// Remove `path`, a stray `*.tmp`, when it is older than the grace window.
///
/// Age is the only test: a tmp file is never "referenced", and one a live writer is still filling
/// has a fresh mtime. The window is never shorter than [`BLOB_GC_GRACE`] even when the caller
/// passes a zero grace (the budget sweep does), because a zero window would reap an in-flight write.
fn sweep_stray_tmp(path: &Path, now: SystemTime, grace: Duration, report: &mut GcReport) -> Result<(), GcError> {
    let io = |source| GcError::Io {
        path: path.to_path_buf(),
        source,
    };
    let meta = std::fs::metadata(path).map_err(io)?;
    let age = meta
        .modified()
        .ok()
        .and_then(|modified| now.duration_since(modified).ok())
        .unwrap_or(Duration::ZERO);
    if age < grace.max(BLOB_GC_GRACE) {
        return Ok(());
    }
    std::fs::remove_file(path).map_err(io)?;
    report.removed += 1;
    report.bytes_freed += meta.len();
    Ok(())
}

/// Union the referenced blob stems across EVERY workspace index under the machine-global cache.
///
/// The blob store is content-addressed and shared by every workspace, so a blob is live iff *any*
/// workspace references it. This enumerates `cache_root()/cache/workspaces/<key>/` and unions
/// [`collect_referenced_hashes`] over each — the complete cross-workspace live set the daemon (the
/// sole fjall writer, which alone sees every workspace) sweeps against. Schema-mismatched or corrupt
/// view caches are removed under the workspace lock and rebuilt lazily; transient I/O and lock
/// failures still abort the sweep so an incomplete live set never drives a delete.
pub fn collect_referenced_hashes_global() -> Result<LiveBlobs, GcError> {
    collect_referenced_hashes_global_in(&cache_root().join(CACHE_DIR).join(WORKSPACES_DIR))
}

/// [`collect_referenced_hashes_global`] against an explicit workspaces directory. Production passes
/// the global `cache/workspaces`; unit tests pass a per-test temp dir so they never read (nor race
/// on) the machine-global cache.
pub(crate) fn collect_referenced_hashes_global_in(workspaces_dir: &Path) -> Result<LiveBlobs, GcError> {
    let mut referenced = LiveBlobs::default();
    if !workspaces_dir.exists() {
        return Ok(referenced);
    }
    for entry in read_dir(workspaces_dir)? {
        let entry = entry.map_err(|source| GcError::Io {
            path: workspaces_dir.to_path_buf(),
            source,
        })?;
        let workspace_dir = entry.path();
        if !workspace_dir.is_dir() {
            continue;
        }
        referenced.extend(collect_workspace_hashes_healing_stale_views(&workspace_dir)?);
    }
    Ok(referenced)
}

/// Cross-workspace reference-counted GC over the machine-global blob store: reference-count against
/// EVERY workspace and reap blobs no workspace points at. This is the destructive counterpart to
/// [`gc_report_only`] — safe ONLY because the daemon (the sole writer) is the single caller that can
/// enumerate every workspace's references at once. Returns the sweep report.
pub fn gc_global_blobs() -> Result<GcReport, GcError> {
    let cache = cache_root().join(CACHE_DIR);
    // One destructive sweep at a time machine-wide: the daemon's maintenance tick and a CLI scan's
    // post-migration cleanup both build a whole-cache live set, so running two at once only doubles
    // the transient memory. A contended sweep is skipped (the holder is doing the same work).
    let Some(_sweep) = SweepLock::try_acquire(&cache) else {
        tracing::debug!("global blob sweep already running in another process; skipping");
        return Ok(GcReport::default());
    };
    gc_global_blobs_in(&cache.join(WORKSPACES_DIR), &global_blobs_dir(), blob_gc_grace())
}

/// The cleanup a scan runs for itself when it displaced artifacts (see
/// [`crate::scanner::ScanStats::displaced_artifacts`]): reference-count against every workspace and
/// reap the blobs the pass orphaned (the `.fm` / `.chunk` / `.rref` blobs of paths that moved to the
/// document tier, the pre-epoch blobs a re-extraction replaced), then record the outcome where
/// `admin cache_stats` reports it. The same bounded, grace-protected sweep the daemon runs, so it is
/// safe against other workspaces' references and in-flight writes; unlike the daemon's it does not
/// reap workspaces or enforce the size budget.
pub fn reclaim_displaced_blobs() -> Result<GcReport, GcError> {
    let report = gc_global_blobs()?;
    persist_gc_state(&report);
    Ok(report)
}

/// [`gc_global_blobs`] against explicit workspaces + blobs directories, so tests reference-count and
/// sweep a per-fixture cache instead of the machine-global store. `grace` mirrors [`gc_blobs_in`]'s:
/// production passes [`BLOB_GC_GRACE`], tests usually `Duration::ZERO`.
pub(crate) fn gc_global_blobs_in(
    workspaces_dir: &Path,
    blobs_dir: &Path,
    grace: Duration,
) -> Result<GcReport, GcError> {
    let referenced = collect_referenced_hashes_global_in(workspaces_dir)?;
    gc_blobs_in(blobs_dir, &referenced, grace)
}

/// The daemon's full cache sweep: reap orphaned workspace cache dirs FIRST, then reference-count and
/// sweep the global blob store.
///
/// Order is load-bearing. An orphaned workspace (its worktree deleted) still votes in the blob GC's
/// cross-workspace live set, so every blob it references is pinned in the machine-global store
/// forever — the cache can only grow. Reaping first drops those votes, so the very same sweep
/// reclaims the blobs the orphan was pinning. The returned [`GcReport`] carries both halves.
pub fn reap_and_gc_global() -> Result<GcReport, GcError> {
    let reaped = reap_orphaned_workspaces()?;
    let mut report = gc_global_blobs()?;
    report.workspaces_reaped = reaped.reaped;
    report.workspace_bytes_freed = reaped.bytes_freed;
    Ok(report)
}

/// The daemon's complete maintenance sweep: [`reap_and_gc_global`], then cache-budget
/// enforcement (preferring cold workspaces and reclaiming their orphaned blobs in the same pass),
/// and finally persistence of the sweep's health and budget outcome to `gc-state.json`.
pub fn reap_gc_and_enforce_budget() -> Result<GcReport, GcError> {
    let mut report = reap_and_gc_global()?;
    if let Some(budget) = cache_budget_bytes() {
        let evicted = enforce_cache_budget(budget)?;
        report.cache_budget_bytes = Some(budget);
        report.cache_bytes_after = Some(evicted.total_bytes_after);
        report.hot_workspaces_evicted = evicted.hot_evicted;
        report.locked_workspaces_skipped = evicted.locked_skipped;
        report.protected_workspaces = evicted.protected;
        if evicted.evicted > 0 {
            report.workspaces_evicted = evicted.evicted;
            report.evicted_bytes_freed = evicted.bytes_freed;
            report.removed += evicted.blobs_removed;
            report.bytes_freed += evicted.blob_bytes_freed;
        }
    }
    persist_gc_state(&report);
    Ok(report)
}

/// Blob GC entry point for the offline CLI `cache gc`: the cross-workspace reference-counted sweep
/// ([`gc_global_blobs`]). It reference-counts against EVERY workspace's indexes (not just
/// `basemind_dir`'s), so it cannot reap a blob another workspace still needs, honours the same
/// young-blob grace and machine-wide sweep lock as the daemon's, and reports what it removed.
/// `basemind_dir` is accepted for the call-site shape only: the sweep is machine-wide.
pub fn run_gc(basemind_dir: &Path) -> Result<GcReport, GcError> {
    let _ = basemind_dir;
    let report = gc_global_blobs()?;
    persist_gc_state(&report);
    Ok(report)
}

/// Non-destructive GC report over the GLOBAL blob store: counts every blob file (`scanned`) and
/// removes nothing. This is the safe standalone behavior while the blob store is machine-global —
/// see [`run_gc`]. `bytes_freed` / `removed` are always `0`.
pub fn gc_report_only() -> Result<GcReport, GcError> {
    let blobs_dir = global_blobs_dir();
    let mut report = GcReport::default();
    if !blobs_dir.exists() {
        return Ok(report);
    }
    for entry in read_dir(&blobs_dir)? {
        let entry = entry.map_err(|source| GcError::Io {
            path: blobs_dir.clone(),
            source,
        })?;
        if entry.path().is_file() {
            report.scanned += 1;
        }
    }
    Ok(report)
}

/// Strip a known blob suffix off a filename, returning the hex stem. `None` if the filename
/// is not a recognized blob (so the caller never treats stray files as reclaimable).
/// `pub(crate)` because the orphan accounting in [`crate::store_cache_admin::cache_stats`] reads
/// the same blob names.
pub(crate) fn blob_stem(file_name: &str) -> Option<&str> {
    BLOB_SUFFIXES.iter().find_map(|suffix| file_name.strip_suffix(suffix))
}

/// Directory read that annotates the failing path with a [`GcError::Io`]. `pub(crate)` because
/// every cache walk — sweep and stats alike — funnels through it.
pub(crate) fn read_dir(dir: &Path) -> Result<std::fs::ReadDir, GcError> {
    std::fs::read_dir(dir).map_err(|source| GcError::Io {
        path: dir.to_path_buf(),
        source,
    })
}

#[cfg(test)]
#[path = "store_gc_tier_tests.rs"]
mod tier_tests;

#[cfg(test)]
#[path = "store_gc_tests.rs"]
mod tests;
