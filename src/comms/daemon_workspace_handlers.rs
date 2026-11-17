//! Workspace-registry and rescan handlers for the [`Broker`](super::Broker). Split out of
//! `daemon.rs` to keep it under the 1000-line `rust-max-lines` cap; this is a second `impl Broker`
//! block and the request dispatch stays in `daemon.rs`.

use super::*;

/// Rescans that may hold a blocking thread at once. A scan holds its thread for the whole pass, and
/// the daemon's blocking pool (16 threads) also runs every agent request, read-stack build and
/// forwarded read. A burst of rescans across workspaces took every thread and queued all of those
/// behind the scans.
const MAX_CONCURRENT_RESCANS: usize = 4;

static RESCAN_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(MAX_CONCURRENT_RESCANS);

/// Run `f` on a blocking thread once one of `slots` is free. The permit moves into the closure, so
/// it is held until the work itself ends, even when the caller stops waiting for it.
async fn on_blocking_slot<T, F>(slots: &'static tokio::sync::Semaphore, f: F) -> Result<T, tokio::task::JoinError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    // The semaphore is a static that is never closed. ~keep
    let permit = slots
        .acquire()
        .await
        .unwrap_or_else(|_| unreachable!("rescan slots are never closed"));
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        f()
    })
    .await
}

impl Broker {
    /// Scan/rescan a workspace on the sole-writer pool. The scan is CPU-bound, so it runs on a
    /// blocking thread while the reactor keeps serving other links. A scan/store error becomes a
    /// `CommsResponse::Error` (never a torn link). Held under a [`WorkGuard`] throughout: the
    /// caller is attached by a plain link only, which does not make the daemon refuse `Stop`.
    pub(super) async fn on_rescan(
        &self,
        root: std::path::PathBuf,
        paths: Option<Vec<std::path::PathBuf>>,
        full: bool,
        embed: bool,
        verify: bool,
    ) -> CommsResponse {
        self.mark_active().await;
        // A tripped scan token means the daemon is draining (every drain route cancels first, ~keep
        // and the token never un-trips). Launching a scan now would only produce a doomed ~keep
        // partial pass that defeats coalescing — refuse instead; the client retries against ~keep
        // the next daemon. ~keep
        // The scan counts as work from here to its end, so a `Stop` is refused instead of ~keep
        // cancelling it. Taken under the registry lock, where `on_stop` reads the count and ~keep
        // starts the drain: a rescan either is counted before that read or sees the drain. ~keep
        let _working = {
            let registry = self.registry.lock().await;
            let draining = matches!(registry.state, LifecycleState::Draining | LifecycleState::Stopped);
            if draining || self.scan_cancel.is_cancelled() {
                return CommsResponse::Error {
                    code: "rescan_draining".to_string(),
                    message: "daemon draining; rescan refused (retry against the next daemon)".to_string(),
                };
            }
            self.begin_work()
        };
        let pool = Arc::clone(&self.workspaces);
        let cancel = self.scan_cancel.clone();
        let started = Instant::now();
        match on_blocking_slot(&RESCAN_SLOTS, move || {
            pool.rescan_as(&root, paths, full, embed, verify, &cancel)
        })
        .await
        {
            // A cancelled pass committed only part of the tree — surface it as an error so no ~keep
            // client mistakes the partial index for a completed rescan. ~keep
            Ok(Ok((_, true))) => CommsResponse::Error {
                code: "rescan_cancelled".to_string(),
                message: "daemon draining; partial scan committed".to_string(),
            },
            Ok(Ok((stats, false))) => CommsResponse::Rescanned {
                scanned: stats.scanned,
                updated: stats.updated,
                docs_indexed: stats.docs_indexed,
                docs_degraded: stats.docs_degraded,
                docs_text_fallback: stats.docs_text_fallback,
                doc_timeouts: stats.doc_timeouts,
                removed: stats.removed,
                elapsed_ms: started.elapsed().as_millis() as u64,
            },
            Ok(Err(error)) => CommsResponse::Error {
                code: "rescan_failed".to_string(),
                message: error.to_string(),
            },
            Err(join) => CommsResponse::Error {
                code: "rescan_panicked".to_string(),
                message: join.to_string(),
            },
        }
    }

    /// Report the daemon's currently-hot workspaces for the statusline.
    pub(super) fn on_accessed_paths(&self) -> CommsResponse {
        CommsResponse::Accessed {
            workspaces: self.workspaces.accessed(),
        }
    }

    /// List every registered workspace in the machine registry.
    pub(super) async fn on_workspaces_list(&self) -> CommsResponse {
        let registry = self.machine_registry.lock().await;
        CommsResponse::Workspaces {
            workspaces: registry.workspaces(),
        }
    }

    /// List a registered repo's worktrees. An unknown repo id yields an empty list.
    pub(super) async fn on_worktrees_list(&self, repo_id: String) -> CommsResponse {
        let registry = self.machine_registry.lock().await;
        CommsResponse::Worktrees {
            worktrees: registry.worktrees(&repo_id),
        }
    }

    /// List a registered repo's local branches. An unknown repo id yields an empty list.
    pub(super) async fn on_branches_list(&self, repo_id: String) -> CommsResponse {
        let registry = self.machine_registry.lock().await;
        CommsResponse::Branches {
            branches: registry.branches(&repo_id),
        }
    }

    /// Drop machine-registry rows whose on-disk path is gone, returning how many were removed.
    ///
    /// The registry is append-only by construction: a row is written when a workspace registers and
    /// nothing ever retires it, so every throwaway checkout and test tempdir stays forever and buries
    /// the live repos `workspace workspaces` exists to surface. Called from the daemon's periodic
    /// maintenance pass; a failure is logged and reported as zero rather than propagated, because a
    /// prune is opportunistic housekeeping and must never take the daemon down.
    pub async fn prune_missing_registry_rows(&self) -> usize {
        match self.registry_blocking(|registry| registry.prune_missing()).await {
            Ok(removed) => removed,
            Err(error) => {
                tracing::warn!(%error, "comms: machine registry prune failed");
                0
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::on_blocking_slot;

    /// Rescans past the bound wait for a slot instead of taking more blocking threads.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rescans_past_the_bound_wait_for_a_slot() {
        static SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
        static RUNNING: AtomicUsize = AtomicUsize::new(0);
        static PEAK: AtomicUsize = AtomicUsize::new(0);
        let tasks: Vec<_> = (0..6)
            .map(|_| {
                tokio::spawn(on_blocking_slot(&SLOTS, || {
                    let now = RUNNING.fetch_add(1, Ordering::SeqCst) + 1;
                    PEAK.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(50));
                    RUNNING.fetch_sub(1, Ordering::SeqCst);
                }))
            })
            .collect();
        for task in tasks {
            task.await.expect("join").expect("blocking work");
        }
        assert_eq!(PEAK.load(Ordering::SeqCst), 2, "at most the slot count run at once");
    }
}
