//! The scanner keeps one grep trigram bloom row per indexed file: written from the bytes the scan
//! already holds, refreshed on edit, removed with the file, and backfilled by a rescan into an index
//! that predates the keyspace. A prefilter row may be missing or stale, never wrong, so each step
//! asserts what grep is allowed to trust.

use std::path::Path;

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

fn hash_of(root: &Path, rel: &str) -> String {
    basemind::hashing::hex(&basemind::hashing::hash_bytes(
        &std::fs::read(root.join(rel)).expect("read"),
    ))
}

fn has_row(store: &Store, hash_hex: &str) -> bool {
    store
        .index_db
        .as_ref()
        .is_some_and(|idx| idx.grep_bloom_row(hash_hex).is_some())
}

fn row_count(store: &Store) -> usize {
    store.index_db.as_ref().map_or(0, |idx| idx.grep_bloom_row_count())
}

fn indexed_stamp(store: &Store, rel: &str) -> (u64, i64) {
    let entry = store.lookup(RelPath::from(rel)).expect("indexed file");
    (entry.size_bytes, entry.mtime)
}

fn live_stamp(root: &Path, rel: &str) -> (u64, i64) {
    let meta = std::fs::metadata(root.join(rel)).expect("stat");
    (meta.len(), basemind::scanner_file::mtime_nanos(&meta))
}

#[test]
fn scan_writes_refreshes_and_sweeps_bloom_rows() {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::write(root.join("a.rs"), "pub fn alpha_function_name() -> u32 { 1 }\n").unwrap();
    std::fs::write(root.join("b.rs"), "pub fn beta_function_name() -> u32 { 2 }\n").unwrap();
    let (a_old, b_old) = (hash_of(root, "a.rs"), hash_of(root, "b.rs"));

    let store = scan(root);
    assert!(
        has_row(&store, &a_old) && has_row(&store, &b_old),
        "scan writes a row per content"
    );
    assert_eq!(row_count(&store), 2);
    drop(store);

    std::fs::write(root.join("a.rs"), "pub fn alpha_function_name() -> u32 { 100_000 }\n").unwrap();
    let a_new = hash_of(root, "a.rs");
    let store = scan(root);
    assert!(has_row(&store, &a_new), "edit writes the new content's row");
    assert!(!has_row(&store, &a_old), "the superseded content's row is swept");
    drop(store);

    std::fs::remove_file(root.join("b.rs")).unwrap();
    let store = scan(root);
    assert!(!has_row(&store, &b_old), "a removed file's row is swept");
    assert!(has_row(&store, &a_new));
    assert_eq!(row_count(&store), 1);
}

#[test]
fn identical_files_share_one_row_which_outlives_either_path() {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let text = "pub fn shared_function_name() -> u32 { 1 }\n";
    std::fs::write(root.join("a.rs"), text).unwrap();
    std::fs::write(root.join("b.rs"), text).unwrap();
    let hash = hash_of(root, "a.rs");

    let store = scan(root);
    assert_eq!(row_count(&store), 1, "two paths, one content, one row");
    drop(store);

    std::fs::remove_file(root.join("b.rs")).unwrap();
    let store = scan(root);
    assert!(has_row(&store, &hash), "a.rs still references the content");
    drop(store);

    std::fs::remove_file(root.join("a.rs")).unwrap();
    let store = scan(root);
    assert_eq!(row_count(&store), 0, "no file references the content any more");
}

#[test]
fn a_touched_file_keeps_its_indexed_stamp_current() {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::write(root.join("a.rs"), "pub fn alpha_function_name() -> u32 { 1 }\n").unwrap();
    let store = scan(root);
    assert_eq!(indexed_stamp(&store, "a.rs"), live_stamp(root, "a.rs"));
    drop(store);

    let file = std::fs::OpenOptions::new().write(true).open(root.join("a.rs")).unwrap();
    file.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(60))
        .unwrap();
    drop(file);
    let store = scan(root);
    assert_eq!(
        indexed_stamp(&store, "a.rs"),
        live_stamp(root, "a.rs"),
        "same content, moved stamp: the entry follows so grep keeps trusting the row"
    );
}

#[test]
fn rescan_backfills_a_row_an_older_index_lacks() {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::write(root.join("a.rs"), "pub fn alpha_function_name() -> u32 { 1 }\n").unwrap();
    let hash = hash_of(root, "a.rs");
    let store = scan(root);
    let idx = store.index_db.clone().expect("index");
    idx.drop_grep_bloom_row_for_test(&hash);
    assert!(!has_row(&store, &hash));
    drop(idx);
    drop(store);

    let store = scan(root);
    assert!(has_row(&store, &hash), "rescan backfills the row");
}
