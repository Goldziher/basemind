//! The incremental pass: [`scan_paths`] and its variants process only the given absolute paths
//! (watcher batches and named rescans) instead of walking the whole tree. Split out of
//! `scanner.rs` to keep it under the module size cap; [`crate::scanner`] re-exports the entry
//! points so callers keep importing them from there.

use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::scanner::{
    EmbedMode, FileResult, FileStatus, NullObserver, Retrieval, ScanCancel, ScanError, ScanObserver, ScanReport,
    ScanSource, scan_with_observer,
};
use crate::scanner_drive::{DriveOutcome, ScanDrive, drive_scan};
use crate::scanner_file::scanner_pool;
use crate::scanner_filter::IndexFilter;
use crate::scanner_flush::{
    REQUEUE_PER_INCREMENTAL, derive_scope, flush_code_removals_if_any, flush_vector_lanes, maintain_lance_if_open,
};
use crate::scanner_lanes::{LANE_BM25_STATS, LANE_CODE_REMOVALS, LANE_RESOLVE, run_optional_lane};
use crate::store::Store;

/// Incremental scan: process only the given absolute paths. Used by the watcher
/// where the debouncer already told us which files changed.
///
/// Paths outside `root`, inside `.basemind/`, or not matching the include globs are
/// silently dropped (the watcher pre-filters but we re-check defensively).
/// Removed files (path no longer exists) are purged from the index. A directory path is expanded
/// into the files under it on disk and the keys the index holds under it, so a renamed or moved
/// directory reported as one path re-indexes its new location and prunes its old one.
pub fn scan_paths(
    root: &Path,
    store: &mut Store,
    config: &Config,
    paths: &[PathBuf],
    embed: EmbedMode,
    retrieval: Retrieval,
) -> Result<ScanReport, ScanError> {
    scan_paths_with_cancel(root, store, config, paths, embed, &ScanCancel::new(), retrieval)
}

/// [`scan_paths`] with a cooperative [`ScanCancel`] token. Unlike [`scan_with_cancel`](crate::scanner::scan_with_cancel), the removal
/// purge still runs on a tripped token — it is derived from path *existence* (the watcher told us
/// the file is gone), not from scan completeness — but the resolve pass and the optional lanes are
/// skipped and the report comes back with `cancelled == true`.
pub fn scan_paths_with_cancel(
    root: &Path,
    store: &mut Store,
    config: &Config,
    paths: &[PathBuf],
    embed: EmbedMode,
    cancel: &ScanCancel,
    retrieval: Retrieval,
) -> Result<ScanReport, ScanError> {
    scan_paths_with_observer(root, store, config, paths, embed, cancel, &mut NullObserver, retrieval)
}

/// [`scan_paths_with_cancel`] reporting each file's outcome to `observer`. The incremental
/// counterpart of [`scan_with_observer`]; the same drive loop, so the same O(chunk) bound holds
/// even when a watcher hands over a very large touched set.
#[allow(clippy::too_many_arguments)]
pub fn scan_paths_with_observer(
    root: &Path,
    store: &mut Store,
    config: &Config,
    paths: &[PathBuf],
    embed: EmbedMode,
    cancel: &ScanCancel,
    observer: &mut dyn ScanObserver,
    retrieval: Retrieval,
) -> Result<ScanReport, ScanError> {
    scan_paths_inner(root, store, config, paths, embed, cancel, observer, false, retrieval)
}

/// [`scan_paths_with_observer`] for paths a person or an agent named (`basemind rescan <paths>`,
/// `index_rescan paths`), as opposed to paths a watcher saw change. They were named because
/// something is thought to be wrong with them, so a named file that reads as unchanged has its
/// blob validated: that is what makes "rescan this path" the fix for a damaged blob. A watcher
/// batch does not pay that read, decompress and decode per file; a branch switch names thousands.
#[allow(clippy::too_many_arguments)]
pub fn scan_named_paths_with_observer(
    root: &Path,
    store: &mut Store,
    config: &Config,
    paths: &[PathBuf],
    embed: EmbedMode,
    cancel: &ScanCancel,
    observer: &mut dyn ScanObserver,
    retrieval: Retrieval,
) -> Result<ScanReport, ScanError> {
    scan_paths_inner(root, store, config, paths, embed, cancel, observer, true, retrieval)
}

