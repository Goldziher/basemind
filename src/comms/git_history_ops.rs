//! The daemon's git-history half: the pool of open per-repo history indexes, and the handler for the
//! forwarded [`GitHistoryOp`]s.
//!
//! Fjall's directory lock is exclusive — even a read-only open takes it — so `git-history.fjall/` has
//! exactly ONE holder. Under the daemon-as-sole-writer model that holder is the daemon: it BUILDS
//! each repo's index (the expensive history walk) and answers the front-ends' history reads from it,
//! exactly as it already owns the code index behind
//! [`WorkspacePool`](super::workspace_pool::WorkspacePool). A session the daemon hosts shares the
//! daemon's handle ([`Broker::hosted_history`]); a forwarded [`GitHistoryOp`] runs here.
//!
//! Split out of `daemon.rs` to keep that file within the module-size cap.

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::daemon::Broker;
use super::protocol::CommsResponse;
use crate::git_history::proto::{GitHistoryOp, GitHistoryReply};
use crate::git_history::{GitHistoryError, GitHistoryIndex};

/// One repo's open git-history index, held by the daemon (the only process allowed to hold it).
pub(crate) struct HistoryEntry {
    /// The open fjall database. Shared by every link; fjall's keyspaces are thread-safe, so reads
    /// run concurrently with each other and with an in-flight build.
    pub(super) index: Arc<GitHistoryIndex>,
    /// Drives the idle sweep, which drops the handle — and with it fjall's lock — for a cold repo.
    last_used: std::sync::Mutex<Instant>,
}

impl HistoryEntry {
    fn touch(&self) {
        *self.last_used.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Instant::now();
    }

