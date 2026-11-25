//! One decoded-outline cache per blob directory, shared by every read stack a host opts in.
//!
//! Each [`MapCache`](crate::engine::MapCache) used to open its own [`L1Cache`], so a daemon hosting
//! the read stacks of twenty worktrees of one repository decoded and held the same outlines twenty
//! times, each copy charged against its own `[resources] max_map_cache_mb`. The outlines are keyed
//! by the content hash of the source file and read from the content-addressed blob directory, which
//! every workspace of a data root already shares on disk. One cache over that directory therefore
//! serves every stack exactly what its own cache would have: a stack names a hash only for content
//! it indexed, and the extract-epoch check on each hit is the same one a private cache applies.
//!
//! The registry holds its caches weakly. A cache lives while some map snapshot of some stack holds
//! it, and the last stack to leave frees it whole; a stack that leaves earlier frees nothing by
//! itself, and its outlines age out of the shared LRU under the shared budget. That budget is one
//! ceiling for all the stacks together: the largest any of them configures, `0` (unbounded)
//! winning, never their sum.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use crate::engine::l1_cache::L1Cache;
use crate::store::Store;

/// The live shared outline caches of one host, by blob directory.
#[cfg_attr(
    not(any(test, all(feature = "comms", any(unix, windows)))),
    allow(dead_code, reason = "only the daemon's workspace pool hosts read stacks to share it")
)]
#[derive(Default)]
pub(crate) struct SharedOutlines {
    open: Mutex<Vec<(PathBuf, Weak<L1Cache>)>>,
}

impl SharedOutlines {
    /// The cache over `blobs_dir`, widened to `budget_bytes`, or a new one when none is live.
    pub(crate) fn cache_for(&self, blobs_dir: &Path, budget_bytes: u64) -> Arc<L1Cache> {
        let mut open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
        open.retain(|(_, cache)| cache.strong_count() > 0);
        let live = open
            .iter()
            .filter(|(dir, _)| dir == blobs_dir)
            .find_map(|(_, cache)| cache.upgrade());
        if let Some(cache) = live {
            cache.widen_budget(budget_bytes);
            return cache;
        }
        let cache = Arc::new(L1Cache::shared(blobs_dir.to_path_buf(), budget_bytes));
        open.push((blobs_dir.to_path_buf(), Arc::downgrade(&cache)));
        cache
    }

    /// How many shared caches are live.
    #[cfg(test)]
    pub(crate) fn live(&self) -> usize {
        let open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
        open.iter().filter(|(_, cache)| cache.strong_count() > 0).count()
    }
}

/// Where a map build takes its outline cache from: a cache of its own at the workspace's budget,
/// or the host's shared one.
#[derive(Clone, Copy)]
pub(crate) struct Outlines<'a> {
    budget_bytes: u64,
    shared: Option<&'a SharedOutlines>,
}

impl<'a> Outlines<'a> {
    /// A cache of the map's own, charged against `budget_bytes` (`0` = unbounded).
    pub(crate) fn private(budget_bytes: u64) -> Self {
        Self {
            budget_bytes,
            shared: None,
        }
    }

    /// The host's shared cache, widened to `budget_bytes` when that is larger.
    pub(crate) fn shared(shared: &'a SharedOutlines, budget_bytes: u64) -> Self {
        Self {
            budget_bytes,
            shared: Some(shared),
        }
    }

    /// The outline cache for a map over `store`.
    pub(crate) fn cache_for(self, store: &Store) -> Arc<L1Cache> {
        match self.shared {
            Some(shared) => shared.cache_for(&store.blobs_dir, self.budget_bytes),
            None => Arc::new(L1Cache::from_store(store, self.budget_bytes)),
        }
    }
}

/// A bare byte budget is a cache of the map's own: [`Outlines::private`].
impl From<u64> for Outlines<'_> {
    fn from(budget_bytes: u64) -> Self {
        Self::private(budget_bytes)
    }
}

#[cfg(test)]
#[path = "l1_shared_tests.rs"]
mod tests;
