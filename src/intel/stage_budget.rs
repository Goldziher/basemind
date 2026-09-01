//! Byte-bounded serial staging for the resolve pass and the cross-file stitch.
//!
//! The primary scan's parallel path has [`crate::scanner_index_batch::WorkerIndexBatch`], which
//! commits on a file count, its own staged-byte budget, *and* the process-wide
//! [`StagedByteLedger`]. The two resolve-tier writers are serial (Fjall staging is the shared
//! bottleneck, so they deliberately stay single-threaded) and used to commit on a count of items
//! alone — 256 files, or 256 cross-file edges. A count is not a memory bound: one generated file's
//! `upsert_resolved_file` stages two index entries per resolved identifier use, so 256 of them can
//! be orders of magnitude more staged bytes than 256 hand-written files. Worse, that staging was
//! invisible to the scan-wide ledger, so it could push the process past the shared ceiling without
//! any worker noticing.
//!
//! [`BudgetedWriter`] gives those two sites the same three bounds as the parallel path, reporting
//! to the same ledger so the scan's total in-flight staged bytes is a real number rather than the
//! parallel path's share of it.

use crate::index::IndexDb;
use crate::index::writer::IndexWriter;
use crate::scanner_index_batch::{SCAN_STAGED_BYTES, StagedByteLedger};

/// Key+value bytes one serial resolve-tier batch may hold staged before it must commit, whatever
/// the item count says. Matches the scanner's per-worker budget: both are sized so a commit's
/// journal write stays well inside Fjall's `max_journaling_size`, and both want fjall's own
/// `check_memtable_rotate` backpressure sampled often rather than once per few hundred items.
const STAGE_BUDGET_BYTES: u64 = 8 * 1024 * 1024;

/// A serial index write batch that commits on an item count, its own byte budget, or the
/// process-wide staged-byte ceiling — whichever trips first.
///
/// The writer is opened lazily and re-opened after every commit, so a caller only ever sees a live
/// batch. Its ledger contribution is released on commit *and* on drop, so an unwind cannot leak
/// budget and permanently shrink the shared ceiling.
pub(crate) struct BudgetedWriter<'a> {
    index_db: &'a IndexDb,
    writer: Option<IndexWriter>,
    staged: usize,
    /// Bytes already reported to `ledger`; released in full on commit and on drop so the ledger
    /// never drifts from the true in-flight total.
    reported_bytes: u64,
    max_items: usize,
    byte_budget: u64,
    ledger: &'static StagedByteLedger,
    /// Names the pass in the commit-failure warning, so the two call sites stay tellable apart in
    /// a log.
    phase: &'static str,
}

impl<'a> BudgetedWriter<'a> {
    pub(crate) fn new(index_db: &'a IndexDb, max_items: usize, phase: &'static str) -> Self {
        Self::with_budget(index_db, max_items, STAGE_BUDGET_BYTES, &SCAN_STAGED_BYTES, phase)
    }

    fn with_budget(
        index_db: &'a IndexDb,
        max_items: usize,
        byte_budget: u64,
        ledger: &'static StagedByteLedger,
        phase: &'static str,
    ) -> Self {
        Self {
            index_db,
            writer: None,
            staged: 0,
            reported_bytes: 0,
            max_items,
            byte_budget,
            ledger,
            phase,
        }
    }

    /// The live batch, opened on first use. Callers stage through this and then call
    /// [`Self::item_staged`] so the bounds are checked at a whole-item boundary — never mid-item,
    /// which would split one logical upsert across two commits.
    pub(crate) fn writer(&mut self) -> &mut IndexWriter {
        let index_db = self.index_db;
        self.writer.get_or_insert_with(|| index_db.writer())
    }

    /// Record that one item finished staging: report its bytes to the ledger and commit if any
    /// bound is now met.
    pub(crate) fn item_staged(&mut self) {
        self.staged += 1;
        let staged_bytes = self.writer.as_ref().map_or(0, IndexWriter::staged_bytes);
        self.ledger.charge(staged_bytes.saturating_sub(self.reported_bytes));
        self.reported_bytes = staged_bytes;
        if self.staged >= self.max_items || staged_bytes >= self.byte_budget || self.ledger.over_ceiling() {
            self.commit();
        }
    }

    /// Flush the staged batch under Fjall's write lock and reset both counters. Returns false when
    /// the commit failed (already logged).
    fn commit(&mut self) -> bool {
        let mut ok = true;
        if let Some(writer) = self.writer.take()
            && let Err(error) = writer.commit()
        {
            tracing::warn!(
                %error,
                phase = self.phase,
                "index commit failed — resolved navigation may be stale"
            );
            ok = false;
        }
        self.release_reported();
        self.staged = 0;
        ok
    }

    fn release_reported(&mut self) {
        self.ledger.release(self.reported_bytes);
        self.reported_bytes = 0;
    }

    /// Commit the trailing partial batch. Returns false when that commit failed, so a caller can
    /// skip its own success log.
    pub(crate) fn finish(mut self) -> bool {
        self.commit()
    }
}

