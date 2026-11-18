use super::*;
use crate::index::view::IndexView;
use crate::path::RelPath;
use crate::search::bm25::ChunkPosting;

fn postings(hash: &str, terms: &[&str]) -> Vec<ChunkPosting> {
    vec![ChunkPosting {
        chunk_id: format!("{hash}:0"),
        doclen: u32::try_from(terms.len()).unwrap(),
        terms: terms.iter().map(|t| ((*t).to_string(), 1)).collect(),
    }]
}

fn upsert(db: &IndexDb, path: &str, hash: &str, terms: &[&str]) {
    let mut writer = db.writer();
    writer
        .upsert_bm25_file(
            &RelPath::from(path),
            &keys::bm25_content_key(hash, "rust", "d"),
            &postings(hash, terms),
        )
        .unwrap();
    writer.commit().unwrap();
}

fn posting_count(db: &IndexDb) -> usize {
    db.code_bm25_postings.iter().count()
}

/// What the full sweep and recompute would leave, to compare the ledger's shortcut against.
fn swept(db: &IndexDb) -> (usize, Option<(u64, u64)>) {
    db.sweep_bm25_postings().unwrap();
    db.recompute_bm25_stats().unwrap();
    (posting_count(db), db.view().bm25_stats())
}

#[test]
fn a_pass_over_an_empty_index_is_exact_and_needs_no_sweep() {
    let dir = tempfile::tempdir().unwrap();
    let db = IndexDb::open(dir.path()).unwrap();
    let pass = db.begin_bm25_pass(PassKind::Full).unwrap();
    assert!(pass.is_exact(), "an empty posting list is exact at zero");
    upsert(&db, "src/a.rs", "a1", &["alpha", "beta"]);
    upsert(&db, "src/b.rs", "b1", &["beta"]);
    pass.finish().unwrap();
    assert_eq!(db.view().bm25_stats(), Some((2, 3)));
    assert_eq!(posting_count(&db), 3);
    assert_eq!(swept(&db), (3, Some((2, 3))), "the ledger agrees with a full recompute");
}

#[test]
fn a_pass_retires_exactly_the_postings_its_replaced_rows_named() {
    let dir = tempfile::tempdir().unwrap();
    let db = IndexDb::open(dir.path()).unwrap();
    let pass = db.begin_bm25_pass(PassKind::Full).unwrap();
    upsert(&db, "src/a.rs", "a1", &["alpha", "beta"]);
    upsert(&db, "src/b.rs", "b1", &["gamma"]);
    upsert(&db, "src/c.rs", "a1", &["alpha", "beta"]);
    pass.finish().unwrap();

    let pass = db.begin_bm25_pass(PassKind::Full).unwrap();
    assert!(
        pass.is_exact(),
        "the marker the last pass stamped vouches for the stored state"
    );
    upsert(&db, "src/a.rs", "a2", &["delta"]);
    upsert(&db, "src/b.rs", "b2", &["epsilon", "zeta"]);
    let mut writer = db.writer();
    writer.remove_bm25_file(&RelPath::from("src/c.rs")).unwrap();
    writer.commit().unwrap();
    pass.finish().unwrap();

    assert_eq!(db.view().bm25_stats(), Some((2, 3)));
    assert_eq!(
        posting_count(&db),
        3,
        "every posting of a1 and b1 is retired; a2 and b2 remain"
    );
    assert_eq!(swept(&db), (3, Some((2, 3))), "nothing was left for a sweep to find");
}

#[test]
fn a_write_outside_a_pass_or_an_unfinished_pass_makes_the_next_pass_sweep() {
    let dir = tempfile::tempdir().unwrap();
    let db = IndexDb::open(dir.path()).unwrap();
    let pass = db.begin_bm25_pass(PassKind::Full).unwrap();
    upsert(&db, "src/a.rs", "a1", &["alpha"]);
    pass.finish().unwrap();

    upsert(&db, "src/a.rs", "a2", &["beta"]);
    let pass = db.begin_bm25_pass(PassKind::Full).unwrap();
    assert!(
        !pass.is_exact(),
        "an incremental write dropped the marker in its own batch"
    );
    pass.finish().unwrap();
    assert_eq!(posting_count(&db), 1, "the fallback sweep retired a1's posting");
    assert_eq!(db.view().bm25_stats(), Some((1, 1)));

    let pass = db.begin_bm25_pass(PassKind::Full).unwrap();
    assert!(pass.is_exact());
    upsert(&db, "src/a.rs", "a3", &["gamma"]);
    drop(pass);
    assert!(!db.bm25_pass.is_open(), "dropping the pass closes the ledger");
    let pass = db.begin_bm25_pass(PassKind::Full).unwrap();
    assert!(!pass.is_exact(), "a pass that never finished left the marker unset");
    pass.finish().unwrap();
    assert_eq!(posting_count(&db), 1);
}