    /// Whether anything besides the broker's map holds this entry or its handle: a hosted
    /// workspace, or an op between its lookup and the end of its run. Read under the map lock, where
    /// every lookup clones the entry, so a holder is either counted here or finds the entry gone.
    pub(crate) fn in_use(self: &Arc<Self>) -> bool {
        Arc::strong_count(self) > 1 || Arc::strong_count(&self.index) > 1
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
    /// A [`GitHistoryOp::Sync`] serializes on the index's build lock, so N callers asking at once
    /// produce ONE build: the winner walks history; the rest wait, then see `Fresh` (`builder::sync`
    /// finds HEAD already an indexed tip and no-ops). The walk is heavy and blocking, so it — and every
    /// read — runs on a blocking thread while the reactor keeps serving other links.
    pub(crate) async fn on_git_history(&self, root: std::path::PathBuf, op: GitHistoryOp) -> CommsResponse {
        match self.run_git_history_inproc(root, op).await {
            Ok(reply) => CommsResponse::GitHistory(reply),
            Err(GitHistoryError::Disabled) => CommsResponse::Error {
                code: "git_history_disabled".to_string(),
                message: GitHistoryError::Disabled.to_string(),
            },
            Err(error @ GitHistoryError::RootRefused(_)) => CommsResponse::Error {
                code: "root_refused".to_string(),
                message: error.to_string(),
            },
            Err(error @ GitHistoryError::ForeignHistory { .. }) => CommsResponse::Error {
                code: "git_history_out_of_scope".to_string(),
                message: error.to_string(),
            },
            Err(error @ GitHistoryError::NoOrderedView { .. }) => CommsResponse::Error {
                code: "git_history_not_ready".to_string(),
                message: error.to_string(),
            },
            Err(error) => CommsResponse::Error {
                code: "git_history_failed".to_string(),
                message: error.to_string(),
            },
        }
    }

    /// Run one git-history op against the repo's daemon-held index, in-process.
    ///
    /// Opens (or reuses) the repo's one handle, the one the sessions the daemon hosts share, and runs
    /// the op on a blocking thread.
    pub(crate) async fn run_git_history_inproc(
        &self,
        root: std::path::PathBuf,
        op: GitHistoryOp,
    ) -> Result<GitHistoryReply, GitHistoryError> {
        self.mark_active().await;
        if !crate::git_history::index_enabled() {
            return Err(GitHistoryError::Disabled);
        }
        // This root arrives from a client RPC and does NOT pass through `WorkspacePool::get_or_open`,
        // so it needs its own trip through the guard: `GitHistoryOp::Sync` walks the whole history
        // and creates `shared_history_basemind_dir(root)`, minting cache state for a root every other
        // consumer refuses (issue #62). The resolved path is what gets used onward, so the directory
        // this creates belongs to the root that was actually checked.
        let root = crate::config::root_guard::workspace_root_verdict(&root).map_err(|refusal| {
            GitHistoryError::RootRefused(crate::config::root_guard::refusal_message(&root, refusal))
        })?;
        let dir = crate::git_history::shared_history_basemind_dir(&root);
        // ~keep Only a sync builds the index; a read of one never built creates nothing.
        if !matches!(op, GitHistoryOp::Sync) && self.history_lookup(&dir).is_none() && !GitHistoryIndex::exists(&dir) {
            return unbuilt_reply(op);
        }
        let entry = self.history_entry(&dir).await?;
        entry.touch();

        // ~keep A first-build `Sync` on a deep repo runs for minutes in `spawn_blocking`, and a client that
        // ~keep gives up leaves it running with no link to keep the daemon busy. Pin it as in-flight work so
        // ~keep the idle reaper cannot tear the process down mid-write to `git-history.fjall/` — the same
        // ~keep guard `run_blob_gc_with_lock_timeout` takes for the other long client-less task. A hosted
        // ~keep workspace's own syncs pin through `HostedHistory::pin`.
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
        let (entry, shed) = self.insert_history(dir, opened);
        // Closing a database joins its fjall workers: off the reactor, and before the open lock is
        // released, so a reopen of a closed directory never meets its lock still held.
        if !shed.is_empty() {
            let _closed = tokio::task::spawn_blocking(move || drop(shed)).await;
        }
        Ok(entry)
    }

    /// Record `opened` as the handle of `dir`, and take out of the map the least recently used
    /// handles nothing holds until at most [`MAX_OPEN_HISTORIES`] stay. Returns the new entry and
    /// the removed ones, which the caller drops on a blocking thread while it still holds
    /// `git_history_open_lock`: closing a database joins its fjall workers.
    pub(super) fn insert_history(
        &self,
        dir: &std::path::Path,
        opened: GitHistoryIndex,
    ) -> (Arc<HistoryEntry>, Vec<Arc<HistoryEntry>>) {
        let entry = Arc::new(HistoryEntry {
            index: Arc::new(opened),
            last_used: std::sync::Mutex::new(Instant::now()),
        });
        let mut map = self
            .git_history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.insert(dir.to_path_buf(), Arc::clone(&entry));
        let shed = shed_over_cap(&mut map);
        if map.len() > MAX_OPEN_HISTORIES {
            tracing::warn!(
                open = map.len(),
                cap = MAX_OPEN_HISTORIES,
                "daemon: every open git-history database is in use; holding more than the cap"
            );
        }
        (entry, shed)
    }

    pub(super) fn history_lookup(&self, dir: &std::path::Path) -> Option<Arc<HistoryEntry>> {
        self.git_history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(dir)
            .map(Arc::clone)
    }
}

/// The git-history handle a hosted workspace shares with the daemon, and the pin its syncs hold.
pub(crate) struct HostedHistory {
    pub(crate) index: Arc<GitHistoryIndex>,
    /// Keeps the daemon from being reaped while a sync the workspace started runs: no client link
    /// waits on that sync, and a first build writes `git-history.fjall/` for minutes.
    pub(crate) pin: crate::engine::workspace::WorkPin,
}

impl Broker {
    /// The git-history index a hosted workspace reads: this daemon's handle on the repo's database,
    /// opened on first use, for [`serve_conn`](super::workspace_pool::WorkspacePool::serve_conn). Runs
    /// on the blocking thread that builds the hosted stack. `None` when the database cannot be opened;
    /// history tools then live-walk.
    pub(crate) fn hosted_history(
        self: &Arc<Self>,
    ) -> impl FnOnce(&std::path::Path) -> Option<HostedHistory> + Send + 'static {
        let broker = Arc::clone(self);
        move |root| {
            let index = broker.hosted_history_index(root)?;
            // Weak: the stack lives in this broker's pool, and an owning pin would keep the broker up. ~keep
            let weak = Arc::downgrade(&broker);
            let pin: crate::engine::workspace::WorkPin =
                Arc::new(move || Box::new(weak.upgrade().map(|broker| broker.begin_owned_work())) as Box<dyn Send>);
            Some(HostedHistory { index, pin })
        }
    }

