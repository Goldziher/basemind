use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rayon::prelude::*;
use thiserror::Error;
use tracing::debug;

use crate::config::Config;
use crate::extract::{self, ExtractError, FileMapL1, FileMapL2};
use crate::git::{GitError, Repo};
use crate::hashing;
use crate::index::{IndexDb, writer::IndexWriter};
use crate::lang;
use crate::path::RelPath;
#[cfg(feature = "code-search")]
use crate::scanner_code::PendingCodeBatch;
#[cfg(feature = "documents")]
use crate::scanner_docs::{PendingDocBatch, extract_and_persist_doc, flush_document_batches, should_extract_document};
use crate::scanner_filter::{Filters, IndexFilter, ignore_walk_builder};
#[cfg(feature = "documents")]
use crate::scanner_lanes::LANE_DOC_REMOVALS;
use crate::scanner_lanes::{
    LANE_BM25_STATS, LANE_CODE_BATCHES, LANE_CODE_REMOVALS, LANE_DOC_BATCHES, LANE_RESOLVE, run_optional_lane,
};
use crate::store::{FileEntry, Store, StoreError};

/// Number of files whose index entries are accumulated into one Fjall write batch before
/// committing. Each `IndexWriter::commit` takes Fjall's single write lock, so committing
/// per file made every rayon worker serialize on that lock (a flamegraph attributed ~14%
/// of scan wall-time to `__psynch_mutexwait` here). Batching `N` files per commit cuts the
/// commit count — and thus the lock-contention — by ~`N`× while keeping each worker's
/// staged work bounded in memory. The per-file read-before-write atomicity is preserved:
/// every file still stages its own deletes+inserts; only the *flush boundary* moved.
const INDEX_COMMIT_BATCH: usize = 256;

/// Candidate-count threshold above which `walk_candidates` emits a visibility warning. This is
/// pure observability — not a hard cap — so a runaway monorepo scan (e.g. a Bazel tree whose
/// generated / vendored dirs slipped past `.gitignore` and the `[scan] exclude` globs) is visible
/// in the logs instead of silently ballooning `.basemind/`.
const LARGE_SCAN_CANDIDATE_WARN: usize = 50_000;

/// Per-rayon-worker accumulator: buffers each file's index upsert into a shared Fjall write
/// batch and commits once `INDEX_COMMIT_BATCH` files have been staged (and once more at the
/// end of the worker's slice). Also carries the worker's `FileResult`s so the parallel fold
/// produces both the scan outcomes and the committed index in one pass.
///
/// Borrows `&IndexDb` (cheap `Arc`-backed handle) for the worker's lifetime. When the store
/// has no index (`index_db == None`, read-only mode) staging is a no-op.
struct WorkerIndexBatch<'a> {
    index: Option<&'a IndexDb>,
    writer: Option<IndexWriter>,
    staged: usize,
    results: Vec<FileResult>,
}

impl<'a> WorkerIndexBatch<'a> {
    fn new(store: &'a Store) -> Self {
        Self {
            index: store.index_db.as_ref(),
            writer: None,
            staged: 0,
            results: Vec::new(),
        }
    }

    /// Stage one file's symbols / calls / imports into the current batch, committing first
    /// if the batch is already full. Returns `false` only when the upsert itself failed
    /// (caller logs); a `None` index is a successful no-op.
    fn stage(&mut self, rel: &RelPath, l1: &FileMapL1, l2: Option<&FileMapL2>) -> bool {
        let Some(index) = self.index else {
            return true;
        };
        let writer = self.writer.get_or_insert_with(|| index.writer());
        if writer.upsert_file(rel, l1, l2).is_err() {
            return false;
        }
        self.staged += 1;
        if self.staged >= INDEX_COMMIT_BATCH {
            self.commit();
        }
        true
    }

    /// Stage one file's BM25 keyword postings into the current batch, reusing the same
    /// [`IndexWriter`] as [`Self::stage`] so the symbol upsert and the keyword postings ride the
    /// same per-file commit. Does not touch the file counter (the file was already counted by
    /// `stage`) and never force-commits. A `None` index is a successful no-op.
    #[cfg(feature = "code-search")]
    fn stage_bm25(&mut self, rel: &RelPath, postings: &[crate::search::bm25::ChunkPosting]) {
        let Some(index) = self.index else {
            return;
        };
        let writer = self.writer.get_or_insert_with(|| index.writer());
        if writer.upsert_bm25_file(rel, postings).is_err() {
            tracing::warn!(rel = %rel, "bm25 upsert failed; keyword search may be incomplete");
        }
    }

