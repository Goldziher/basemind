//! The primary scan's chunked drive loop, its memory governor, and the observer that replaced
//! `ScanReport.results`.
//!
//! ## What this module removes
//!
//! The scan used to be one `run_candidates` call over the whole corpus: every candidate's
//! [`FileResult`] — including its staged `FileEntry`, its pending document batch and its pending
//! code-chunk batch — was materialised into a single `Vec` and only then drained. Peak live
//! per-file state was therefore O(corpus), on a path (`WorkspacePool::rescan`) that discards every
//! one of those results. Issue #62 is what that costs.
//!
//! Here the candidates are driven through [`crate::chunk_drive`] in chunks: rayon still runs the
//! whole chunk in parallel, but the chunk's results are absorbed and dropped before the next chunk
//! is computed, so live `FileResult`s are O(chunk). What survives a chunk is only what the stale
//! purge and the post-barrier lanes genuinely need — path sets and metadata-only batch descriptors.
//!
//! ## Advisory at the leaves, actuating here
//!
//! [`crate::backpressure::FootprintGate`] parks a worker while the process is over its memory
//! ceiling and, after `DEFAULT_MAX_WAIT`, admits anyway. At a leaf (a single document extraction, a
//! single large-file parse) that is the only defensible policy: refusing would fail the scan, and
//! there is no smaller unit of work to fall back to.
//!
//! At the drive loop there *is* an alternative, so waiting out the gate here is not merely
//! observed, it is acted on: [`DriveGovernor`] halves the chunk size and the effective worker count
//! on every [`AdmitOutcome::WaitedOut`], and ratchets both back toward their baselines after
//! [`RECOVERY_CLEAR_ADMITS`] consecutive clear admits. A scan that cannot hold its ceiling therefore
//! converges on a narrower, slower pass instead of a dead one.
//!
//! The admit runs between chunks, holding no index batch and staging nothing — parking inside the
//! per-file Fjall staging would turn a memory problem into a liveness problem.

use std::cell::Cell;
use std::path::Path;

use crate::backpressure::{AdmitOutcome, FootprintGate};
use crate::chunk_drive::{ChunkCut, drive_chunks_governed};
use crate::config::{Config, MaxFootprint};
use crate::scanner::{FileResult, FileStatus, ScanCancel, ScanSource, ScanStats};
use crate::scanner_file::{run_candidates, scanner_pool};
use crate::scanner_filter::Filters;
use crate::store::Store;

/// Alias that's `PendingDocBatch` under the `documents` feature and `()` otherwise. Lets the drive
/// loop keep one signature while still carrying real values when the feature is on.
#[cfg(feature = "documents")]
pub(crate) type PendingDocBatchOpt = crate::scanner_docs::PendingDocBatch;
#[cfg(not(feature = "documents"))]
pub(crate) type PendingDocBatchOpt = ();

/// Alias that's `PendingCodeBatch` under the `code-search` feature and `()` otherwise.
#[cfg(feature = "code-search")]
pub(crate) type PendingCodeBatchOpt = crate::scanner_code::PendingCodeBatch;
#[cfg(not(feature = "code-search"))]
pub(crate) type PendingCodeBatchOpt = ();

/// Where a scan's per-file outcomes go.
///
/// `ScanReport` used to carry a `Vec<FileResult>` that every caller paid for and only two ever
/// read. An observer moves that choice to the caller: `basemind scan` streams each line straight to
/// stdout, the MCP rescan projects paths out of it, and the daemon's `WorkspacePool::rescan` — the
/// path issue #62 was filed against — installs [`NullObserver`] and materialises nothing at all.
///
/// A result reaches the observer *after* its buffered payloads (the index entry, the document /
/// code batches) have been drained into the store, so an observer that retains results retains only
/// the path and the status.
pub trait ScanObserver {
    /// One file's outcome.
    fn on_file(&mut self, result: FileResult);

    /// End of one drive chunk; `live_results` is how many [`FileResult`]s that chunk held live at
    /// once. The bound this module exists to enforce is observable through exactly this hook.
    fn on_batch(&mut self, live_results: usize) {
        let _ = live_results;
    }
}

