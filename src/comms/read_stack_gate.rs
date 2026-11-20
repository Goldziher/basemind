//! The gate hosted read-stack builds pass: how many may run at once across every workspace the
//! daemon hosts.
//!
//! A build holds an O(corpus) map while it runs, so a reconnect storm after a daemon restart would
//! otherwise run one per workspace at once, and the warm cap only trims after each finishes. One at
//! a time made a storm across twenty worktrees queue every first use behind nineteen builds; the gate
//! admits [`crate::mcp::background_pool::build_slots`] of them, which the background pool keeps a
//! thread spare beside for term warms.

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use tokio::sync::Semaphore;

/// The gate every hosted read-stack build takes a permit of. An `Arc` so the permit can be owned by
/// the build itself ([`run_gated_build`]).
pub(crate) static READ_STACK_BUILD_GATE: LazyLock<Arc<Semaphore>> = LazyLock::new(new_build_gate);

/// How long a read-stack build may queue for the gate before giving up, so wedged builds (network
/// FS, a giant repo) cannot park every other workspace's first use forever.
pub(crate) const READ_STACK_BUILD_GATE_WAIT: Duration = Duration::from_secs(15 * 60);

/// A gate of [`crate::mcp::background_pool::build_slots`] permits.
fn new_build_gate() -> Arc<Semaphore> {
    Arc::new(Semaphore::new(crate::mcp::background_pool::build_slots()))
}

/// Run `build` on the background pool ([`crate::mcp::background_pool`]) while holding one permit of
/// `gate`. The permit is owned by the build's closure, so it is released when the BUILD ends, not
/// when the awaiting future is dropped: a cancelled caller (client disconnect, timeout, drain) cannot
/// free its permit while its build is still running and let one more O(corpus) build start beside it.
pub(crate) async fn run_gated_build<T: Send + 'static>(
    gate: Arc<Semaphore>,
    wait: Duration,
    build: impl FnOnce() -> T + Send + 'static,
) -> anyhow::Result<T> {
    let permit = tokio::time::timeout(wait, gate.acquire_owned())
        .await
        .map_err(|_| anyhow::anyhow!("timed out after {wait:?} waiting for the read stack build gate"))?
        .map_err(|closed| anyhow::anyhow!("read stack build gate closed: {closed}"))?;
    crate::mcp::background_pool::background()
        .run(move || {
            let _permit = permit;
            build()
        })
        .await
        .ok_or_else(|| anyhow::anyhow!("hosted read stack build panicked"))
}

#[cfg(test)]
#[path = "read_stack_gate_tests.rs"]
mod tests;
