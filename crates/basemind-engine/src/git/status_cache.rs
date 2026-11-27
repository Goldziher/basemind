//! The working-tree status of one worktree, kept between calls.
//!
//! A status is one `lstat` per index entry plus the untracked walk: seconds on a large checkout,
//! and agents ask for it repeatedly within moments (every `changed`-scoped call, every `git_diff`
//! status). A computed status is kept until something that can change it happens: the index file
//! changes (size or mtime), `HEAD` moves, the file watcher saw a batch of edits
//! ([`StatusCache::invalidate`]), or [`MAX_AGE`] passes. The age bound covers edits the watcher
//! filters out (excluded or non-indexable paths), which still change a status.

use std::fs;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use super::{GitError, WorkingTreeStatus};

/// The longest a status is served without recomputing, whatever else is unchanged.
const MAX_AGE: Duration = Duration::from_secs(5);

/// How untracked files are listed: the two statuses [`super::Repo`] offers, kept apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Listing {
    /// As `status.showUntrackedFiles` says (directories may collapse).
    Porcelain,
    /// Every untracked file on its own.
    PerFile,
}

impl Listing {
    fn slot(self) -> usize {
        match self {
            Listing::Porcelain => 0,
            Listing::PerFile => 1,
        }
    }
}

/// What a cached status was computed against; any difference makes it stale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Stamp {
    /// The index file's mtime and size, `None` when it cannot be read (no index yet).
    index: Option<(SystemTime, u64)>,
    head: Option<gix::ObjectId>,
    generation: u64,
}

impl Stamp {
    /// The stamp of `local`'s worktree now, at watcher generation `generation`.
    pub(super) fn read(local: &gix::Repository, generation: u64) -> Self {
        let index = fs::metadata(local.git_dir().join("index"))
            .ok()
            .and_then(|meta| Some((meta.modified().ok()?, meta.len())));
        Self {
            index,
            head: local.head_id().ok().map(gix::Id::detach),
            generation,
        }
    }
}

struct Entry {
    stamp: Stamp,
    computed: Instant,
    status: WorkingTreeStatus,
}

/// The cached statuses of one worktree, one per [`Listing`].
#[derive(Default)]
pub(super) struct StatusCache {
    slots: Mutex<[Option<Entry>; 2]>,
    generation: AtomicU64,
    computations: AtomicU64,
}

impl StatusCache {
    /// Drops every cached status: the watcher saw edits the index and `HEAD` do not show.
    pub(super) fn invalidate(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    /// How many statuses were computed rather than served from the cache.
    pub(super) fn computations(&self) -> u64 {
        self.computations.load(Ordering::Acquire)
    }

    /// The cached status for `listing` when it is still valid, else `compute`'s, which is then
    /// kept. The lock is held while computing, so concurrent callers share one computation.
    ///
    /// The stamp is read before `compute` runs, so an edit during the computation leaves the entry
    /// stamped older than the state it may reflect and the next call recomputes.
    pub(super) fn get_or_compute(
        &self,
        local: &gix::Repository,
        listing: Listing,
        compute: impl FnOnce() -> Result<WorkingTreeStatus, GitError>,
    ) -> Result<WorkingTreeStatus, GitError> {
        let mut slots = self.slots.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let stamp = Stamp::read(local, self.generation.load(Ordering::Acquire));
        let slot = &mut slots[listing.slot()];
        if let Some(entry) = slot.as_ref()
            && entry.stamp == stamp
            && entry.computed.elapsed() < MAX_AGE
        {
            return Ok(entry.status.clone());
        }
        let status = compute()?;
        self.computations.fetch_add(1, Ordering::AcqRel);
        *slot = Some(Entry {
            stamp,
            computed: Instant::now(),
            status: status.clone(),
        });
        Ok(status)
    }
}