impl Drop for BudgetedWriter<'_> {
    /// Give back this batch's share of the ledger even when it never reached a commit — the
    /// resolve pass runs inside `scanner_lanes::run_optional_lane`, which catches an unwind and
    /// lets the scan continue, and a leaked contribution would permanently shrink the shared
    /// ceiling for the rest of the process.
    fn drop(&mut self) {
        self.release_reported();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intel::model::{FileResolvedRefs, ResolvedEdge};
    use crate::path::RelPath;

    fn fresh_index() -> (tempfile::TempDir, IndexDb) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = IndexDb::open(dir.path()).expect("open index");
        (dir, db)
    }

    /// One file's worth of resolved edges — the structure that makes an item count meaningless as
    /// a memory bound, since each edge stages a key in both `refs_by_def` and `refs_by_path`.
    fn edge_heavy(edges: u32) -> FileResolvedRefs {
        let mut refs = FileResolvedRefs::new("typescript");
        refs.intra = (0..edges)
            .map(|i| ResolvedEdge {
                use_start: i * 32,
                use_end: i * 32 + 8,
                def_start: 4,
                def_end: 12,
            })
            .collect();
        refs
    }

    /// The defect this exists for: a handful of edge-heavy files must commit on staged bytes long
    /// before the item counter is anywhere near its bound.
    #[test]
    fn the_byte_budget_commits_long_before_the_item_count_bound() {
        static LEDGER: StagedByteLedger = StagedByteLedger::new(u64::MAX);
        let (_dir, db) = fresh_index();
        let refs = edge_heavy(64);

        let mut batch = BudgetedWriter::with_budget(&db, 256, 8 * 1024, &LEDGER, "test");
        for i in 0..8u32 {
            batch
                .writer()
                .upsert_resolved_file(&RelPath::from(format!("src/f{i}.ts")), &refs)
                .expect("stage resolved file");
            batch.item_staged();
        }

        assert!(
            batch.staged < 256,
            "the item-count bound must not be what committed here"
        );
        assert!(
            db.refs_by_def.iter().next().is_some(),
            "the byte budget must have forced a commit already"
        );
        assert!(batch.finish());
        assert_eq!(LEDGER.in_flight(), 0);
    }

    /// The shared ceiling is only a bound if the ledger tracks the true in-flight total: it must
    /// return to zero across many commits and survive a caller that unwinds mid-batch.
    #[test]
    fn the_ledger_does_not_drift_across_commits_or_a_panic() {
        static LEDGER: StagedByteLedger = StagedByteLedger::new(64 * 1024);
        let (_dir, db) = fresh_index();
        let refs = edge_heavy(8);

        let mut batch = BudgetedWriter::with_budget(&db, 256, 4 * 1024, &LEDGER, "test");
        for i in 0..64u32 {
            batch
                .writer()
                .upsert_resolved_file(&RelPath::from(format!("src/f{i}.ts")), &refs)
                .expect("stage resolved file");
            batch.item_staged();
            assert_eq!(
                LEDGER.in_flight(),
                batch.reported_bytes,
                "the sole batch's contribution must be exactly what the ledger holds"
            );
        }
        assert!(batch.finish());
        assert_eq!(
            LEDGER.in_flight(),
            0,
            "the ledger must be empty once every batch committed"
        );

        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut batch = BudgetedWriter::with_budget(&db, 256, u64::MAX, &LEDGER, "test");
            batch
                .writer()
                .upsert_resolved_file(&RelPath::from("src/pathological.ts"), &refs)
                .expect("stage resolved file");
            batch.item_staged();
            assert!(LEDGER.in_flight() > 0, "staging must charge the ledger");
            panic!("pathological file");
        }));
        assert!(caught.is_err());
        assert_eq!(LEDGER.in_flight(), 0, "a panicking batch must not leak budget");
    }

    /// Byte-bounded batching is a flush-boundary change only: committing per item and committing
    /// once at the end must leave identical index contents.
    #[test]
    fn commit_granularity_does_not_change_index_contents() {
        static LEDGER: StagedByteLedger = StagedByteLedger::new(u64::MAX);
        let (_eager_dir, eager) = fresh_index();
        let (_lazy_dir, lazy) = fresh_index();
        let refs = edge_heavy(6);

        for (db, budget) in [(&eager, 1u64), (&lazy, u64::MAX)] {
            let mut batch = BudgetedWriter::with_budget(db, 256, budget, &LEDGER, "test");
            for i in 0..12u32 {
                batch
                    .writer()
                    .upsert_resolved_file(&RelPath::from(format!("src/f{i}.ts")), &refs)
                    .expect("stage resolved file");
                batch.item_staged();
            }
            assert!(batch.finish());
        }

        let dump = |db: &IndexDb| -> Vec<Vec<u8>> {
            db.refs_by_def
                .iter()
                .map(|guard| (*guard.into_inner().expect("read entry").0).to_vec())
                .collect()
        };
        assert_eq!(dump(&eager), dump(&lazy));
        assert_eq!(LEDGER.in_flight(), 0);
    }
}
