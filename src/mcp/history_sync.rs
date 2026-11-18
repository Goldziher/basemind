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
/// already running or this is the one-shot CLI. A daemon-hosted session shares the daemon's handle,
/// whose build lock serializes syncs, so N sessions cause one walk.
///
/// The one-shot CLI never syncs: it exits in milliseconds, and a first build on a deep repo is a
/// minutes-long walk.
pub(super) fn request(shared: &Workspace) {
    if shared.git_history.is_none() {
        return;
    }
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
    });
}