/// Drops every result. The right observer for any caller that reads only `ScanReport.stats`.
pub struct NullObserver;

impl ScanObserver for NullObserver {
    fn on_file(&mut self, _result: FileResult) {}
}

/// Retains every result, reproducing the old `ScanReport.results` for callers that genuinely want
/// the whole list (the watcher's per-batch rendering, tests). O(corpus) by construction — that is
/// the cost the trait exists to make explicit rather than universal.
#[derive(Debug, Default)]
pub struct CollectObserver {
    results: Vec<FileResult>,
}

impl CollectObserver {
    pub fn new() -> Self {
        Self::default()
    }

    /// The results collected so far.
    pub fn results(&self) -> &[FileResult] {
        &self.results
    }

    /// Take the collected results, leaving the observer empty.
    pub fn take(&mut self) -> Vec<FileResult> {
        std::mem::take(&mut self.results)
    }
}

impl ScanObserver for CollectObserver {
    fn on_file(&mut self, result: FileResult) {
        self.results.push(result);
    }
}

/// Consecutive clear admits before the governor widens one step back toward its baseline. Four is
/// long enough that a single lucky sample cannot undo a narrowing, short enough that a transient
/// spike (one oversized document, one embedding batch) does not cost the rest of the scan its
/// parallelism.
const RECOVERY_CLEAR_ADMITS: usize = 4;

/// Floor on the drive chunk, independent of the worker count: below this the serial absorb pass
/// dominates and the drive stops being a parallel scan at all.
const MIN_BASE_CHUNK_ITEMS: usize = 1024;

/// Chunk items per worker at the baseline. Enough work in flight that rayon can balance a chunk
/// whose files differ by orders of magnitude in parse cost.
const CHUNK_ITEMS_PER_WORKER: usize = 64;

/// The baseline chunk size for a pool of `workers` threads.
fn base_chunk_items(workers: usize) -> usize {
    MIN_BASE_CHUNK_ITEMS.max(workers.saturating_mul(CHUNK_ITEMS_PER_WORKER))
}

/// How the next chunk should be shaped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DrivePlan {
    /// Maximum candidates in the chunk.
    pub(crate) items: usize,
    /// Effective worker ceiling for the chunk. Equal to the pool size when unthrottled.
    pub(crate) workers: usize,
}

/// The actuating half of the memory ceiling: samples the footprint gate between chunks and turns a
/// sustained overshoot into a narrower chunk and fewer concurrent workers.
///
/// Generic over the gate's sampler so tests drive the over/under transition deterministically.
pub(crate) struct DriveGovernor<S = fn() -> Option<u64>>
where
    S: Fn() -> Option<u64>,
{
    gate: FootprintGate<S>,
    base_items: usize,
    base_workers: usize,
    items: usize,
    workers: usize,
    clear_streak: usize,
}

impl DriveGovernor {
    /// A governor for the `[resources] max_footprint_mb` setting, baselined at `base_items`
    /// candidates per chunk across `base_workers` pool threads.
    pub(crate) fn new(setting: MaxFootprint, base_items: usize, base_workers: usize) -> Self {
        Self::with_gate(FootprintGate::new(setting), base_items, base_workers)
    }
}