/// A posting no forward row names: garbage only a sweep removes, so it shows which path ran.
fn plant_orphan(db: &IndexDb) -> Vec<u8> {
    let orphan = keys::code_bm25_posting("orphan", &keys::bm25_content_key("gone", "rust", "d"), 0).unwrap();
    db.code_bm25_postings
        .insert(&orphan, keys::code_bm25_posting_value(1, 1))
        .unwrap();
    orphan
}

fn exact_db_with(files: &[(&str, &str, &[&str])]) -> (tempfile::TempDir, IndexDb) {
    let dir = tempfile::tempdir().unwrap();
    let db = IndexDb::open(dir.path()).unwrap();
    let pass = db.begin_bm25_pass(PassKind::Full).unwrap();
    for (path, hash, terms) in files {
        upsert(&db, path, hash, terms);
    }
    pass.finish().unwrap();
    (dir, db)
}

/// M1: a read error partway through a file's forward rows must not leave a delta for deletes
/// that were never staged, and must make the pass fall back to the exact recompute and sweep.
#[test]
fn a_forward_read_error_mid_prefix_forces_the_fallback() {
    let mut two_chunks = postings("a1", &["alpha"]);
    two_chunks.push(ChunkPosting {
        chunk_id: "a1:1".to_string(),
        doclen: 1,
        terms: vec![("beta".to_string(), 1)],
    });
    let dir = tempfile::tempdir().unwrap();
    let db = IndexDb::open(dir.path()).unwrap();
    let pass = db.begin_bm25_pass(PassKind::Full).unwrap();
    upsert(&db, "src/b.rs", "b1", &["gamma"]);
    let mut writer = db.writer();
    writer
        .upsert_bm25_file(
            &RelPath::from("src/a.rs"),
            &keys::bm25_content_key("a1", "rust", "d"),
            &two_chunks,
        )
        .unwrap();
    writer.commit().unwrap();
    pass.finish().unwrap();
    let orphan = plant_orphan(&db);

    let pass = db.begin_bm25_pass(PassKind::Full).unwrap();
    let mut writer = db.writer();
    writer.fail_forward_read_after = Some(1);
    assert!(
        writer
            .upsert_bm25_file(
                &RelPath::from("src/a.rs"),
                &keys::bm25_content_key("a2", "rust", "d"),
                &postings("a2", &["delta"])
            )
            .is_err()
    );
    writer.fail_forward_read_after = None;
    writer
        .upsert_bm25_file(
            &RelPath::from("src/c.rs"),
            &keys::bm25_content_key("c1", "rust", "d"),
            &postings("c1", &["eps"]),
        )
        .unwrap();
    writer.commit().unwrap();
    pass.finish().unwrap();

    let stamped = db.view().bm25_stats();
    assert!(
        db.code_bm25_postings.get(&orphan).unwrap().is_none(),
        "a pass that saw a staging error sweeps"
    );
    assert_eq!(swept(&db).1, stamped, "the stamped stats equal a recompute");
}

/// M1: a forward row that does not decode is deleted and leaves `N`, but the ledger cannot
/// count it, so the pass must recompute rather than stamp a stale `N`.
#[test]
fn an_undecodable_forward_row_forces_the_recompute() {
    let (_dir, db) = exact_db_with(&[("src/a.rs", "a1", &["alpha"])]);
    let corrupt = keys::code_bm25_by_path(
        &RelPath::from("src/c.rs"),
        &keys::bm25_content_key("c1", "rust", "d"),
        0,
    );
    db.code_bm25_by_path.insert(&corrupt, vec![0u8]).unwrap();
    db.recompute_bm25_stats().unwrap();
    assert_eq!(db.view().bm25_stats().map(|(n, _)| n), Some(2));

    let pass = db.begin_bm25_pass(PassKind::Full).unwrap();
    let mut writer = db.writer();
    writer.remove_bm25_file(&RelPath::from("src/c.rs")).unwrap();
    writer.commit().unwrap();
    pass.finish().unwrap();

    let stamped = db.view().bm25_stats();
    assert_eq!(
        swept(&db).1,
        stamped,
        "the stamped N counts the undecodable row's removal"
    );
}

