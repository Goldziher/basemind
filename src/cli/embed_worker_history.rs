//! The git-history phase of `basemind embed-worker`.
//!
//! A one-shot `basemind scan` whose git-history index owes a from-scratch rebuild (none was built,
//! a build was cut off, or the shallow clone it was built in has been deepened) does not run it
//! before it returns: a deep history takes minutes to walk. It records the build as owed
//! (`crate::git_history::progress`) and starts the worker, which builds the index here. An append
//! of the commits made since the last sync stays with the scan.
//!
//! The build holds the workspace's store lock on the same loan as the embedding: it waits while a
//! one-shot command holds the lock, stops before its next chunk of commits when a writer asks for
//! it, and leaves the index to a `serve`, `watch` or daemon that takes it. A stopped build starts
//! again from scratch once the lock is free: the builder rebuilds a cut-off index rather than
//! resume it. Until the index is built the history tools answer from the live walk.

use std::path::Path;

use anyhow::Context as _;

use super::{Asks, Background, take_writer};
use crate::git::Repo;
use crate::git_history::builder::{self, BuildControl, RebuildOutcome};
use crate::git_history::{GitHistoryError, GitHistoryIndex, progress};
use crate::store::LockHolder;

/// The commits the from-scratch build the next sync of `index` owes would store, or `None` when
/// that sync appends, finds HEAD indexed, backs off a failed build or refuses the repository: work
/// a scan runs inline.
pub fn owed(index: &GitHistoryIndex, repo: &Repo) -> Option<usize> {
    builder::full_rebuild_due(index, repo)?;
    repo.reachable_commit_count()
        .inspect_err(|error| tracing::warn!(%error, "could not count the commits of the git-history build"))
        .ok()
}

/// Whether the worker `background` names takes the build recorded as owed in `basemind_dir`. When
/// none does (the worker is off or did not start), the record is withdrawn and the caller builds
/// the index itself.
pub fn taken(basemind_dir: &Path, background: Background) -> bool {
    let taken = matches!(background, Background::Started | Background::AlreadyRunning);
    if !taken {
        progress::clear(basemind_dir);
    }
    taken
}

/// The line a scan prints for a build of `commits` commits it handed to `background`.
pub fn summary_line(commits: usize, background: Background) -> String {
    match background {
        Background::AlreadyRunning => {
            format!(
                "git-history: building in the background ({commits} commits, by the background pass already running)"
            )
        }
        _ => format!(
            "git-history: building in the background ({commits} commits; progress: `git_history_build` in `basemind status`)"
        ),
    }
}

/// The `git_history` object `scan --json` reports for a build it handed off.
pub fn json(commits: usize, background: Background) -> serde_json::Value {
    serde_json::json!({
        "pending_commits": commits,
        "background": background.as_str(),
    })
}

/// What the phase did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase {
    /// No build was owed.
    NotOwed,
    /// The index was built, storing this many commits.
    Built(u32),
    /// A long-lived writer took the store lock; the build is left to it.
    Stopped,
    /// The build failed (the log says why); the history tools live-walk.
    Failed,
}

impl Phase {
    /// The commits the phase stored, when it built the index.
    pub(super) fn commits(self) -> Option<u32> {
        match self {
            Self::Built(commits) => Some(commits),
            _ => None,
        }
    }
}

/// Build the git-history index of `root` when a scan recorded the build as owed.
pub(super) fn run(root: &Path, asks: &Asks) -> Phase {
    let basemind_dir = crate::git_history::shared_history_basemind_dir(root);
    if !progress::is_owed(&basemind_dir) {
        return Phase::NotOwed;
    }
    let repo = match Repo::discover(root) {
        Ok(repo) if crate::git_history::index_enabled() => repo,
        _ => {
            progress::clear(&basemind_dir);
            return Phase::NotOwed;
        }
    };
    build(&repo, &basemind_dir, asks).unwrap_or_else(|error| {
        tracing::warn!(error = %format!("{error:#}"), "the background git-history build failed; history tools live-walk");
        progress::clear(&basemind_dir);
        Phase::Failed
    })
}

fn build(repo: &Repo, basemind_dir: &Path, asks: &Asks) -> anyhow::Result<Phase> {
    loop {
        let Some(_lock) = take_writer(asks, || {
            crate::store_lock::acquire_lock_as(&asks.workspace, LockHolder::EmbedWorker)
        })
        .context("take the store lock for the git-history build")?
        else {
            return Ok(Phase::Stopped);
        };
        // Declared after the lock, so dropped before it: the next holder finds the database free.
        let index = GitHistoryIndex::open(basemind_dir).context("open the git-history index")?;
        let control = Yielding {
            asks,
            basemind_dir,
            started_unix_ms: crate::embed_progress::now_unix_ms(),
        };
        match builder::sync_with(&index, repo, basemind_dir, &control) {
            Ok(outcome) => return Ok(built(&outcome)),
            Err(GitHistoryError::Cancelled) => {
                tracing::info!("a writer asked for the store lock; the git-history build starts again once it is free");
            }
            Err(error) => return Err(anyhow::Error::new(error).context("build the git-history index")),
        }
    }
}

fn built(outcome: &RebuildOutcome) -> Phase {
    match *outcome {
        RebuildOutcome::FullRebuild { commits, .. } => Phase::Built(commits),
        RebuildOutcome::Incremental { added } => Phase::Built(added),
        RebuildOutcome::Fresh | RebuildOutcome::Skipped { .. } => Phase::NotOwed,
    }
}

/// Stops the build when a writer asks for the store lock, and records its progress for
/// `index_status`.
struct Yielding<'a> {
    asks: &'a Asks,
    basemind_dir: &'a Path,
    started_unix_ms: u64,
}

impl BuildControl for Yielding<'_> {
    fn stop(&self) -> bool {
        self.asks.asked()
    }

    fn progress(&self, done: u32, total: u32) {
        progress::report(self.basemind_dir, self.started_unix_ms, done as usize, total as usize);
    }
}

#[cfg(test)]
#[path = "embed_worker_history_tests.rs"]
mod tests;