    fn hosted_history_index(&self, root: &std::path::Path) -> Option<Arc<GitHistoryIndex>> {
        let dir = crate::git_history::shared_history_basemind_dir(root);
        if let Some(entry) = self.history_lookup(&dir) {
            entry.touch();
            return Some(Arc::clone(&entry.index));
        }
        let _opening = self.git_history_open_lock.blocking_lock();
        if let Some(entry) = self.history_lookup(&dir) {
            return Some(Arc::clone(&entry.index));
        }
        match GitHistoryIndex::open(&dir) {
            Ok(opened) => {
                let (entry, shed) = self.insert_history(&dir, opened);
                drop(shed);
                Some(Arc::clone(&entry.index))
            }
            Err(error) => {
                tracing::warn!(%error, root = %root.display(), "git-history index unavailable to the hosted workspace");
                None
            }
        }
    }
}

/// Git-history databases the daemon keeps open at once. Each holds up to
/// [`CACHED_FILES`](crate::git_history::CACHED_FILES) descriptors plus its memtables and block
/// cache, so without a cap every repository asked about within the idle TTL stays resident; the
/// descriptor budget ([`crate::comms::fd_budget`]) counts this many. A database in use is never
/// closed, so while more than this many are in use at once the daemon holds them all and warns.
pub(crate) const MAX_OPEN_HISTORIES: usize = 8;

/// Remove the least recently used entries no one else holds until the map is within
/// [`MAX_OPEN_HISTORIES`], returning them for the caller to drop outside the lock. An entry in use
/// (a hosted workspace shares it, or an op is running on it) is never removed: its fjall lock would
/// stay held and the next open of the database would fail on it.
fn shed_over_cap(map: &mut ahash::AHashMap<std::path::PathBuf, Arc<HistoryEntry>>) -> Vec<Arc<HistoryEntry>> {
    let mut shed = Vec::new();
    while map.len() > MAX_OPEN_HISTORIES {
        let victim = map
            .iter()
            .filter(|(_, entry)| !entry.in_use())
            .max_by_key(|(_, entry)| entry.idle_for())
            .map(|(dir, _)| dir.clone());
        let Some(victim) = victim else { break };
        shed.extend(map.remove(&victim));
    }
    shed
}

/// What an index that was never built answers to an op other than a sync: no indexed HEAD, and a
/// commit read refused as [`GitHistoryError::NoOrderedView`] rather than answered with no commits,
/// which would read as a path with no history. The caller live-walks either way.
fn unbuilt_reply(op: GitHistoryOp) -> Result<GitHistoryReply, GitHistoryError> {
    match op {
        GitHistoryOp::RecentCommits { head, .. }
        | GitHistoryOp::CommitsTouching { head, .. }
        | GitHistoryOp::WindowCommits { head, .. }
        | GitHistoryOp::SearchCommits { head, .. } => Err(GitHistoryError::NoOrderedView { head }),
        GitHistoryOp::Sync | GitHistoryOp::IndexedHead => Ok(GitHistoryReply::IndexedHead(None)),
    }
}

/// The index as `head` sees it, or as the most recently synced tip sees it when the op names none.
/// A head with no ordered view (no indexed tip, or a tip whose order is unproven) is refused: an
/// empty answer would be indistinguishable from a path with no history.
fn view(
    index: &GitHistoryIndex,
    head: Option<String>,
) -> Result<crate::git_history::reader::HistoryAt<'_>, GitHistoryError> {
    let found = head.as_deref().map_or_else(|| index.latest(), |head| index.at(head));
    found.ok_or(GitHistoryError::NoOrderedView { head })
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
            head,
        } => Ok(GitHistoryReply::Commits(view(index, head)?.recent_commits(
            skip,
            take,
            include_files,
        ))),
        GitHistoryOp::CommitsTouching { path, skip, take, head } => view(index, head)?
            .commits_touching(&path, skip, take)
            .map(GitHistoryReply::Commits),
        GitHistoryOp::WindowCommits { window, head } => {
            Ok(GitHistoryReply::Commits(view(index, head)?.window_commits(window)))
        }
        GitHistoryOp::SearchCommits {
            query,
            scope,
            skip,
            take,
            head,
        } => Ok(GitHistoryReply::Commits(
            view(index, head)?.search_commits(&query, scope, skip, take),
        )),
    }
}

#[cfg(test)]
#[path = "git_history_ops_tests.rs"]
mod tests;
