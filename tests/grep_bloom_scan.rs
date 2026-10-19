//! The scanner keeps one grep trigram bloom row per indexed file: written from the bytes the scan
//! already holds, refreshed on edit, removed with the file, and backfilled by a rescan into an index
//! that predates the keyspace. A prefilter row may be missing or stale, never wrong, so each step
//! asserts what grep is allowed to trust.

use std::path::Path;
use std::time::{Duration, SystemTime};

use basemind::path::RelPath;
use basemind::scanner::{EmbedMode, ScanCancel, ScanSource};
use basemind::store::{Store, VIEW_WORKING};

fn config(root: &Path) -> basemind::config::Config {
    let mut cfg = basemind::config::default_for_root(root);
    cfg.documents.embed = false;
    cfg.code_search.embed = false;
    cfg
}

/// Run `work` on a fresh thread, as the scanner's own callers do.
fn off_main<T: Send>(work: impl FnOnce() -> T + Send) -> T {
    let _ = basemind::lang::ensure_grammars().expect("grammar bootstrap");
    std::thread::scope(|scope| scope.spawn(work).join().expect("scan thread"))
}

fn scan_cancellable(root: &Path, cancel: &ScanCancel) -> Store {
    let cfg = config(root);
    off_main(|| {
        let mut store = Store::open(root, VIEW_WORKING).expect("open store");
        basemind::scanner::scan_with_cancel(
            root,
            &mut store,
            &cfg,
            ScanSource::WorkingTree,
            EmbedMode::Inline,
            cancel,
        )
        .expect("scan");
        store
    })
}

fn scan(root: &Path) -> Store {
    scan_cancellable(root, &ScanCancel::new())
}

/// The watcher's path: rescan only `rels` into the open `store`.
fn scan_paths(root: &Path, store: &mut Store, rels: &[&str]) {
    let cfg = config(root);
    let paths: Vec<_> = rels.iter().map(|rel| root.join(rel)).collect();
    off_main(|| basemind::scanner::scan_paths(root, store, &cfg, &paths, EmbedMode::Inline).expect("scan_paths"));
}

/// Move a file's mtime an hour back, past the window in which its stamp is too fresh to trust.
fn age(root: &Path, rel: &str) {
    let file = std::fs::OpenOptions::new().write(true).open(root.join(rel)).unwrap();
    file.set_modified(SystemTime::now() - Duration::from_secs(3600))
        .unwrap();
}

fn write_aged(root: &Path, rel: &str, text: &str) {
    std::fs::write(root.join(rel), text).unwrap();
    age(root, rel);
}

