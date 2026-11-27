//! The scanner's per-worker index write batch and the bounds it commits on.
//!
//! Carved out of `scanner_file.rs` (which is near the 1000-line module cap): the per-file
//! pipeline stays there, while the *flush policy* — how much work may sit staged in a Fjall
//! write batch before it must hit disk — lives here, because it changes for a different reason
//! (memory governance) than the pipeline does.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::extract::{FileMapL1, FileMapL2};
use crate::index::keys::ContentKey;
use crate::index::{IndexDb, writer::IndexWriter};
use crate::path::RelPath;
use crate::scanner::{FileResult, FileStatus};
use crate::store::Store;

/// Number of files whose index entries are accumulated into one Fjall write batch before
/// committing. Each `IndexWriter::commit` takes Fjall's single write lock, so committing
/// per file made every rayon worker serialize on that lock (a flamegraph attributed ~14%
/// of scan wall-time to `__psynch_mutexwait` here). Batching `N` files per commit cuts the
/// commit count — and thus the lock-contention — by ~`N`× while keeping each worker's
/// staged work bounded in memory. The per-file read-before-write atomicity is preserved:
/// every file still stages its own deletes+inserts; only the *flush boundary* moved.
///
/// This bound alone is not a memory bound — see [`INDEX_COMMIT_BATCH_BYTES`].
const INDEX_COMMIT_BATCH: usize = 256;

/// Resident staged bytes (see `index::writer::staged_entry_bytes`) a single worker may hold before
/// it must commit, whatever the file count says.
///
/// A file count says nothing about bytes: one file stages every symbol (full msgpack `Symbol`,
/// signature included), every import, call and implementation, one BM25 posting per
/// `(term, chunk)` pair, plus a delete key per pre-existing entry — and fjall keeps all of it
/// resident until the batch commits. A machine-generated 2 MiB source file can stage tens of
/// thousands of entries on its own, so 256 such files per worker times one batch per rayon
/// worker was an unbounded, machine-dependent multiplier (a scan was observed at 43.8 GiB RSS
/// before the daemon was OOM-killed). Committing on *either* bound keeps the common case —
/// small files — on the cheap file-count cadence while capping the pathological one.
///
/// A second, free benefit: `WriteBatch::commit` journals the batch and then runs fjall's own
/// `check_memtable_rotate` + `local_backpressure` for each affected keyspace. Sampling that
/// signal once per 256 files per worker let an entry-dense scan race past fjall asking it to
/// stall; a byte-bounded batch samples it an order of magnitude more often, and bounds the
/// journal write burst to the same budget.
const INDEX_COMMIT_BATCH_BYTES: u64 = 8 * 1024 * 1024;

/// Ceiling on staged-but-uncommitted index bytes summed across *every* live worker.
///
/// [`INDEX_COMMIT_BATCH_BYTES`] alone bounds the scan at `num_cpus × 8 MiB` — still a
/// machine-dependent multiplier that grows exactly on the big machines that scan the big
/// repos. The process-wide ledger closes that: a worker whose own staging took the total over
/// this ceiling commits at that boundary, before it stages another file.
const INDEX_STAGED_BYTES_CEILING: u64 = 64 * 1024 * 1024;

/// How much of its *own* staging a worker must be holding before the shared ceiling may force it
/// to commit. Applies to the ceiling branch only — the per-worker bounds are unconditional.
///
/// Without a floor the ceiling has no hysteresis, and past `INDEX_STAGED_BYTES_CEILING /
/// INDEX_COMMIT_BATCH_BYTES` workers (8, here) the process sits *pinned* at the ceiling: one worker
/// commits, the rest push the total back over within a few files, and the next worker to reach a
/// file boundary commits too. Average staging per commit collapses to `ceiling / workers`, so the
/// commit cadence degrades linearly with core count — 256 files per commit before this batching, but
/// ~34 at 16 workers and ~8 at 64 — which is precisely the fjall write-lock contention the batching
/// exists to remove (a flamegraph put it at ~14% of scan wall-time). Bigger machines would get the
/// worse cadence, which is backwards.
///
/// 1 MiB of *resident staged index* is on the order of a hundred ordinary source files, so the floor
/// keeps any core count at no worse than the 8-worker cadence, while costing at most
/// `workers × (this + one file)` above the ceiling in the worst case — ~64 MiB of overshoot at 64
/// workers, against the 43.8 GiB RSS that motivated the ceiling.
const CEILING_MIN_COMMIT_BYTES: u64 = 1024 * 1024;

