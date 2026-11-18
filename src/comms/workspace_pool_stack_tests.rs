//! Hosted read stacks against the eviction sweeps: when a session's pin is taken, and what a drain
//! releases.

use std::sync::Arc;
use std::time::Duration;

use super::{DEFAULT_HOT_CAP, WorkspacePool, no_history, workspace_with_sources};

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
    let (shared, conn) = pool
        .serve_conn(workspace.path(), no_history())
        .await
        .expect("build hosted read stack");
    let weak = Arc::downgrade(&shared.workspace);
    drop(shared);

    assert_eq!(pool.shed_read_stacks().await, 1);
    assert_eq!(pool.warm_read_stacks(), 0);
    tokio::time::timeout(Duration::from_secs(5), async {
        while weak.upgrade().is_some() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the stack and its watcher are released");
    drop(conn);
}
