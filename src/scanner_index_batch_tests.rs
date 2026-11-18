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
        extract_epoch: crate::extract::EXTRACT_EPOCH,
    }
}

/// The content key of each test file: its own, so every staged file writes rows of its own.
fn ck(rel: &str) -> ContentKey {
    crate::index::keys::content_key(rel, "rust", crate::extract::EXTRACT_EPOCH)
}

fn stage_own(batch: &mut WorkerIndexBatch<'_>, rel: &RelPath, l1: &FileMapL1) -> bool {
    batch.stage(rel, &ck(&rel.to_str_lossy()), l1, None)
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
        dump(db.keyspace_for_test(crate::index::view::Part::SymbolsByPath)),
        dump(db.keyspace_for_test(crate::index::view::Part::SymbolsByName)),
        dump(db.keyspace_for_test(crate::index::view::Part::CallsByPath)),
        dump(db.keyspace_for_test(crate::index::view::Part::CallsByCallee)),
        dump(db.keyspace_for_test(crate::index::view::Part::ImportsByModule)),
        dump(db.keyspace_for_test(crate::index::view::Part::ImportsByPath)),
        dump(db.keyspace_for_test(crate::index::view::Part::ImplementationsByTrait)),
        dump(db.keyspace_for_test(crate::index::view::Part::ImplementationsByPath)),
        dump(db.keyspace_for_test(crate::index::view::Part::CodeBm25Postings)),
        dump(db.keyspace_for_test(crate::index::view::Part::CodeBm25ByPath)),
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
        assert!(stage_own(&mut batch, &RelPath::from(format!("src/f{i}.rs")), &l1));
    }

    assert!(
        batch.staged < INDEX_COMMIT_BATCH,
        "the file-count bound must not be what committed here"
    );
    assert!(
        db.keyspace_for_test(crate::index::view::Part::SymbolsByPath)
            .iter()
            .next()
            .is_some(),
        "the byte budget must have forced a commit already"
    );
    drop(batch);
    assert_eq!(LEDGER.in_flight(), 0);
}

/// The converse of the test above, and the only cover the *cadence* bound has: a worker
/// staging ordinary files that come nowhere near either byte bound must still commit on the
/// file counter. [`INDEX_COMMIT_BATCH`] is not a memory bound (that is the byte budget and the
/// shared ceiling) — it is what keeps a scan of small files off a commit-per-file cadence, and
/// what keeps a worker from holding a batch open for its entire slice.
#[test]
fn the_file_count_bound_commits_when_neither_byte_bound_is_near() {
    static ROOMY: StagedByteLedger = StagedByteLedger::new(u64::MAX);
    let (_dir, db) = fresh_index();
    let l1 = bulky_l1(1);

    let mut batch = WorkerIndexBatch::with_budget(Some(&db), u64::MAX, &ROOMY);
    for i in 0..INDEX_COMMIT_BATCH - 1 {
        assert!(stage_own(&mut batch, &RelPath::from(format!("src/f{i}.rs")), &l1));
    }
    assert!(
        db.keyspace_for_test(crate::index::view::Part::SymbolsByPath)
            .iter()
            .next()
            .is_none(),
        "nothing may commit before the file counter reaches the bound"
    );

    assert!(stage_own(&mut batch, &RelPath::from("src/last.rs"), &l1));
    assert!(
        db.keyspace_for_test(crate::index::view::Part::SymbolsByPath)
            .iter()
            .next()
            .is_some(),
        "the {INDEX_COMMIT_BATCH}th file must force the commit"
    );
    assert_eq!(batch.staged, 0, "the file counter resets at its own commit");
    drop(batch);
    assert_eq!(ROOMY.in_flight(), 0);
}

