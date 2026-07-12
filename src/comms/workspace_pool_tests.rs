//! Unit tests for [`WorkspacePool`](super::WorkspacePool). Included from `workspace_pool.rs` via a
//! `#[cfg(test)] #[path = "workspace_pool_tests.rs"] mod tests;` declaration, so `super` resolves to
//! the `workspace_pool` module. Every test seeds an isolated global cache first so writes land in a
//! tempdir, never the real XDG data home.

use std::time::Duration;

use super::*;

/// A temp workspace holding two trivial Rust sources — enough for the scanner to index symbols.
fn workspace_with_sources() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("alpha.rs"), "pub fn alpha() -> u32 { 1 }\n").expect("write alpha");
    std::fs::write(dir.path().join("beta.rs"), "pub fn beta() -> u32 { 2 }\n").expect("write beta");
    dir
}

#[test]
fn rescan_indexes_sources_and_is_idempotent() {
    store::init_isolated_cache();
    let pool = WorkspacePool::new(DEFAULT_HOT_CAP);
    let ws = workspace_with_sources();

    let first = pool.rescan(ws.path(), None, false).expect("first scan");
    assert_eq!(first.scanned, 2, "both sources considered");
    assert_eq!(first.updated, 2, "both sources newly indexed");

    let second = pool.rescan(ws.path(), None, false).expect("second scan");
    assert_eq!(second.scanned, 2, "both sources still considered");
    assert_eq!(second.updated, 0, "nothing changed on the second pass");
    assert_eq!(second.skipped_unchanged, 2, "both sources skipped as unchanged");
}

#[test]
fn lru_eviction_keeps_only_the_most_recent_within_the_cap() {
    store::init_isolated_cache();
    let pool = WorkspacePool::new(1);
    let ws1 = workspace_with_sources();
    let ws2 = workspace_with_sources();

    pool.rescan(ws1.path(), None, false).expect("scan ws1");
    assert_eq!(pool.len(), 1);

    pool.rescan(ws2.path(), None, false).expect("scan ws2");
    assert_eq!(pool.len(), 1, "cap of 1 holds a single hot workspace");

    let hot = pool.accessed();
    assert_eq!(hot.len(), 1);
    assert_eq!(hot[0].root, ws2.path(), "the most-recently-used workspace survived");
}

#[test]
fn evicted_workspace_lazily_reopens_with_its_committed_index_intact() {
    store::init_isolated_cache();
    // A cap of 1 forces the second open to evict the first from RAM.
    let pool = WorkspacePool::new(1);
    let ws1 = workspace_with_sources();
    let ws2 = workspace_with_sources();

    pool.rescan(ws1.path(), None, false).expect("scan ws1");
    let hot_files = pool
        .with_workspace(ws1.path(), |store| store.index.files.len())
        .expect("read ws1 while hot");
    assert_eq!(hot_files, 2, "ws1's two sources are indexed while it is hot");

    // Opening ws2 past the cap evicts ws1 from RAM — its on-disk cache survives.
    pool.rescan(ws2.path(), None, false).expect("scan ws2");
    assert_eq!(pool.len(), 1, "cap of 1 holds a single hot workspace");
    assert!(
        pool.accessed().iter().all(|w| w.root != ws1.path()),
        "ws1 must have been evicted from the hot set"
    );

    // Re-requesting ws1 lazily reopens it from disk (no rescan); the committed index is intact.
    let recovered = pool
        .with_workspace(ws1.path(), |store| {
            (
                store.index.files.len(),
                store.lookup("alpha.rs").is_some(),
                store.lookup("beta.rs").is_some(),
            )
        })
        .expect("reopen evicted ws1");
    assert_eq!(
        recovered,
        (2, true, true),
        "the reopened workspace recovers its indexed files from disk without a rescan"
    );
}

#[test]
fn accessed_reports_the_hot_set() {
    store::init_isolated_cache();
    let pool = WorkspacePool::new(DEFAULT_HOT_CAP);
    let ws = workspace_with_sources();
    pool.rescan(ws.path(), None, false).expect("scan");

    let hot = pool.accessed();
    assert_eq!(hot.len(), 1);
    assert_eq!(hot[0].root, ws.path());
    assert_eq!(hot[0].key, store::workspace_key(ws.path()));
}

#[test]
fn evict_idle_zero_drops_every_entry() {
    store::init_isolated_cache();
    let pool = WorkspacePool::new(DEFAULT_HOT_CAP);
    let ws = workspace_with_sources();
    pool.rescan(ws.path(), None, false).expect("scan");
    assert_eq!(pool.len(), 1);

    let dropped = pool.evict_idle(Duration::ZERO);
    assert_eq!(dropped, 1, "a zero idle window evicts everything");
    assert_eq!(pool.len(), 0);
}
