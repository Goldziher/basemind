//! The scanner keeps one grep trigram bloom row per indexed file: written from the bytes the scan
//! already holds, refreshed on edit, removed with the file, and backfilled by a rescan into an index
//! that predates the keyspace. A prefilter row may be missing or stale, never wrong, so each step
//! asserts what grep is allowed to trust.

use std::path::Path;

use basemind::index::grep_bloom::row_stamp;
use basemind::path::RelPath;
use basemind::store::{Store, VIEW_WORKING};

fn scan(root: &Path) -> Store {
    let mut cfg = basemind::config::default_for_root(root);
    cfg.documents.embed = false;
    cfg.code_search.embed = false;
    let _ = basemind::lang::ensure_grammars().expect("grammar bootstrap");
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let mut store = Store::open(root, VIEW_WORKING).expect("open store");
                basemind::scanner::scan(
                    root,
                    &mut store,
                    &cfg,
                    basemind::scanner::ScanSource::WorkingTree,
                    basemind::scanner::EmbedMode::Inline,
                )
                .expect("scan");
                store
            })
            .join()
            .expect("scan thread")
    })
}

fn stamp(store: &Store, rel: &str) -> Option<(u64, i64)> {
    let row = store.index_db.as_ref()?.grep_bloom_row(&RelPath::from(rel))?;
    row_stamp(&row)
}

fn live_stamp(root: &Path, rel: &str) -> (u64, i64) {
    let meta = std::fs::metadata(root.join(rel)).expect("stat");
    (meta.len(), basemind::scanner_file::mtime_nanos(&meta))
}

#[test]
fn scan_writes_refreshes_and_removes_bloom_rows() {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::write(root.join("a.rs"), "pub fn alpha_function_name() -> u32 { 1 }\n").unwrap();
    std::fs::write(root.join("b.rs"), "pub fn beta_function_name() -> u32 { 2 }\n").unwrap();

    let store = scan(root);
    assert_eq!(stamp(&store, "a.rs"), Some(live_stamp(root, "a.rs")), "scan stamps the row");
    assert_eq!(stamp(&store, "b.rs"), Some(live_stamp(root, "b.rs")));
    drop(store);

    // Edit: the row follows the new bytes.
    std::fs::write(root.join("a.rs"), "pub fn alpha_function_name() -> u32 { 100_000 }\n").unwrap();
    let store = scan(root);
    assert_eq!(stamp(&store, "a.rs"), Some(live_stamp(root, "a.rs")), "edit refreshes the row");
    drop(store);

    // Delete: the row goes with the file.
    std::fs::remove_file(root.join("b.rs")).unwrap();
    let store = scan(root);
    assert_eq!(stamp(&store, "b.rs"), None, "a removed file leaves no row");
    assert!(stamp(&store, "a.rs").is_some());
}

#[test]
fn rescan_backfills_a_row_an_older_index_lacks() {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::write(root.join("a.rs"), "pub fn alpha_function_name() -> u32 { 1 }\n").unwrap();
    let store = scan(root);
    // Simulate an index built before the keyspace existed: the file is indexed, its bloom is gone.
    let idx = store.index_db.clone().expect("index");
    idx.drop_grep_bloom_row_for_test(&RelPath::from("a.rs"));
    assert_eq!(stamp(&store, "a.rs"), None);
    drop(idx);
    drop(store);

    // Nothing changed on disk, so the scan classes the file Unchanged -- and must still backfill.
    let store = scan(root);
    assert_eq!(stamp(&store, "a.rs"), Some(live_stamp(root, "a.rs")), "rescan backfills the row");
}
