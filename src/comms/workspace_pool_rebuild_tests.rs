//! A workspace whose index was wiped under a populated file map: queued for, and repaired by, a full
//! rebuild.

use super::*;

/// Drop the group index of `root`, leaving `index.msgpack` intact: the state a partition revision
/// bump leaves behind (an empty, recreated index over a populated file map).
fn wipe_fjall(root: &Path) {
    let dir = crate::store_layout::group_index_dir(root).join(crate::index::INDEX_DIR);
    std::fs::remove_dir_all(dir).expect("remove fjall dir");
}

#[test]
fn a_workspace_opened_over_a_wiped_index_is_marked_and_rebuilt_by_the_next_rescan() {
    store::init_isolated_cache();
    let ws = workspace_with_sources();
    {
        let pool = WorkspacePool::new(DEFAULT_HOT_CAP);
        pool.rescan(ws.path(), None, false, false, &ScanCancel::default())
            .expect("initial scan");
    }
    wipe_fjall(ws.path());

    let pool = WorkspacePool::new(DEFAULT_HOT_CAP);
    let entry = pool.get_or_open(ws.path()).expect("open");
    assert!(
        entry.rebuild_pending.load(std::sync::atomic::Ordering::Acquire),
        "an empty index over a populated file map must be queued for a full rebuild"
    );
    let status = store::read_status_sidecar(&entry.store.lock().expect("store").basemind_dir).expect("sidecar");
    assert!(
        status.is_degraded(),
        "admin status must read degraded while the rebuild is owed"
    );

    // An incremental request still runs full while the rebuild is pending.
    let alpha = vec![PathBuf::from("alpha.rs")];
    let (stats, cancelled) = pool
        .rescan(ws.path(), Some(alpha), false, false, &ScanCancel::default())
        .expect("rescan");
    assert!(!cancelled);
    assert_eq!(stats.updated, 2, "both files are re-extracted into the empty index");
    assert!(!entry.rebuild_pending.load(std::sync::atomic::Ordering::Acquire));
    let repopulated = pool
        .with_workspace(ws.path(), |s| {
            !s.index_db.as_ref().expect("index").view().symbols_index_is_empty() && !s.index_rebuild_needed
        })
        .expect("workspace");
    assert!(repopulated, "the fjall index holds symbols again");
}

#[test]
fn a_populated_workspace_with_an_intact_index_is_not_queued_for_rebuild() {
    store::init_isolated_cache();
    let ws = workspace_with_sources();
    {
        let pool = WorkspacePool::new(DEFAULT_HOT_CAP);
        pool.rescan(ws.path(), None, false, false, &ScanCancel::default())
            .expect("initial scan");
    }
    let pool = WorkspacePool::new(DEFAULT_HOT_CAP);
    let entry = pool.get_or_open(ws.path()).expect("open");
    assert!(!entry.rebuild_pending.load(std::sync::atomic::Ordering::Acquire));
}