/// Lock-free ledger of index bytes staged but not yet committed, summed over the live
/// [`WorkerIndexBatch`]es. Each batch reports only its own delta, and releases its whole
/// contribution on commit *and* on drop.
///
/// The drop release is what makes the ledger safe to hold in a `static`. `run_candidates` is *not*
/// wrapped by `scanner_lanes::run_optional_lane` — only the optional post-extraction lanes are — so
/// a panic in `process_file` unwinds the whole scan. Rayon drops the fold accumulator on that path,
/// and without `Drop` releasing, the dead worker's contribution would stay charged for the lifetime
/// of the *process*: a long-lived daemon that scans repeatedly would shrink its own ceiling scan by
/// scan until every file forced a commit.
pub(crate) struct StagedByteLedger {
    in_flight: AtomicU64,
    ceiling: u64,
    /// Commits performed by the batches reporting here. Carries no policy — it exists so a test can
    /// assert on commit *cadence*, which is the only deterministic way to catch a cadence
    /// regression on a loaded machine (wall-clock deltas are noise).
    commits: AtomicU64,
}

impl StagedByteLedger {
    pub(crate) const fn new(ceiling: u64) -> Self {
        Self {
            in_flight: AtomicU64::new(0),
            ceiling,
            commits: AtomicU64::new(0),
        }
    }

    pub(crate) fn charge(&self, bytes: u64) {
        self.in_flight.fetch_add(bytes, Ordering::Relaxed);
    }

    pub(crate) fn release(&self, bytes: u64) {
        self.in_flight.fetch_sub(bytes, Ordering::Relaxed);
    }

    pub(crate) fn in_flight(&self) -> u64 {
        self.in_flight.load(Ordering::Relaxed)
    }

    pub(crate) fn over_ceiling(&self) -> bool {
        self.in_flight() >= self.ceiling
    }

    fn record_commit(&self) {
        self.commits.fetch_add(1, Ordering::Relaxed);
    }

    #[cfg(test)]
    fn commits(&self) -> u64 {
        self.commits.load(Ordering::Relaxed)
    }
}

/// The scan-wide ledger every real [`WorkerIndexBatch`] reports to. Tests pass their own so the
/// assertions stay hermetic against whatever else the test binary is scanning in parallel.
pub(crate) static SCAN_STAGED_BYTES: StagedByteLedger = StagedByteLedger::new(INDEX_STAGED_BYTES_CEILING);

/// Per-rayon-worker accumulator: buffers each file's index upsert into a shared Fjall write
/// batch and commits once `INDEX_COMMIT_BATCH` files *or* [`INDEX_COMMIT_BATCH_BYTES`] have been
/// staged — or once the process-wide [`StagedByteLedger`] is over its ceiling and this worker holds
/// at least [`CEILING_MIN_COMMIT_BYTES`] of its own — and once more at the end of the worker's
/// slice. Also carries the worker's `FileResult`s so the parallel fold produces both the scan
/// outcomes and the committed index in one pass.
///
/// Borrows `&IndexDb` (cheap `Arc`-backed handle) for the worker's lifetime. When the store
/// has no index (`index_db == None`, read-only mode) staging is a no-op.
pub(crate) struct WorkerIndexBatch<'a> {
    index: Option<&'a IndexDb>,
    writer: Option<IndexWriter>,
    staged: usize,
    /// Bytes this batch has already reported to `ledger`; released in full on commit and on
    /// drop, so the ledger can never drift away from the true in-flight total.
    reported_bytes: u64,
    byte_budget: u64,
    ledger: &'static StagedByteLedger,
    results: Vec<FileResult>,
    /// `results[..settled]` have every index row committed or already disowned; the rest still ride
    /// the open batch.
    settled: usize,
    /// The file being staged lost rows (a failed upsert, or a failed commit of a batch holding some
    /// of them) before its result was pushed, so [`Self::push_result`] must disown it.
    lost_in_flight: bool,
}

