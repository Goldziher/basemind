use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use thiserror::Error;
use tracing::debug;

use crate::config::Config;
use crate::git::{GitError, Repo};
use crate::scan_evidence::{
    PHASE_CANDIDATES, PHASE_EXTRACT, PHASE_FLUSH, PHASE_LANES, PHASE_PURGE, PHASE_RESOLVE, ScanBreadcrumb,
};
#[cfg(feature = "documents")]
use crate::scan_retrieval::PendingDocBatch;
use crate::scanner_candidates::candidates_for_source;
#[cfg(feature = "code-search")]
use crate::scanner_code::PendingCodeBatch;
use crate::scanner_drive::{DriveOutcome, ProgressObserver, ScanDrive, drive_scan};
use crate::scanner_file::scanner_pool;
use crate::scanner_filter::Filters;
use crate::scanner_flush::{
    REQUEUE_ALL, REQUEUE_PER_INCREMENTAL, derive_scope, flush_code_removals_if_any, flush_vector_lanes,
    maintain_lance_if_open,
};
use crate::scanner_integrity::{abort_on_fault, ensure_space, finish_resolve, flush_code_map, preflight};
use crate::scanner_lanes::{LANE_BM25_STATS, LANE_CODE_REMOVALS, LANE_EMBED_POLICY, LANE_RESOLVE, run_optional_lane};
use crate::store::{FileEntry, ScanHealth, Store, StoreError};

/// The per-file pipeline (`process_file` and the read / classify / extract helpers around it) and
/// the rayon pool it runs on live in their own module to keep this file under the module size cap.
/// [`looks_binary`] is re-exported so callers keep importing it from `crate::scanner`.
pub use crate::scanner_file::looks_binary;

/// The `scan.extra_roots` operator grant lives with the candidate walk it gates; re-exported here
/// so embedders reach it through the scanner's public surface.
pub use crate::scanner_candidates::ALLOW_EXTRA_ROOTS_ENV;

