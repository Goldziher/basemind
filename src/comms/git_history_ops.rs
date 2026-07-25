//! The daemon's git-history half: the pool of open per-repo history indexes, and the handler for the
//! forwarded [`GitHistoryOp`]s.
//!
//! Fjall's directory lock is exclusive — even a read-only open takes it — so `git-history.fjall/` has
//! exactly ONE holder. Under the daemon-as-sole-writer model that holder is the daemon: it BUILDS
//! each repo's index (the expensive history walk) and answers the front-ends' history reads from it,
//! exactly as it already owns the code index behind
//! [`WorkspacePool`](super::workspace_pool::WorkspacePool). A `daemon_writer` serve holds no handle
//! and forwards both halves here.
//!
//! Split out of `daemon.rs` to keep that file within the module-size cap.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use super::daemon::Broker;
use super::protocol::CommsResponse;
use crate::git_history::proto::{GitHistoryOp, GitHistoryReply};
use crate::git_history::{GitHistoryError, GitHistoryIndex};

/// One repo's open git-history index, held by the daemon (the only process allowed to hold it).
pub(crate) struct HistoryEntry {
    /// The open fjall database. Shared by every link; fjall's keyspaces are thread-safe, so reads
    /// run concurrently with each other and with an in-flight build.
    index: Arc<GitHistoryIndex>,
    /// Serializes SYNCS of this repo. This is the point of the daemon owning the build: N serve
    /// sessions asking at once collapse to ONE history walk, and the rest observe `Fresh`.
    build_lock: Arc<Mutex<()>>,
    /// Drives the idle sweep, which drops the handle — and with it fjall's lock — for a cold repo.
    last_used: std::sync::Mutex<Instant>,
}

impl HistoryEntry {
    fn touch(&self) {
        *self.last_used.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Instant::now();
    }

    /// How long this entry has sat unrequested. The daemon's periodic sweep sheds it past the TTL.
    pub(crate) fn idle_for(&self) -> Duration {
        self.last_used
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .elapsed()
    }
}

impl Broker {
    /// Run a forwarded git-history op against the repo's index — the BUILD and the reads, because the
    /// daemon is the only process that may hold the database.
    ///
    /// A [`GitHistoryOp::Sync`] takes the repo's build lock, so N sessions asking at once produce ONE
    /// build: the winner walks history; the rest wait, then see `Fresh` (`builder::sync` compares
    /// `last_indexed_head` to HEAD and no-ops). The walk is heavy and blocking, so it — and every
    /// read — runs on a blocking thread while the reactor keeps serving other links.
    pub(crate) async fn on_git_history(&self, root: std::path::PathBuf, op: GitHistoryOp) -> CommsResponse {
        match self.run_git_history_inproc(root, op).await {
            Ok(reply) => CommsResponse::GitHistory(reply),
            Err(GitHistoryError::Disabled) => CommsResponse::Error {
                code: "git_history_disabled".to_string(),
                message: GitHistoryError::Disabled.to_string(),
            },
            Err(error) => CommsResponse::Error {
                code: "git_history_failed".to_string(),
                message: error.to_string(),
            },
        }
    }

    /// Run one git-history op against the repo's daemon-held index, in-process.
    ///
    /// The single funnel for BOTH the forwarded socket path ([`on_git_history`](Self::on_git_history))
    /// and a daemon-hosted connection (via [`HistoryHost`](crate::git_history::remote::HistoryHost)):
    /// it opens (or reuses) the repo's one handle, serializes a [`Sync`](GitHistoryOp::Sync) on that
    /// repo's build lock — so N callers arriving at once produce ONE history walk — and runs the op on
    /// a blocking thread. Routing every caller here is what keeps the index single-holder and the
    /// build single-flight no matter how the request arrived.
    pub(crate) async fn run_git_history_inproc(
        &self,
        root: std::path::PathBuf,
        op: GitHistoryOp,
    ) -> Result<GitHistoryReply, GitHistoryError> {
        self.mark_active().await;
        if !crate::git_history::index_enabled() {
            return Err(GitHistoryError::Disabled);
        }
        let dir = crate::git_history::shared_history_basemind_dir(&root);
        let entry = self.history_entry(&dir).await?;
        entry.touch();

        let _build_guard = match op {
            GitHistoryOp::Sync => Some(entry.build_lock.clone().lock_owned().await),
            _ => None,
        };
        // ~keep A first-build `Sync` on a deep repo runs for minutes in `spawn_blocking`, and the hosted
        // ~keep path detaches it with no client link to keep the daemon busy. Pin it as in-flight work so
        // ~keep the idle reaper cannot tear the process down mid-write to `git-history.fjall/` — the same
        // ~keep guard `run_blob_gc_with_lock_timeout` takes for the other long client-less task.
        let _working = matches!(op, GitHistoryOp::Sync).then(|| self.begin_work());
        let index = Arc::clone(&entry.index);
        let err_dir = dir.clone();
        tokio::task::spawn_blocking(move || run_git_history_op(&index, &root, &dir, op))
            .await
            .map_err(|join| GitHistoryError::Io {
                path: err_dir,
                source: std::io::Error::other(join.to_string()),
            })?
    }