/// Withdraw `result`'s claim that its file is indexed: its rows did not reach the index, so the file
/// map must not record it. It is reported as a failed extraction, which leaves it out of the file map
/// and makes the next scan process it again.
fn disown(result: &mut FileResult) {
    if matches!(result.status, FileStatus::Updated { .. }) {
        *result = FileResult::bare(
            std::mem::take(&mut result.path),
            FileStatus::ExtractFailed {
                msg: "index commit failed".to_string(),
            },
        );
    }
}

impl<'a> WorkerIndexBatch<'a> {
    pub(crate) fn new(store: &'a Store) -> Self {
        Self::with_budget(store.index_db.as_ref(), INDEX_COMMIT_BATCH_BYTES, &SCAN_STAGED_BYTES)
    }

    fn with_budget(index: Option<&'a IndexDb>, byte_budget: u64, ledger: &'static StagedByteLedger) -> Self {
        Self {
            index,
            writer: None,
            staged: 0,
            reported_bytes: 0,
            byte_budget,
            ledger,
            results: Vec::new(),
            settled: 0,
            lost_in_flight: false,
        }
    }

    /// Stage one file's symbols / calls (under `content`) / imports into the current batch,
    /// committing afterwards if any bound is now met. Returns `false` only when the upsert itself failed
    /// (caller logs); a `None` index is a successful no-op.
    pub(crate) fn stage(
        &mut self,
        rel: &RelPath,
        content: &ContentKey,
        l1: &FileMapL1,
        l2: Option<&FileMapL2>,
    ) -> bool {
        let Some(index) = self.index else {
            return true;
        };
        let writer = self.writer.get_or_insert_with(|| index.writer());
        // A failed upsert has still staged whatever it wrote before the error (deletes, then ~keep
        // inserts, both before the read or encode that can fail), and those bytes are resident ~keep
        // until the next commit — so they must be reported and must be able to force one. ~keep
        let staged_ok = writer.upsert_file(rel, content, l1, l2).is_ok();
        if staged_ok {
            self.staged += 1;
        } else {
            self.lost_in_flight = true;
        }
        self.commit_if_full();
        staged_ok
    }

    /// Stage one file's BM25 keyword postings into the current batch, reusing the same
    /// [`IndexWriter`] as [`Self::stage`] so the symbol upsert and the keyword postings ride the
    /// same commit where they fit in one. A `None` index is a successful no-op.
    ///
    /// The postings do *not* bump the file counter (the file was already counted by `stage`) but
    /// they DO count against the byte budget and may force a commit. They are the single largest
    /// staged contributor — one `code_bm25_postings` entry per `(term, chunk)` pair plus a
    /// `code_bm25_by_path` value carrying the chunk's full msgpack term list — and this staging
    /// runs on essentially every scan (`code_search.enabled` defaults on, independent of
    /// `embed`). Before this counted, BM25 rode entirely outside the only bound that existed.
    #[cfg(feature = "code-search")]
    pub(crate) fn stage_bm25(
        &mut self,
        rel: &RelPath,
        content: &crate::index::keys::Bm25Key,
        postings: &[crate::search::bm25::ChunkPosting],
    ) {
        let Some(index) = self.index else {
            return;
        };
        let writer = self.writer.get_or_insert_with(|| index.writer());
        if writer.upsert_bm25_file(rel, content, postings).is_err() {
            tracing::warn!(rel = %rel, "bm25 upsert failed; the file is left for the next scan");
            self.lost_in_flight = true;
        }
        self.commit_if_full();
    }

    /// Stage the grep trigram bloom for the content `hash_hex`, whose bytes the scanner already
    /// holds. No extra read: the bloom is built from the very buffer the extractor parsed. Counts
    /// against the byte budget (rows are up to 1/8 of the file) and may force a commit, exactly as
    /// [`Self::stage_bm25`] does. A `None` index is a no-op.
    pub(crate) fn stage_grep_bloom(&mut self, hash_hex: &str, bytes: &[u8]) {
        let Some(index) = self.index else {
            return;
        };
        let row = crate::index::grep_bloom::build_row(bytes);
        self.writer
            .get_or_insert_with(|| index.writer())
            .upsert_grep_bloom(hash_hex, row);
        self.commit_if_full();
    }

