use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use thiserror::Error;
use tracing::debug;

use crate::config::Config;
use crate::git::{GitError, Repo};
use crate::path::RelPath;
#[cfg(feature = "code-search")]
use crate::scanner_code::PendingCodeBatch;
#[cfg(feature = "documents")]
use crate::scanner_docs::{PendingDocBatch, flush_document_batches};
use crate::scanner_file::{run_candidates, scanner_pool};
use crate::scanner_filter::{Filters, IndexFilter, ignore_walk_builder};
#[cfg(feature = "documents")]
use crate::scanner_lanes::LANE_DOC_REMOVALS;
use crate::scanner_lanes::{
    LANE_BM25_STATS, LANE_CODE_BATCHES, LANE_CODE_REMOVALS, LANE_DOC_BATCHES, LANE_RESOLVE, run_optional_lane,
};
use crate::store::{FileEntry, Store, StoreError};

/// The per-file pipeline (`process_file` and the read / classify / extract helpers around it) and
/// the rayon pool it runs on live in their own module to keep this file under the module size cap.
/// [`looks_binary`] is re-exported so callers keep importing it from `crate::scanner`.
pub use crate::scanner_file::looks_binary;

/// Candidate-count threshold above which `walk_candidates` emits a visibility warning. This is
/// pure observability — not a hard cap — so a runaway monorepo scan (e.g. a Bazel tree whose
/// generated / vendored dirs slipped past `.gitignore` and the `[scan] exclude` globs) is visible
/// in the logs instead of silently ballooning `.basemind/`.
const LARGE_SCAN_CANDIDATE_WARN: usize = 50_000;