/// The production *wiring*, as distinct from the mechanism.
///
/// Every other test in this file injects its own byte budget and its own ledger, so together
/// they prove `WorkerIndexBatch` commits when a budget tells it to — and prove nothing about
/// the budget a real scan runs with. Setting both `INDEX_COMMIT_BATCH_BYTES` and
/// `INDEX_STAGED_BYTES_CEILING` to `u64::MAX` was measured to leave this entire file green,
/// and `tests/scan_memory_budget.rs` green with it (479 MiB against 503 MiB unmutated, i.e.
/// inside that test's noise): the two constants that do the bounding in a real scan had no
/// cover at all. That is the shape of the inert `max_footprint_mb` which let issue #62 happen
/// — a bound that parses, validates, and does nothing.
///
/// So this test pins the constant rather than a stand-in, by staging through the same budget
/// `WorkerIndexBatch::new` passes and requiring the commit to come from bytes and not from the
/// file counter.
#[test]
fn the_production_byte_budget_commits_before_the_file_counter() {
    let (_dir, db) = fresh_index();
    // Big enough that a real 8 MiB budget is crossed in tens of files rather than hundreds,
    // which is what leaves room for the file counter to be ruled out as the cause. ~keep
    let l1 = bulky_l1(8192);

    let mut batch = WorkerIndexBatch::with_budget(Some(&db), INDEX_COMMIT_BATCH_BYTES, &SCAN_STAGED_BYTES);
    let mut files = 0usize;
    while db
        .keyspace_for_test(crate::index::view::Part::SymbolsByPath)
        .iter()
        .next()
        .is_none()
        && files < INDEX_COMMIT_BATCH
    {
        assert!(stage_own(&mut batch, &RelPath::from(format!("src/f{files}.rs")), &l1));
        files += 1;
    }

    assert!(
        db.keyspace_for_test(crate::index::view::Part::SymbolsByPath)
            .iter()
            .next()
            .is_some(),
        "staging {INDEX_COMMIT_BATCH} bulky files must have committed something"
    );
    assert!(
        files < INDEX_COMMIT_BATCH,
        "the production byte budget must force a commit before the {INDEX_COMMIT_BATCH}-file \
         counter reaches it, or INDEX_COMMIT_BATCH_BYTES is not a memory bound in a real scan; \
         committed only after {files} files"
    );
    drop(batch);
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

    fn postings(chunks: u32, terms: u32) -> Vec<ChunkPosting> {
        (0..chunks)
            .map(|chunk| ChunkPosting {
                chunk_id: format!("hash:{chunk}"),
                doclen: terms,
                terms: (0..terms).map(|t| (format!("term_{t:04}"), 1)).collect(),
            })
            .collect()
    }

    // Control: the same call, far under the budget, must NOT commit. Without it the assertion ~keep
    // below would also pass for a `stage_bm25` that flushed unconditionally — which is exactly ~keep
    // the per-file fjall write-lock contention the batching exists to remove. ~keep
    let (_control_dir, control) = fresh_index();
    let mut under_budget = WorkerIndexBatch::with_budget(Some(&control), 8 * 1024 * 1024, &LEDGER);
    under_budget.stage_bm25(
        &RelPath::from("src/small.rs"),
        &crate::index::keys::bm25_content_key("hash", "rust", "digest"),
        &postings(1, 4),
    );
    assert!(
        control
            .keyspace_for_test(crate::index::view::Part::CodeBm25Postings)
            .iter()
            .next()
            .is_none(),
        "staging well under the byte budget must leave the batch uncommitted"
    );
    drop(under_budget);
    assert_eq!(LEDGER.in_flight(), 0);

    let mut batch = WorkerIndexBatch::with_budget(Some(&db), 8 * 1024, &LEDGER);
    batch.stage_bm25(
        &RelPath::from("src/generated.rs"),
        &crate::index::keys::bm25_content_key("hash", "rust", "digest"),
        &postings(4, 512),
    );

    assert_eq!(batch.staged, 0, "bm25 staging must not bump the file counter");
    assert!(
        db.keyspace_for_test(crate::index::view::Part::CodeBm25Postings)
            .iter()
            .next()
            .is_some(),
        "bm25 staging alone must be able to force a commit"
    );
    drop(batch);
    assert_eq!(LEDGER.in_flight(), 0);
}

