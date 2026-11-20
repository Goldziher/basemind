//! Hosted read stacks against the eviction sweeps: when a session's pin is taken, and what a drain
//! releases.

use std::sync::Arc;
use std::time::Duration;

use super::{DEFAULT_HOT_CAP, WorkspacePool, hosted_stack, no_history, settled, workspace_with_sources};

/// A sweep that runs the moment a stack is handed out must find it pinned. When the pin was a
/// second step, a trim or idle sweep in between dropped the stack under the session, which then
/// served an orphan whose watcher had stopped while the next session built a second stack.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stack_is_pinned_before_it_is_handed_out() {
    crate::store::init_isolated_cache();
    let pool = Arc::new(WorkspacePool::new(DEFAULT_HOT_CAP));
    let workspace = workspace_with_sources();

    let (shared, conn) = pool
        .serve_conn(workspace.path(), no_history())
        .await
        .expect("build hosted read stack");
    assert_eq!(
        pool.evict_idle_read_stacks(Duration::ZERO),
        0,
        "a stack with a session is not idle"
    );
    assert_eq!(pool.warm_read_stacks(), 1);

    drop(conn);
    drop(shared);
    assert_eq!(
        pool.evict_idle_read_stacks(Duration::ZERO),
        1,
        "unpinned once the session ends"
    );
}

/// A drain sheds every hosted stack, pinned or not. A stack holds the pool (its writer) and the
/// broker's git-history index, so a stack left in its slot kept both alive, with the comms
/// store and every workspace store, after the daemon lock had been released.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shedding_releases_every_stack_and_its_watcher() {
    crate::store::init_isolated_cache();
    let pool = Arc::new(WorkspacePool::new(DEFAULT_HOT_CAP));
    let workspace = workspace_with_sources();
    // Indexed first, so the stack boots into the catch-up pass, which lets go of it before the
    // workspace reads settled; an initial scan leaves an embedding pass holding it. ~keep
    pool.rescan(
        workspace.path(),
        None,
        false,
        false,
        &crate::scanner::ScanCancel::default(),
    )
    .expect("scan workspace");
    let (shared, conn) = pool
        .serve_conn(workspace.path(), no_history())
        .await
        .expect("build hosted read stack");
    settled(&shared).await;
    let weak = Arc::downgrade(&shared.workspace);
    let watcher_exited = only_watcher_exited(&pool);
    drop(shared);

    assert_eq!(pool.shed_read_stacks().await, 1);
    assert_eq!(pool.warm_read_stacks(), 0);
    watcher_exited.await;
    assert_eq!(weak.strong_count(), 0, "the stack and its watcher are released");
    drop(conn);
}

/// The group database behind `root`'s writer handle, held weakly: the allocation outlives the
/// database, so a later handle can be told apart from it by address.
fn writer_group(pool: &WorkspacePool, root: &std::path::Path) -> std::sync::Weak<crate::index::group::GroupDb> {
    pool.with_workspace(root, |store| {
        let db = store.index_db.as_ref().expect("the writer holds the index");
        Arc::downgrade(&db.group.as_ref().expect("a group view").group)
    })
    .expect("hot workspace")
}

