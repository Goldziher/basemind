use super::*;
use crate::config::ConfigV1;
use crate::index::view::IndexView;
use crate::scanner::{EmbedMode, ScanReport, ScanSource, scan};
use crate::store::{VIEW_WORKING, init_isolated_cache};

fn one_file_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let status = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(dir.path())
        .status()
        .expect("run git init");
    assert!(status.success());
    std::fs::write(dir.path().join("alpha.rs"), b"pub fn alpha() -> u32 { 1 }\n").expect("write");
    dir
}

fn full_scan(root: &std::path::Path, store: &mut Store) -> ScanReport {
    scan(
        root,
        store,
        &ConfigV1::with_defaults(),
        ScanSource::WorkingTree,
        EmbedMode::Deferred,
    )
    .expect("a non-fatal commit failure degrades the scan, it does not fail it")
}

/// A commit that fails without a crash loses its batch just as a power cut does. The file map must
/// not record the batch's files, or every later scan would skip them as unchanged over an index that
/// never received their rows.
#[test]
fn a_file_whose_index_commit_failed_is_left_out_of_the_file_map_and_indexed_by_the_next_scan() {
    init_isolated_cache();
    let dir = one_file_repo();
    let root = dir.path();
    let mut store = Store::open(root, VIEW_WORKING).expect("open");
    store.index_db.as_ref().expect("index").fault.inject_commit_failures(1);

    let report = full_scan(root, &mut store);
    assert_eq!(report.stats.updated, 0);
    assert_eq!(
        report.stats.extract_failed, 1,
        "the lost file is reported, not counted as indexed"
    );
    assert!(store.lookup("alpha.rs").is_none());
    assert!(store.index_view().expect("index").symbols_index_is_empty());

    drop(store);
    let mut store = Store::open(root, VIEW_WORKING).expect("reopen");
    assert!(
        store.index.files.is_empty(),
        "the flushed file map does not claim the file"
    );

    let report = full_scan(root, &mut store);
    assert_eq!(report.stats.updated, 1, "the next scan indexes the file");
    assert!(store.lookup("alpha.rs").is_some());
    assert_eq!(
        store
            .index_view()
            .expect("index")
            .symbols_by_name_lookup("alpha", 4)
            .len(),
        1
    );
}

/// The resolve pass writes its rows after the code map was sealed, and the file map already calls
/// their files unchanged, so nothing would rewrite a row a power cut took. Both scan entry points
/// must leave the journal synced past them.
#[test]
fn rows_written_after_the_seal_are_synced_before_a_scan_returns() {
    init_isolated_cache();
    let dir = one_file_repo();
    let root = dir.path();
    let outer = root.join("outer.py");
    std::fs::write(&outer, b"def outer(a):\n    count = 1\n    return count + a\n").expect("write");
    let mut store = Store::open(root, VIEW_WORKING).expect("open");

    full_scan(root, &mut store);
    let db = store.index_db.clone().expect("index");
    let resolved = db.resolution_rows().expect("rows");
    assert!(
        !resolved.is_empty(),
        "the resolve pass wrote rows after the seal, or this test proves nothing"
    );
    assert!(
        !db.has_unsynced_tail(),
        "a full scan syncs what it wrote after the seal"
    );

    std::fs::write(&outer, b"def outer(a):\n    total = 2\n    return total + total + a\n").expect("rewrite");
    let report = crate::scanner::scan_paths(
        root,
        &mut store,
        &ConfigV1::with_defaults(),
        &[outer],
        EmbedMode::Deferred,
    )
    .expect("incremental scan");
    assert_eq!(report.stats.updated, 1);
    assert_ne!(
        db.resolution_rows().expect("rows"),
        resolved,
        "the incremental resolve pass rewrote rows after the seal"
    );
    assert!(
        !db.has_unsynced_tail(),
        "an incremental scan syncs what it wrote after the seal"
    );
}