/// S3: a commit that reports an error may still have reached the journal, so the pass cannot
/// vouch for its tally and must sweep.
#[test]
fn a_commit_error_during_a_pass_forces_the_fallback() {
    let (_dir, db) = exact_db_with(&[("src/a.rs", "a1", &["alpha"])]);
    let orphan = plant_orphan(&db);
    let pass = db.begin_bm25_pass(PassKind::Full).unwrap();
    let mut writer = db.writer();
    writer
        .upsert_bm25_file(
            &RelPath::from("src/a.rs"),
            &keys::bm25_content_key("a2", "rust", "d"),
            &postings("a2", &["beta"]),
        )
        .unwrap();
    db.fault.inject_commit_failures(1);
    assert!(writer.commit().is_err());
    pass.finish().unwrap();
    assert!(
        db.code_bm25_postings.get(&orphan).unwrap().is_none(),
        "a pass that saw a commit error sweeps"
    );
}

#[test]
fn a_pass_whose_retired_keys_outgrow_the_ceiling_sweeps_instead() {
    let ledger = Bm25PassLedger::default();
    let generation = ledger.open();
    let mut delta = Bm25Delta {
        generation,
        retired_bytes: RETIRED_KEYS_CEILING_BYTES + 1,
        ..Bm25Delta::default()
    };
    delta.retired.insert([0; keys::BM25_KEY_LEN], vec![vec![1]]);
    assert!(ledger.absorb(delta));
    assert!(ledger.overflowed());
    let state = ledger.close(generation).expect("the pass is open");
    assert!(
        state.overflowed && state.retired.is_empty(),
        "the keys are dropped, not kept"
    );
}

fn group_views(dir: &std::path::Path, names: &[&str]) -> (std::sync::Arc<super::super::group::GroupDb>, Vec<IndexDb>) {
    let group = super::super::group::GroupDb::open_shared(dir, || Ok(Box::new(()))).unwrap();
    let memory = dir.join("memory");
    let views = names
        .iter()
        .map(|name| {
            let id = group.allocate_view_id(name, "working").unwrap();
            group.open_view(id, &memory, false).unwrap()
        })
        .collect();
    (group, views)
}

fn postings_of(db: &IndexDb, hash: &str) -> usize {
    let content = keys::bm25_content_key(hash, "rust", "d");
    db.code_bm25_postings
        .iter()
        .filter(|row| {
            row.as_ref()
                .ok()
                .and_then(|(key, _)| keys::parse_code_bm25_posting_chunk(key))
                .is_some_and(|(key, _)| key == content)
        })
        .count()
}

/// M2a: a new worktree's first full pass on a populated group starts from exact zero stats and
/// does not sweep the group's posting list.
#[test]
fn a_new_view_on_a_populated_group_takes_the_shortcut() {
    let dir = tempfile::tempdir().unwrap();
    let (_group, views) = group_views(dir.path(), &["ws-a", "ws-b"]);
    let (a, b) = (&views[0], &views[1]);
    let pass = a.begin_bm25_pass(PassKind::Full).unwrap();
    upsert(a, "src/a.rs", "a1", &["alpha", "beta"]);
    pass.finish().unwrap();

    let pass = b.begin_bm25_pass(PassKind::Full).unwrap();
    assert!(pass.is_exact(), "a view with no forward rows is exact at zero");
    upsert(b, "src/a.rs", "a1", &["alpha", "beta"]);
    let outcome = pass.finish().unwrap();
    assert!(
        !outcome.swept,
        "nothing was owed, so the group's postings were not walked"
    );
    assert_eq!(b.view().bm25_stats(), Some((1, 2)));
}

/// M2b: postings a sibling orphaned outside a pass are collected by any view's next full pass.
#[test]
fn garbage_a_sibling_left_is_collected_by_any_views_full_pass() {
    let dir = tempfile::tempdir().unwrap();
    let (_group, views) = group_views(dir.path(), &["ws-a", "ws-b"]);
    let (a, b) = (&views[0], &views[1]);
    for db in [a, b] {
        let pass = db.begin_bm25_pass(PassKind::Full).unwrap();
        upsert(db, "src/a.rs", "a1", &["alpha"]);
        pass.finish().unwrap();
    }
    let pass = b.begin_bm25_pass(PassKind::Full).unwrap();
    upsert(b, "src/x.rs", "x1", &["xray"]);
    pass.finish().unwrap();
    assert_eq!(postings_of(b, "x1"), 1);

    b.purge_paths(&[RelPath::from("src/x.rs")]).unwrap();
    let pass = a.begin_bm25_pass(PassKind::Full).unwrap();
    let outcome = pass.finish().unwrap();
    assert!(outcome.swept, "the sweep a sibling's purge owed is paid by this view");
    assert_eq!(postings_of(a, "x1"), 0, "x1's postings, held by no view, are collected");
    assert_eq!(postings_of(a, "a1"), 1, "a posting both views hold stays");

    let pass = b.begin_bm25_pass(PassKind::Full).unwrap();
    assert!(
        !pass.finish().unwrap().swept,
        "the debt was cleared by the sweep that paid it"
    );
}