/// A poisoned writer handle is replaced by a NEW database even while a hosted stack serves the
/// workspace. The hosted store used to own a clone of the handle, so the reopen found the poisoned
/// database still open, got it back from the process registry, and every later commit failed.
///
/// The stack settles before the poisoning: its boot catch-up is a whole-tree pass through the same
/// writer, so a catch-up still pending would meet the poison first and reopen the database before
/// the test's own pass ran (issue #202). Its watcher scans only on a file event, and nothing in the
/// tree changes until the database has been replaced.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_poisoned_index_is_reopened_while_a_hosted_stack_serves_the_workspace() {
    crate::store::init_isolated_cache();
    let pool = Arc::new(WorkspacePool::new(DEFAULT_HOT_CAP));
    let workspace = workspace_with_sources();
    let root = workspace.path();
    pool.rescan(root, None, true, false, &crate::scanner::ScanCancel::default())
        .expect("scan");
    let (shared, conn) = pool.serve_conn(root, no_history()).await.expect("hosted stack");
    settled(&shared).await;
    assert!(
        shared.workspace.store_read().read_index_view().is_some(),
        "the hosted store reads the writer's index"
    );

    let poisoned = writer_group(&pool, root);
    pool.with_workspace(root, |store| {
        let db = store.index_db.as_ref().expect("index");
        db.record_commit_error(&crate::index::IndexError::Fjall(fjall::Error::Poisoned));
        assert!(db.is_poisoned());
    })
    .expect("hot workspace");

    let rescan = {
        let (pool, root) = (Arc::clone(&pool), root.to_path_buf());
        tokio::task::spawn_blocking(move || pool.rescan(&root, None, true, false, &Default::default()))
            .await
            .expect("join")
    };
    assert!(
        matches!(
            &rescan,
            Err(super::super::WorkspacePoolError::Scan(crate::scanner::ScanError::IndexFault(reason)))
                if reason.starts_with("index handle poisoned")
        ),
        "the test's own pass meets the poisoned handle: {rescan:?}"
    );

    let reopened = writer_group(&pool, root);
    assert!(
        poisoned.upgrade().is_none(),
        "every handle on the poisoned database was released"
    );
    assert!(!std::sync::Weak::ptr_eq(&poisoned, &reopened), "a different database");
    std::fs::write(root.join("gamma.rs"), "pub fn gamma() -> u32 { 3 }\n").expect("write gamma");
    // ~keep The watcher may commit gamma.rs before this pass takes the writer, so the pass's counts
    // ~keep say nothing; the row the hosted store reads afterwards is the proof either way.
    let (_, cancelled) = {
        let (pool, root) = (Arc::clone(&pool), root.to_path_buf());
        tokio::task::spawn_blocking(move || pool.rescan(&root, None, true, false, &Default::default()))
            .await
            .expect("join")
            .expect("the reopened handle commits")
    };
    assert!(!cancelled);
    let hosted = shared.workspace.store_read().read_index_view();
    let hosted = hosted.expect("the hosted store reads the reopened index");
    assert_eq!(
        hosted.symbols_by_name_lookup("gamma", 8).len(),
        1,
        "the hosted store reads gamma.rs from the new database"
    );
    drop((shared, conn));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_eviction_stops_hosted_watcher_and_releases_read_stack() {
    crate::store::init_isolated_cache();
    let pool = Arc::new(WorkspacePool::new(DEFAULT_HOT_CAP));
    let workspace = workspace_with_sources();
    pool.rescan(
        workspace.path(),
        None,
        false,
        false,
        &crate::scanner::ScanCancel::default(),
    )
    .expect("scan workspace");

    // ~keep Settled: the catch-up pass holds the stack until it ends, as any scan of it would.
    let shared = hosted_stack(&pool, workspace.path()).await;
    let weak = Arc::downgrade(&shared.workspace);
    let watcher_exited = only_watcher_exited(&pool);
    drop(shared);

    assert_eq!(
        pool.evict_idle(Duration::ZERO),
        1,
        "workspace is removed from the hot pool"
    );
    watcher_exited.await;
    assert_eq!(
        weak.strong_count(),
        0,
        "once its watcher has exited, nothing holds the evicted read stack"
    );
}

/// Resolves once the watcher of `pool`'s only hosted stack has exited, after the stack is dropped.
/// A stopping watcher hands its OS stream back and releases the stack on its own thread, so how
/// long that takes is up to the scheduler; the bound here is a hang guard, not a deadline.
fn only_watcher_exited(pool: &WorkspacePool) -> impl std::future::Future<Output = ()> + use<> {
    let entries: Vec<Arc<super::super::WorkspaceEntry>> = pool.lock_map().values().cloned().collect();
    assert_eq!(entries.len(), 1, "one hosted workspace");
    let slot = entries[0].serve_state.try_lock().expect("the stack slot is free");
    let exited = slot.as_ref().expect("a hosted stack").watcher_exited();
    async move {
        tokio::time::timeout(Duration::from_secs(120), exited.wait())
            .await
            .expect("the watcher thread exits");
    }
}
