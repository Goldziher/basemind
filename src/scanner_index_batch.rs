//! The scanner's per-worker index write batch and the bounds it commits on.
//!
//! Carved out of `scanner_file.rs` (which is near the 1000-line module cap): the per-file
//! pipeline stays there, while the *flush policy* — how much work may sit staged in a Fjall
//! write batch before it must hit disk — lives here, because it changes for a different reason
//! (memory governance) than the pipeline does.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::extract::{FileMapL1, FileMapL2};
use crate::index::{IndexDb, writer::IndexWriter};
use crate::path::RelPath;
use crate::scanner::FileResult;
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
        }
    }

    /// Stage one file's symbols / calls / imports into the current batch, committing afterwards
    /// if any bound is now met. Returns `false` only when the upsert itself failed
    /// (caller logs); a `None` index is a successful no-op.
    pub(crate) fn stage(&mut self, rel: &RelPath, l1: &FileMapL1, l2: Option<&FileMapL2>) -> bool {
        let Some(index) = self.index else {
            return true;
        };
        let writer = self.writer.get_or_insert_with(|| index.writer());
        // A failed upsert has still staged whatever it wrote before the error (deletes, then ~keep
        // inserts, both before the read or encode that can fail), and those bytes are resident ~keep
        // until the next commit — so they must be reported and must be able to force one. ~keep
        let staged_ok = writer.upsert_file(rel, l1, l2).is_ok();
        if staged_ok {
            self.staged += 1;
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
    pub(crate) fn stage_bm25(&mut self, rel: &RelPath, postings: &[crate::search::bm25::ChunkPosting]) {
        let Some(index) = self.index else {
            return;
        };
        let writer = self.writer.get_or_insert_with(|| index.writer());
        if writer.upsert_bm25_file(rel, postings).is_err() {
            tracing::warn!(rel = %rel, "bm25 upsert failed; keyword search may be incomplete");
        }
        self.commit_if_full();
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

    /// Flush the staged batch under Fjall's write lock and reset both counters.
    fn commit(&mut self) {
        if let Some(writer) = self.writer.take() {
            self.ledger.record_commit();
            if writer.commit().is_err() {
                tracing::warn!("index batch commit failed; reference search may be incomplete");
            }
        }
        self.release_reported();
        self.staged = 0;
    }

    fn release_reported(&mut self) {
        self.ledger.release(self.reported_bytes);
        self.reported_bytes = 0;
    }

    /// Record one file's scan outcome alongside the index work, so the parallel fold carries
    /// both out of the worker in a single pass.
    pub(crate) fn push_result(&mut self, result: FileResult) {
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
mod tests {
    use super::*;
    use crate::extract::{Symbol, SymbolKind};

    fn fresh_index() -> (tempfile::TempDir, IndexDb) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = IndexDb::open(dir.path()).expect("open index");
        (dir, db)
    }

    /// One file's worth of symbols, sized so a handful of files blow a small byte budget while
    /// staying far under [`INDEX_COMMIT_BATCH`] files.
    fn bulky_l1(symbols: usize) -> FileMapL1 {
        FileMapL1 {
            schema_ver: crate::extract::SCHEMA_VER,
            language: "rust".to_string(),
            size_bytes: 0,
            had_errors: false,
            error_count: 0,
            symbols: (0..symbols)
                .map(|i| Symbol {
                    name: format!("generated_symbol_{i:04}"),
                    kind: SymbolKind::Function,
                    start_byte: i as u32 * 16,
                    end_byte: i as u32 * 16 + 8,
                    start_row: 0,
                    start_col: 0,
                    signature: Some("x".repeat(256)),
                    decorators: Vec::new(),
                })
                .collect(),
            imports: Vec::new(),
            implementations: Vec::new(),
            rationale: Vec::new(),
        }
    }

    fn dump(keyspace: &fjall::Keyspace) -> Vec<(Vec<u8>, Vec<u8>)> {
        keyspace
            .iter()
            .map(|guard| {
                let (k, v) = guard.into_inner().expect("read entry");
                ((*k).to_vec(), (*v).to_vec())
            })
            .collect()
    }

    /// Every partition the scanner's staging touches, in a fixed order, so two runs can be
    /// compared byte for byte.
    fn dump_all(db: &IndexDb) -> Vec<Vec<(Vec<u8>, Vec<u8>)>> {
        vec![
            dump(&db.symbols_by_path),
            dump(&db.symbols_by_name),
            dump(&db.calls_by_path),
            dump(&db.calls_by_callee),
            dump(&db.imports_by_module),
            dump(&db.imports_by_path),
            dump(&db.implementations_by_trait),
            dump(&db.implementations_by_path),
            dump(&db.code_bm25_postings),
            dump(&db.code_bm25_by_path),
        ]
    }

    /// The defect this whole two-bound scheme exists for: 256 files is not a memory bound, so a
    /// worker must commit on staged *bytes* long before the file counter notices.
    #[test]
    fn byte_budget_commits_long_before_the_file_count_bound() {
        static LEDGER: StagedByteLedger = StagedByteLedger::new(u64::MAX);
        let (_dir, db) = fresh_index();
        let l1 = bulky_l1(32);

        let mut batch = WorkerIndexBatch::with_budget(Some(&db), 8 * 1024, &LEDGER);
        for i in 0..8u32 {
            assert!(batch.stage(&RelPath::from(format!("src/f{i}.rs")), &l1, None));
        }

        assert!(
            batch.staged < INDEX_COMMIT_BATCH,
            "the file-count bound must not be what committed here"
        );
        assert!(
            db.symbols_by_path.iter().next().is_some(),
            "the byte budget must have forced a commit already"
        );
        drop(batch);
        assert_eq!(LEDGER.in_flight(), 0);
    }

    /// BM25 postings used to ride entirely outside the only bound that existed — `stage_bm25`
    /// touched neither the counter nor the commit. They are the largest single staged
    /// contributor, so their staging must be able to force a commit on its own.
    ///
    /// `code-search` is not a default feature, so a plain `cargo test` compiles this out; CI covers
    /// it on the two `full` legs (`full` ⊇ `code-search`), and locally it needs
    /// `cargo test --features code-search`.
    #[cfg(feature = "code-search")]
    #[test]
    fn bm25_only_staging_hits_the_byte_budget_and_commits() {
        use crate::search::bm25::ChunkPosting;
        static LEDGER: StagedByteLedger = StagedByteLedger::new(u64::MAX);
        let (_dir, db) = fresh_index();

        let postings: Vec<ChunkPosting> = (0..4)
            .map(|chunk| ChunkPosting {
                chunk_id: format!("hash:{chunk}"),
                doclen: 512,
                terms: (0..512).map(|t| (format!("term_{t:04}"), 1)).collect(),
            })
            .collect();

        let mut batch = WorkerIndexBatch::with_budget(Some(&db), 8 * 1024, &LEDGER);
        batch.stage_bm25(&RelPath::from("src/generated.rs"), &postings);

        assert_eq!(batch.staged, 0, "bm25 staging must not bump the file counter");
        assert!(
            db.code_bm25_postings.iter().next().is_some(),
            "bm25 staging alone must be able to force a commit"
        );
        drop(batch);
        assert_eq!(LEDGER.in_flight(), 0);
    }

    /// The shared ceiling is only a bound if the ledger tracks the true in-flight total: it must
    /// return to zero across many commits, and it must survive a worker that unwinds mid-file —
    /// nothing contains a `process_file` panic, and the ledger is a `static` that outlives the
    /// scan, so a leaked contribution would shrink the ceiling for every later scan in the process.
    #[test]
    fn the_ledger_does_not_drift_across_commits_or_a_worker_panic() {
        static LEDGER: StagedByteLedger = StagedByteLedger::new(64 * 1024);
        let (_dir, db) = fresh_index();
        let l1 = bulky_l1(8);

        let mut batch = WorkerIndexBatch::with_budget(Some(&db), 4 * 1024, &LEDGER);
        for i in 0..64u32 {
            assert!(batch.stage(&RelPath::from(format!("src/f{i}.rs")), &l1, None));
            assert_eq!(
                LEDGER.in_flight(),
                batch.reported_bytes,
                "the sole worker's contribution must be exactly what the ledger holds"
            );
            assert!(
                batch.reported_bytes < LEDGER.ceiling,
                "staged bytes must stay under the shared ceiling"
            );
        }
        batch.finish();
        assert_eq!(
            LEDGER.in_flight(),
            0,
            "the ledger must be empty once every batch committed"
        );

        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut batch = WorkerIndexBatch::with_budget(Some(&db), u64::MAX, &LEDGER);
            assert!(batch.stage(&RelPath::from("src/pathological.rs"), &l1, None));
            assert!(LEDGER.in_flight() > 0, "staging must charge the ledger");
            panic!("pathological file");
        }));
        assert!(caught.is_err());
        assert_eq!(LEDGER.in_flight(), 0, "a panicking file must not leak budget");
    }

    /// `upsert_file` stages its deletes and its inserts *before* the read or the encode that can
    /// fail, so a failure leaves real bytes in the writer. Reporting them only on the worker's next
    /// *successful* file would let a batch hold megabytes of tombstones that no bound can see.
    #[test]
    fn a_failed_upsert_still_reports_the_bytes_it_staged() {
        static LEDGER: StagedByteLedger = StagedByteLedger::new(u64::MAX);
        let (_dir, db) = fresh_index();
        let l1 = bulky_l1(8);

        let mut batch = WorkerIndexBatch::with_budget(Some(&db), u64::MAX, &LEDGER);
        assert!(batch.stage(&RelPath::from("src/ok.rs"), &l1, None));
        let after_ok = LEDGER.in_flight();
        assert!(after_ok > 0, "the successful file must be on the ledger");

        batch
            .writer
            .as_mut()
            .expect("writer exists after a stage")
            .fail_after_staging = true;
        assert!(
            !batch.stage(&RelPath::from("src/doomed.rs"), &l1, None),
            "the injected failure must be reported to the caller"
        );

        let staged = batch
            .writer
            .as_ref()
            .expect("no bound can have committed here")
            .staged_bytes();
        assert!(staged > after_ok, "the failed upsert did stage bytes before failing");
        assert_eq!(
            LEDGER.in_flight(),
            staged,
            "bytes staged by a failed upsert stay resident until the next commit, so they must be \
             on the ledger and able to force one"
        );
        drop(batch);
        assert_eq!(LEDGER.in_flight(), 0);
    }

    /// The shared ceiling has no hysteresis of its own: past `ceiling / per-worker budget` workers
    /// the process sits pinned over it, and without [`CEILING_MIN_COMMIT_BYTES`] every worker then
    /// commits at almost every file — a cadence that degrades linearly with core count, which is
    /// exactly the fjall write-lock contention this batching exists to remove. Counting commits (not
    /// timing them) is the only assertion that stays deterministic on a loaded machine.
    #[test]
    fn a_pinned_ceiling_does_not_degenerate_into_a_commit_per_file() {
        static PINNED: StagedByteLedger = StagedByteLedger::new(64 * 1024);
        const FILES: u32 = 200;
        let (_dir, db) = fresh_index();
        let l1 = bulky_l1(32);

        // The other N-1 workers hold the total over the ceiling for the whole slice; charging ~keep
        // their share directly is what a real scan looks like from one worker's point of view, ~keep
        // and keeps this deterministic without a rayon pool. ~keep
        PINNED.charge(PINNED.ceiling);
        let commits_before = PINNED.commits();
        let mut worker = WorkerIndexBatch::with_budget(Some(&db), u64::MAX, &PINNED);
        assert!(worker.stage(&RelPath::from("src/f0.rs"), &l1, None));
        let per_file = worker.reported_bytes;
        for i in 1..FILES {
            assert!(worker.stage(&RelPath::from(format!("src/f{i}.rs")), &l1, None));
        }
        assert!(
            (FILES as usize) < INDEX_COMMIT_BATCH,
            "the file-count bound must not be what commits here"
        );
        worker.finish();

        let commits = PINNED.commits() - commits_before;
        let cadence_bound = per_file * u64::from(FILES) / CEILING_MIN_COMMIT_BYTES + 2;
        assert!(
            commits <= cadence_bound,
            "a pinned ceiling must still commit on the {CEILING_MIN_COMMIT_BYTES}-byte floor: \
             {commits} commits for {FILES} files ({per_file} B each), expected at most {cadence_bound}"
        );
        assert!(
            commits * 20 <= u64::from(FILES),
            "cadence collapsed to {commits} commits for {FILES} files — a floorless ceiling commits per file"
        );
        PINNED.release(PINNED.ceiling);
        assert_eq!(PINNED.in_flight(), 0);
    }

    /// Byte-bounded batching is a flush-boundary change only: committing per file and committing
    /// once at the end must leave byte-identical index contents.
    #[test]
    fn batching_granularity_does_not_change_index_contents() {
        static LEDGER: StagedByteLedger = StagedByteLedger::new(u64::MAX);
        let (_eager_dir, eager) = fresh_index();
        let (_lazy_dir, lazy) = fresh_index();
        let l1 = bulky_l1(6);
        let rels: Vec<RelPath> = (0..12u32).map(|i| RelPath::from(format!("src/f{i}.rs"))).collect();

        for (db, budget) in [(&eager, 1u64), (&lazy, u64::MAX)] {
            let mut batch = WorkerIndexBatch::with_budget(Some(db), budget, &LEDGER);
            for rel in &rels {
                assert!(batch.stage(rel, &l1, None));
                #[cfg(feature = "code-search")]
                batch.stage_bm25(
                    rel,
                    &[crate::search::bm25::ChunkPosting {
                        chunk_id: format!("{rel}:0"),
                        doclen: 3,
                        terms: vec![("spawn".to_string(), 2), ("task".to_string(), 1)],
                    }],
                );
            }
            batch.finish();
        }

        assert_eq!(dump_all(&eager), dump_all(&lazy));
        assert_eq!(LEDGER.in_flight(), 0);
    }
}