/// The shared ceiling must be able to force a commit *by itself*. It is the only bound that can
/// see the other workers — [`INDEX_COMMIT_BATCH_BYTES`] alone still permits `workers × 8 MiB`,
/// the machine-dependent multiplier that grows exactly on the machines that scan the big repos —
/// so a worker nowhere near either of its own bounds must still commit when the process is over
/// the ceiling. Both of this worker's own bounds are disarmed here (`u64::MAX` byte budget, and
/// the commit lands well inside [`INDEX_COMMIT_BATCH`] files), leaving the ceiling as the only
/// thing that could have committed.
#[test]
fn the_shared_ceiling_alone_forces_a_commit() {
    static CEILINGED: StagedByteLedger = StagedByteLedger::new(4 * 1024);
    let (_dir, db) = fresh_index();
    let l1 = bulky_l1(64);

    let mut worker = WorkerIndexBatch::with_budget(Some(&db), u64::MAX, &CEILINGED);
    let mut committed_after = None;
    for i in 0..(INDEX_COMMIT_BATCH - 1) {
        assert!(stage_own(&mut worker, &RelPath::from(format!("src/f{i}.rs")), &l1));
        if CEILINGED.commits() > 0 {
            committed_after = Some(i + 1);
            break;
        }
    }

    let files = committed_after.expect("a worker over the shared ceiling must commit");
    assert!(
        files < INDEX_COMMIT_BATCH,
        "the file-count bound must not be what committed here"
    );
    assert!(
        db.keyspace_for_test(crate::index::view::Part::SymbolsByPath)
            .iter()
            .next()
            .is_some(),
        "the ceiling commit must have reached disk"
    );
    worker.finish();
    assert_eq!(CEILINGED.in_flight(), 0);
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
        assert!(stage_own(&mut batch, &RelPath::from(format!("src/f{i}.rs")), &l1));
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
        assert!(stage_own(&mut batch, &RelPath::from("src/pathological.rs"), &l1));
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
    assert!(stage_own(&mut batch, &RelPath::from("src/ok.rs"), &l1));
    let after_ok = LEDGER.in_flight();
    assert!(after_ok > 0, "the successful file must be on the ledger");

    batch
        .writer
        .as_mut()
        .expect("writer exists after a stage")
        .fail_after_staging = true;
    assert!(
        !stage_own(&mut batch, &RelPath::from("src/doomed.rs"), &l1),
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

fn updated(path: &str) -> FileResult {
    let mut result = FileResult::bare(
        path.to_string(),
        FileStatus::Updated {
            had_errors: false,
            error_count: 0,
            reused: false,
        },
    );
    result.upsert = Some(crate::store::FileEntry {
        hash_hex: "00".to_string(),
        language: "rust".to_string(),
        size_bytes: 0,
        mtime: 0,
        extract_epoch: crate::extract::EXTRACT_EPOCH,
        content_key: None,
    });
    result
}

/// `(path, still claims to be indexed)` for each result, in order.
fn claims(results: &[FileResult]) -> Vec<(&str, bool)> {
    results
        .iter()
        .map(|result| (result.path.as_str(), result.upsert.is_some()))
        .collect()
}

/// A lost batch must take back exactly its own files: the ones already pushed since the last
/// good commit and the one whose staging triggered the commit. Files committed before it, files
/// staged after it, and files that never claimed new rows keep their outcome.
#[test]
fn a_failed_commit_disowns_exactly_the_files_riding_that_batch() {
    static LEDGER: StagedByteLedger = StagedByteLedger::new(u64::MAX);
    let (_dir, db) = fresh_index();
    let l1 = bulky_l1(4);
    let mut batch = WorkerIndexBatch::with_budget(Some(&db), u64::MAX, &LEDGER);

    assert!(stage_own(&mut batch, &RelPath::from("src/committed.rs"), &l1));
    batch.push_result(updated("src/committed.rs"));
    batch.commit();

    assert!(stage_own(&mut batch, &RelPath::from("src/lost.rs"), &l1));
    batch.push_result(updated("src/lost.rs"));
    batch.push_result(FileResult::bare("src/unchanged.rs".to_string(), FileStatus::Unchanged));
    assert!(stage_own(&mut batch, &RelPath::from("src/in_flight.rs"), &l1));
    db.fault.inject_commit_failures(1);
    batch.commit();
    batch.push_result(updated("src/in_flight.rs"));

    assert!(stage_own(&mut batch, &RelPath::from("src/after.rs"), &l1));
    batch.push_result(updated("src/after.rs"));
    let results = batch.finish();

    assert_eq!(
        claims(&results),
        vec![
            ("src/committed.rs", true),
            ("src/lost.rs", false),
            ("src/unchanged.rs", false),
            ("src/in_flight.rs", false),
            ("src/after.rs", true),
        ]
    );
    assert!(matches!(results[1].status, FileStatus::ExtractFailed { .. }));
    assert!(matches!(results[2].status, FileStatus::Unchanged));
    assert!(matches!(results[3].status, FileStatus::ExtractFailed { .. }));
    let indexed: ahash::AHashSet<ContentKey> = dump(db.keyspace_for_test(crate::index::view::Part::SymbolsByPath))
        .iter()
        .filter_map(|(key, _)| crate::index::keys::parse_content_row(key))
        .map(|(content, _)| content)
        .collect();
    let expected: ahash::AHashSet<ContentKey> = ["src/committed.rs", "src/after.rs"].into_iter().map(ck).collect();
    assert_eq!(indexed, expected, "the claims match what the index actually holds");
    assert_eq!(LEDGER.in_flight(), 0);
}

/// A failed upsert leaves partial rows staged, so its file is no more indexed than one whose
/// commit failed, and must not be recorded either.
#[test]
fn a_failed_upsert_disowns_its_file() {
    static LEDGER: StagedByteLedger = StagedByteLedger::new(u64::MAX);
    let (_dir, db) = fresh_index();
    let l1 = bulky_l1(4);
    let mut batch = WorkerIndexBatch::with_budget(Some(&db), u64::MAX, &LEDGER);

    assert!(stage_own(&mut batch, &RelPath::from("src/ok.rs"), &l1));
    batch.push_result(updated("src/ok.rs"));
    batch.writer.as_mut().expect("writer").fail_after_staging = true;
    assert!(!stage_own(&mut batch, &RelPath::from("src/doomed.rs"), &l1));
    batch.push_result(updated("src/doomed.rs"));
    batch.writer.as_mut().expect("writer").fail_after_staging = false;
    assert!(stage_own(&mut batch, &RelPath::from("src/next.rs"), &l1));
    batch.push_result(updated("src/next.rs"));

    assert_eq!(
        claims(&batch.finish()),
        vec![("src/ok.rs", true), ("src/doomed.rs", false), ("src/next.rs", true)]
    );
}

/// A file whose keyword postings failed to stage would otherwise keep its place in the file map
/// and never be seen by keyword search: no later scan re-stages an unchanged file.
#[cfg(feature = "code-search")]
#[test]
fn a_failed_bm25_upsert_disowns_its_file() {
    static LEDGER: StagedByteLedger = StagedByteLedger::new(u64::MAX);
    let (_dir, db) = fresh_index();
    let l1 = bulky_l1(4);
    let postings = [crate::search::bm25::ChunkPosting {
        chunk_id: "hash:0".to_string(),
        doclen: 1,
        terms: vec![("term".to_string(), 1)],
    }];
    let mut batch = WorkerIndexBatch::with_budget(Some(&db), u64::MAX, &LEDGER);

    assert!(stage_own(&mut batch, &RelPath::from("src/ok.rs"), &l1));
    batch.stage_bm25(
        &RelPath::from("src/ok.rs"),
        &crate::index::keys::bm25_content_key("hash", "rust", "digest"),
        &postings,
    );
    batch.push_result(updated("src/ok.rs"));
    assert!(stage_own(&mut batch, &RelPath::from("src/doomed.rs"), &l1));
    batch.writer.as_mut().expect("writer").fail_after_staging = true;
    batch.stage_bm25(
        &RelPath::from("src/doomed.rs"),
        &crate::index::keys::bm25_content_key("hash", "rust", "digest"),
        &postings,
    );
    batch.writer.as_mut().expect("writer").fail_after_staging = false;
    batch.push_result(updated("src/doomed.rs"));

    assert_eq!(
        claims(&batch.finish()),
        vec![("src/ok.rs", true), ("src/doomed.rs", false)]
    );
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
    assert!(stage_own(&mut worker, &RelPath::from("src/f0.rs"), &l1));
    let per_file = worker.reported_bytes;
    for i in 1..FILES {
        assert!(stage_own(&mut worker, &RelPath::from(format!("src/f{i}.rs")), &l1));
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
            assert!(stage_own(&mut batch, rel, &l1));
            #[cfg(feature = "code-search")]
            batch.stage_bm25(
                rel,
                &crate::index::keys::bm25_content_key(&rel.to_str_lossy(), "rust", "digest"),
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

/// A bloom row is staged through the same byte ledger as every other entry, a rescan backfills a
/// row an older index lacks, and a row that is already current costs no write at all.
#[test]
fn grep_bloom_rows_are_budgeted_backfilled_and_not_rewritten_when_current() {
    static LEDGER: StagedByteLedger = StagedByteLedger::new(u64::MAX);
    let (_index_dir, db) = fresh_index();
    let dir = tempfile::tempdir().expect("tempdir");
    let body = "fn needle_function() {}\n".repeat(40);
    std::fs::write(dir.path().join("a.rs"), &body).expect("write");
    let hash = crate::hashing::hex(&crate::hashing::hash_bytes(body.as_bytes()));
    let rel = RelPath::from("a.rs");

    let mut batch = WorkerIndexBatch::with_budget(Some(&db), u64::MAX, &LEDGER);
    batch.backfill_grep_bloom(dir.path(), &rel, &hash);
    assert!(
        LEDGER.in_flight() > 0,
        "the staged bloom row must count against the byte ledger"
    );
    batch.finish();
    assert_eq!(LEDGER.in_flight(), 0);
    assert!(db.grep_bloom_current(&hash), "backfill wrote the row");

    let mut again = WorkerIndexBatch::with_budget(Some(&db), u64::MAX, &LEDGER);
    again.backfill_grep_bloom(dir.path(), &rel, &hash);
    again.restamp_grep_bloom(&hash, b"ignored");
    assert_eq!(LEDGER.in_flight(), 0, "a current row is neither rebuilt nor restaged");
    again.finish();

    db.drop_grep_bloom_row_for_test(&hash);
    let mut moved = WorkerIndexBatch::with_budget(Some(&db), u64::MAX, &LEDGER);
    moved.restamp_grep_bloom(&hash, body.as_bytes());
    assert!(LEDGER.in_flight() > 0, "a missing row is re-staged");
    moved.finish();
    assert!(db.grep_bloom_current(&hash));
}

/// The bytes on disk must hash to the indexed hash, or the row would describe other content.
#[test]
fn backfill_refuses_bytes_that_no_longer_match_the_indexed_hash() {
    static LEDGER: StagedByteLedger = StagedByteLedger::new(u64::MAX);
    let (_index_dir, db) = fresh_index();
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("a.rs"), "fn edited() {}\n").expect("write");
    let mut batch = WorkerIndexBatch::with_budget(Some(&db), u64::MAX, &LEDGER);
    batch.backfill_grep_bloom(dir.path(), &RelPath::from("a.rs"), "not-the-hash");
    batch.finish();
    assert_eq!(db.grep_bloom_row_count(), 0);
}