    /// Fetch (or lazily open) the git-history index for `dir`.
    ///
    /// The open takes fjall's exclusive lock, so it is serialized on `git_history_open_lock`: two
    /// racing first-touches must not both call `open` on the same database — the loser would fail on
    /// the lock instead of sharing the winner's handle (the same cold-open race the workspace pool
    /// hit). Re-checks the map under the lock, so the loser returns the winner's entry.
    async fn history_entry(&self, dir: &std::path::Path) -> Result<Arc<HistoryEntry>, GitHistoryError> {
        if let Some(entry) = self.history_lookup(dir) {
            return Ok(entry);
        }
        let _opening = self.git_history_open_lock.lock().await;
        if let Some(entry) = self.history_lookup(dir) {
            return Ok(entry);
        }
        let open_dir = dir.to_path_buf();
        let opened = tokio::task::spawn_blocking(move || GitHistoryIndex::open(&open_dir))
            .await
            .map_err(|join| GitHistoryError::Io {
                path: dir.to_path_buf(),
                source: std::io::Error::other(join.to_string()),
            })??;
        let entry = Arc::new(HistoryEntry {
            index: Arc::new(opened),
            build_lock: Arc::new(Mutex::new(())),
            last_used: std::sync::Mutex::new(Instant::now()),
        });
        self.git_history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(dir.to_path_buf(), Arc::clone(&entry));
        Ok(entry)
    }

    fn history_lookup(&self, dir: &std::path::Path) -> Option<Arc<HistoryEntry>> {
        self.git_history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(dir)
            .map(Arc::clone)
    }
}

/// The in-process git-history seam: a daemon-hosted connection runs its history ops directly against
/// the daemon that hosts it, sharing the same single handle and build lock the forwarded path uses,
/// instead of dialing the daemon over its own socket.
impl crate::git_history::remote::HistoryHost for Broker {
    fn run_history(
        &self,
        root: std::path::PathBuf,
        op: GitHistoryOp,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<GitHistoryReply, GitHistoryError>> + Send + '_>>
    {
        Box::pin(self.run_git_history_inproc(root, op))
    }
}

/// Execute one git-history op against the daemon-held index. Blocking (fjall reads, and a full
/// history walk for `Sync`); the caller runs it on a blocking thread.
fn run_git_history_op(
    index: &GitHistoryIndex,
    root: &std::path::Path,
    dir: &std::path::Path,
    op: GitHistoryOp,
) -> Result<GitHistoryReply, GitHistoryError> {
    match op {
        GitHistoryOp::Sync => {
            let repo = crate::git::Repo::discover(root)?;
            let outcome = crate::git_history::builder::sync(index, &repo, dir)?;
            tracing::info!(root = %root.display(), ?outcome, "git-history index sync complete");
            Ok(GitHistoryReply::Synced(outcome.into()))
        }
        GitHistoryOp::IndexedHead => Ok(GitHistoryReply::IndexedHead(index.last_indexed_head_hex())),
        GitHistoryOp::RecentCommits {
            skip,
            take,
            include_files,
        } => Ok(GitHistoryReply::Commits(index.recent_commits(
            skip,
            take,
            include_files,
        ))),
        GitHistoryOp::CommitsTouching { path, skip, take } => {
            Ok(GitHistoryReply::Commits(index.commits_touching(&path, skip, take)))
        }
        GitHistoryOp::WindowCommits { window } => Ok(GitHistoryReply::Commits(index.window_commits(window))),
        GitHistoryOp::SearchCommits {
            query,
            scope,
            skip,
            take,
        } => Ok(GitHistoryReply::Commits(
            index.search_commits(&query, scope, skip, take),
        )),
    }
}
