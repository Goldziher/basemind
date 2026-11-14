//! Off-reactor execution of synchronous [`CommsStore`] calls for the [`Broker`](super::Broker).
//!
//! The store is fjall: every call is blocking disk I/O, a write can fsync, and `list_threads`,
//! `prune`, and history scans walk key ranges. The daemon serves every link from one small tokio
//! runtime, so running those calls inline on a worker lets a slow disk stall `Ping`/`Status` and every
//! other connection. Handlers instead hand a closure to [`Broker::store_blocking`] (reads) or
//! [`Broker::store_write`] (mutations), which run it on tokio's blocking pool.

use std::sync::Arc;

use super::*;

impl Broker {
    /// Run `f` against the store on the blocking pool. No async lock is held across the call, so
    /// the reactor keeps serving other links while the disk works.
    ///
    /// A panic inside `f` is re-raised on the caller (the same outcome as running it inline); a
    /// runtime shutdown that cancels the task surfaces as an [`CommsStoreError::Io`] error response.
    pub(crate) async fn store_blocking<T, F>(&self, f: F) -> Result<T, CommsStoreError>
    where
        T: Send + 'static,
        F: FnOnce(&CommsStore) -> Result<T, CommsStoreError> + Send + 'static,
    {
        let store = Arc::clone(&self.store);
        #[cfg(test)]
        let delay = Arc::clone(&self.store_delay_ms);
        let joined = tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            {
                // One-shot: only the next store call pays the injected latency.
                let ms = delay.swap(0, Ordering::SeqCst);
                if ms > 0 {
                    std::thread::sleep(Duration::from_millis(ms));
                }
            }
            f(&store)
        })
        .await;
        match joined {
            Ok(result) => result,
            Err(join) if join.is_panic() => std::panic::resume_unwind(join.into_panic()),
            Err(join) => Err(CommsStoreError::Io {
                path: std::path::PathBuf::from("comms store"),
                source: std::io::Error::other(join.to_string()),
            }),
        }
    }

    /// [`store_blocking`](Self::store_blocking) for a mutation. Writers take the broker's write
    /// gate (a plain mutex, only ever held on a blocking thread) so the read-modify-write sequences
    /// the handlers perform — a post's `seq` allocation, thread-record updates — stay serialized
    /// exactly as they were when every handler ran inline, instead of racing on the blocking pool.
    pub(crate) async fn store_write<T, F>(&self, f: F) -> Result<T, CommsStoreError>
    where
        T: Send + 'static,
        F: FnOnce(&CommsStore) -> Result<T, CommsStoreError> + Send + 'static,
    {
        let gate = Arc::clone(&self.store_write_gate);
        self.store_blocking(move |store| {
            let _held = gate.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            f(store)
        })
        .await
    }

    /// Apply the retention policy once: the daemon's startup and hourly sweep. It runs through
    /// `store_write` because it rewrites thread records and deletes
    /// memberships, which would otherwise race a concurrent post, archive, or membership change
    /// and write back a stale thread record. Failures are logged, never returned: a failed sweep
    /// must not stop the maintenance loop that called it.
    pub async fn run_retention_maintenance(&self) {
        let swept = self
            .store_write(|store| {
                Ok(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| retention_sweep(store))).is_ok())
            })
            .await;
        match swept {
            Ok(true) => {}
            Ok(false) => tracing::warn!("comms: retention maintenance panicked"),
            Err(error) => tracing::warn!(%error, "comms: retention maintenance task failed"),
        }
    }

    /// Run `f` against the machine registry on the blocking pool. Its mutations rewrite a file and
    /// its prune stats every row's path, so none of that belongs on a worker. The registry's async
    /// lock is taken owned and moved into the closure: other registry users queue behind it (that
    /// is the lock's job) but no runtime worker is parked while it is held.
    pub(crate) async fn registry_blocking<T, F>(&self, f: F) -> T
    where
        T: Send + 'static,
        F: FnOnce(&mut MachineRegistry) -> T + Send + 'static,
    {
        let mut guard = Arc::clone(&self.machine_registry).lock_owned().await;
        match tokio::task::spawn_blocking(move || f(&mut guard)).await {
            Ok(value) => value,
            Err(join) if join.is_panic() => std::panic::resume_unwind(join.into_panic()),
            // Only a runtime shutdown cancels a blocking task; the caller is being torn down too.
            Err(_) => std::future::pending().await,
        }
    }
}

/// Apply the effective machine-global comms retention policy. Called at startup and hourly so a
/// frequently restarted daemon still converges, and so policy is visible in structured logs.
fn retention_sweep(store: &CommsStore) {
    use crate::comms::store::{EPHEMERAL_AGENT_TTL, MESSAGE_TTL, THREAD_IDLE_TTL, THREAD_RETENTION_TTL};

    let mut succeeded = true;
    match store.prune_expired(MESSAGE_TTL) {
        Ok(count) if count > 0 => tracing::info!(pruned = count, "comms: pruned expired messages"),
        Ok(_) => {}
        Err(error) => {
            succeeded = false;
            tracing::warn!(%error, "comms: message prune failed");
        }
    }
    match store.archive_idle(THREAD_IDLE_TTL) {
        Ok(count) if count > 0 => tracing::info!(archived = count, "comms: archived idle threads"),
        Ok(_) => {}
        Err(error) => {
            succeeded = false;
            tracing::warn!(%error, "comms: thread archive failed");
        }
    }
    match store.purge_archived(THREAD_RETENTION_TTL) {
        Ok(count) if count > 0 => tracing::info!(purged = count, "comms: purged archived threads"),
        Ok(_) => {}
        Err(error) => {
            succeeded = false;
            tracing::warn!(%error, "comms: archived-thread purge failed");
        }
    }
    match store.prune_ephemeral_agents(EPHEMERAL_AGENT_TTL) {
        Ok(count) if count > 0 => tracing::info!(pruned = count, "comms: pruned stale ephemeral agents"),
        Ok(_) => {}
        Err(error) => {
            succeeded = false;
            tracing::warn!(%error, "comms: ephemeral-agent prune failed");
        }
    }
    match crate::comms::identity::prune_expired_claims(crate::comms::identity::CLAIM_TTL) {
        Ok(count) if count > 0 => tracing::info!(pruned = count, "comms: pruned stale identity claims"),
        Ok(_) => {}
        Err(error) => {
            succeeded = false;
            tracing::warn!(%error, "comms: identity-claim prune failed");
        }
    }
    if succeeded && let Err(error) = store.record_maintenance() {
        tracing::warn!(%error, "comms: record maintenance completion failed");
    }
}
