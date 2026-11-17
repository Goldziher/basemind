//! Hosted read stacks against the eviction sweeps: when a session's pin is taken.

use std::sync::Arc;
use std::time::Duration;

use super::{DEFAULT_HOT_CAP, UnusedHistoryHost, WorkspacePool, workspace_with_sources};

fn hosts(
    pool: &Arc<WorkspacePool>,
) -> (
    Arc<dyn crate::mcp::HostBackend>,
    Arc<dyn crate::git_history::remote::HistoryHost>,
) {
    (pool.clone(), Arc::new(UnusedHistoryHost))
}

/// A sweep that runs the moment a stack is handed out must find it pinned. When the pin was a
/// second step, a trim or idle sweep in between dropped the stack under the session, which then
/// served an orphan whose watcher had stopped while the next session built a second stack.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stack_is_pinned_before_it_is_handed_out() {
    crate::store::init_isolated_cache();
    let pool = Arc::new(WorkspacePool::new(DEFAULT_HOT_CAP));
    let workspace = workspace_with_sources();
    let (host, history) = hosts(&pool);

    let (shared, conn) = pool
        .serve_conn(workspace.path(), host, history)
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