/// The retrieval stack a scan writes through; this build's is `crate::retrieval_wiring::backend`.
pub use crate::scan_retrieval::Retrieval;
/// Where a scan's per-file outcomes go. Replaced `ScanReport.results`, which every caller paid for
/// and only two ever read; the chunked drive loop that keeps live results O(chunk) lives with it.
pub use crate::scanner_drive::{CollectObserver, NullObserver, ScanObserver};
/// The incremental pass over given paths lives in its own module to keep this file under the module
/// size cap; its entry points are re-exported so callers keep importing them from `crate::scanner`.
pub use crate::scanner_incremental::{
    scan_named_paths_with_observer, scan_paths, scan_paths_with_cancel, scan_paths_with_observer,
};

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
    #[error("invalid [languages] config: {0}")]
    BadLanguage(String),
    #[error("git error: {0}")]
    Git(#[from] GitError),
    /// The walk hit `crate::config::ScanConfig::max_candidates`. Raised from the walk loop
    /// itself — before extraction, before any index write — so a pathological root costs one
    /// truncated walk instead of a 40 GiB resident scan (issue #62).
    ///
    /// `candidates` is what the walk actually counted, including the bounded survey it runs past
    /// the ceiling to find the heaviest contributor; it is a lower bound on the true total, never
    /// the ceiling parroted back.
    #[error(
        "scan aborted: {root} yielded at least {candidates} candidate files, over the [scan] \
         max_candidates ceiling of {cap}. Largest contributors: {}. Add them to [scan] exclude or \
         .gitignore, drop the [scan] extra_roots entry that named them, or raise [scan] \
         max_candidates.",
        render_top_dirs(.top_dirs)
    )]
    TooManyCandidates {
        candidates: usize,
        cap: usize,
        root: String,
        top_dirs: Vec<(String, usize)>,
    },
    /// The walk visited far more filesystem entries than it kept — bounded as a multiple of
    /// `max_candidates` — so it never reached the candidate ceiling but was burning the syscalls
    /// and wall time that ceiling exists to prevent. A different failure with a different fix: the
    /// root is too broad, or the excludes are file-shaped (`**/generated`) so whole trees are
    /// walked one file at a time instead of being pruned at the directory (`**/generated/**`).
    #[error(
        "scan aborted: the walk under {root} visited {visited} filesystem entries and kept only \
         {candidates} of them, so it is enumerating a tree it will never finish. Largest \
         contributors: {}. Prune them with directory-shaped patterns (`dir/**`) in [scan] exclude \
         or .gitignore, point the scan at a narrower root, or raise [scan] max_candidates (the \
         walk budget scales with it; {cap} today).",
        render_top_dirs(.top_dirs)
    )]
    WalkTooLarge {
        visited: usize,
        candidates: usize,
        cap: usize,
        root: String,
        top_dirs: Vec<(String, usize)>,
    },
    /// An index commit failed in a way no retry cures: the disk is full, or the index handle is
    /// poisoned. Raised instead of finishing the pass, because every later commit fails the same
    /// way and the result would be an index that reads complete over half-written partitions.
    #[error("scan aborted: the index cannot be written ({0}). Free disk space and rescan.")]
    IndexFault(String),
    /// The volume lacks room for the index; see [`crate::disk_space`].
    #[error("scan refused: {0}")]
    InsufficientSpace(#[from] crate::disk_space::InsufficientSpace),
    /// The OS refused to spawn the scan worker pool's threads; see `crate::scanner_file::scanner_pool`.
    #[error("scan aborted: the scan worker pool could not be started: {0}")]
    WorkerPool(#[from] rayon::ThreadPoolBuildError),
}

/// `node_modules (812431), .cache (194002)` — the contributor list in [`ScanError::TooManyCandidates`].
fn render_top_dirs(top_dirs: &[(String, usize)]) -> String {
    if top_dirs.is_empty() {
        return "(none)".to_string();
    }
    top_dirs
        .iter()
        .map(|(name, count)| format!("{name} ({count})"))
        .collect::<Vec<_>>()
        .join(", ")
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
    /// Subset of `docs_indexed` chunked by the linear fallback instead of xberg's markdown splitter
    /// because the file exceeded `[documents] markdown_chunk_max_bytes` / `plain_text_chunk_max_bytes`.
    pub docs_degraded: usize,
    /// Subset of `docs_degraded` (and of `docs_indexed`): xberg's structured extractor failed with a
    /// parse or security-limit error on a text-like file (a Helm template named `.yaml`, a CSV over the
    /// table-cell limit, an SVG with an oversized entity) and the file was re-read as `text/plain`
    /// through the linear chunker. Distinct from `doc_timeouts` and `extract_failed`.
    pub docs_text_fallback: usize,
    /// Subset of `docs_indexed` with more chunks than `[documents] max_chunks_per_document`: indexed
    /// without vectors, so only the lexical lane finds them.
    pub docs_over_chunk_cap: usize,
    /// Documents abandoned because extraction overran `[documents] extraction_timeout_secs` (also
    /// counted in `extract_failed`). Each is tombstoned, so it is retried only once the file or the
    /// extraction limits change.
    pub doc_timeouts: usize,
    /// Documents skipped without extraction because they already timed out, unchanged, under the same
    /// limits (see [`crate::store_doc_tombstone`]). Not failures: the earlier timeout was already
    /// counted in `doc_timeouts`.
    pub docs_tombstoned: usize,
    /// Documents not extracted this scan because the document-phase budget (`[documents]
    /// phase_budget_secs`) was spent or the abandoned-thread cap (`max_abandoned_extractions`) was
    /// reached. Not failures: the next scan picks them up.
    pub docs_deferred: usize,
    /// Timed-out extraction threads still running in this process when the scan ended (a gauge, not a
    /// per-scan count).
    pub abandoned_extractions: usize,
    /// Subset of `updated` re-extracted only because the entry's blob predated
    /// [`crate::extract::EXTRACT_EPOCH`] (an extractor fix that changes the output of unchanged
    /// files, e.g. shorter symbol signatures). Zero once an index has caught up.
    pub refreshed_extraction: usize,
    /// Paths that changed tier in this pass: a code-mapped path now routed to the document tier
    /// (prose, data and config files), or the reverse. Their old-tier index rows were purged.
    pub tier_migrated: usize,
}

impl ScanStats {
    /// True when this pass displaced blobs, index rows or chunks that no live entry references any
    /// more, so a blob sweep is worth running now rather than at the next maintenance tick.
    pub fn displaced_artifacts(&self) -> bool {
        self.tier_migrated > 0 || self.refreshed_extraction > 0
    }
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
    /// parallel `scanner_docs_lane::process_doc` stashes it; the apply loop drains it into
    /// `index.doc_files` so the next scan can skip unchanged docs and the blob GC keeps the
    /// `.doc.msgpack` cache alive.
    #[cfg(feature = "documents")]
    pub(crate) doc_upsert: Option<crate::store::DocEntry>,
    /// Internal: buffered timeout tombstone for a document that overran its extraction budget; the
    /// apply loop drains it into `index.doc_tombstones`.
    #[cfg(feature = "documents")]
    pub(crate) doc_tombstone: Option<crate::store_doc_tombstone::DocTombstone>,
    /// Internal: buffered code-chunk batch when this source file went through the code-search
    /// branch. Drained by the single-threaded `VectorStore::write_code_rows` pass into LanceDB.
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
            #[cfg(feature = "documents")]
            doc_tombstone: None,
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
    /// The document-tier counterpart of [`FileStatus::Unchanged`]: the doc entry is current. Kept
    /// distinct so the stale purge never mistakes a current document for a current code entry (a
    /// path that changed tier would otherwise keep its old entry alive in the other tier).
    #[cfg(feature = "documents")]
    DocUnchanged,
    Removed,
    SkippedTooLarge {
        size: u64,
    },
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
        /// True when the linear fallback chunker produced the chunks (drives `docs_degraded`).
        degraded: bool,
        /// True when structured extraction failed and the file was re-read as `text/plain` (drives
        /// `docs_text_fallback`; implies `degraded`).
        text_fallback: bool,
        /// True when the document is over `[documents] max_chunks_per_document` and was not embedded
        /// (drives `docs_over_chunk_cap`).
        over_chunk_cap: bool,
        /// Wall-clock milliseconds this document spent in extraction, chunking and persisting.
        elapsed_ms: u64,
    },
    /// Subset of ExtractFailed: document extraction overran `[documents] extraction_timeout_secs`
    /// and was abandoned.
    #[cfg(feature = "documents")]
    DocTimedOut,
    /// The document already timed out, unchanged, under the same extraction limits: skipped without
    /// a retry (counted as `docs_tombstoned`). Editing the file or changing the limits retries it.
    #[cfg(feature = "documents")]
    DocTombstoned,
    /// The document was not extracted because the document-phase budget is spent or the abandoned-thread
    /// cap is reached (counted as `docs_deferred`). Not a failure: the next scan picks it up.
    #[cfg(feature = "documents")]
    DocDeferred,
}

