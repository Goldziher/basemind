//! Off-reactor execution of the machine-registry calls of the [`Broker`](super::Broker): its
//! mutations rewrite a file and its prune stats every row's path, so none of that belongs on an
//! async worker.

use std::sync::Arc;

use super::*;

impl Broker {
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

    /// Run `read` against the machine registry once the rows of `repo_id`, or every row with
    /// `None`, are reconciled with the disk, so a listing never shows a branch or worktree that is
    /// gone or misses one that was added. The lock is taken once per repo, so a `Hello` waits for
    /// one repository's enumeration at most. A failed reconciliation is logged and the rows are read
    /// as they stand.
    pub(crate) async fn refreshed_registry<T, F>(&self, repo_id: Option<String>, read: F) -> T
    where
        T: Send + 'static,
        F: FnOnce(&MachineRegistry) -> T + Send + 'static,
    {
        let ids = match repo_id {
            Some(id) => vec![id],
            None => {
                let (ids, outcome) = self
                    .registry_blocking(|registry| {
                        let outcome = registry.refresh_stale_workspaces();
                        (registry.repo_ids(), outcome)
                    })
                    .await;
                if let Err(error) = outcome {
                    tracing::warn!(%error, "comms: machine registry workspace refresh failed");
                }
                ids
            }
        };
        for id in ids {
            let target = id.clone();
            let outcome = self
                .registry_blocking(move |registry| registry.refresh_stale_repo(&target))
                .await;
            if let Err(error) = outcome {
                tracing::warn!(%error, repo_id = %id, "comms: machine registry refresh failed");
            }
        }
        self.registry_blocking(move |registry| read(registry)).await
    }
}