    /// Flush the staged batch under Fjall's write lock and reset the counter.
    fn commit(&mut self) {
        if let Some(writer) = self.writer.take()
            && writer.commit().is_err()
        {
            tracing::warn!("index batch commit failed; reference search may be incomplete");
        }
        self.staged = 0;
    }

    /// Commit the trailing partial batch and hand back the worker's results.
    fn finish(mut self) -> Vec<FileResult> {
        self.commit();
        self.results
    }
}

/// Drive the per-file pipeline across `candidates` on the rayon pool, batching index commits
/// per worker. Order of the returned `FileResult`s is unspecified (the parallel fold
/// concatenates per-worker slices) — every consumer keys by `path`, never by position.
#[allow(clippy::too_many_arguments)]
/// Per-worker stack size for the scanner's rayon pool. Tree-sitter parse trees and the recursive
/// msgpack (de)serialization of large extraction blobs can recurse deeper than rayon's small default
/// worker stack (~2 MiB), overflowing it on pathological inputs (deeply-nested or machine-generated
/// files) — observed as a hard `stack overflow` abort on a full fresh scan of a large real repo. A
/// full scan of that repo was measured to complete under a 128 MiB stack, so 256 MiB carries a 2×
/// margin. The stack is reserved lazily (VA only, not committed until touched), so this large ceiling
/// costs nothing on the common shallow path.
const SCANNER_STACK_SIZE: usize = 256 * 1024 * 1024;

/// Process-wide rayon pool the scanner runs its per-file `par_iter` on: sized like the default global
/// pool but with a much larger per-worker stack (see [`SCANNER_STACK_SIZE`]). Lazily built once.
fn scanner_pool() -> &'static rayon::ThreadPool {
    static POOL: std::sync::OnceLock<rayon::ThreadPool> = std::sync::OnceLock::new();
    POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .stack_size(SCANNER_STACK_SIZE)
            .thread_name(|i| format!("bm-scan-{i}"))
            .build()
            .expect("build scanner rayon pool")
    })
}

#[allow(clippy::too_many_arguments)]
fn run_candidates(
    candidates: &[String],
    root: &Path,
    filters: &Filters,
    store: &Store,
    source: &ScanSource<'_>,
    config: &Config,
    scope: &str,
    embed: EmbedMode,
) -> Vec<FileResult> {
    scanner_pool().install(|| {
        candidates
            .par_iter()
            .fold(
                || WorkerIndexBatch::new(store),
                |mut batch, rel| {
                    let result = process_file(root, rel, filters, store, source, config, scope, &mut batch, embed);
                    batch.results.push(result);
                    batch
                },
            )
            .map(WorkerIndexBatch::finish)
            .reduce(Vec::new, |mut a, mut b| {
                a.append(&mut b);
                a
            })
    })
}

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
    fn bare(path: String, status: FileStatus) -> Self {
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
    },
}

#[derive(Debug, Clone, Default)]
pub struct ScanReport {
    pub results: Vec<FileResult>,
    pub stats: ScanStats,
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
    let submodule_roots = submodule_roots_for_source(root, &source);
    let filters = Filters::build(config, submodule_roots)?;
    let candidates = candidates_for_source(root, config, &filters, &source)?;
    debug!(count = candidates.len(), kind = source.label(), "scan candidates");

    let scope = derive_scope(root, &source);

    let outcomes: Vec<FileResult> = run_candidates(&candidates, root, &filters, store, &source, config, &scope, embed);

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

