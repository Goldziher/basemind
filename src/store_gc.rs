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

use ahash::AHashSet;
use serde::Serialize;
use thiserror::Error;

use crate::store::{
    CACHE_DIR, INDEX_FILE, StoreError, VIEWS_DIR, WORKSPACES_DIR, acquire_lock, cache_root, global_blobs_dir,
    read_index,
};

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

/// The orphaned-workspace reaper — the other half of keeping the machine-global cache bounded.
/// Lives in its own module (like `store_lock.rs`) to keep this file under the module size cap;
/// re-exported here so callers see one GC surface.
pub use crate::store_gc_workspace::{ReapReport, reap_orphaned_workspaces};

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
pub fn collect_referenced_hashes(basemind_dir: &Path) -> Result<AHashSet<String>, GcError> {
    let mut referenced = AHashSet::new();
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
            referenced.insert(entry.hash_hex.clone());
        }
        for entry in index.doc_files.values() {
            referenced.insert(entry.hash_hex.clone());
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
pub fn gc_blobs(referenced: &AHashSet<String>) -> Result<GcReport, GcError> {
    gc_blobs_in(&global_blobs_dir(), referenced)
}

/// Sweep an explicit blob directory. The seam production reaches via [`gc_blobs`] (passing the
/// global store) and unit tests reach with a per-test temp dir, so tests never touch — nor race
/// on — the machine-global blob store or each other.
fn gc_blobs_in(blobs_dir: &Path, referenced: &AHashSet<String>) -> Result<GcReport, GcError> {
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
        let is_legacy = LEGACY_BLOB_SUFFIXES.iter().any(|suffix| file_name.ends_with(suffix));
        let Some(stem) = blob_stem(file_name) else {
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
        if referenced.contains(stem) {
            continue;
        }
        let size = std::fs::metadata(&path)
            .map_err(|source| GcError::Io {
                path: path.clone(),
                source,
            })?
            .len();
        std::fs::remove_file(&path).map_err(|source| GcError::Io {
            path: path.clone(),
            source,
        })?;
        report.removed += 1;
        report.bytes_freed += size;
    }
    Ok(report)
}

/// Union the referenced blob stems across EVERY workspace index under the machine-global cache.
///
/// The blob store is content-addressed and shared by every workspace, so a blob is live iff *any*
/// workspace references it. This enumerates `cache_root()/cache/workspaces/<key>/` and unions
/// [`collect_referenced_hashes`] over each — the complete cross-workspace live set the daemon (the
/// sole fjall writer, which alone sees every workspace) sweeps against. An unreadable workspace
/// index propagates the error exactly like the single-workspace collector: an incomplete live set
/// must never drive a delete.
pub fn collect_referenced_hashes_global() -> Result<AHashSet<String>, GcError> {
    collect_referenced_hashes_global_in(&cache_root().join(CACHE_DIR).join(WORKSPACES_DIR))
}

/// [`collect_referenced_hashes_global`] against an explicit workspaces directory. Production passes
/// the global `cache/workspaces`; unit tests pass a per-test temp dir so they never read (nor race
/// on) the machine-global cache.
pub(crate) fn collect_referenced_hashes_global_in(workspaces_dir: &Path) -> Result<AHashSet<String>, GcError> {
    let mut referenced = AHashSet::new();
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
        referenced.extend(collect_referenced_hashes(&workspace_dir)?);
    }
    Ok(referenced)
}

/// Cross-workspace reference-counted GC over the machine-global blob store: reference-count against
/// EVERY workspace and reap blobs no workspace points at. This is the destructive counterpart to
/// [`gc_report_only`] — safe ONLY because the daemon (the sole writer) is the single caller that can
/// enumerate every workspace's references at once. Returns the sweep report.
pub fn gc_global_blobs() -> Result<GcReport, GcError> {
    gc_global_blobs_in(&cache_root().join(CACHE_DIR).join(WORKSPACES_DIR), &global_blobs_dir())
}