/// M2c: emptiness is judged on the view's own forward rows, not on the shared posting list, so a
/// chunk with no terms (a forward row with no postings) is still counted.
#[test]
fn a_view_whose_chunks_have_no_terms_gets_the_right_n() {
    let dir = tempfile::tempdir().unwrap();
    let db = IndexDb::open(dir.path()).unwrap();
    upsert(&db, "src/empty.rs", "e1", &[]);
    assert!(db.code_bm25_postings.iter().next().is_none(), "no term, no posting");
    let pass = db.begin_bm25_pass(PassKind::Full).unwrap();
    upsert(&db, "src/a.rs", "a1", &["alpha"]);
    pass.finish().unwrap();
    assert_eq!(
        db.view().bm25_stats(),
        Some((2, 1)),
        "the term-less chunk counts toward N"
    );
}

/// S4: a second handle on the same view (a reopen, a background lane) shares the open pass's
/// ledger, so its writes are counted rather than missed.
#[test]
fn a_second_handle_on_the_view_writes_into_the_open_pass() {
    let dir = tempfile::tempdir().unwrap();
    let group = super::super::group::GroupDb::open_shared(dir.path(), || Ok(Box::new(()))).unwrap();
    let memory = dir.path().join("memory");
    let id = group.allocate_view_id("ws-a", "working").unwrap();
    let first = group.open_view(id, &memory, false).unwrap();
    let pass = first.begin_bm25_pass(PassKind::Full).unwrap();
    upsert(&first, "src/a.rs", "a1", &["alpha"]);
    upsert(&first, "src/b.rs", "b1", &["beta"]);
    pass.finish().unwrap();

    let second = group.open_view(id, &memory, false).unwrap();
    let pass = first.begin_bm25_pass(PassKind::Full).unwrap();
    upsert(&second, "src/a.rs", "a2", &["gamma", "delta"]);
    let outcome = pass.finish().unwrap();
    let stamped = first.view().bm25_stats();
    assert!(
        !outcome.swept,
        "the write went through the shared ledger, so nothing was owed"
    );
    assert_eq!(postings_of(&first, "a1"), 0, "the second handle's orphan was retired");
    assert_eq!(swept(&first).1, stamped);
}

/// S4: a batch staged before a pass began and committed during it is in neither the pass's base
/// nor its tally, so the pass must not vouch for its stats.
#[test]
fn a_batch_staged_before_the_pass_and_committed_during_it_is_not_lost() {
    let (_dir, db) = exact_db_with(&[("src/a.rs", "a1", &["alpha"])]);
    let mut early = db.writer();
    early
        .upsert_bm25_file(
            &RelPath::from("src/b.rs"),
            &keys::bm25_content_key("b1", "rust", "d"),
            &postings("b1", &["beta", "gamma"]),
        )
        .unwrap();
    let pass = db.begin_bm25_pass(PassKind::Full).unwrap();
    early.commit().unwrap();
    pass.finish().unwrap();
    let stamped = db.view().bm25_stats();
    assert_eq!(swept(&db).1, stamped, "the stats include the early batch");

    let pass = db.begin_bm25_pass(PassKind::Full).unwrap();
    pass.finish().unwrap();
    assert_eq!(swept(&db).1, db.view().bm25_stats(), "and the next pass still agrees");
}

/// S5: the stats do not depend on the retirement, so a retirement that fails must still leave the
/// pass's stats written, and the garbage owed to the next full pass.
#[test]
fn a_failed_retirement_still_writes_the_stats_and_owes_the_sweep() {
    let (_dir, db) = exact_db_with(&[("src/a.rs", "a1", &["alpha"])]);
    let pass = db.begin_bm25_pass(PassKind::Full).unwrap();
    upsert(&db, "src/a.rs", "a2", &["beta", "gamma"]);
    upsert(&db, "src/b.rs", "b1", &["delta"]);
    db.fault.inject_commit_failures(1);
    assert!(pass.finish().is_err(), "the retirement's commit failed");
    assert_eq!(db.view().bm25_stats(), Some((2, 3)), "the stats were written first");

    let pass = db.begin_bm25_pass(PassKind::Full).unwrap();
    assert!(
        pass.finish().unwrap().swept,
        "the failed retirement's garbage is still owed"
    );
    assert_eq!(postings_of(&db, "a1"), 0);
}