/// Aggregate outcome of one scan. Per-file outcomes are delivered to the caller's
/// [`ScanObserver`] as they are absorbed, never accumulated here — see `crate::scanner_drive`.
#[derive(Debug, Clone, Default)]
pub struct ScanReport {
    pub stats: ScanStats,
    /// True when the scan was interrupted by a [`ScanCancel`] token. The per-file work that
    /// completed before the trip is committed (blobs, fjall batches, `index.msgpack`); everything
    /// else — remaining candidates, the stale purge, the resolve/doc lanes — was skipped, so the
    /// caller must not treat the report as a complete pass.
    pub cancelled: bool,
    /// Peak memory footprint per scan phase (`candidates`, `extract`, `purge`, `flush`, `resolve`,
    /// `lanes` — the document and LanceDB flushes run in `lanes`), in the order the phases ran.
    /// Process-local: not part of any wire protocol.
    pub phase_peaks: Vec<crate::scan_phase_peaks::PhasePeak>,
    /// Dependencies `[documents.dependencies]` names whose documentation was not indexed, and why.
    pub deps_unavailable: Vec<crate::deps_docs::DepUnavailable>,
    /// Bytes this pass wrote to the view's file map: the whole map for a full pass, the changed
    /// entries for an incremental one.
    pub file_map_bytes_written: u64,
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
    retrieval: Retrieval,
) -> Result<ScanReport, ScanError> {
    scan_with_cancel(root, store, config, source, embed, &ScanCancel::new(), retrieval)
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
    retrieval: Retrieval,
) -> Result<ScanReport, ScanError> {
    scan_with_observer(root, store, config, source, embed, cancel, &mut NullObserver, retrieval)
}

