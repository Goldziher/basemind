//! Scanner-level BM25 upkeep: which path a pass takes, and that its end state is exact.

use std::path::Path;

use super::LAST_BM25_OUTCOME;
use crate::config::Config;
use crate::index::view::IndexView;
use crate::scanner::{EmbedMode, ScanSource, scan, scan_paths};
use crate::store::{Store, VIEW_WORKING};

fn config() -> Config {
    let mut config = crate::config::default_for_root(Path::new("."));
    config.code_search.enabled = true;
    config.code_search.embed = false;
    config
}

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

fn full_scan(root: &Path, store: &mut Store, config: &Config) -> crate::index::bm25_pass::PassOutcome {
    LAST_BM25_OUTCOME.with(|last| last.set(None));
    scan(root, store, config, ScanSource::WorkingTree, EmbedMode::Inline).unwrap();
    LAST_BM25_OUTCOME
        .with(std::cell::Cell::get)
        .expect("the full pass finished its BM25 pass")
}

/// What a sweep and recompute would leave: the posting count and the view's stats.
fn exact_state(store: &Store) -> (usize, Option<(u64, u64)>) {
    let db = store.index_db.as_ref().unwrap();
    let before = (db.posting_count_for_test(), db.view().bm25_stats());
    db.sweep_bm25_postings().unwrap();
    db.recompute_bm25_stats().unwrap();
    let after = (db.posting_count_for_test(), db.view().bm25_stats());
    assert_eq!(before, after, "the pass left the state a sweep and recompute would");
    after
}

/// S1: a watcher batch between two full passes must not cost the second one a sweep or a
/// recompute, and the state stays exact across full -> incremental -> full.
#[test]
fn a_full_pass_after_a_watcher_batch_takes_the_shortcut() {
    crate::store::init_isolated_cache();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(root.join("basemind.toml"), "[scan]\n").unwrap();
    write(root, "src/a.rs", "pub fn alpha() -> u32 { 1 }\n");
    write(root, "src/b.rs", "pub fn beta() -> u32 { 2 }\n");
    let config = config();
    let mut store = Store::open(root, VIEW_WORKING).unwrap();

    full_scan(root, &mut store, &config);
    exact_state(&store);

    write(
        root,
        "src/a.rs",
        "pub fn gamma() -> u32 { 3 }\npub fn delta() -> u32 { 4 }\n",
    );
    std::fs::remove_file(root.join("src/b.rs")).unwrap();
    scan_paths(
        root,
        &mut store,
        &config,
        &[root.join("src/a.rs"), root.join("src/b.rs")],
        EmbedMode::Inline,
    )
    .unwrap();
    exact_state(&store);

    let outcome = full_scan(root, &mut store, &config);
    assert!(
        outcome.stats_from_ledger,
        "the watcher batch left the stats vouched for"
    );
    assert!(
        !outcome.swept,
        "the watcher batch retired what it orphaned; nothing was owed"
    );
    exact_state(&store);
}