    if matches!(source, ScanSource::WorkingTree) {
        let precise = config.code_intel.precise_resolution;
        run_optional_lane(LANE_RESOLVE, || {
            scanner_pool().install(|| crate::intel::resolve_pass::resolve_pass(root, store, precise));
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
        // The doc-removal lane is the ONE post-barrier lane that edits `store.index` (it drops the
        // `doc_files` entries it purges from LanceDB), so its edits need a second flush.
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
    let outcomes: Vec<FileResult> =
        run_candidates(&rels, root, filter.filters(), store, &source, config, &scope, embed);

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

    flush_code_map(store)?;

    let precise = config.code_intel.precise_resolution;
    run_optional_lane(LANE_RESOLVE, || {
        scanner_pool().install(|| crate::intel::resolve_pass::resolve_pass_incremental(root, store, &rels, precise));
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
        // See `flush_code_map`: the doc-removal lane is the only post-barrier `store.index` mutator.
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
            FileStatus::DocIndexed { .. } => {
                report.stats.docs_indexed += 1;
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

/// True when the extraction blobs required to classify a file as `Unchanged` are present on disk for
/// `hash_hex`: the combined-filemap blob always, plus the code-chunk sidecar when code-search
/// chunking is enabled (so toggling the feature on re-chunks rather than being skipped as unchanged).
fn extraction_sidecars_present(store: &Store, config: &Config, hash_hex: &str) -> bool {
    #[cfg(not(feature = "code-search"))]
    let _ = config;
    if !store.blob_path_fm_hex(hash_hex).exists() {
        return false;
    }
    #[cfg(feature = "code-search")]
    if crate::scanner_code::should_chunk(config) && !store.blob_path_chunk_hex(hash_hex).exists() {
        return false;
    }
    true
}

/// True when the on-disk chunk sidecar already satisfies THIS scan's embedding requirement, so an
/// otherwise-unchanged file may be short-circuited as `Unchanged`.
///
/// This is distinct from [`extraction_sidecars_present`] (blob *presence*, which governs l1/l2/chunk
/// reuse): a chunk-only sidecar is present but carries no vectors. A `Deferred` pass never embeds, so
/// any present sidecar satisfies it; an `Inline` pass over an embed-eligible file requires embeddings
/// for the current preset to already exist, else the file must be re-processed to fill them. The
/// daemon writes chunk-only sidecars in `Deferred`, which a later `Inline` pass upgrades in place —
/// without this gate the Inline pass would short-circuit past `chunk_and_embed` and never embed.
#[cfg(feature = "code-search")]
fn embed_state_satisfied(store: &Store, config: &Config, rel: &str, hash_hex: &str, mode: EmbedMode) -> bool {
    if !matches!(mode, EmbedMode::Inline) {
        return true;
    }
    let cfg = &config.code_search;
    if !cfg.embed || crate::scanner_filter::embed_excluded(rel, &cfg.embed_exclude) {
        return true;
    }
    // A schema/state peek is enough here — this is a stat-only fast path, so avoid the full
    // chunk-text decode `read_chunks_by_hex` would pay for on every unchanged, embed-eligible file.
    match store.peek_chunk_state(hash_hex) {
        // A file with no chunks has nothing to embed; anything with vectors for the active preset is
        // already satisfied. A non-empty, unembedded (dim-0) sidecar must be re-processed.
        Ok(Some(peek)) => {
            peek.chunks.is_empty()
                || (peek.embedding_dim > 0
                    && peek.embedding_model == config.documents.embedding_preset
                    && peek.embeddings.len() == peek.chunks.len())
        }
        _ => false,
    }
}

/// Without `code-search` there is no embedding tier, so every scan's embed requirement is trivially
/// satisfied.
#[cfg(not(feature = "code-search"))]
fn embed_state_satisfied(_store: &Store, _config: &Config, _rel: &str, _hash_hex: &str, _mode: EmbedMode) -> bool {
    true
}

/// Process a single relative path. Returns a `FileResult`; if the file is being
/// updated, the new `FileEntry` is attached via `FileResult::upsert` so the caller
/// can apply it to the store from the single-threaded apply loop.
#[allow(clippy::too_many_arguments)]
fn process_file(
    root: &Path,
    rel: &str,
    filters: &Filters,
    store: &Store,
    source: &ScanSource<'_>,
    config: &Config,
    scope: &str,
    index_batch: &mut WorkerIndexBatch<'_>,
    embed: EmbedMode,
) -> FileResult {
    #[cfg(not(feature = "documents"))]
    {
        let _ = (config, scope);
    }
    #[cfg(not(any(feature = "documents", feature = "code-search")))]
    {
        let _ = embed;
    }
    let lang = match lang::detect(Path::new(rel)) {
        Some(l) => l,
        None => {
            #[cfg(feature = "documents")]
            {
                if matches!(source, ScanSource::WorkingTree) {
                    return process_doc(root, rel, filters, store, config, scope, embed);
                }
            }
            return FileResult::bare(rel.to_string(), FileStatus::SkippedNoLang);
        }
    };

    if matches!(source, ScanSource::WorkingTree)
        && let Some(existing) = store.lookup(rel)
        && existing.mtime != 0
        && let Ok(meta) = std::fs::metadata(root.join(rel))
    {
        let mtime = mtime_nanos(&meta);
        if meta.len() == existing.size_bytes
            && mtime == existing.mtime
            && extraction_sidecars_present(store, config, &existing.hash_hex)
            && embed_state_satisfied(store, config, rel, &existing.hash_hex, embed)
        {
            return FileResult::bare(rel.to_string(), FileStatus::Unchanged);
        }
    }

    let (bytes, size_bytes, mtime) = match source {
        ScanSource::WorkingTree => match read_working_tree(root, rel, filters) {
            Ok(triple) => triple,
            Err(status) => {
                return FileResult::bare(rel.to_string(), status);
            }
        },
        ScanSource::Staged(repo) => match read_via_git(filters, repo.read_blob_staged(rel)) {
            Ok(triple) => triple,
            Err(status) => {
                return FileResult::bare(rel.to_string(), status);
            }
        },
        ScanSource::Rev { repo, sha } => match read_via_git(filters, repo.read_blob_at_rev(sha, rel)) {
            Ok(triple) => triple,
            Err(status) => {
                return FileResult::bare(rel.to_string(), status);
            }
        },
    };

    if looks_binary(&bytes) {
        return FileResult::bare(rel.to_string(), FileStatus::SkippedBinary);
    }

    if std::str::from_utf8(&bytes).is_err() {
        return FileResult::bare(rel.to_string(), FileStatus::SkippedNonUtf8);
    }

    let hash = hashing::hash_bytes(&bytes);
    let hex_buf = hashing::hex_buf(&hash);
    let hash_hex_str = hashing::hex_str(&hex_buf);

    let sidecars_present = extraction_sidecars_present(store, config, hash_hex_str);
    if let Some(existing) = store.lookup(rel)
        && existing.hash_hex == hash_hex_str
        && sidecars_present
        && embed_state_satisfied(store, config, rel, hash_hex_str, embed)
    {
        return FileResult::bare(rel.to_string(), FileStatus::Unchanged);
    }

    let want_l2 = filters.eager_l2 && store.index_db.is_some();

    let reused_pair: Option<(FileMapL1, Option<FileMapL2>)> = if sidecars_present {
        match store.read_l1_by_hex(hash_hex_str) {
            Ok(Some(l1)) => {
                let l2 = if want_l2 {
                    store.read_l2_by_hex(hash_hex_str).unwrap_or(None)
                } else {
                    None
                };
                if want_l2 && l2.is_none() { None } else { Some((l1, l2)) }
            }
            _ => None,
        }
    } else {
        None
    };
    let reused = reused_pair.is_some();

    let (l1, l2_opt): (FileMapL1, Option<FileMapL2>) = match reused_pair {
        Some(pair) => pair,
        None => match extract::extract_l1_l2(lang, &bytes, want_l2) {
            Ok(pair) => pair,
            Err(ExtractError::ParseTimeout(_)) => {
                return FileResult::bare(rel.to_string(), FileStatus::ParseTimedOut);
            }
            Err(source) => {
                return FileResult::bare(
                    rel.to_string(),
                    FileStatus::ExtractFailed {
                        msg: format_extract_err(&source),
                    },
                );
            }
        },
    };

    let l2: Option<FileMapL2> = l2_opt;
    if !reused && let Err(e) = store.write_filemap_hex(hash_hex_str, &l1, l2.as_ref()) {
        return FileResult::bare(rel.to_string(), FileStatus::ExtractFailed { msg: e.to_string() });
    }

    let rel_path = RelPath::from(rel);
    if !index_batch.stage(&rel_path, &l1, l2.as_ref()) {
        tracing::warn!(rel, "index upsert failed; reference search may be incomplete");
    }

    #[cfg(feature = "code-search")]
    let code_batch = if crate::scanner_code::should_chunk(config) {
        match crate::scanner_code::chunk_and_embed(store, rel, &bytes, &l1, l2.as_ref(), hash_hex_str, config, embed) {
            Ok(batch) => batch,
            Err(error) => {
                tracing::debug!(
                    rel,
                    ?error,
                    "code chunk/embed failed; skipping code-search for this file"
                );
                None
            }
        }
    } else {
        None
    };

    #[cfg(feature = "code-search")]
    if let Some(batch) = &code_batch {
        index_batch.stage_bm25(&rel_path, &batch.bm25);
    }

    let entry = FileEntry {
        hash_hex: hash_hex_str.to_string(),
        language: lang.to_string(),
        size_bytes,
        mtime,
    };
    FileResult {
        path: rel.to_string(),
        status: FileStatus::Updated {
            had_errors: l1.had_errors,
            error_count: l1.error_count,
            reused,
        },
        upsert: Some(entry),
        #[cfg(feature = "documents")]
        doc_batch: None,
        #[cfg(feature = "documents")]
        doc_upsert: None,
        #[cfg(feature = "code-search")]
        code_batch,
    }
}

/// Document-tier branch: file had no tree-sitter language; check `[documents]` config and
/// route through xberg. Always returns a `FileResult` — falls back to `SkippedNoLang`
/// when documents are disabled or the MIME type is filtered out.
#[cfg(feature = "documents")]
fn process_doc(
    root: &Path,
    rel: &str,
    filters: &Filters,
    store: &Store,
    config: &Config,
    scope: &str,
    embed: EmbedMode,
) -> FileResult {
    let abs = root.join(rel);
    let effective_scope = crate::scanner_docs::doc_scope_for(rel, scope, config);
    let scope = effective_scope.as_ref();
    let Some(mime_type) = should_extract_document(&abs, &config.documents) else {
        return FileResult::bare(rel.to_string(), FileStatus::SkippedNoLang);
    };
    let (bytes, size_bytes, mtime) = match read_working_tree(root, rel, filters) {
        Ok(triple) => triple,
        Err(status) => return FileResult::bare(rel.to_string(), status),
    };
    let hash = hashing::hash_bytes(&bytes);
    let hex_buf = hashing::hex_buf(&hash);
    let hash_hex = hashing::hex_str(&hex_buf);

    if let Some(existing) = store.lookup_doc(rel)
        && existing.hash_hex == hash_hex
        && existing.embedding_preset == config.documents.embedding_preset
        && store.blob_path_doc_hex(hash_hex).exists()
    {
        return FileResult::bare(rel.to_string(), FileStatus::Unchanged);
    }

    let doc_entry = crate::store::DocEntry {
        hash_hex: hash_hex.to_string(),
        embedding_preset: config.documents.embedding_preset.clone(),
        size_bytes,
        mtime,
    };
    match extract_and_persist_doc(
        store,
        rel,
        &abs,
        &hash,
        &mime_type,
        &config.documents,
        &config.llm,
        scope,
        embed,
    ) {
        Ok(Some(batch)) => {
            let status = FileStatus::DocIndexed {
                chunk_count: batch.chunk_count,
                embedding_dim: batch.embedding_dim,
            };
            let doc_upsert = match embed {
                EmbedMode::Inline => Some(doc_entry),
                EmbedMode::Deferred => None,
            };
            FileResult {
                path: rel.to_string(),
                status,
                upsert: None,
                doc_batch: Some(batch),
                doc_upsert,
                #[cfg(feature = "code-search")]
                code_batch: None,
            }
        }
        Ok(None) => FileResult::bare(rel.to_string(), FileStatus::SkippedNoLang),
        Err(error) => {
            let msg = format!("document extract: {error:#}");
            if is_unsupported_format_error(&msg) {
                tracing::debug!(path = rel, reason = %msg, "skipping file: not an extractable document");
                FileResult::bare(rel.to_string(), FileStatus::SkippedNoLang)
            } else {
                tracing::debug!(path = rel, error = %msg, "document extraction failed");
                FileResult::bare(rel.to_string(), FileStatus::ExtractFailed { msg })
            }
        }
    }
}

/// True when a document-extraction error means the file's format simply has no xberg
/// extractor (xberg's `UnsupportedFormat` → "Unsupported format: <mime>"), as opposed to a
/// genuine extraction failure on a real document. Such files are skipped, not failed.
#[cfg(feature = "documents")]
fn is_unsupported_format_error(msg: &str) -> bool {
    msg.to_ascii_lowercase().contains("unsupported format")
}

/// File mtime as nanoseconds since the Unix epoch (0 when unavailable). Nanosecond resolution — not
/// seconds — so the mtime+size fast-path in `process_file` is effectively race-free: a same-size edit
/// would have to reproduce the exact nanosecond mtime to be missed. The value is only ever compared
/// against a previously-stored one (never displayed), so the unit is a pure internal detail. `i64`
/// nanos overflow in year 2262; saturate rather than wrap.
fn mtime_nanos(metadata: &std::fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn read_working_tree(root: &Path, rel: &str, filters: &Filters) -> Result<(Vec<u8>, u64, i64), FileStatus> {
    let abs = root.join(rel);
    let metadata = std::fs::metadata(&abs).map_err(|e| FileStatus::ReadFailed {
        kind: e.kind(),
        msg: e.to_string(),
    })?;
    if metadata.len() > filters.max_file_bytes {
        return Err(FileStatus::SkippedTooLarge { size: metadata.len() });
    }
    let bytes = std::fs::read(&abs).map_err(|e| FileStatus::ReadFailed {
        kind: e.kind(),
        msg: e.to_string(),
    })?;
    let mtime = mtime_nanos(&metadata);
    let size = metadata.len();
    Ok((bytes, size, mtime))
}

fn read_via_git(filters: &Filters, blob: Result<Option<Vec<u8>>, GitError>) -> Result<(Vec<u8>, u64, i64), FileStatus> {
    let blob = blob.map_err(|e| FileStatus::ReadFailed {
        kind: std::io::ErrorKind::Other,
        msg: e.to_string(),
    })?;
    let bytes = blob.ok_or(FileStatus::ReadFailed {
        kind: std::io::ErrorKind::NotFound,
        msg: "blob not present in this git source".to_string(),
    })?;
    if bytes.len() as u64 > filters.max_file_bytes {
        return Err(FileStatus::SkippedTooLarge {
            size: bytes.len() as u64,
        });
    }
    let size = bytes.len() as u64;
    Ok((bytes, size, 0))
}

fn format_extract_err(e: &ExtractError) -> String {
    e.to_string()
}

/// First-byte heuristic for "definitely not source code": a NUL byte in the first 8 KiB.
/// PNG, ELF, Mach-O, .so/.dylib, .wasm, compiled .pyc/.class, and most archive formats hit
/// this within the first 16 bytes. Source code never contains a NUL byte legitimately. The
/// scan is bounded so we never traverse a multi-megabyte binary just to classify it.
pub fn looks_binary(bytes: &[u8]) -> bool {
    let probe = &bytes[..bytes.len().min(8 * 1024)];
    memchr::memchr(0, probe).is_some()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "documents")]
    #[test]
    fn unsupported_format_error_is_a_skip_not_a_failure() {
        assert!(is_unsupported_format_error(
            "document extract: Unsupported format: application/x-wais-source"
        ));
        assert!(is_unsupported_format_error("Unsupported Format: text/x-foo"));
        assert!(!is_unsupported_format_error(
            "document extract: failed to parse PDF: corrupt xref table"
        ));
        assert!(!is_unsupported_format_error(
            "document extract: OCR engine returned no text"
        ));
    }

    /// The containment fix rests on rayon re-raising a worker's panic on the thread that called
    /// `ThreadPool::install`. If that ever stopped holding, `run_optional_lane` would be catching
    /// nothing and a panicking resolve pass would abort the scan again — so pin it directly.
    #[test]
    fn rayon_install_reraises_a_worker_panic_on_the_calling_thread() {
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            scanner_pool().install(|| {
                (0..64u32).into_par_iter().for_each(|i| {
                    assert_ne!(i, 17, "worker panic");
                });
            });
        }));
        assert!(caught.is_err(), "a panic on a rayon worker must unwind into `install`");
    }

    /// The lane guard is what turns that re-raised panic into a logged degradation: the body runs,
    /// it panics on a worker, and control still returns to the caller.
    #[test]
    fn run_optional_lane_contains_a_panicking_rayon_lane() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let entered = std::sync::Arc::new(AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&entered);
        run_optional_lane("test_lane", move || {
            flag.store(true, Ordering::SeqCst);
            scanner_pool().install(|| {
                (0..64u32).into_par_iter().for_each(|i| {
                    assert_ne!(i, 17, "worker panic");
                });
            });
            unreachable!("the rayon lane above must have panicked");
        });
        assert!(entered.load(Ordering::SeqCst), "the lane body must have run");
    }

    #[test]
    fn looks_binary_detects_nul_in_first_kib() {
        let mut data = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        data.extend_from_slice(&[0; 32]);
        assert!(looks_binary(&data));
    }

    #[test]
    fn looks_binary_accepts_plain_source() {
        assert!(!looks_binary(b"pub fn hello() {}\n"));
        assert!(!looks_binary(b""));
    }

    #[test]
    fn looks_binary_ignores_nul_past_probe_window() {
        let mut data = vec![b'/'; 8 * 1024];
        data.push(0);
        assert!(!looks_binary(&data));
    }
}