impl<S> DriveGovernor<S>
where
    S: Fn() -> Option<u64>,
{
    pub(crate) fn with_gate(gate: FootprintGate<S>, base_items: usize, base_workers: usize) -> Self {
        let base_items = base_items.max(1);
        let base_workers = base_workers.max(1);
        Self {
            gate,
            base_items,
            base_workers,
            items: base_items,
            workers: base_workers,
            clear_streak: 0,
        }
    }

    /// Admit the next chunk, returning its shape.
    ///
    /// `WaitedOut` — the gate gave up waiting and admitted anyway — is the only outcome that
    /// narrows: the process is over its ceiling and staying there, so the next chunk must be
    /// cheaper. `Throttled` (over, then under within the wait) holds steady: the gate already did
    /// its job and shrinking would pay parallelism for a spike that has passed. Everything else
    /// counts toward recovery.
    pub(crate) fn admit(&mut self) -> DrivePlan {
        match self.gate.admit() {
            AdmitOutcome::WaitedOut => self.narrow(),
            AdmitOutcome::Throttled => self.clear_streak = 0,
            AdmitOutcome::Clear | AdmitOutcome::Disabled | AdmitOutcome::Unavailable => self.widen(),
        }
        DrivePlan {
            items: self.items,
            workers: self.workers,
        }
    }

    fn narrow(&mut self) {
        self.items = (self.items / 2).max(1);
        self.workers = (self.workers / 2).max(1);
        self.clear_streak = 0;
        tracing::warn!(
            chunk_items = self.items,
            workers = self.workers,
            "scan over its memory ceiling for the full admit wait; narrowing the drive chunk"
        );
    }

    fn widen(&mut self) {
        if self.items >= self.base_items && self.workers >= self.base_workers {
            return;
        }
        self.clear_streak += 1;
        if self.clear_streak < RECOVERY_CLEAR_ADMITS {
            return;
        }
        self.clear_streak = 0;
        self.items = self.items.saturating_mul(2).min(self.base_items);
        self.workers = self.workers.saturating_mul(2).min(self.base_workers);
    }
}

/// The parts of a scan invocation the drive loop needs but never mutates.
pub(crate) struct ScanDrive<'a> {
    pub(crate) root: &'a Path,
    pub(crate) filters: &'a Filters,
    pub(crate) source: &'a ScanSource<'a>,
    pub(crate) config: &'a Config,
    pub(crate) scope: &'a str,
    pub(crate) embed: crate::scanner::EmbedMode,
    pub(crate) cancel: &'a ScanCancel,
}

/// What the drive loop hands back: the deferred batch descriptors, plus the path sets the stale
/// purge derives from. Every field is a *projection* of the per-file results — no field can hold a
/// `FileResult`, which is what keeps the O(corpus) part unrepresentable past a chunk boundary.
#[derive(Default)]
pub(crate) struct DriveOutcome {
    pub(crate) doc_batches: Vec<PendingDocBatchOpt>,
    pub(crate) code_batches: Vec<PendingCodeBatchOpt>,
    /// Paths this pass indexed or confirmed unchanged; anything in the file map and not here is
    /// stale.
    pub(crate) seen: ahash::AHashSet<String>,
    /// The document-tier counterpart of `seen`.
    #[cfg(feature = "documents")]
    pub(crate) doc_seen: ahash::AHashSet<String>,
}

/// Everything the absorb pass mutates, threaded through the driver rather than captured — the
/// parallel `process` half needs `&Store` at the same time this half needs `&mut Store`.
struct DriveCtx<'a> {
    store: &'a mut Store,
    stats: &'a mut ScanStats,
    observer: &'a mut dyn ScanObserver,
    out: DriveOutcome,
}

/// Drive `candidates` through the per-file pipeline in memory-bounded chunks.
///
/// Returns once every candidate has been processed, or early when `drive.cancel` trips — the
/// caller must then skip the stale purge, because the unprocessed candidates are missing from
/// [`DriveOutcome::seen`] and would be mistaken for deletions.
pub(crate) fn drive_scan(
    drive: &ScanDrive<'_>,
    candidates: &[String],
    store: &mut Store,
    stats: &mut ScanStats,
    observer: &mut dyn ScanObserver,
) -> DriveOutcome {
    let pool_threads = scanner_pool(drive.config.resources.scan_threads).current_num_threads();
    let mut governor = DriveGovernor::new(
        drive.config.resources.max_footprint_mb,
        base_chunk_items(pool_threads),
        pool_threads,
    );
    // The governor decides the worker ceiling in `next_cut`; `process` reads it one statement ~keep
    // later on the same thread. A `Cell` passes it across without either closure having to ~keep
    // borrow the governor, which only one of them may. ~keep
    let workers = Cell::new(pool_threads);
    let mut ctx = DriveCtx {
        store,
        stats,
        observer,
        out: DriveOutcome::default(),
    };

    drive_chunks_governed(
        &mut ctx,
        candidates,
        || {
            if drive.cancel.is_cancelled() {
                return None;
            }
            let plan = governor.admit();
            workers.set(plan.workers);
            Some(ChunkCut::new(plan.items, u64::MAX))
        },
        // Chunks are cut on count alone: a candidate's cost is not knowable before it is read, and
        // the bytes a chunk touches are held by at most one worker each, never by the chunk.
        |_, _| 0,
        |ctx: &DriveCtx<'_>, chunk| {
            let max_workers = if workers.get() >= pool_threads {
                0
            } else {
                workers.get()
            };
            run_candidates(
                chunk,
                drive.root,
                drive.filters,
                ctx.store,
                drive.source,
                drive.config,
                drive.scope,
                drive.embed,
                drive.cancel,
                max_workers,
            )
        },
        |ctx: &mut DriveCtx<'_>, produced| apply_outcomes(ctx, produced),
    );

    ctx.out
}