#[allow(clippy::too_many_arguments)]
fn scan_paths_inner(
    root: &Path,
    store: &mut Store,
    config: &Config,
    paths: &[PathBuf],
    embed: EmbedMode,
    cancel: &ScanCancel,
    observer: &mut dyn ScanObserver,
    verify_named: bool,
    retrieval: Retrieval,
) -> Result<ScanReport, ScanError> {
    let source = ScanSource::WorkingTree;
    #[cfg(any(unix, windows))] // Blobs written without an fsync; free inside a hosting process. ~keep
    let _hosting = crate::scan_evidence::HostingMarker::begin();
    crate::scanner_integrity::claim_writer(store)?;
    // A changed manifest or lockfile re-resolves the dependency documentation, which a full pass does.
    if crate::scanner_integrity::needs_full_rebuild(store)? || crate::deps_docs::manifest_touched(root, config, paths) {
        return scan_with_observer(root, store, config, source, embed, cancel, observer, retrieval);
    }
    store.retry_vector_store();
    let mut filter = IndexFilter::new(root, config, retrieval)?;
    if verify_named {
        filter.verify_blobs();
    }
    // Named and watched paths alike: a watcher reports a renamed or moved directory as one path.
    let expanded =
        crate::scanner_named::expand_named_dirs(root, &store.index, store.key_order(), config, filter.filters(), paths);
    let paths = expanded.as_slice();

    let mut rels: Vec<String> = Vec::with_capacity(paths.len());
    let mut removed: Vec<String> = Vec::new();
    // Touched paths that are not (or no longer) indexed: kept only if a resolver consulted them.
    let mut other: Vec<String> = Vec::new();
    #[cfg(feature = "documents")]
    let mut doc_removed: Vec<String> = Vec::new();
    for abs in paths {
        let rel = match abs.strip_prefix(root) {
            Ok(p) => {
                let lossy = p.to_string_lossy();
                #[cfg(windows)]
                {
                    lossy.replace('\\', "/")
                }
                #[cfg(not(windows))]
                {
                    lossy.into_owned()
                }
            }
            Err(_) => continue,
        };
        if rel.is_empty() || rel.starts_with(crate::config::BASEMIND_DIR) {
            continue;
        }
        if !abs.exists() {
            if store.lookup(&rel).is_some() {
                removed.push(rel);
                continue;
            }
            other.push(rel.clone());
            #[cfg(feature = "documents")]
            if store.lookup_doc(&rel).is_some() || store.has_doc_tombstone(&rel) {
                doc_removed.push(rel);
            }
            continue;
        }
        if !filter.is_indexable(abs) {
            // Exists but a full scan would no longer index it (newly excluded / gitignored, now a
            // symlink or non-regular file): an entry left from an earlier scan is stale.
            other.push(rel.clone());
            if store.lookup(&rel).is_some() {
                removed.push(rel.clone());
            }
            #[cfg(feature = "documents")]
            if store.lookup_doc(&rel).is_some() || store.has_doc_tombstone(&rel) {
                doc_removed.push(rel);
            }
            continue;
        }
        rels.push(rel);
    }
    rels.sort();
    rels.dedup();

    #[cfg(feature = "documents")]
    let nothing_removed = removed.is_empty() && doc_removed.is_empty();
    #[cfg(not(feature = "documents"))]
    let nothing_removed = removed.is_empty();
    let other = store.index_db.as_ref().map_or_else(Vec::new, |db| {
        crate::intel::resolve_pass::probed_only(&db.view(), other)
    });
    if rels.is_empty() && nothing_removed && other.is_empty() {
        return Ok(ScanReport::default());
    }

    let scope = derive_scope(root, &source);
    let mut report = ScanReport::default();
    let file_map_before = store.file_map_bytes_written();
    // The contents these paths hold now: once the pass has replaced or dropped them, a content no
    // other path holds leaves its grep bloom row too (a long-lived watcher never runs the full sweep).
    let mut retired: Vec<String> = rels
        .iter()
        .chain(&removed)
        .filter_map(|rel| store.lookup(rel).map(|entry| entry.hash_hex.clone()))
        .collect();
    // Before `begin_rows`: its purge of unlisted rows deletes forward rows too. ~keep
    let bm25_pass = crate::scanner_flush::begin_bm25_pass_if_any(store, config, false);
    crate::scanner_integrity::begin_rows(store)?;
    let drive = ScanDrive {
        root,
        filters: filter.filters(),
        source: &source,
        config,
        scope: &scope,
        embed,
        cancel,
    };
    let DriveOutcome {
        doc_batches,
        code_batches,
        seen,
        #[cfg(feature = "documents")]
        doc_seen,
    } = drive_scan(&drive, &rels, store, &mut report.stats, observer)?;

    // A path that still exists but ended up neither kept as code nor as a document (its language
    // was disabled, it grew past the size cap, turned binary, ...) is dropped exactly as a full
    // scan drops it: judged by the same `seen` sets.
    // Skipped on a cancelled pass: the paths the drive never reached are absent from the sets.
    if !cancel.is_cancelled() {
        for rel in &rels {
            if !seen.contains(rel) && store.lookup(rel).is_some() {
                removed.push(rel.clone());
                #[cfg(feature = "documents")]
                if doc_seen.contains(rel) {
                    report.stats.tier_migrated += 1;
                }
            }
            #[cfg(feature = "documents")]
            if !doc_seen.contains(rel) && store.lookup_doc(rel).is_some() {
                doc_removed.push(rel.clone());
                if seen.contains(rel) {
                    report.stats.tier_migrated += 1;
                }
            }
        }
    }
    removed.sort();
    removed.dedup();
    #[cfg(feature = "documents")]
    {
        doc_removed.sort();
        doc_removed.dedup();
    }

    retired.extend(
        removed
            .iter()
            .filter_map(|rel| Some(store.lookup(rel)?.hash_hex.clone())),
    );
    let (purged, purge_failure) = crate::scanner_integrity::purge_files(store, &removed);
    for rel in purged {
        observer.on_file(FileResult::bare(rel.clone(), FileStatus::Removed));
        report.stats.removed += 1;
    }

    #[cfg(feature = "documents")]
    for rel in &doc_removed {
        observer.on_file(FileResult::bare(rel.clone(), FileStatus::Removed));
        report.stats.removed += 1;
    }

    crate::scanner_integrity::drop_unheld_grep_bloom_rows(store, &retired);

    if cancel.is_cancelled() || purge_failure.is_some() {
        store.flush_delta()?;
    } else {
        store.flush_ending_rows_pass_delta()?;
    }
    store.write_status_sidecar();
    if let Some(error) = purge_failure {
        return Err(error);
    }

    // Cancelled: the removals above already ran (path-existence-derived, safe on a partial pass) ~keep
    // and the code map is durable; skip the resolve pass and the removal lanes. The batch flushes ~keep
    // still run — the completed files' entries are already recorded, and dropping their rows ~keep
    // would leave them tracked-but-rowless (blob-sourced, no inference; bounded). ~keep
    if cancel.is_cancelled() {
        report.cancelled = true;
        flush_vector_lanes(
            store,
            config,
            filter.filters(),
            &scope,
            doc_batches,
            code_batches,
            REQUEUE_PER_INCREMENTAL,
        )?;
        report.file_map_bytes_written = store.file_map_bytes_written() - file_map_before;
        return Ok(report);
    }

    let precise = config.code_intel.precise_resolution;
    let pool = scanner_pool(config.resources.scan_threads)?;
    run_optional_lane(LANE_RESOLVE, || {
        pool.install(|| {
            crate::intel::resolve_pass::resolve_pass_incremental(
                root,
                store,
                &crate::intel::resolve_pass::PathDelta {
                    changed: &rels,
                    removed: &removed,
                    other: &other,
                },
                precise,
                config.resources.max_footprint_mb,
            )
        });
    });

    flush_vector_lanes(
        store,
        config,
        filter.filters(),
        &scope,
        doc_batches,
        code_batches,
        REQUEUE_PER_INCREMENTAL,
    )?;
    run_optional_lane(LANE_CODE_REMOVALS, || {
        flush_code_removals_if_any(store, config, filter.filters(), &scope, &removed);
    });
    #[cfg(feature = "documents")]
    crate::scanner_integrity::remove_docs_and_flush(store, config, filter.filters(), &scope, &doc_removed)?;
    run_optional_lane(LANE_BM25_STATS, || {
        crate::scanner_flush::finish_bm25_pass(store, config, bm25_pass, false)
    });
    run_optional_lane(crate::scanner_lanes::LANE_LANCE_MAINTENANCE, || {
        maintain_lance_if_open(store, filter.filters())
    });
    crate::scanner_integrity::finish_index_writes(store);
    report.file_map_bytes_written = store.file_map_bytes_written() - file_map_before;
    Ok(report)
}

#[cfg(test)]
#[path = "scanner_file_map_tests.rs"]
mod file_map_tests;
