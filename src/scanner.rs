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
            // upsert_file only errors on a Fjall read / decode fault over the existing
            // entries (effectively unreachable on a basemind-written index). The partially
            // staged file rides along in the batch and self-corrects on the next scan via
            // the read-before-write delete — matching the existing "consistent but slightly
            // stale" crash guarantee.
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
fn run_candidates(
    candidates: &[String],
    root: &Path,
    filters: &Filters,
    store: &Store,
    source: &ScanSource<'_>,
    config: &Config,
    scope: &str,
) -> Vec<FileResult> {
    candidates
        .par_iter()
        .fold(
            || WorkerIndexBatch::new(store),
            |mut batch, rel| {
                let result = process_file(root, rel, filters, store, source, config, scope, &mut batch);
                batch.results.push(result);
                batch
            },
        )
        .map(WorkerIndexBatch::finish)
        .reduce(Vec::new, |mut a, mut b| {
            a.append(&mut b);
            a
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
    // Filters work on forward-slash strings; non-UTF-8 submodule roots are extremely rare
    // and lossy here only affects which paths the scanner *skips* (still indexed if lossy).
    paths.into_iter().map(|p| p.to_str_lossy().into_owned()).collect()
}

/// One-shot scan: enumerate every candidate file *via the requested source*, process them
/// in parallel, purge stale index entries, flush the index, return a typed report.
///
/// Source-aware behavior:
/// - `WorkingTree` uses `ignore::WalkBuilder` to walk the on-disk tree and `std::fs::read`.
/// - `Staged` and `Rev` enumerate paths via gix and read bytes via gix.
pub fn scan(root: &Path, store: &mut Store, config: &Config, source: ScanSource<'_>) -> Result<ScanReport, ScanError> {
    let submodule_roots = submodule_roots_for_source(root, &source);
    let filters = Filters::build(config, submodule_roots)?;
    let candidates = candidates_for_source(root, config, &filters, &source)?;
    debug!(count = candidates.len(), kind = source.label(), "scan candidates");

    let scope = derive_scope(root, &source);

    let outcomes: Vec<FileResult> = run_candidates(&candidates, root, &filters, store, &source, config, &scope);

    // Borrow path strings directly from `outcomes` — avoids one String clone per indexed
    // file. The `seen` set is built and consumed before `outcomes` is moved into
    // `apply_outcomes`, so the `&str` borrows remain valid for the full window of use.
    let seen: ahash::AHashSet<&str> = outcomes
        .iter()
        .filter_map(|r| match &r.status {
            FileStatus::Updated { .. } | FileStatus::Unchanged => Some(r.path.as_str()),
            _ => None,
        })
        .collect();

    // Compute stale keys while `outcomes` (and thus `seen`) are still alive, then consume
    // both. `store` is not yet mutably borrowed here, so this read is fine.
    let stale: Vec<String> = store
        .index
        .files
        .keys()
        .filter(|k| !seen.contains(k.to_str_lossy().as_ref()))
        .map(|k| k.to_str_lossy().into_owned())
        .collect();
    // `seen` is no longer needed — drop it explicitly to release the borrows into `outcomes`.
    drop(seen);

    // Doc-tier stale set: a doc is "seen" if it was re-indexed (produced a batch) or skipped
    // unchanged. Any `doc_files` key not seen this scan was deleted or is no longer a doc → its
    // LanceDB rows + tracking entry must be purged. Code-file rels that leak into `doc_seen` are
    // inert (they never match a `doc_files` key). Computed before `apply_outcomes` mutates the store.
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

    // Purge index entries for files no longer present / no longer allowed.
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

    // Second pass: resolve scope/import-bound references now that the primary index is complete.
    // Only for a working-tree scan: the resolve pass reads source bytes + resolves imports against
    // the live filesystem, so on a `Staged`/`Rev` scan those bytes would not match the `FileEntry`
    // blob hash the facts are keyed by (content-addressing violation). The resolved tier targets the
    // working view; other sources skip it rather than persist wrongly-keyed facts. The pass itself
    // (parallel compute + serial staging + the cross-file join) lives in `crate::intel::resolve_pass`.
    if matches!(source, ScanSource::WorkingTree) {
        crate::intel::resolve_pass::resolve_pass(root, store);
    }

    flush_doc_batches_if_any(store, config, &scope, doc_batches);
    flush_code_batches_if_any(store, config, &scope, code_batches);
    flush_code_removals_if_any(store, config, &scope, &stale);
    #[cfg(feature = "documents")]
    flush_doc_removals_if_any(store, config, &scope, &doc_stale);
    finalize_bm25_stats_if_any(store, config);
    store.flush()?;
    Ok(report)
}

/// Incremental scan: process only the given absolute paths. Used by the watcher
/// where the debouncer already told us which files changed.
///
/// Paths outside `root`, inside `.basemind/`, or not matching the include globs are
/// silently dropped (the watcher pre-filters but we re-check defensively).
/// Removed files (path no longer exists) are purged from the index.
pub fn scan_paths(root: &Path, store: &mut Store, config: &Config, paths: &[PathBuf]) -> Result<ScanReport, ScanError> {
    let source = ScanSource::WorkingTree;
    let filter = IndexFilter::new(root, config)?;

    let mut rels: Vec<String> = Vec::with_capacity(paths.len());
    let mut removed: Vec<String> = Vec::new();
    // Removed docs live in `doc_files` (not `files`), so the code `store.lookup` check misses them.
    #[cfg(feature = "documents")]
    let mut doc_removed: Vec<String> = Vec::new();
    for abs in paths {
        let rel = match abs.strip_prefix(root) {
            Ok(p) => p.to_string_lossy().replace('\\', "/"),
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
        // Full indexability: include/exclude globs AND the nested-`.gitignore` hierarchy, matching
        // what a full scan would keep. Prevents the watcher from indexing a gitignored file the
        // full scan skips (and later purges).
        if !filter.is_indexable(abs) {
            continue;
        }
        rels.push(rel);
    }
    rels.sort();
    rels.dedup();

    // Nothing the scanner would index changed and nothing was removed — every event was excluded or
    // gitignored. Skip `run_candidates`, the doc-batch flush, and the unconditional `store.flush()`
    // (a full index re-serialize + fsync). This is the hot no-op path the serve watcher hits on
    // gitignored / nested-`.basemind` churn (issue #33); doing real work here pegged multi-core CPU.
    #[cfg(feature = "documents")]
    let nothing_removed = removed.is_empty() && doc_removed.is_empty();
    #[cfg(not(feature = "documents"))]
    let nothing_removed = removed.is_empty();
    if rels.is_empty() && nothing_removed {
        return Ok(ScanReport::default());
    }

    let scope = derive_scope(root, &source);
    let outcomes: Vec<FileResult> = run_candidates(&rels, root, filter.filters(), store, &source, config, &scope);

    let mut report = ScanReport::default();
    let (doc_batches, code_batches) = apply_outcomes(store, &mut report, outcomes);

    for rel in &removed {
        store.remove(rel);
        if let Some(idx) = store.index_db.as_ref() {
            let mut w = idx.writer();
            let rel = RelPath::from(rel.as_str());
            // Purge the file's resolved edges too. `resolve_pass` recomputes wholesale over the
            // CURRENT file set, so a removed file's stale `refs_by_def` / `refs_by_path` entries are
            // never revisited — they must be dropped explicitly here (mirrors the full `scan`).
            let res = w.remove_file(&rel).and_then(|()| w.remove_resolved_file(&rel));
            #[cfg(feature = "code-search")]
            let res = res.and_then(|()| w.remove_bm25_file(&rel));
            let _ = res.and_then(|()| w.commit());
        }
        report.results.push(FileResult::bare(rel.clone(), FileStatus::Removed));
        report.stats.removed += 1;
    }

    // Second pass: resolve scope/import-bound references, scoped to what actually changed. Only the
    // changed files' intra facts are restaged, and only the affected importer set (changed files
    // plus every file that imports one — the reverse-import invariant) is re-stitched; every other
    // file's resolved edges are left untouched. See `crate::intel::resolve_pass`.
    crate::intel::resolve_pass::resolve_pass_incremental(root, store, &rels);

    flush_doc_batches_if_any(store, config, &scope, doc_batches);
    flush_code_batches_if_any(store, config, &scope, code_batches);
    flush_code_removals_if_any(store, config, &scope, &removed);
    #[cfg(feature = "documents")]
    flush_doc_removals_if_any(store, config, &scope, &doc_removed);
    finalize_bm25_stats_if_any(store, config);
    store.flush()?;
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
            } => {
                report.stats.updated += 1;
                if *had_errors {
                    report.stats.updated_with_warnings += 1;
                }
                // The entry update was already buffered by process_file via the side
                // channel below. We can't safely mutate the store from inside the
                // parallel map, so process_file stashes the entry on the FileResult.
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
        // Pull buffered entry off the result, if any, and upsert it into the index.
        // `.take()` moves the owned `FileEntry` / `PendingDocBatch` out of `o` without the
        // heap clone of `FileEntry.hash_hex` / `FileEntry.language` that a `.clone()` would
        // do — runs once per scanned file, so trimming the alloc adds up across 39 k files.
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
    // For git sources we still apply the configured include/exclude filters so the user can
    // turn things off via `.basemind/basemind.toml`.
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
    let walker = ignore_walk_builder(root, config.scan.respect_gitignore, false).build();
    for dent in walker.flatten() {
        if !dent.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let path = dent.path();
        let rel = match path.strip_prefix(root) {
            Ok(p) => p,
            Err(_) => continue,
        };
        // On Unix / Apple-Silicon paths are always valid UTF-8 and contain no separator
        // backslashes, so `to_str()` (borrow, zero alloc) feeds the filter check directly and the
        // owned `String` is allocated only for the entries that pass — skipping the allocation for
        // the majority excluded by gitignore or the `filters.allows` check. Non-UTF-8 paths are
        // silently skipped. On Windows the walker yields `\`-separated paths, but index keys are
        // `/`-separated (the incremental `scan_paths` path normalizes identically), so normalize
        // there; the extra allocation is Windows-only and never touches the Unix hot path.
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
) -> FileResult {
    // No-op marker to keep the `scope`/`config` params in use when the feature is off.
    #[cfg(not(feature = "documents"))]
    {
        let _ = (config, scope);
    }
    let lang = match lang::detect(Path::new(rel)) {
        Some(l) => l,
        None => {
            #[cfg(feature = "documents")]
            {
                if matches!(source, ScanSource::WorkingTree) {
                    return process_doc(root, rel, filters, store, config, scope);
                }
            }
            return FileResult::bare(rel.to_string(), FileStatus::SkippedNoLang);
        }
    };

    // mtime+size fast-path (WorkingTree only): when the stored entry's size AND mtime both still
    // match the file on disk and its extraction sidecars are present, the content is unchanged —
    // return without reading the bytes or computing the blake3 hash. On a large monorepo this turns a
    // warm rescan of an unchanged file into a single `stat()` instead of a full read + hash.
    //
    // Racy tradeoff: mtime has 1-second resolution, so a same-second edit that keeps the byte size
    // identical is missed until the next content change. The live serve watcher re-indexes real edits
    // from filesystem events (not mtime polling), so this only narrows a full `scan`; git and most
    // build tools accept the same window. Guarded on `mtime != 0` (git sources record 0 = unknown).
    if matches!(source, ScanSource::WorkingTree)
        && let Some(existing) = store.lookup(rel)
        && existing.mtime != 0
        && let Ok(meta) = std::fs::metadata(root.join(rel))
    {
        let mtime = mtime_nanos(&meta);
        if meta.len() == existing.size_bytes
            && mtime == existing.mtime
            && extraction_sidecars_present(store, config, &existing.hash_hex)
        {
            return FileResult::bare(rel.to_string(), FileStatus::Unchanged);
        }
    }

    // Source-aware byte read + size check + mtime.
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

    // Cheap NUL-byte scan in the first 8 KiB — anything that's actually binary (ONGs,
    // .wasm, sourcemaps with embedded base64+NULs, etc.) is filtered before tree-sitter
    // ever sees it. Faster than the SIMD UTF-8 validator and gives a clearer diagnostic
    // (`skipped_binary` vs `skipped_non_utf8`) when the file passes UTF-8 by coincidence.
    if looks_binary(&bytes) {
        return FileResult::bare(rel.to_string(), FileStatus::SkippedBinary);
    }

    if std::str::from_utf8(&bytes).is_err() {
        return FileResult::bare(rel.to_string(), FileStatus::SkippedNonUtf8);
    }

    let hash = hashing::hash_bytes(&bytes);
    // Compare against the stored hash without allocating a String on the common
    // unchanged-file path. `hex_buf` encodes into a stack buffer; `hex_str` borrows it.
    // The owned `String` is deferred to `FileEntry` construction on the actual update path.
    let hex_buf = hashing::hex_buf(&hash);
    let hash_hex_str = hashing::hex_str(&hex_buf);

    // Content-hash unchanged check (the fallback when the mtime+size fast-path above missed — e.g. a
    // touched-but-unchanged file, or mtime unavailable). `extraction_sidecars_present` also enforces
    // that toggling code-search on re-chunks a file rather than skipping it as unchanged with an empty
    // `code_chunks` table.
    if let Some(existing) = store.lookup(rel)
        && existing.hash_hex == hash_hex_str
        && extraction_sidecars_present(store, config, hash_hex_str)
    {
        return FileResult::bare(rel.to_string(), FileStatus::Unchanged);
    }

    let want_l2 = filters.eager_l2 && store.index_db.is_some();

    // Parse once and run both tiers against the shared tree. When eager_l2 is off only L1
    // runs; when it's on L2 runs against the same Tree with no second parse. L2 failure is
    // non-fatal (extract_l1_l2 returns None for the L2 slot rather than propagating).
    let (l1, l2_opt): (FileMapL1, Option<FileMapL2>) = match extract::extract_l1_l2(lang, &bytes, want_l2) {
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
    };

    // Persist both extraction tiers as one content-addressed frame — one `open` + `write` +
    // atomic `rename` instead of a separate write per tier. L1 is essential, so a write
    // failure is fatal for this file (the index stage below is skipped).
    let l2: Option<FileMapL2> = l2_opt;
    if let Err(e) = store.write_filemap_hex(hash_hex_str, &l1, l2.as_ref()) {
        return FileResult::bare(rel.to_string(), FileStatus::ExtractFailed { msg: e.to_string() });
    }

    // Stage the file's symbols / calls / imports into the worker's Fjall write batch. The
    // batch commits every `INDEX_COMMIT_BATCH` files (and once at the worker's end) rather
    // than per file, so workers no longer serialize on Fjall's write lock per file.
    let rel_path = RelPath::from(rel);
    if !index_batch.stage(&rel_path, &l1, l2.as_ref()) {
        tracing::warn!(rel, "index upsert failed; reference search may be incomplete");
    }

    // Code-search tier: chunk + embed from the cached L1/L2 + source bytes (no re-parse). The
    // embed compute runs here in the parallel worker; the LanceDB write is deferred to the serial
    // apply pass. Best-effort — a chunk/embed failure never aborts the file's index update.
    #[cfg(feature = "code-search")]
    let code_batch = if crate::scanner_code::should_chunk(config) {
        match crate::scanner_code::chunk_and_embed(store, rel, &bytes, &l1, l2.as_ref(), hash_hex_str, config) {
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

    // Stage the file's BM25 keyword postings onto the same worker batch as its symbol upsert. Runs
    // whenever chunks were produced — independent of embeddings, so the keyword lane works even with
    // `[code_search] embed = false`.
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
fn process_doc(root: &Path, rel: &str, filters: &Filters, store: &Store, config: &Config, scope: &str) -> FileResult {
    let abs = root.join(rel);
    // External-root docs (absolute key) get their own LanceDB scope keyed by the owning extra
    // root, so out-of-repo documents don't pollute the repository's doc scope. Retrieval is
    // unaffected — `search_documents` has no scope filter — so this only partitions storage.
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

    // Unchanged-skip: identical content, same embedding preset, and the `.doc.msgpack` blob still on
    // disk → nothing to do. Mirrors the code tier's early-return (this function's `store` is an
    // immutable borrow, so the lookup is a cheap read on the parallel worker). Note we DON'T re-stamp
    // `doc_files` here — the prior entry already holds; the doc-seen set (computed in `scan`) keeps it
    // from being pruned.
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
    ) {
        Ok(Some(batch)) => {
            let status = FileStatus::DocIndexed {
                chunk_count: batch.chunk_count,
                embedding_dim: batch.embedding_dim,
            };
            FileResult {
                path: rel.to_string(),
                status,
                upsert: None,
                doc_batch: Some(batch),
                doc_upsert: Some(doc_entry),
                #[cfg(feature = "code-search")]
                code_batch: None,
            }
        }
        Ok(None) => FileResult::bare(rel.to_string(), FileStatus::SkippedNoLang),
        Err(error) => {
            let msg = format!("document extract: {error:#}");
            // "Unsupported format" means xberg has no extractor for this MIME — i.e. the
            // file is not an extractable document (e.g. a source file in a language tree-sitter
            // didn't recognize, which `mime_guess` maps to `application/x-wais-source`). That's a
            // skip, not a failure: it shouldn't inflate the failed count or read as a real error.
            // Genuine extraction errors (corrupt PDF, OCR failure, …) still surface as failures.
            if is_unsupported_format_error(&msg) {
                // Skip, but log it — a non-extractable file shouldn't vanish silently.
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
    // Git sources don't have an mtime. 0 just means "unknown" — the existing hash-equality
    // check is what actually decides whether to re-extract.
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
        // xberg's UnsupportedFormat for a non-document (e.g. an `.app.src` source file that
        // `mime_guess` maps to `application/x-wais-source`) → skip, not a counted failure.
        assert!(is_unsupported_format_error(
            "document extract: Unsupported format: application/x-wais-source"
        ));
        // Case-insensitive on the xberg phrasing.
        assert!(is_unsupported_format_error("Unsupported Format: text/x-foo"));
        // A genuine extraction failure on a real document stays a failure.
        assert!(!is_unsupported_format_error(
            "document extract: failed to parse PDF: corrupt xref table"
        ));
        assert!(!is_unsupported_format_error(
            "document extract: OCR engine returned no text"
        ));
    }

    #[test]
    fn looks_binary_detects_nul_in_first_kib() {
        // Synthetic "PNG-like" prefix.
        let mut data = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        data.extend_from_slice(&[0; 32]);
        assert!(looks_binary(&data));
    }

    #[test]
    fn looks_binary_accepts_plain_source() {
        assert!(!looks_binary(b"pub fn hello() {}\n"));
        assert!(!looks_binary(b"")); // empty is fine, downstream UTF-8 step decides
    }

    #[test]
    fn looks_binary_ignores_nul_past_probe_window() {
        // 8 KiB of clean source, then a NUL — outside the probe window, should not flip.
        let mut data = vec![b'/'; 8 * 1024];
        data.push(0);
        assert!(!looks_binary(&data));
    }
}