/// Drain one chunk's parallel-map results into the single-threaded store, the stats, and the
/// observer. The `Vec<FileResult>` dies with this call — that is the whole bound.
fn apply_outcomes(ctx: &mut DriveCtx<'_>, outcomes: Vec<FileResult>) {
    ctx.observer.on_batch(outcomes.len());
    for mut o in outcomes {
        ctx.stats.scanned += 1;
        match &o.status {
            FileStatus::Updated {
                had_errors,
                error_count: _,
                reused,
            } => {
                ctx.stats.updated += 1;
                if *had_errors {
                    ctx.stats.updated_with_warnings += 1;
                }
                if *reused {
                    ctx.stats.reused_extraction += 1;
                }
            }
            FileStatus::Unchanged => ctx.stats.skipped_unchanged += 1,
            FileStatus::SkippedTooLarge { .. } => ctx.stats.skipped_too_large += 1,
            FileStatus::SkippedNonUtf8 => ctx.stats.skipped_non_utf8 += 1,
            FileStatus::SkippedNoLang => ctx.stats.skipped_no_lang += 1,
            FileStatus::SkippedBinary => ctx.stats.skipped_binary += 1,
            FileStatus::Removed => ctx.stats.removed += 1,
            FileStatus::ReadFailed { .. } => ctx.stats.read_failed += 1,
            FileStatus::ExtractFailed { .. } => ctx.stats.extract_failed += 1,
            FileStatus::ParseTimedOut => {
                ctx.stats.extract_failed += 1;
                ctx.stats.parse_timeouts += 1;
            }
            #[cfg(feature = "documents")]
            FileStatus::DocIndexed { reused, .. } => {
                ctx.stats.docs_indexed += 1;
                if *reused {
                    ctx.stats.reused_doc_extraction += 1;
                }
            }
        }
        if matches!(o.status, FileStatus::Updated { .. } | FileStatus::Unchanged) {
            ctx.out.seen.insert(o.path.clone());
        }
        // A `DocIndexed` status is set by exactly the `process_doc` arm that attaches a doc batch, ~keep
        // so the status is a faithful stand-in for a `doc_batch` this loop is about to take. ~keep
        #[cfg(feature = "documents")]
        if matches!(
            o.status,
            FileStatus::DocIndexed { .. } | FileStatus::Updated { .. } | FileStatus::Unchanged
        ) {
            ctx.out.doc_seen.insert(o.path.clone());
        }
        if let Some(entry) = o.upsert.take() {
            ctx.store.upsert(&o.path, entry);
        }
        #[cfg(feature = "documents")]
        if let Some(entry) = o.doc_upsert.take() {
            ctx.store.upsert_doc(&o.path, entry);
        }
        #[cfg(feature = "documents")]
        if let Some(batch) = o.doc_batch.take() {
            ctx.out.doc_batches.push(batch);
        }
        #[cfg(feature = "code-search")]
        if let Some(batch) = o.code_batch.take() {
            ctx.out.code_batches.push(batch);
        }
        ctx.observer.on_file(FileResult::bare(o.path, o.status));
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    const MB: u64 = 1024 * 1024;

    /// A governor whose gate is always over the ceiling, with a wait short enough to test.
    fn always_over(base_items: usize, base_workers: usize) -> DriveGovernor<fn() -> Option<u64>> {
        let sampler: fn() -> Option<u64> = || Some(u64::MAX);
        DriveGovernor::with_gate(
            FootprintGate::with_sampler(1, sampler).with_timing(Duration::from_millis(1), Duration::from_millis(2)),
            base_items,
            base_workers,
        )
    }

    /// A governor whose gate is always clear.
    fn always_clear(base_items: usize, base_workers: usize) -> DriveGovernor<fn() -> Option<u64>> {
        let sampler: fn() -> Option<u64> = || Some(MB);
        DriveGovernor::with_gate(FootprintGate::with_sampler(4096, sampler), base_items, base_workers)
    }

    #[test]
    fn the_baseline_chunk_is_a_thousand_items_or_sixty_four_per_worker() {
        assert_eq!(base_chunk_items(1), 1024);
        assert_eq!(base_chunk_items(8), 1024);
        assert_eq!(base_chunk_items(16), 1024);
        assert_eq!(base_chunk_items(32), 2048);
    }

    /// F4: waiting out the gate is *acted on*, not merely logged — chunk and worker budgets halve
    /// on every sustained overshoot and bottom out at one rather than at zero.
    #[test]
    fn waiting_out_the_gate_halves_the_chunk_and_the_workers() {
        let mut governor = always_over(1024, 16);
        let plans: Vec<DrivePlan> = (0..6).map(|_| governor.admit()).collect();
        let items: Vec<usize> = plans.iter().map(|p| p.items).collect();
        let workers: Vec<usize> = plans.iter().map(|p| p.workers).collect();
        assert_eq!(items, vec![512, 256, 128, 64, 32, 16]);
        assert_eq!(workers, vec![8, 4, 2, 1, 1, 1]);
    }

    #[test]
    fn a_clear_gate_never_narrows_below_the_baseline() {
        let mut governor = always_clear(1024, 16);
        for _ in 0..8 {
            assert_eq!(
                governor.admit(),
                DrivePlan {
                    items: 1024,
                    workers: 16
                }
            );
        }
    }

    /// Recovery is a ratchet, not a snap-back: it takes `RECOVERY_CLEAR_ADMITS` clear admits per
    /// doubling, and it never overshoots the baseline.
    #[test]
    fn the_governor_ratchets_back_up_after_a_run_of_clear_admits() {
        let mut governor = always_over(1024, 16);
        governor.admit();
        governor.admit();
        assert_eq!(governor.items, 256);
        assert_eq!(governor.workers, 4);

        let clear: fn() -> Option<u64> = || Some(MB);
        governor.gate = FootprintGate::with_sampler(4096, clear);
        for _ in 0..RECOVERY_CLEAR_ADMITS - 1 {
            assert_eq!(governor.admit().items, 256, "a partial streak must not widen");
        }
        assert_eq!(governor.admit().items, 512);
        for _ in 0..RECOVERY_CLEAR_ADMITS {
            governor.admit();
        }
        assert_eq!(
            governor.admit(),
            DrivePlan {
                items: 1024,
                workers: 16
            }
        );
    }

    /// The composition the scanner actually uses: governor → `next_cut` → `drive_chunks_governed`.
    /// Asserted on the chunk lengths the driver cut, never on timing.
    #[test]
    fn a_governed_drive_narrows_its_chunks_under_sustained_pressure() {
        let items: Vec<u32> = (0..65).collect();
        let mut governor = always_over(64, 8);
        let mut lengths: Vec<usize> = Vec::new();
        drive_chunks_governed(
            &mut lengths,
            &items,
            || Some(ChunkCut::new(governor.admit().items, u64::MAX)),
            |_, _| 0,
            |_, chunk| vec![chunk.len()],
            |lengths: &mut Vec<usize>, produced| lengths.extend(produced),
        );
        assert_eq!(lengths, vec![32, 16, 8, 4, 2, 1, 1, 1]);
        assert_eq!(lengths.iter().sum::<usize>(), 65, "the drive stops when items run out");
    }
}