/// [`scan_with_cancel`] reporting each file's outcome to `observer` as it is absorbed.
///
/// This is the full-fidelity entry point: `scan` and `scan_with_cancel` are it with a
/// [`NullObserver`]. Per-file results are never accumulated — the observer sees each one inside the
/// drive chunk that produced it and the chunk's results die at its boundary — so a caller that
/// wants the old whole-corpus `Vec` must ask for it explicitly via [`CollectObserver`].
#[allow(clippy::too_many_arguments)]
pub fn scan_with_observer(
    root: &Path,
    store: &mut Store,
    config: &Config,
    source: ScanSource<'_>,
    embed: EmbedMode,
    cancel: &ScanCancel,
    observer: &mut dyn ScanObserver,
    retrieval: Retrieval,
) -> Result<ScanReport, ScanError> {
    // Held for the whole pass and dropped on every exit — the two cancellation returns, the `?` on ~keep
    // a walk that hit the candidate ceiling, an unwinding panic. That is what makes a surviving ~keep
    // record mean "this process never ran a destructor", i.e. a hard kill (issue #62). Only the ~keep
    // full-tree pass gets one: `scan_paths` runs per watcher batch and would churn the file. ~keep
    let mut breadcrumb = ScanBreadcrumb::begin(root);
    // Paired with the breadcrumb, and for the same reason: when the kernel kills the process there
    // is no exit path left to log from, so the growth curve has to have been written down as it
    // happened. The breadcrumb says a scan died here; this says how its memory got there. ~keep
    let phase_peaks = crate::scan_phase_peaks::PhasePeaks::default();
    let _memory_log = crate::scan_evidence::MemoryLog::start(root, phase_peaks.clone());
    let unlisted = preflight(store)?;

    let submodule_roots = submodule_roots_for_source(root, &source);
    let mut filters = Filters::build(config, submodule_roots, retrieval)?;
    let policy = matches!(source, ScanSource::WorkingTree)
        .then(|| crate::scanner_policy::detect(store, config))
        .filter(|p| matches!(embed, EmbedMode::Inline) && p.changed);
    filters.reflush_embeds = policy.as_ref().is_some_and(|p| p.reflush);
    filters.verify_blobs = crate::scan_evidence::blobs_need_verifying(&store.basemind_dir);
    filters.vector_rows_unsure = filters.verify_blobs;
    advance(&mut breadcrumb, &phase_peaks, PHASE_CANDIDATES, None);
    let candidates = candidates_for_source(root, config, &filters, &source, cancel)?;
    debug!(count = candidates.len(), kind = source.label(), "scan candidates");
    crate::scanner_integrity::ensure_scan_space(store, root, &source, &filters, config, &candidates)?;
    store.reap_legacy_index()?;
    advance(&mut breadcrumb, &phase_peaks, PHASE_EXTRACT, Some(candidates.len()));

    let scope = derive_scope(root, &source);
    let mut report = ScanReport::default();
    let file_map_before = store.file_map_bytes_written();
    let drive = ScanDrive {
        root,
        filters: &filters,
        source: &source,
        config,
        scope: &scope,
        embed,
        cancel,
    };
    let bm25_pass = crate::scanner_flush::begin_bm25_pass_if_any(store, config, true);
    let mut progress = ProgressObserver::new(observer, breadcrumb.as_mut());
    let driven = drive_scan(&drive, &candidates, store, &mut report.stats, &mut progress)?;
    let (seen_count, total_candidates) = (driven.seen.len(), candidates.len());
    // Before the cancel check and the stale purge: a drive stopped by a fatal commit failure is a ~keep
    // partial pass too, and must not be flushed or purged as if it were whole. ~keep
    abort_on_fault(store, seen_count, total_candidates)?;

    // Cancelled mid-pass: commit what completed and return BEFORE the stale purge. The purge below ~keep
    // treats every indexed file absent from `driven.seen` as deleted — on a partial pass that would ~keep
    // wipe the entry of every candidate the cancellation skipped. The batch flushes still run: ~keep
    // the drive loop recorded the completed files' entries (embedded=true), so dropping their ~keep
    // LanceDB rows here would leave them tracked-but-rowless — quiescent, never re-flushed. The ~keep
    // rows come from already-persisted blobs (no model inference), and of what earlier scans left ~keep
    // owing only an incremental pass's share is taken, so a draining daemon is not held up. ~keep
    if cancel.is_cancelled() {
        report.cancelled = true;
        flush_code_map(
            store,
            &ScanHealth::partial("cancelled", seen_count, total_candidates),
            false,
        )?;
        let (docs, code) = (driven.doc_batches, driven.code_batches);
        flush_vector_lanes(store, config, &filters, &scope, docs, code, REQUEUE_PER_INCREMENTAL)?;
        phase_peaks.sample_now();
        report.phase_peaks = phase_peaks.snapshot();
        report.file_map_bytes_written = store.file_map_bytes_written() - file_map_before;
        return Ok(report);
    }

    advance(&mut breadcrumb, &phase_peaks, PHASE_PURGE, None);
    let working = store.view == crate::store::VIEW_WORKING && matches!(source, ScanSource::WorkingTree);
    report.deps_unavailable = crate::deps_docs::record(&store.basemind_dir, working, filters.dep_docs());

    // Derived AFTER the drive rather than before it: the entries the drive upserted are all in ~keep
    // `seen`, so they are excluded either way, and reading the file map here keeps the whole ~keep
    // derivation out of the per-file results' lifetime. ~keep
    let stale: Vec<String> = store
        .index
        .files
        .keys()
        .filter(|k| !driven.seen.contains(k.to_str_lossy().as_ref()))
        .map(|k| k.to_str_lossy().into_owned())
        .collect();

    #[cfg(feature = "documents")]
    let doc_stale: Vec<String> = store
        .index
        .doc_files
        .keys()
        .filter(|k| !driven.doc_seen.contains(k.to_str_lossy().as_ref()))
        .map(|k| k.to_str_lossy().into_owned())
        .collect();

    // A tombstone outlives the failed extraction on purpose, but not the file: drop the ones whose path
    // is no longer a candidate of this full pass.
    #[cfg(feature = "documents")]
    if !store.index.doc_tombstones.is_empty() {
        let live: ahash::AHashSet<&str> = candidates.iter().map(String::as_str).collect();
        store.retain_doc_tombstones(|rel| live.contains(rel.to_str_lossy().as_ref()));
    }

    // A path in both "stale code entry" and "seen as document" (or the reverse) changed tier. Counted
    // here, where both sets are still alive, so the report can say a migration happened.
    #[cfg(feature = "documents")]
    {
        report.stats.tier_migrated = stale.iter().filter(|k| driven.doc_seen.contains(k.as_str())).count()
            + doc_stale.iter().filter(|k| driven.seen.contains(k.as_str())).count();
    }

    // The path sets have done their job; only the metadata-only batch descriptors outlive them.
    let DriveOutcome {
        doc_batches,
        code_batches,
        ..
    } = driven;

    let (purged, purge_failure) = crate::scanner_integrity::purge_files(store, &stale);
    for k in purged {
        observer.on_file(FileResult::bare(k.clone(), FileStatus::Removed));
        report.stats.removed += 1;
    }
    let purge_failure = purge_failure.or_else(|| unlisted.purge(store).err());
    // Only a working-tree pass writes bloom rows; a rev or staged pass would sweep them all away.
    if matches!(source, ScanSource::WorkingTree) {
        crate::scanner_integrity::sweep_grep_bloom_rows(store);
    }

    if let Some(change) = &policy {
        // Recorded only when the purge ran to completion (a panic leaves `applied` false too), so a
        // failed purge is retried by the next scan instead of being marked done.
        let mut applied = false;
        run_optional_lane(LANE_EMBED_POLICY, || {
            applied = crate::scanner_policy::reconcile(store, config, &filters, &scope);
        });
        if applied {
            crate::scanner_policy::record(store, change);
        }
    }

    advance(&mut breadcrumb, &phase_peaks, PHASE_FLUSH, None);
    // A whole pass re-indexed every file: the rebuild the emptied file map asked for is done. ~keep
    store.index.rebuild_done();
    store.index_rebuild_needed = false;
    let health = match &purge_failure {
        Some(error) => ScanHealth::partial(format!("purge failed: {error}"), seen_count, total_candidates),
        None => ScanHealth::resolving(),
    };
    flush_code_map(store, &health, purge_failure.is_none())?;
    if let Some(error) = purge_failure {
        return Err(error);
    }

    // A token tripped after the last per-file check still stops the pass here, once the code map ~keep
    // is durable but before the resolve pass (which can run for minutes on a large repo). The ~keep
    // batch flushes still run — the completed files' entries are already recorded, and dropping ~keep
    // their rows would leave them tracked-but-rowless (blob-sourced, no inference; bounded). ~keep
    if cancel.is_cancelled() {
        report.cancelled = true;
        flush_vector_lanes(
            store,
            config,
            &filters,
            &scope,
            doc_batches,
            code_batches,
            REQUEUE_PER_INCREMENTAL,
        )?;
        phase_peaks.sample_now();
        report.phase_peaks = phase_peaks.snapshot();
        report.file_map_bytes_written = store.file_map_bytes_written() - file_map_before;
        return Ok(report);
    }

    if matches!(source, ScanSource::WorkingTree) {
        advance(&mut breadcrumb, &phase_peaks, PHASE_RESOLVE, None);
        ensure_space(store)?;
        let precise = config.code_intel.precise_resolution;
        let mut resolved = false;
        let pool = scanner_pool(config.resources.scan_threads)?;
        run_optional_lane(LANE_RESOLVE, || {
            resolved = pool.install(|| {
                crate::intel::resolve_pass::resolve_pass(root, store, precise, config.resources.max_footprint_mb)
            });
        });
        finish_resolve(store, resolved)?;
    }

    advance(&mut breadcrumb, &phase_peaks, PHASE_LANES, None);
    flush_vector_lanes(store, config, &filters, &scope, doc_batches, code_batches, REQUEUE_ALL)?;
    run_optional_lane(LANE_CODE_REMOVALS, || {
        flush_code_removals_if_any(store, config, &filters, &scope, &stale);
    });
    #[cfg(feature = "documents")]
    crate::scanner_integrity::remove_docs_and_flush(store, config, &filters, &scope, &doc_stale)?;
    run_optional_lane(LANE_BM25_STATS, || {
        crate::scanner_flush::finish_bm25_pass(store, config, bm25_pass, true)
    });
    run_optional_lane(crate::scanner_lanes::LANE_GROUP_SWEEP, || store.sweep_group_logged());
    run_optional_lane(crate::scanner_lanes::LANE_LANCE_MAINTENANCE, || {
        maintain_lance_if_open(store, &filters)
    });
    crate::scanner_integrity::finish_index_writes(store);
    // A whole working-tree pass has now validated every blob it kept. ~keep
    if filters.verify_blobs && matches!(source, ScanSource::WorkingTree) {
        crate::scan_evidence::blobs_verified(&store.basemind_dir);
    }
    phase_peaks.sample_now();
    report.phase_peaks = phase_peaks.snapshot();
    report.file_map_bytes_written = store.file_map_bytes_written() - file_map_before;
    Ok(report)
}

/// Move the pass's breadcrumb (and the per-phase memory tally) to `phase`, if one could be written at all. Folding the `Option` in
/// here keeps every phase boundary a single line and keeps "no breadcrumb" from being a condition
/// the scan branches on: evidence is never a precondition for the work.
fn advance(
    breadcrumb: &mut Option<ScanBreadcrumb>,
    peaks: &crate::scan_phase_peaks::PhasePeaks,
    phase: &'static str,
    candidates: Option<usize>,
) {
    peaks.enter(phase);
    if let Some(crumb) = breadcrumb.as_mut() {
        crumb.advance(phase, candidates);
    }
}
