//! Keeping the git-history index caught up with HEAD on a long-lived serve.
//!
//! A serve syncs the index once at start, and again whenever a history call finds it behind HEAD
//! (`git_history_if_fresh` in `engine/git/common.rs`): a commit made while the server runs is otherwise never
//! indexed, and every later history call stays on the bounded live walk. One sync runs at a time per
//! read stack; calls that find the index behind while it runs answer from the live walk.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::engine::Workspace;

/// Clears the in-flight flag when the sync ends, however it ends.
struct Syncing(Arc<AtomicBool>);

impl Drop for Syncing {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Bring the git-history index up to date with HEAD, off the calling thread, unless a sync is
/// already running or this is the one-shot CLI. A session whose handle is daemon-backed asks the
/// daemon (which serializes builds per repo, so N sessions cause one walk); a daemon-hosted handle
/// syncs through the in-process seam; a session that holds the database syncs in-process.
///
/// Keyed off the handle the routing produced, not off `daemon_writer`: a non-`daemon_writer`
/// session that found a live daemon also holds a forwarding handle, and asking the builder to write
/// through it is a guaranteed [`NotLocal`](crate::git_history::GitHistoryError::NotLocal).
///
/// The one-shot CLI never syncs: it exits in milliseconds, and a first build on a deep repo is a
/// minutes-long walk.
pub(super) fn request(shared: &Workspace) {
    let Some(index) = shared.git_history.as_deref() else {
        return;
    };
    if !shared.history_catch_up {
        return;
    }
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    if shared
        .history_syncing
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let syncing = Syncing(Arc::clone(&shared.history_syncing));
    #[cfg(all(feature = "comms", any(unix, windows)))]
    if let Some(host) = crate::git_history::remote::history_host(index) {
        let root = shared.root.clone();
        runtime.spawn(async move {
            let _syncing = syncing;
            match host
                .run_history(root, crate::git_history::proto::GitHistoryOp::Sync)
                .await
            {
                Ok(reply) => tracing::info!(?reply, "git-history index synced in-process (hosted)"),
                Err(error) => tracing::warn!(%error, "git-history in-process sync failed; history tools live-walk"),
            }
        });
        return;
    }
    #[cfg(all(feature = "comms", any(unix, windows)))]
    if index.forwarder().is_some_and(|forwarder| forwarder.hosts_in_process()) {
        return;
    }
    #[cfg(all(feature = "comms", any(unix, windows)))]
    if index.is_daemon_backed() {
        let root = shared.root.clone();
        runtime.spawn(async move {
            let _syncing = syncing;
            match crate::git_history::remote::request_sync(root).await {
                Some(outcome) => tracing::info!(?outcome, "git-history index synced by the daemon"),
                None => tracing::warn!("git-history index sync unavailable; history tools live-walk"),
            }
        });
        return;
    }
    let _ = index;
    let (Some(git_history), Some(repo)) = (shared.git_history.clone(), shared.repo.clone()) else {
        return;
    };
    let history_dir = crate::git_history::shared_history_basemind_dir(&shared.root);
    runtime.spawn_blocking(move || {
        let _syncing = syncing;
        match crate::git_history::builder::sync(&git_history, &repo, &history_dir) {
            Ok(outcome) => tracing::info!(?outcome, "git-history index sync complete"),
            Err(error) => tracing::warn!(%error, "git-history index sync failed; tools live-walk"),
        }
        // Fjall 3.1 can deadlock while dropping its final database handle after a background sync
        // (fjall-rs/fjall#260). This path is only used by a standalone MCP process; the
        // daemon-backed path above owns and closes the shared database. Keep this clone alive until
        // process exit so Tokio runtime shutdown cannot hang.
        std::mem::forget(git_history);
    });
}