/// [`gc_global_blobs`] against explicit workspaces + blobs directories, so tests reference-count and
/// sweep a per-fixture cache instead of the machine-global store.
pub(crate) fn gc_global_blobs_in(workspaces_dir: &Path, blobs_dir: &Path) -> Result<GcReport, GcError> {
    let referenced = collect_referenced_hashes_global_in(workspaces_dir)?;
    gc_blobs_in(blobs_dir, &referenced)
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

/// Blob GC entry point for the CLI `cache gc` / a single-workspace caller.
///
/// The blob store is machine-global now (shared by every workspace), so a single caller can only
/// enumerate ONE workspace's references — never the full live set across the machine. A real sweep
/// from here would reap blobs other workspaces still need, so this is a non-destructive report
/// (`removed == 0`) that still inspects the store (`scanned` = current blob count). Cross-workspace
/// reference-counted GC is the daemon's job (Track E). The `basemind_dir` is taken under the
/// store's advisory lock so the report is consistent against a concurrent scan of that workspace.
pub fn run_gc(basemind_dir: &Path) -> Result<GcReport, GcError> {
    let _lock = acquire_lock(basemind_dir)?;
    gc_report_only()
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
mod tests {
    use super::*;
    use crate::store::{BLOBS_DIR, FileEntry, INDEX_FILE, Index};
    use crate::store_cache_admin::{TELEMETRY_FILENAME, cache_stats_in, clear_component_in};
    use std::fs;
    use std::path::PathBuf;

    /// A referenced + an orphan blob, with a hand-written `views/working/index.msgpack`
    /// pointing only at the referenced stem.
    ///
    /// Since the blob store went machine-global, these tests keep their blobs in a *per-fixture*
    /// temp `blobs/` dir under `basemind_dir` and drive GC / stats through the `gc_blobs_in` /
    /// `cache_stats_in` seams — never the real global store. That keeps each test hermetic and
    /// parallel-safe (colliding hex stems across tests can't clobber one another).
    struct Fixture {
        _tmp: tempfile::TempDir,
        basemind_dir: PathBuf,
        /// Per-fixture blob directory (`basemind_dir/blobs`), passed to the `_in` seams.
        blobs_dir: PathBuf,
        referenced_stem: String,
        orphan_stem: String,
        orphan_len: u64,
    }

    fn build_fixture() -> Fixture {
        let tmp = tempfile::tempdir().expect("tempdir");
        let basemind_dir = tmp.path().join(".basemind");
        let blobs = basemind_dir.join(BLOBS_DIR);
        let working = basemind_dir.join(VIEWS_DIR).join("working");
        fs::create_dir_all(&blobs).expect("mk blobs");
        fs::create_dir_all(&working).expect("mk view");

        let referenced_stem = "a".repeat(64);
        let orphan_stem = "b".repeat(64);

        fs::write(blobs.join(format!("{referenced_stem}.fm.msgpack")), b"fm").expect("write ref fm");
        let orphan_bytes = b"orphan-blob-bytes";
        let orphan_len = orphan_bytes.len() as u64;
        fs::write(blobs.join(format!("{orphan_stem}.fm.msgpack")), orphan_bytes).expect("write orphan");

        let mut index = Index::empty();
        index.files.insert(
            crate::path::RelPath::from("src/main.rs"),
            FileEntry {
                hash_hex: referenced_stem.clone(),
                language: "rust".to_string(),
                size_bytes: 2,
                mtime: 0,
            },
        );
        let bytes = rmp_serde::to_vec_named(&index).expect("encode index");
        fs::write(working.join(INDEX_FILE), bytes).expect("write index");

        Fixture {
            _tmp: tmp,
            basemind_dir,
            blobs_dir: blobs,
            referenced_stem,
            orphan_stem,
            orphan_len,
        }
    }

    #[test]
    fn cache_stats_counts_git_history_and_reconciles_total() {
        let fx = build_fixture();

        let gh_dir = fx.basemind_dir.join(crate::git_history::GIT_HISTORY_DIR);
        fs::create_dir_all(&gh_dir).expect("mk git-history");
        let gh_payload = b"git-history-index-bytes-XXXXXXXX";
        fs::write(gh_dir.join("commits.fjall"), gh_payload).expect("write gh blob");

        let stray = b"lockmeta";
        fs::write(fx.basemind_dir.join(".lock.meta"), stray).expect("write stray");

        let stats = cache_stats_in(&fx.basemind_dir, &fx.blobs_dir).expect("cache_stats");

        assert!(
            stats.git_history_bytes >= gh_payload.len() as u64,
            "git-history.fjall/ must be counted (got {})",
            stats.git_history_bytes
        );
        assert!(
            stats.other_bytes >= stray.len() as u64,
            "the unattributed stray file lands in other_bytes (got {})",
            stats.other_bytes
        );

        // Blobs are global now, so the workspace tree still contains a per-fixture `blobs/` here,
        // but `total_bytes` is defined as the workspace tree PLUS the (passed) blob store. With the
        // fixture blobs living under the workspace dir, the global blob bytes are double-counted by
        // construction — assert the definition directly rather than against a bare tree walk.
        assert_eq!(
            stats.total_bytes,
            dir_size(&fx.basemind_dir).expect("dir_size") + stats.blobs_bytes,
            "total_bytes is the workspace tree plus the global blob store"
        );
        let component_sum = stats.blobs_bytes
            + stats.views_bytes
            + stats.lance_bytes
            + stats.git_cache_bytes
            + stats.telemetry_bytes
            + stats.git_history_bytes;
        assert_eq!(
            stats.total_bytes,
            component_sum + stats.other_bytes,
            "components + other must reconcile to total"
        );
    }

    #[test]
    fn cache_stats_degrades_when_index_unreadable() {
        let fx = build_fixture();
        let working = fx.basemind_dir.join(VIEWS_DIR).join("working");
        fs::write(working.join(INDEX_FILE), b"\xff\xff not-msgpack \x00").expect("corrupt index");

        assert!(
            collect_referenced_hashes(&fx.basemind_dir).is_err(),
            "an unreadable index must fail the delete-path safety check"
        );

        let stats = cache_stats_in(&fx.basemind_dir, &fx.blobs_dir).expect("cache_stats must not hard-fail");
        assert!(
            !stats.blob_accounting_ok,
            "orphan accounting must be flagged unavailable"
        );
        assert_eq!(
            stats.orphan_blob_count, 0,
            "orphan count is 0 (skipped), not a real zero"
        );
        assert!(stats.blob_count >= 2, "blob files are still counted by size walk");
        assert!(stats.total_bytes > 0, "sizes are still reported");
        assert_eq!(
            stats.total_bytes,
            dir_size(&fx.basemind_dir).expect("dir_size") + stats.blobs_bytes,
            "total still reconciles to the workspace tree plus the blob store"
        );
    }

    #[test]
    fn should_collect_only_referenced_stem() {
        let fx = build_fixture();
        let referenced = collect_referenced_hashes(&fx.basemind_dir).expect("collect");
        assert_eq!(referenced.len(), 1, "exactly one live stem");
        assert!(referenced.contains(&fx.referenced_stem), "live stem present");
        assert!(
            !referenced.contains(&fx.orphan_stem),
            "orphan stem must not be referenced"
        );
    }

    #[test]
    fn should_remove_only_orphan_blob() {
        let fx = build_fixture();
        let referenced = collect_referenced_hashes(&fx.basemind_dir).expect("collect");
        let report = gc_blobs_in(&fx.blobs_dir, &referenced).expect("gc");

        assert_eq!(report.scanned, 2, "one ref blob + one orphan inspected");
        assert_eq!(report.removed, 1, "only the orphan removed");
        assert_eq!(
            report.bytes_freed, fx.orphan_len,
            "freed bytes equal the orphan's exact length"
        );

        let blobs = fx.basemind_dir.join(BLOBS_DIR);
        assert!(
            blobs.join(format!("{}.fm.msgpack", fx.referenced_stem)).exists(),
            "referenced filemap survives"
        );
        assert!(
            !blobs.join(format!("{}.fm.msgpack", fx.orphan_stem)).exists(),
            "orphan filemap gone"
        );
    }

    #[test]
    fn should_reclaim_legacy_split_tier_blobs_even_when_stem_is_referenced() {
        let fx = build_fixture();
        let blobs = fx.basemind_dir.join(BLOBS_DIR);
        fs::write(blobs.join(format!("{}.l1.msgpack", fx.referenced_stem)), b"legacy-l1").expect("write legacy l1");
        fs::write(blobs.join(format!("{}.l2.msgpack", fx.referenced_stem)), b"legacy-l2").expect("write legacy l2");

        let referenced = collect_referenced_hashes(&fx.basemind_dir).expect("collect");
        assert!(
            referenced.contains(&fx.referenced_stem),
            "stem is referenced by the live index"
        );
        let report = gc_blobs_in(&fx.blobs_dir, &referenced).expect("gc");

        assert_eq!(report.removed, 3, "two legacy split blobs + the orphan filemap");
        assert!(
            !blobs.join(format!("{}.l1.msgpack", fx.referenced_stem)).exists(),
            "legacy l1 reclaimed despite a referenced stem"
        );
        assert!(
            !blobs.join(format!("{}.l2.msgpack", fx.referenced_stem)).exists(),
            "legacy l2 reclaimed despite a referenced stem"
        );
        assert!(
            blobs.join(format!("{}.fm.msgpack", fx.referenced_stem)).exists(),
            "the live combined filemap survives"
        );
    }

    #[test]
    fn should_report_one_orphan_before_gc_and_zero_after() {
        let fx = build_fixture();

        let before = cache_stats_in(&fx.basemind_dir, &fx.blobs_dir).expect("stats before");
        assert_eq!(before.blob_count, 2, "two blob files on disk");
        assert_eq!(before.orphan_blob_count, 1, "one orphan before GC");
        assert_eq!(
            before.per_view_file_count,
            vec![("working".to_string(), 1)],
            "single working view with one indexed file"
        );

        // Mirror `run_gc`'s mark+sweep against the per-fixture blob dir (the global-store equivalent
        // `run_gc` takes the store lock and sweeps `global_blobs_dir()`, which these hermetic tests
        // deliberately avoid).
        let referenced = collect_referenced_hashes(&fx.basemind_dir).expect("collect");
        gc_blobs_in(&fx.blobs_dir, &referenced).expect("gc");

        let after = cache_stats_in(&fx.basemind_dir, &fx.blobs_dir).expect("stats after");
        assert_eq!(after.blob_count, 1, "orphan reaped");
        assert_eq!(after.orphan_blob_count, 0, "no orphans remain");
    }

    #[test]
    fn should_clear_only_blobs_component() {
        let fx = build_fixture();
        fs::write(fx.basemind_dir.join(TELEMETRY_FILENAME), b"{}\n").expect("telemetry");

        clear_component_in(&fx.basemind_dir, CacheComponent::Blobs, &fx.blobs_dir).expect("clear blobs");

        let blobs = &fx.blobs_dir;
        let remaining: Vec<_> = fs::read_dir(blobs)
            .expect("read blobs")
            .filter_map(Result::ok)
            .collect();
        assert!(remaining.is_empty(), "blobs dir emptied: {remaining:?}");
        assert!(blobs.exists(), "blobs dir itself preserved");

        assert!(
            fx.basemind_dir
                .join(VIEWS_DIR)
                .join("working")
                .join(INDEX_FILE)
                .exists(),
            "view index untouched by Blobs clear"
        );
        assert!(
            fx.basemind_dir.join(TELEMETRY_FILENAME).exists(),
            "telemetry untouched by Blobs clear"
        );
    }

    /// Build a fixture with two scanned views (`working` + `rev-abc`), each with a real
    /// `index.msgpack`, sharing the blob store. Returns the basemind dir.
    fn build_two_view_fixture() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let basemind_dir = tmp.path().join(".basemind");
        for view in ["working", "rev-abc"] {
            let view_dir = basemind_dir.join(VIEWS_DIR).join(view);
            fs::create_dir_all(&view_dir).expect("mk view");
            let mut index = Index::empty();
            index.files.insert(
                crate::path::RelPath::from("src/main.rs"),
                FileEntry {
                    hash_hex: "a".repeat(64),
                    language: "rust".to_string(),
                    size_bytes: 2,
                    mtime: 0,
                },
            );
            let bytes = rmp_serde::to_vec_named(&index).expect("encode");
            fs::write(view_dir.join(INDEX_FILE), bytes).expect("write index");
        }
        (tmp, basemind_dir)
    }

    #[test]
    fn should_clear_single_view_and_leave_others_intact() {
        let (_tmp, basemind_dir) = build_two_view_fixture();

        clear_single_view(&basemind_dir, "rev-abc").expect("clear one view");

        assert!(
            !basemind_dir.join(VIEWS_DIR).join("rev-abc").exists(),
            "named view removed"
        );
        assert!(
            basemind_dir.join(VIEWS_DIR).join("working").join(INDEX_FILE).exists(),
            "other view survives single-view clear"
        );
    }

    #[test]
    fn clear_single_view_is_idempotent_for_missing_view() {
        let (_tmp, basemind_dir) = build_two_view_fixture();
        clear_single_view(&basemind_dir, "rev-does-not-exist").expect("missing view is a no-op");
        assert!(basemind_dir.join(VIEWS_DIR).join("working").exists());
        assert!(basemind_dir.join(VIEWS_DIR).join("rev-abc").exists());
    }

    #[test]
    fn clear_single_view_rejects_path_traversal() {
        let (_tmp, basemind_dir) = build_two_view_fixture();
        for bad in ["..", "a/b", "../escape", ""] {
            assert!(
                clear_single_view(&basemind_dir, bad).is_err(),
                "invalid view name {bad:?} must be rejected"
            );
        }
        assert!(basemind_dir.join(VIEWS_DIR).join("working").exists());
    }

    #[test]
    fn blob_stem_recovers_stem_for_every_known_suffix() {
        assert_eq!(blob_stem("deadbeef.fm.msgpack"), Some("deadbeef"));
        assert_eq!(blob_stem("deadbeef.doc.msgpack"), Some("deadbeef"));
        assert_eq!(blob_stem("deadbeef.chunk.msgpack"), Some("deadbeef"));
        assert_eq!(blob_stem("deadbeef.rref.msgpack"), Some("deadbeef"));
        assert_eq!(blob_stem("deadbeef.tmp"), None);
    }

    #[test]
    fn should_reclaim_unreferenced_chunk_and_rref_but_keep_referenced() {
        let fx = build_fixture();
        let blobs = &fx.blobs_dir;

        fs::write(
            blobs.join(format!("{}.chunk.msgpack", fx.referenced_stem)),
            b"ref-chunk",
        )
        .expect("ref chunk");
        fs::write(blobs.join(format!("{}.rref.msgpack", fx.referenced_stem)), b"ref-rref").expect("ref rref");
        fs::write(blobs.join(format!("{}.chunk.msgpack", fx.orphan_stem)), b"orphan-chunk").expect("orphan chunk");
        fs::write(blobs.join(format!("{}.rref.msgpack", fx.orphan_stem)), b"orphan-rref").expect("orphan rref");

        let referenced = collect_referenced_hashes(&fx.basemind_dir).expect("collect");
        gc_blobs_in(&fx.blobs_dir, &referenced).expect("gc");

        assert!(
            blobs.join(format!("{}.chunk.msgpack", fx.referenced_stem)).exists(),
            "referenced chunk survives"
        );
        assert!(
            blobs.join(format!("{}.rref.msgpack", fx.referenced_stem)).exists(),
            "referenced rref survives"
        );
        assert!(
            !blobs.join(format!("{}.chunk.msgpack", fx.orphan_stem)).exists(),
            "orphan chunk reclaimed"
        );
        assert!(
            !blobs.join(format!("{}.rref.msgpack", fx.orphan_stem)).exists(),
            "orphan rref reclaimed"
        );
    }

    /// Build a workspace dir (`<workspaces>/<key>/views/working/index.msgpack`) whose index
    /// references each stem in `stems`. Mirrors the global cache's per-workspace layout.
    fn seed_workspace(workspaces_dir: &Path, key: &str, stems: &[&str]) {
        let working = workspaces_dir.join(key).join(VIEWS_DIR).join("working");
        fs::create_dir_all(&working).expect("mk workspace view");
        let mut index = Index::empty();
        for (i, stem) in stems.iter().enumerate() {
            index.files.insert(
                crate::path::RelPath::from(format!("src/f{i}.rs").as_str()),
                FileEntry {
                    hash_hex: (*stem).to_string(),
                    language: "rust".to_string(),
                    size_bytes: 2,
                    mtime: 0,
                },
            );
        }
        let bytes = rmp_serde::to_vec_named(&index).expect("encode index");
        fs::write(working.join(INDEX_FILE), bytes).expect("write index");
    }

    #[test]
    fn global_gc_keeps_a_blob_referenced_by_any_workspace_and_reaps_the_orphan() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspaces = tmp.path().join("workspaces");
        let blobs = tmp.path().join("blobs");
        fs::create_dir_all(&blobs).expect("mk blobs");

        let stem_a = "a".repeat(64); // referenced by workspace A only
        let stem_b = "b".repeat(64); // referenced by workspace B only
        let orphan = "c".repeat(64); // referenced by NEITHER
        fs::write(blobs.join(format!("{stem_a}.fm.msgpack")), b"fm-a").expect("blob a");
        fs::write(blobs.join(format!("{stem_b}.fm.msgpack")), b"fm-b").expect("blob b");
        let orphan_bytes = b"orphan-blob-bytes";
        fs::write(blobs.join(format!("{orphan}.fm.msgpack")), orphan_bytes).expect("orphan blob");

        seed_workspace(&workspaces, "key-a", &[&stem_a]);
        seed_workspace(&workspaces, "key-b", &[&stem_b]);

        let referenced = collect_referenced_hashes_global_in(&workspaces).expect("union");
        assert_eq!(referenced.len(), 2, "the union spans both workspaces");
        assert!(referenced.contains(&stem_a) && referenced.contains(&stem_b));
        assert!(!referenced.contains(&orphan), "orphan referenced by no workspace");

        let report = gc_global_blobs_in(&workspaces, &blobs).expect("global gc");
        assert_eq!(report.scanned, 3, "all three blobs inspected");
        assert_eq!(report.removed, 1, "only the cross-workspace orphan reaped");
        assert_eq!(report.bytes_freed, orphan_bytes.len() as u64);

        assert!(
            blobs.join(format!("{stem_a}.fm.msgpack")).exists(),
            "blob referenced by workspace A survives"
        );
        assert!(
            blobs.join(format!("{stem_b}.fm.msgpack")).exists(),
            "blob referenced by workspace B survives (union, not per-workspace)"
        );
        assert!(!blobs.join(format!("{orphan}.fm.msgpack")).exists(), "orphan reaped");
    }

    #[test]
    fn global_gc_propagates_an_unreadable_workspace_index() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspaces = tmp.path().join("workspaces");
        let working = workspaces.join("key-a").join(VIEWS_DIR).join("working");
        fs::create_dir_all(&working).expect("mk view");
        fs::write(working.join(INDEX_FILE), b"\xff\xff not-msgpack \x00").expect("corrupt index");

        assert!(
            collect_referenced_hashes_global_in(&workspaces).is_err(),
            "an unreadable workspace index must fail the union (never drive a partial delete)"
        );
    }

    #[test]
    fn should_round_trip_component_tokens() {
        for component in [
            CacheComponent::Blobs,
            CacheComponent::Views,
            CacheComponent::Lance,
            CacheComponent::GitCache,
            CacheComponent::Telemetry,
            CacheComponent::All,
        ] {
            let token = component.as_str();
            let parsed: CacheComponent = token.parse().expect("parse token");
            assert_eq!(parsed, component, "round-trip {token}");
        }
        assert!("nonsense".parse::<CacheComponent>().is_err(), "unknown token rejected");
    }
}