fn orphan_row(store: &Store, key: &str) {
    put_raw(
        store,
        key,
        basemind::index::grep_bloom::build_row(b"nothing indexed holds this"),
    );
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
    write_aged(root, "a.rs", "pub fn alpha_function_name() -> u32 { 1 }\n");
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

/// A row exactly as the path-keyed layout (`ROW_VERSION` 1) wrote it:
/// `version(1) | size(8) | mtime_ns(8)`, then an all-zero bit array. All-zero bits reject every
/// needle, so a reader that trusted this row would wrongly skip the file.
fn v1_row() -> Vec<u8> {
    let mut row = vec![0u8; 17 + 64];
    row[0] = 1;
    row
}

fn put_raw(store: &Store, key: &str, row: Vec<u8>) {
    let idx = store.index_db.as_ref().expect("index");
    let mut writer = idx.writer();
    writer.upsert_grep_bloom(key, row);
    writer.commit().expect("commit");
}

fn raw_row(store: &Store, key: &str) -> Option<Vec<u8>> {
    store
        .index_db
        .as_ref()
        .and_then(|idx| idx.grep_bloom_row(key))
        .map(|row| row.to_vec())
}

fn grep_entry(store: &Store, root: &Path, rel: &str) -> basemind::index::grep_bloom::GrepEntry {
    let entry = store.lookup(RelPath::from(rel)).expect("indexed file");
    basemind::index::grep_bloom::GrepEntry {
        rel: RelPath::from(rel),
        hash_hex: hash_of(root, rel),
        size_bytes: entry.size_bytes,
        mtime_ns: entry.mtime,
    }
}

/// Replace the current row set with what an index built before the content-hash re-key holds: a
/// version-1 row under the path, and (as an A2 build wrote) a version-1 row under the hash.
fn downgrade_to_v1(store: &Store, rel: &str, hash: &str) {
    store
        .index_db
        .as_ref()
        .expect("index")
        .drop_grep_bloom_row_for_test(hash);
    put_raw(store, rel, v1_row());
    put_raw(store, hash, v1_row());
    assert_eq!(raw_row(store, rel).expect("path row")[0], 1, "fixture writes a v1 row");
    assert_eq!(raw_row(store, hash).expect("hash row")[0], 1, "fixture writes a v1 row");
}

#[test]
fn v1_rows_are_absent_on_read_so_grep_scans_every_file() {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    write_aged(root, "a.rs", "pub fn alpha_function_name() -> u32 { 1 }\n");
    let hash = hash_of(root, "a.rs");
    let store = scan(root);
    let entry = grep_entry(&store, root, "a.rs");

    let absent = basemind::index::grep_bloom::Needle::compile("zzz_absent_literal").expect("needle");
    let present = basemind::index::grep_bloom::Needle::compile("alpha_function_name").expect("needle");
    let idx = store.index_db.clone().expect("index");
    let verdicts = |needle| idx.grep_skip_verdicts(root, std::slice::from_ref(&entry), needle);
    assert_eq!(
        verdicts(&absent),
        vec![true],
        "a current row skips a file lacking the literal"
    );

    downgrade_to_v1(&store, "a.rs", &hash);
    assert_eq!(
        verdicts(&absent),
        vec![false],
        "a v1 row (which rejects everything) must not skip"
    );
    assert_eq!(verdicts(&present), vec![false]);
}

#[test]
fn the_next_incremental_scan_sweeps_v1_rows_and_rebuilds_content_keyed_ones() {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    write_aged(root, "a.rs", "pub fn alpha_function_name() -> u32 { 1 }\n");
    let hash = hash_of(root, "a.rs");
    let store = scan(root);
    downgrade_to_v1(&store, "a.rs", &hash);
    drop(store);

    let store = scan(root);
    assert!(raw_row(&store, "a.rs").is_none(), "the path-keyed row is swept");
    let rebuilt = raw_row(&store, &hash).expect("rebuilt under the content hash");
    assert_eq!(rebuilt[0], 2, "the v1 row under the hash was replaced by a v2 row");
    assert_eq!(row_count(&store), 1);
    let entry = grep_entry(&store, root, "a.rs");
    let absent = basemind::index::grep_bloom::Needle::compile("zzz_absent_literal").expect("needle");
    let idx = store.index_db.clone().expect("index");
    assert_eq!(idx.grep_skip_verdicts(root, &[entry], &absent), vec![true]);
}

#[test]
fn migrating_grep_bloom_rows_does_not_wipe_the_index() {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    write_aged(root, "a.rs", "pub fn alpha_function_name() -> u32 { 1 }\n");
    let hash = hash_of(root, "a.rs");
    let store = scan(root);
    downgrade_to_v1(&store, "a.rs", &hash);
    drop(store);

    let store = scan(root);
    assert!(!store.reset_stale_schema && !store.index_rebuild_needed);
    assert!(
        !store.index_db.as_ref().expect("index").wiped_on_open(),
        "rows migrate by row version, so no user re-indexes for them"
    );
    assert!(store.lookup(RelPath::from("a.rs")).is_some());
}

#[test]
fn a_cancelled_scan_does_not_sweep() {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    write_aged(root, "a.rs", "pub fn alpha_function_name() -> u32 { 1 }\n");
    let store = scan(root);
    orphan_row(&store, "orphan");
    drop(store);

    let cancel = ScanCancel::new();
    cancel.cancel();
    let store = scan_cancellable(root, &cancel);
    assert!(
        has_row(&store, "orphan"),
        "a partial pass cannot tell dead rows from unvisited ones"
    );
    drop(store);

    let store = scan(root);
    assert!(!has_row(&store, "orphan"), "the next complete pass sweeps it");
}

#[test]
fn scan_paths_deletes_the_superseded_row_on_edit_and_on_delete() {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    write_aged(root, "a.rs", "pub fn alpha_function_name() -> u32 { 1 }\n");
    write_aged(root, "b.rs", "pub fn beta_function_name() -> u32 { 2 }\n");
    let (a_old, b_old) = (hash_of(root, "a.rs"), hash_of(root, "b.rs"));
    let mut store = scan(root);

    std::fs::write(root.join("a.rs"), "pub fn alpha_function_name() -> u32 { 100_000 }\n").unwrap();
    let a_new = hash_of(root, "a.rs");
    scan_paths(root, &mut store, &["a.rs"]);
    assert!(
        has_row(&store, &a_new) && !has_row(&store, &a_old),
        "an edit moves the row"
    );
    assert!(has_row(&store, &b_old), "an untouched file keeps its row");

    std::fs::remove_file(root.join("b.rs")).unwrap();
    scan_paths(root, &mut store, &["b.rs"]);
    assert!(!has_row(&store, &b_old), "a deleted file's row goes");
    assert_eq!(row_count(&store), 1, "no growth across incremental edits");
}

#[test]
fn scan_paths_keeps_a_row_while_another_path_still_holds_the_content() {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let shared = "pub fn shared_function_name() -> u32 { 1 }\n";
    write_aged(root, "a.rs", shared);
    write_aged(root, "b.rs", shared);
    let hash = hash_of(root, "a.rs");
    let mut store = scan(root);

    std::fs::write(root.join("a.rs"), "pub fn changed_function_name() -> u32 { 2 }\n").unwrap();
    scan_paths(root, &mut store, &["a.rs"]);
    assert!(has_row(&store, &hash), "b.rs still holds the old content");

    std::fs::remove_file(root.join("b.rs")).unwrap();
    scan_paths(root, &mut store, &["b.rs"]);
    assert!(!has_row(&store, &hash), "the last holder is gone");
}

#[test]
fn scan_paths_keeps_a_row_a_path_in_the_same_batch_moved_onto() {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    write_aged(root, "a.rs", "pub fn alpha_function_name() -> u32 { 1 }\n");
    write_aged(root, "b.rs", "pub fn beta_function_name() -> u32 { 2 }\n");
    let (a_old, b_old) = (hash_of(root, "a.rs"), hash_of(root, "b.rs"));
    let mut store = scan(root);

    std::fs::write(root.join("a.rs"), "pub fn beta_function_name() -> u32 { 2 }\n").unwrap();
    std::fs::write(root.join("b.rs"), "pub fn gamma_function_name() -> u32 { 3 }\n").unwrap();
    scan_paths(root, &mut store, &["a.rs", "b.rs"]);
    assert!(has_row(&store, &b_old), "a.rs now holds b.rs's old content");
    assert!(!has_row(&store, &a_old));
    assert_eq!(row_count(&store), 2);
}

#[test]
fn a_freshly_written_file_is_never_skipped_until_a_later_scan_restamps_it() {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    write_aged(root, "warm.rs", "pub fn warm_up_function() -> u32 { 0 }\n");
    let mut store = scan(root);
    let absent = basemind::index::grep_bloom::Needle::compile("zzz_absent_literal").expect("needle");
    let verdict = |store: &Store| {
        let entry = grep_entry(store, root, "a.rs");
        let idx = store.index_db.clone().expect("index");
        idx.grep_skip_verdicts(root, &[entry], &absent)[0]
    };

    // The first scan paid for the grammar bootstrap; an incremental pass on the open store is
    // quick, so the new file's mtime is well inside the window when it is read.
    std::fs::write(root.join("a.rs"), "pub fn alpha_function_name() -> u32 { 1 }\n").unwrap();
    scan_paths(root, &mut store, &["a.rs"]);
    assert_eq!(
        indexed_stamp(&store, "a.rs").1,
        0,
        "a stamp this close to the clock is not recorded"
    );
    assert!(!verdict(&store), "an untrusted stamp keeps the file a candidate");

    age(root, "a.rs");
    scan_paths(root, &mut store, &["a.rs"]);
    assert_eq!(
        indexed_stamp(&store, "a.rs"),
        live_stamp(root, "a.rs"),
        "restamped once settled"
    );
    assert!(verdict(&store), "a settled stamp is trusted again");
}