    /// [`Self::stage_grep_bloom`] unless the content already has an intact row. For the
    /// content-unchanged-but-restamped path, which is re-entered on every scan until the store's own
    /// entry is refreshed and must not rewrite an identical row each time.
    pub(crate) fn restamp_grep_bloom(&mut self, hash_hex: &str, bytes: &[u8]) {
        if self.index.is_some_and(|index| index.grep_bloom_current(hash_hex)) {
            return;
        }
        self.stage_grep_bloom(hash_hex, bytes);
    }

    /// Backfill the bloom of a file the scan classed `Unchanged` (so its bytes were NOT read): an
    /// index built before blooms existed, or one whose row was swept, gets its row on the next
    /// rescan without a wipe. A current row costs one point lookup; only a missing one costs a read,
    /// once. A file that cannot be read, or whose bytes no longer hash to `hash_hex`, is left
    /// rowless (a candidate, never skipped).
    pub(crate) fn backfill_grep_bloom(&mut self, root: &std::path::Path, rel: &RelPath, hash_hex: &str) {
        use std::io::Read;
        let Some(index) = self.index else {
            return;
        };
        if index.grep_bloom_current(hash_hex) {
            return;
        }
        let Ok(file) = std::fs::File::open(root.join(rel.to_path_buf())) else {
            return;
        };
        let Ok(meta) = file.metadata() else {
            return;
        };
        if !meta.is_file() {
            return;
        }
        let mut bytes = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
        if file.take(meta.len().saturating_add(1)).read_to_end(&mut bytes).is_err() {
            return;
        }
        if crate::hashing::hex(&crate::hashing::hash_bytes(&bytes)) != hash_hex {
            return;
        }
        self.stage_grep_bloom(hash_hex, &bytes);
    }

    /// Report what the writer has staged since the last report to the ledger, and commit when
    /// this worker is over either of its own bounds or the process is over the shared ceiling.
    /// Called only at whole-file staging boundaries, so a commit never splits one upsert.
    fn commit_if_full(&mut self) {
        let staged_bytes = self.writer.as_ref().map_or(0, IndexWriter::staged_bytes);
        self.ledger.charge(staged_bytes.saturating_sub(self.reported_bytes));
        self.reported_bytes = staged_bytes;
        let ceiling_hit = self.ledger.over_ceiling() && staged_bytes >= CEILING_MIN_COMMIT_BYTES;
        if self.staged >= INDEX_COMMIT_BATCH || staged_bytes >= self.byte_budget || ceiling_hit {
            self.commit();
        }
    }

    /// Flush the staged batch under Fjall's write lock and reset both counters. A failed commit
    /// disowns every file with rows in the batch, pushed or still being staged.
    fn commit(&mut self) {
        if let Some(writer) = self.writer.take() {
            self.ledger.record_commit();
            if let Err(error) = writer.commit() {
                if self.index.is_some_and(|index| index.fatal_fault().is_some()) {
                    tracing::error!(%error, "index commit failed fatally (disk full or poisoned handle); the scan will abort");
                } else {
                    tracing::warn!(%error, "index batch commit failed; its files are left for the next scan");
                }
                self.results[self.settled..].iter_mut().for_each(disown);
                self.lost_in_flight = true;
            }
        }
        self.settled = self.results.len();
        self.release_reported();
        self.staged = 0;
    }

    fn release_reported(&mut self) {
        self.ledger.release(self.reported_bytes);
        self.reported_bytes = 0;
    }

    /// Record one file's scan outcome alongside the index work, so the parallel fold carries
    /// both out of the worker in a single pass.
    pub(crate) fn push_result(&mut self, mut result: FileResult) {
        if std::mem::take(&mut self.lost_in_flight) {
            disown(&mut result);
        }
        self.results.push(result);
    }

    /// Commit the trailing partial batch and hand back the worker's results.
    pub(crate) fn finish(mut self) -> Vec<FileResult> {
        self.commit();
        std::mem::take(&mut self.results)
    }
}

impl Drop for WorkerIndexBatch<'_> {
    /// Give back this worker's share of the ledger even when the worker never reaches a commit —
    /// an unwind out of `process_file` drops the fold accumulator, and a leaked contribution
    /// would permanently shrink the shared ceiling until it forced a commit per file.
    fn drop(&mut self) {
        self.release_reported();
    }
}

#[cfg(test)]
#[path = "scanner_index_batch_tests.rs"]
mod tests;