/// What state of the repository the scanner indexes from.
///
/// - `WorkingTree` (today's default) — walk the filesystem via `ignore::WalkBuilder`,
///   read bytes via `std::fs::read`.
/// - `Staged` — list paths from the git index, read blob bytes from the index. Lets the
///   pre-commit hook index *what is about to be committed* rather than whatever stale work
///   is sitting in the working tree.
/// - `Rev { sha }` — list the tree at `sha`, read blob bytes from that tree.
#[derive(Clone)]
pub enum ScanSource<'a> {
    WorkingTree,
    Staged(&'a Repo),
    Rev { repo: &'a Repo, sha: String },
}

impl<'a> ScanSource<'a> {
    fn label(&self) -> String {
        match self {
            ScanSource::WorkingTree => "working tree".to_string(),
            ScanSource::Staged(_) => "staged index".to_string(),
            ScanSource::Rev { sha, .. } => format!("rev {}", &sha[..7.min(sha.len())]),
        }
    }
}

#[derive(Debug, Error)]
pub enum ScanError {
    #[error("store error: {0}")]
    Store(#[from] StoreError),
    #[error("invalid glob in config: {0}")]
    BadGlob(String),
    #[error("git error: {0}")]
    Git(#[from] GitError),
}

/// Aggregate counters for a single scan invocation.
/// Computed from the per-file results; kept for backwards-compat assertions in tests.
#[derive(Debug, Default, Clone, Copy)]
pub struct ScanStats {
    pub scanned: usize,
    pub updated: usize,
    pub updated_with_warnings: usize,
    /// Subset of `updated` whose extraction was reused from an existing content-addressed blob
    /// (shared across views / worktrees) instead of re-parsed. High on a fresh worktree scan whose
    /// blobs are shared with the main worktree — that work is exactly what the reuse path saves.
    pub reused_extraction: usize,
    pub skipped_unchanged: usize,
    pub skipped_too_large: usize,
    pub skipped_non_utf8: usize,
    pub skipped_no_lang: usize,
    pub skipped_binary: usize,
    pub removed: usize,
    pub read_failed: usize,
    pub extract_failed: usize,
    /// Parse-timeout subset of `extract_failed`. Distinguished so users can spot pathological
    /// files separately from "actual" grammar errors.
    pub parse_timeouts: usize,
    /// Documents (non-source files) successfully extracted via xberg and (when embeddings
    /// were configured) pushed to LanceDB. Always present in `ScanStats` so callers that don't
    /// compile the `documents` feature still get a stable struct shape; stays `0` in that mode.
    pub docs_indexed: usize,
    /// Subset of `docs_indexed` served from an already-persisted `.doc.msgpack` blob instead of a
    /// fresh xberg extraction (+ embedding). Mirrors `reused_extraction` for the doc tier: rename /
    /// rewrite churn should show up here, never as fresh extraction work (issue #44).
    pub reused_doc_extraction: usize,
}

/// Per-file result. Every file the scanner *considered* shows up here.
/// SkippedNoLang is included so callers can render or hide it via verbosity.
#[derive(Debug, Clone)]
pub struct FileResult {
    /// Relative path, forward-slash separated.
    pub path: String,
    pub status: FileStatus,
    /// Internal: buffered FileEntry when the file was updated. The parallel `process_file`
    /// stashes the entry here; the single-threaded apply loop drains it into the store.
    /// Not part of the public surface — always `None` once `apply_outcomes` returns.
    pub(crate) upsert: Option<FileEntry>,
    /// Internal: buffered document batch when this file went through the xberg branch.
    /// Drained by the single-threaded `flush_document_batches` pass into LanceDB.
    #[cfg(feature = "documents")]
    pub(crate) doc_batch: Option<PendingDocBatch>,
    /// Internal: buffered [`DocEntry`] for the document tier (the doc analogue of `upsert`). The
    /// parallel `process_doc` stashes it; the apply loop drains it into `index.doc_files` so the
    /// next scan can skip unchanged docs and the blob GC keeps the `.doc.msgpack` cache alive.
    #[cfg(feature = "documents")]
    pub(crate) doc_upsert: Option<crate::store::DocEntry>,
    /// Internal: buffered code-chunk batch when this source file went through the code-search
    /// branch. Drained by the single-threaded `flush_code_batches` pass into LanceDB.
    #[cfg(feature = "code-search")]
    pub(crate) code_batch: Option<PendingCodeBatch>,
}

impl FileResult {
    /// Construct a minimal result with no buffered side-channel data. Helper used by every
    /// `process_file` exit point so we only edit one site when the carrier shape grows.
    pub(crate) fn bare(path: String, status: FileStatus) -> Self {
        Self {
            path,
            status,
            upsert: None,
            #[cfg(feature = "documents")]
            doc_batch: None,
            #[cfg(feature = "documents")]
            doc_upsert: None,
            #[cfg(feature = "code-search")]
            code_batch: None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum FileStatus {
    Updated {
        had_errors: bool,
        error_count: u32,
        /// True when the extraction was reused from an existing content-addressed blob (shared
        /// across views / worktrees) rather than re-parsed. The index entry is written either
        /// way; this only distinguishes a cache hit from a real tree-sitter parse and drives the
        /// `reused_extraction` scan counter.
        reused: bool,
    },
    Unchanged,
    Removed,
    SkippedTooLarge {
        size: u64,
    },
    SkippedNonUtf8,
    SkippedNoLang,
    /// Pre-flight NUL-byte scan flagged this as binary even though the extension claimed a
    /// supported language (e.g. a vendored PNG saved as `image.ts`). Cheap to detect and avoids
    /// the cost of running the grammar over noise.
    SkippedBinary,
    ReadFailed {
        kind: std::io::ErrorKind,
        msg: String,
    },
    ExtractFailed {
        msg: String,
    },
    /// Subset of ExtractFailed: parse exceeded the configured timeout.
    ParseTimedOut,
    /// File was non-source but went through the xberg document tier instead of being
    /// dropped at `SkippedNoLang`. `chunk_count` reflects how many chunks were extracted;
    /// `embedding_dim` is the vector dimension (zero when embeddings were disabled).
    #[cfg(feature = "documents")]
    DocIndexed {
        chunk_count: usize,
        embedding_dim: u16,
        /// True when the doc was served from the cached `.doc.msgpack` blob rather than freshly
        /// extracted (mirrors `Updated::reused`); drives the `reused_doc_extraction` counter.
        reused: bool,
    },
}

#[derive(Debug, Clone, Default)]
pub struct ScanReport {
    pub results: Vec<FileResult>,
    pub stats: ScanStats,
    /// True when the scan was interrupted by a [`ScanCancel`] token. The per-file work that
    /// completed before the trip is committed (blobs, fjall batches, `index.msgpack`); everything
    /// else — remaining candidates, the stale purge, the resolve/doc lanes — was skipped, so the
    /// caller must not treat the report as a complete pass.
    pub cancelled: bool,
}

/// Cooperative cancellation token for a scan.
///
/// Cancellation is per-file granularity: the scanner checks the token once before each candidate
/// (one `Relaxed` atomic load — hot-path discipline, no ordering needed because the flag is
/// monotonic and purely advisory) and again between the durability barrier and the optional
/// enrichment lanes. A tripped token never tears state: completed files are already committed
/// per-batch, and the caller returns before the stale purge so unscanned files are not mistaken
/// for deleted ones. Clone freely — all clones share the flag.
#[derive(Clone, Debug, Default)]
pub struct ScanCancel(Arc<AtomicBool>);

impl ScanCancel {
    /// A fresh, untripped token.
    pub fn new() -> Self {
        Self::default()
    }

    /// Trip the token. Idempotent; every in-flight scan sharing it stops at its next check.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Whether the token has been tripped.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// Pull submodule roots for the active scan source. WorkingTree opens a fresh `Repo` on the
/// root (cheap; fails silently when the directory isn't a repo). Staged/Rev reuses the
/// repo handle already carried by `ScanSource`. Failures degrade to an empty Vec so a
/// missing or malformed `.gitmodules` never blocks the scan.
pub(crate) fn submodule_roots_for_source(root: &Path, source: &ScanSource<'_>) -> Vec<String> {
    let paths = match source {
        ScanSource::Staged(repo) | ScanSource::Rev { repo, .. } => repo.submodule_paths(),
        ScanSource::WorkingTree => match Repo::discover(root) {
            Ok(r) => r.submodule_paths(),
            Err(_) => Vec::new(),
        },
    };
    paths.into_iter().map(|p| p.to_str_lossy().into_owned()).collect()
}

/// Whether the expensive embedding step runs during the scan.
///
/// - `Inline` (today's default; used by the CLI `basemind scan`, the watcher, and manual `rescan`)
///   embeds during the scan — code chunks and documents get their vectors + LanceDB rows in one pass.
/// - `Deferred` skips embedding: the scan still writes the code-map, the BM25 keyword lane, and the
///   content-addressed blobs, but emits **no** vector rows and does **not** persist the
///   embedding-completion markers (the `.chunk.msgpack` sidecar / the doc `DocEntry`). Serve boot uses
///   this for a fast first pass, then runs a second `Inline` scan in the background to fill vectors in.
///
/// It is threaded as an explicit parameter rather than mutated onto `config` because the serve path
/// shares a single `Arc<Config>` across every reader — mutating it would poison concurrent queries.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EmbedMode {
    Inline,
    Deferred,
}

/// One-shot scan: enumerate every candidate file *via the requested source*, process them
/// in parallel, purge stale index entries, flush the index, return a typed report.
///
/// Source-aware behavior:
/// - `WorkingTree` uses `ignore::WalkBuilder` to walk the on-disk tree and `std::fs::read`.
/// - `Staged` and `Rev` enumerate paths via gix and read bytes via gix.
pub fn scan(
    root: &Path,
    store: &mut Store,
    config: &Config,
    source: ScanSource<'_>,
    embed: EmbedMode,
) -> Result<ScanReport, ScanError> {
    scan_with_cancel(root, store, config, source, embed, &ScanCancel::new())
}

/// [`scan`] with a cooperative [`ScanCancel`] token. A tripped token makes the pass return early
/// with `report.cancelled == true`: completed files are committed and the code map flushed, but the
/// stale purge and the resolve/doc lanes are skipped — a partial pass must never treat unscanned
/// files as deleted.
pub fn scan_with_cancel(
    root: &Path,
    store: &mut Store,
    config: &Config,
    source: ScanSource<'_>,
    embed: EmbedMode,
    cancel: &ScanCancel,
) -> Result<ScanReport, ScanError> {
    let submodule_roots = submodule_roots_for_source(root, &source);
    let filters = Filters::build(config, submodule_roots)?;
    let candidates = candidates_for_source(root, config, &filters, &source)?;
    debug!(count = candidates.len(), kind = source.label(), "scan candidates");

    let scope = derive_scope(root, &source);

    let outcomes: Vec<FileResult> = run_candidates(
        &candidates,
        root,
        &filters,
        store,
        &source,
        config,
        &scope,
        embed,
        cancel,
    );

    // Cancelled mid-pass: commit what completed and return BEFORE the stale purge. The purge below ~keep
    // treats every indexed file absent from `outcomes` as deleted — on a partial pass that would ~keep
    // wipe the entry of every candidate the cancellation skipped. The batch flushes still run: ~keep
    // apply_outcomes recorded the completed files' entries (embedded=true), so dropping their ~keep
    // LanceDB rows here would leave them tracked-but-rowless — quiescent, never re-flushed. The ~keep
    // rows come from already-persisted blobs (no model inference), so this stays bounded. ~keep
    if cancel.is_cancelled() {
        let mut report = ScanReport {
            cancelled: true,
            ..ScanReport::default()
        };
        let (doc_batches, code_batches) = apply_outcomes(store, &mut report, outcomes);
        flush_code_map(store)?;
        run_optional_lane(LANE_DOC_BATCHES, || {
            flush_doc_batches_if_any(store, config, &scope, doc_batches);
        });
        run_optional_lane(LANE_CODE_BATCHES, || {
            flush_code_batches_if_any(store, config, &scope, code_batches);
        });
        return Ok(report);
    }

    let seen: ahash::AHashSet<&str> = outcomes
        .iter()
        .filter_map(|r| match &r.status {
            FileStatus::Updated { .. } | FileStatus::Unchanged => Some(r.path.as_str()),
            _ => None,
        })
        .collect();

    let stale: Vec<String> = store
        .index
        .files
        .keys()
        .filter(|k| !seen.contains(k.to_str_lossy().as_ref()))
        .map(|k| k.to_str_lossy().into_owned())
        .collect();
    drop(seen);

    #[cfg(feature = "documents")]
    let doc_stale: Vec<String> = {
        let doc_seen: ahash::AHashSet<&str> = outcomes
            .iter()
            .filter(|r| r.doc_batch.is_some() || matches!(r.status, FileStatus::Unchanged | FileStatus::Updated { .. }))
            .map(|r| r.path.as_str())
            .collect();
        store
            .index
            .doc_files
            .keys()
            .filter(|k| !doc_seen.contains(k.to_str_lossy().as_ref()))
            .map(|k| k.to_str_lossy().into_owned())
            .collect()
    };

    let mut report = ScanReport::default();
    let (doc_batches, code_batches) = apply_outcomes(store, &mut report, outcomes);

    for k in &stale {
        store.remove(k);
        if let Some(idx) = store.index_db.as_ref() {
            let mut w = idx.writer();
            let rel = RelPath::from(k.as_str());
            let res = w.remove_file(&rel).and_then(|()| w.remove_resolved_file(&rel));
            #[cfg(feature = "code-search")]
            let res = res.and_then(|()| w.remove_bm25_file(&rel));
            let _ = res.and_then(|()| w.commit());
        }
        report.results.push(FileResult::bare(k.clone(), FileStatus::Removed));
        report.stats.removed += 1;
    }

    flush_code_map(store)?;

    // A token tripped after the last per-file check still stops the pass here, once the code map ~keep
    // is durable but before the resolve pass (which can run for minutes on a large repo). The ~keep
    // batch flushes still run — the completed files' entries are already recorded, and dropping ~keep
    // their rows would leave them tracked-but-rowless (blob-sourced, no inference; bounded). ~keep
    if cancel.is_cancelled() {
        report.cancelled = true;
        run_optional_lane(LANE_DOC_BATCHES, || {
            flush_doc_batches_if_any(store, config, &scope, doc_batches);
        });
        run_optional_lane(LANE_CODE_BATCHES, || {
            flush_code_batches_if_any(store, config, &scope, code_batches);
        });
        return Ok(report);
    }

    if matches!(source, ScanSource::WorkingTree) {
        let precise = config.code_intel.precise_resolution;
        run_optional_lane(LANE_RESOLVE, || {
            scanner_pool(config.resources.scan_threads)
                .install(|| crate::intel::resolve_pass::resolve_pass(root, store, precise));
        });
    }

    run_optional_lane(LANE_DOC_BATCHES, || {
        flush_doc_batches_if_any(store, config, &scope, doc_batches);
    });
    run_optional_lane(LANE_CODE_BATCHES, || {
        flush_code_batches_if_any(store, config, &scope, code_batches);
    });
    run_optional_lane(LANE_CODE_REMOVALS, || {
        flush_code_removals_if_any(store, config, &scope, &stale);
    });
    #[cfg(feature = "documents")]
    if !doc_stale.is_empty() {
        run_optional_lane(LANE_DOC_REMOVALS, || {
            flush_doc_removals_if_any(store, config, &scope, &doc_stale);
        });
        store.flush()?;
    }
    run_optional_lane(LANE_BM25_STATS, || finalize_bm25_stats_if_any(store, config));
    Ok(report)
}

/// Persist the file map (`index.msgpack`) — the durability barrier that must run BEFORE the optional
/// post-extraction lanes.
///
/// Blobs and the Fjall index are committed per file, but the file map is a single msgpack rewrite at
/// the end. When it was written *after* the optional lanes, any lane that panicked (the
/// `stack-graphs` stitcher) or hung until the operator killed the process (the embedding-model
/// download on a blackholed IPv6 route) left the workspace with gigabytes of committed blobs beside
/// an `index.msgpack` reporting `file_count: 0` — a silently empty code map that forced a full
/// re-scan on every launch. Flushing here makes the code map durable the moment it is complete;
/// every lane after this point is enrichment, and a lane that dies costs only its own tier.
///
/// A lane that mutates `store.index` must flush again after itself (only the doc-removal lane does).
fn flush_code_map(store: &Store) -> Result<(), ScanError> {
    store.flush()?;
    Ok(())
}

/// Incremental scan: process only the given absolute paths. Used by the watcher
/// where the debouncer already told us which files changed.
///
/// Paths outside `root`, inside `.basemind/`, or not matching the include globs are
/// silently dropped (the watcher pre-filters but we re-check defensively).
/// Removed files (path no longer exists) are purged from the index.
pub fn scan_paths(
    root: &Path,
    store: &mut Store,
    config: &Config,
    paths: &[PathBuf],
    embed: EmbedMode,
) -> Result<ScanReport, ScanError> {
    scan_paths_with_cancel(root, store, config, paths, embed, &ScanCancel::new())
}

/// [`scan_paths`] with a cooperative [`ScanCancel`] token. Unlike [`scan_with_cancel`], the removal
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
) -> Result<ScanReport, ScanError> {
    let source = ScanSource::WorkingTree;
    let filter = IndexFilter::new(root, config)?;

    let mut rels: Vec<String> = Vec::with_capacity(paths.len());
    let mut removed: Vec<String> = Vec::new();
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
            #[cfg(feature = "documents")]
            if store.lookup_doc(&rel).is_some() {
                doc_removed.push(rel);
            }
            continue;
        }
        if !filter.is_indexable(abs) {
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
    if rels.is_empty() && nothing_removed {
        return Ok(ScanReport::default());
    }

    let scope = derive_scope(root, &source);
    let outcomes: Vec<FileResult> = run_candidates(
        &rels,
        root,
        filter.filters(),
        store,
        &source,
        config,
        &scope,
        embed,
        cancel,
    );

    let mut report = ScanReport::default();
    let (doc_batches, code_batches) = apply_outcomes(store, &mut report, outcomes);

    for rel in &removed {
        store.remove(rel);
        if let Some(idx) = store.index_db.as_ref() {
            let mut w = idx.writer();
            let rel = RelPath::from(rel.as_str());
            let res = w.remove_file(&rel).and_then(|()| w.remove_resolved_file(&rel));
            #[cfg(feature = "code-search")]
            let res = res.and_then(|()| w.remove_bm25_file(&rel));
            let _ = res.and_then(|()| w.commit());
        }
        report.results.push(FileResult::bare(rel.clone(), FileStatus::Removed));
        report.stats.removed += 1;
    }

    // Doc-tier removals (file gone from disk, tracked only in `doc_files`) are purged by the
    // doc-removals lane below; count them here so the report doesn't silently drop them.
    #[cfg(feature = "documents")]
    for rel in &doc_removed {
        report.results.push(FileResult::bare(rel.clone(), FileStatus::Removed));
        report.stats.removed += 1;
    }

    flush_code_map(store)?;

    // Cancelled: the removals above already ran (path-existence-derived, safe on a partial pass) ~keep
    // and the code map is durable; skip the resolve pass and the removal lanes. The batch flushes ~keep
    // still run — the completed files' entries are already recorded, and dropping their rows ~keep
    // would leave them tracked-but-rowless (blob-sourced, no inference; bounded). ~keep
    if cancel.is_cancelled() {
        report.cancelled = true;
        run_optional_lane(LANE_DOC_BATCHES, || {
            flush_doc_batches_if_any(store, config, &scope, doc_batches);
        });
        run_optional_lane(LANE_CODE_BATCHES, || {
            flush_code_batches_if_any(store, config, &scope, code_batches);
        });
        return Ok(report);
    }

    let precise = config.code_intel.precise_resolution;
    run_optional_lane(LANE_RESOLVE, || {
        scanner_pool(config.resources.scan_threads)
            .install(|| crate::intel::resolve_pass::resolve_pass_incremental(root, store, &rels, precise));
    });

    run_optional_lane(LANE_DOC_BATCHES, || {
        flush_doc_batches_if_any(store, config, &scope, doc_batches);
    });
    run_optional_lane(LANE_CODE_BATCHES, || {
        flush_code_batches_if_any(store, config, &scope, code_batches);
    });
    run_optional_lane(LANE_CODE_REMOVALS, || {
        flush_code_removals_if_any(store, config, &scope, &removed);
    });
    #[cfg(feature = "documents")]
    if !doc_removed.is_empty() {
        run_optional_lane(LANE_DOC_REMOVALS, || {
            flush_doc_removals_if_any(store, config, &scope, &doc_removed);
        });
        store.flush()?;
    }
    run_optional_lane(LANE_BM25_STATS, || finalize_bm25_stats_if_any(store, config));
    Ok(report)
}

/// Drain the parallel-map results back into the single-threaded store + report. Returns the
/// list of buffered document batches so the caller can flush them into LanceDB after the
/// index is consistent.
#[cfg_attr(not(feature = "documents"), allow(clippy::needless_pass_by_ref_mut))]
fn apply_outcomes(
    store: &mut Store,
    report: &mut ScanReport,
    outcomes: Vec<FileResult>,
) -> (Vec<PendingDocBatchOpt>, Vec<PendingCodeBatchOpt>) {
    #[cfg_attr(not(feature = "documents"), allow(unused_mut))]
    let mut doc_batches: Vec<PendingDocBatchOpt> = Vec::new();
    #[cfg_attr(not(feature = "code-search"), allow(unused_mut))]
    let mut code_batches: Vec<PendingCodeBatchOpt> = Vec::new();
    for mut o in outcomes {
        report.stats.scanned += 1;
        match &o.status {
            FileStatus::Updated {
                had_errors,
                error_count: _,
                reused,
            } => {
                report.stats.updated += 1;
                if *had_errors {
                    report.stats.updated_with_warnings += 1;
                }
                if *reused {
                    report.stats.reused_extraction += 1;
                }
            }
            FileStatus::Unchanged => report.stats.skipped_unchanged += 1,
            FileStatus::SkippedTooLarge { .. } => report.stats.skipped_too_large += 1,
            FileStatus::SkippedNonUtf8 => report.stats.skipped_non_utf8 += 1,
            FileStatus::SkippedNoLang => report.stats.skipped_no_lang += 1,
            FileStatus::SkippedBinary => report.stats.skipped_binary += 1,
            FileStatus::Removed => report.stats.removed += 1,
            FileStatus::ReadFailed { .. } => report.stats.read_failed += 1,
            FileStatus::ExtractFailed { .. } => report.stats.extract_failed += 1,
            FileStatus::ParseTimedOut => {
                report.stats.extract_failed += 1;
                report.stats.parse_timeouts += 1;
            }
            #[cfg(feature = "documents")]
            FileStatus::DocIndexed { reused, .. } => {
                report.stats.docs_indexed += 1;
                if *reused {
                    report.stats.reused_doc_extraction += 1;
                }
            }
        }
        if let Some(entry) = o.upsert.take() {
            store.upsert(&o.path, entry);
        }
        #[cfg(feature = "documents")]
        if let Some(entry) = o.doc_upsert.take() {
            store.upsert_doc(&o.path, entry);
        }
        #[cfg(feature = "documents")]
        if let Some(batch) = o.doc_batch.take() {
            doc_batches.push(batch);
        }
        #[cfg(feature = "code-search")]
        if let Some(batch) = o.code_batch.take() {
            code_batches.push(batch);
        }
        let cleared = FileResult::bare(o.path, o.status);
        report.results.push(cleared);
    }
    (doc_batches, code_batches)
}

/// Alias that's `PendingDocBatch` under the `documents` feature and `()` otherwise. Lets
/// `apply_outcomes` keep one signature while still returning real values when the feature
/// is on.
#[cfg(feature = "documents")]
type PendingDocBatchOpt = PendingDocBatch;
#[cfg(not(feature = "documents"))]
type PendingDocBatchOpt = ();

/// Alias that's `PendingCodeBatch` under the `code-search` feature and `()` otherwise. Keeps
/// `apply_outcomes`' return type consistent across feature sets.
#[cfg(feature = "code-search")]
type PendingCodeBatchOpt = PendingCodeBatch;
#[cfg(not(feature = "code-search"))]
type PendingCodeBatchOpt = ();

fn candidates_for_source(
    root: &Path,
    config: &Config,
    filters: &Filters,
    source: &ScanSource<'_>,
) -> Result<Vec<String>, ScanError> {
    let raw = match source {
        ScanSource::WorkingTree => walk_candidates(root, config, filters),
        ScanSource::Staged(repo) => repo.list_paths_staged()?,
        ScanSource::Rev { repo, sha } => repo.list_paths_rev(sha)?,
    };
    let mut out: Vec<String> = match source {
        ScanSource::WorkingTree => raw,
        _ => raw
            .into_iter()
            .filter(|rel| filters.allows(rel))
            .filter(|rel| !rel.starts_with(crate::config::BASEMIND_DIR))
            .collect(),
    };
    out.sort();
    out.dedup();
    Ok(out)
}

fn walk_candidates(root: &Path, config: &Config, filters: &Filters) -> Vec<String> {
    let mut out = Vec::new();
    let walker = ignore_walk_builder(root, config.scan.respect_gitignore, config.scan.follow_symlinks).build();
    for dent in walker.flatten() {
        if !dent.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let path = dent.path();
        let rel = match path.strip_prefix(root) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let Some(rel_str) = rel.to_str() else {
            continue;
        };
        #[cfg(windows)]
        let rel_owned = rel_str.replace('\\', "/");
        #[cfg(windows)]
        let rel_str = rel_owned.as_str();
        if !filters.allows(rel_str) {
            continue;
        }
        out.push(rel_str.to_string());
    }
    crate::scanner_filter::walk_extra_roots(root, config, filters, &mut out);
    if out.len() > LARGE_SCAN_CANDIDATE_WARN {
        tracing::warn!(
            candidates = out.len(),
            "scan candidate set is very large; check .gitignore / [scan] exclude globs for generated or vendored trees"
        );
    }
    out
}

/// Compute the LanceDB scope key for this scan. Git sources reuse the existing remote-URL
/// scope derivation; the working-tree path falls back to a workdir-rooted key when there's
/// no git remote (or no git repo at all).
fn derive_scope(root: &Path, source: &ScanSource<'_>) -> String {
    match source {
        ScanSource::Staged(repo) | ScanSource::Rev { repo, .. } => crate::git::scope_key(repo),
        ScanSource::WorkingTree => match Repo::discover(root) {
            Ok(repo) => crate::git::scope_key(&repo),
            Err(_) => format!("path:{}", root.display()),
        },
    }
}

/// Push the buffered document batches into LanceDB. No-op without the `documents` feature.
#[cfg(feature = "documents")]
fn flush_doc_batches_if_any(store: &mut Store, config: &Config, scope: &str, batches: Vec<PendingDocBatchOpt>) {
    if batches.is_empty() {
        return;
    }
    let _ = flush_document_batches(store, scope, batches, &config.documents.embedding_preset);
}

#[cfg(not(feature = "documents"))]
fn flush_doc_batches_if_any(_store: &mut Store, _config: &Config, _scope: &str, _batches: Vec<PendingDocBatchOpt>) {}

/// Purge `documents` LanceDB rows + `doc_files` entries for docs removed since the last scan. Called
/// after the batch flush so it reuses an already-open LanceStore. Only referenced under `documents`.
#[cfg(feature = "documents")]
fn flush_doc_removals_if_any(store: &mut Store, config: &Config, scope: &str, stale: &[String]) {
    crate::scanner_docs::delete_stale_documents(store, config, scope, stale);
}

/// Push the buffered code-chunk batches into LanceDB. No-op without the `code-search` feature.
#[cfg(feature = "code-search")]
fn flush_code_batches_if_any(store: &mut Store, config: &Config, scope: &str, batches: Vec<PendingCodeBatchOpt>) {
    if batches.is_empty() {
        return;
    }
    let _ = crate::scanner_code::flush_code_batches(store, scope, batches, &config.documents.embedding_preset);
}

#[cfg(not(feature = "code-search"))]
fn flush_code_batches_if_any(_store: &mut Store, _config: &Config, _scope: &str, _batches: Vec<PendingCodeBatchOpt>) {}

/// Purge `code_chunks` rows for files removed since the last scan. No-op without `code-search`.
/// Called after the batch flush so it reuses an already-open LanceStore.
#[cfg(feature = "code-search")]
fn flush_code_removals_if_any(store: &mut Store, config: &Config, scope: &str, stale: &[String]) {
    crate::scanner_code::delete_stale_code_chunks(store, config, scope, stale);
}

#[cfg(not(feature = "code-search"))]
fn flush_code_removals_if_any(_store: &mut Store, _config: &Config, _scope: &str, _stale: &[String]) {}

/// Recompute the corpus-global BM25 stats once the per-file postings have all committed, so the
/// keyword lane's `N` / `avgdl` reflect this scan. Single-threaded; no-op without `code-search` or
/// when chunking is disabled. No-op on a `None` (read-only) index.
#[cfg(feature = "code-search")]
fn finalize_bm25_stats_if_any(store: &Store, config: &Config) {
    if !crate::scanner_code::should_chunk(config) {
        return;
    }
    if let Some(db) = store.index_db.as_ref()
        && let Err(error) = db.recompute_bm25_stats()
    {
        tracing::warn!(?error, "recompute bm25 stats failed; keyword search may be stale");
    }
}

#[cfg(not(feature = "code-search"))]
fn finalize_bm25_stats_if_any(_store: &Store, _config: &Config) {}
