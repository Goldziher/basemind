//! Workspace-registry and rescan handlers for the [`Broker`](super::Broker). Split out of
//! `daemon.rs` to keep it under the 1000-line `rust-max-lines` cap; this is a second `impl Broker`
//! block and the request dispatch stays in `daemon.rs`.

use super::*;

impl Broker {
    /// Scan/rescan a workspace on the sole-writer pool. The scan is CPU-bound, so it runs on a
    /// blocking thread while the reactor keeps serving other links. A scan/store error becomes a
    /// `CommsResponse::Error` (never a torn link).
    pub(super) async fn on_rescan(
        &self,
        root: std::path::PathBuf,
        paths: Option<Vec<std::path::PathBuf>>,
        full: bool,
        embed: bool,
    ) -> CommsResponse {
        self.mark_active().await;
        // A tripped scan token means the daemon is draining (every drain route cancels first, ~keep
        // and the token never un-trips). Launching a scan now would only produce a doomed ~keep
        // partial pass that defeats coalescing — refuse instead; the client retries against ~keep
        // the next daemon. ~keep
        if self.scan_cancel.is_cancelled() {
            return CommsResponse::Error {
                code: "rescan_draining".to_string(),
                message: "daemon draining; rescan refused (retry against the next daemon)".to_string(),
            };
        }
        let _rescan_guard = self.blob_gc_lock.read().await;
        let pool = Arc::clone(&self.workspaces);
        let cancel = self.scan_cancel.clone();
        let started = Instant::now();
        match tokio::task::spawn_blocking(move || pool.rescan(&root, paths, full, embed, &cancel)).await {
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

    /// Advisory-claim a worktree. An unknown `(repo_id, name)` returns `held = false`.
    pub(super) async fn on_worktree_claim(&self, repo_id: String, name: String, claimant: String) -> CommsResponse {
        match self
            .registry_blocking(move |registry| registry.claim_worktree(&repo_id, &name, &claimant))
            .await
        {
            Ok(held) => CommsResponse::ClaimOutcome { held },
            Err(error) => registry_error(error),
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

    /// Release an advisory worktree claim held by `claimant`.
    pub(super) async fn on_worktree_release(&self, repo_id: String, name: String, claimant: String) -> CommsResponse {
        match self
            .registry_blocking(move |registry| registry.release_worktree(&repo_id, &name, &claimant))
            .await
        {
            Ok(held) => CommsResponse::ClaimOutcome { held },
            Err(error) => registry_error(error),
        }
    }
}
