//! The daemon's scan slots: how many scans may hold a blocking thread at once, whoever asked for
//! them — a client's rescan over the socket, or the watcher of a read stack the daemon hosts.
//!
//! A scan holds its thread for the whole pass, and the daemon's blocking pool also runs every
//! agent request and forwarded read. Waiting for a slot holds no thread, so the reads behind a
//! burst of scans are answered while the scans queue.

use std::sync::Arc;

/// Scans that may hold a blocking thread at once. A burst of rescans across workspaces once took
/// every thread of the blocking pool and queued every read behind the scans.
const MAX_CONCURRENT_RESCANS: usize = 4;

/// The slots every daemon scan takes one of.
pub(crate) static RESCAN_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(MAX_CONCURRENT_RESCANS);

/// Run `f` on a blocking thread once one of `slots` is free. The permit moves into the closure, so
/// it is held until the work itself ends, even when the caller stops waiting for it.
pub(crate) async fn on_blocking_slot<T, F>(
    slots: &'static tokio::sync::Semaphore,
    f: F,
) -> Result<T, tokio::task::JoinError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let permit = acquire(slots).await;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        f()
    })
    .await
}

/// Run the passes `next` yields, each on a blocking thread holding one of `slots`, until it yields
/// none.
///
/// `next` is asked only once a slot is held, so the batches that arrive while the drain waits are
/// merged into the pass it takes ([`crate::engine::scan_queue::ScanQueue::take`]). A pass that
/// panics is logged and the drain goes on with the next one.
pub(crate) async fn drain<P, N, R>(slots: &'static tokio::sync::Semaphore, next: N, run: R)
where
    P: Send + 'static,
    N: Fn() -> Option<P> + Send,
    R: Fn(P) + Send + Sync + 'static,
{
    let run = Arc::new(run);
    loop {
        let permit = acquire(slots).await;
        let Some(pass) = next() else {
            return;
        };
        let run = Arc::clone(&run);
        let ran = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            run(pass)
        })
        .await;
        if let Err(error) = ran {
            tracing::error!(%error, "queued watcher scan panicked");
        }
    }
}

async fn acquire(slots: &'static tokio::sync::Semaphore) -> tokio::sync::SemaphorePermit<'static> {
    // The semaphores are statics that are never closed. ~keep
    slots
        .acquire()
        .await
        .unwrap_or_else(|_| unreachable!("scan slots are never closed"))
}

#[cfg(test)]
#[path = "scan_slots_tests.rs"]
mod tests;
