//! The daemon's hot-index pool: the machinery that lets the broker be the machine's **sole fjall
//! writer**.
//!
//! Front-ends (`basemind serve`) open each workspace's store *read-only* and forward every write
//! (scan / rescan) to the daemon over the socket. The daemon runs those scans through this pool so
//! exactly one process ever holds a workspace's exclusive index lock — dissolving the multi-session
//! single-holder problem where a second read-write session would degrade to read-only.
//!
//! Each hot workspace is an [`WorkspaceEntry`] holding an open read-write [`Store`] behind its own
//! `Mutex`. The outer map lock is held only for lookup / insertion / LRU bookkeeping — never across
//! a scan — so scans of distinct workspaces run concurrently while concurrent scans of the *same*
//! workspace serialize on that workspace's store lock (one writer, no double-open). The pool is
//! bounded: opening a cold workspace past the cap evicts the least-recently-used entry.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use ahash::AHashMap;
use serde::{Deserialize, Serialize};

use crate::config::{self, Config};
use crate::scanner::{self, EmbedMode, ScanSource, ScanStats};
use crate::store::{self, LockHolder, Store, VIEW_WORKING};

/// Default number of workspaces the daemon keeps hot in RAM at once. A cold workspace opened past
/// this evicts the least-recently-used entry; it re-opens lazily on its next request.
pub(crate) const DEFAULT_HOT_CAP: usize = 16;

/// Failure opening or scanning a workspace through the pool. Surfaced to the dispatch layer, which
/// maps it to a [`CommsResponse::Error`](super::protocol::CommsResponse::Error) rather than tearing
/// down the link.
#[derive(Debug, thiserror::Error)]
pub(crate) enum WorkspacePoolError {
    /// The workspace's read-write store could not be opened (e.g. the index lock is held by another
    /// process that has not yet migrated to the daemon-as-writer model).
    #[error("open workspace store: {0}")]
    Store(#[from] store::StoreError),
    /// The scan itself failed.
    #[error("scan workspace: {0}")]
    Scan(#[from] scanner::ScanError),
    /// The workspace config could not be loaded (a genuine parse/IO error; a missing file falls
    /// back to defaults and never reaches here).
    #[error("load workspace config: {0}")]
    Config(#[from] config::ConfigError),
}

/// One hot workspace: an open read-write store plus the resolved config and LRU bookkeeping.
struct WorkspaceEntry {
    /// The open read-write store. Behind its own lock so concurrent scans of the SAME workspace
    /// serialize here (one writer) while different workspaces proceed in parallel.
    store: Mutex<Store>,
    /// Resolved config for this workspace, captured at open time.
    config: Config,
    /// Canonical workspace root.
    root: PathBuf,
    /// Stable workspace key (blake3 of the canonical root).
    key: String,
    /// Last time a request touched this entry; drives LRU eviction and the statusline idle report.
    last_used: Mutex<Instant>,
}

impl WorkspaceEntry {
    /// Read the last-used instant, recovering from a poisoned lock (a panic mid-scan must not
    /// wedge the whole pool).
    fn last_used(&self) -> Instant {
        *self.last_used.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Stamp this entry as used now.
    fn touch(&self) {
        *self.last_used.lock().unwrap_or_else(PoisonError::into_inner) = Instant::now();
    }
}

/// A snapshot row describing one workspace the daemon currently holds hot. Returned to the
/// statusline via the [`AccessedPaths`](super::protocol::CommsRequest::AccessedPaths) RPC.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessedWorkspace {
    /// Canonical workspace root.
    pub root: PathBuf,
    /// Stable workspace key.
    pub key: String,
    /// Seconds since this workspace was last touched.
    pub idle_secs: u64,
}

/// The bounded pool of hot read-write workspaces owned by the daemon.
pub(crate) struct WorkspacePool {
    /// Hot entries keyed by [`store::workspace_key`]. The lock guards the map structure only —
    /// scans run against a cloned `Arc<WorkspaceEntry>` after the lock is released.
    map: Mutex<AHashMap<String, std::sync::Arc<WorkspaceEntry>>>,
    /// Serializes COLD opens against each other. fjall's index lock is exclusive, so two threads
    /// opening the SAME cold workspace concurrently would leave the loser failing on the lock — not
    /// merely losing the insert race — because the failure happens inside `Store::open`, before the
    /// post-open "prefer the stored entry" reconciliation can run. Holding this across the open (with
    /// a re-check under it) guarantees exactly one opener per key. Opens are one-time-per-workspace
    /// and fast, so serializing them across workspaces too is a non-issue; it never wraps a scan.
    open_lock: Mutex<()>,
    /// Maximum hot entries; opening past this evicts the least-recently-used.
    cap: usize,
}

impl WorkspacePool {
    /// Construct an empty pool bounded at `cap` hot workspaces.
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            map: Mutex::new(AHashMap::new()),
            open_lock: Mutex::new(()),
            cap: cap.max(1),
        }
    }

    /// Lock the map, recovering from poisoning.
    fn lock_map(&self) -> std::sync::MutexGuard<'_, AHashMap<String, std::sync::Arc<WorkspaceEntry>>> {
        self.map.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Scan (or incrementally rescan) `root`, opening it into the pool if cold. Returns the scan
    /// stats. The scan runs OUTSIDE the map lock; only bookkeeping is done under it.
    ///
    /// `full` forces a complete working-tree scan and overrides `paths`. Otherwise, a non-empty
    /// `paths` drives an incremental rescan of just those files; `None`/empty falls back to a full
    /// working-tree scan.
    ///
    /// `embed` picks the embed mode. The default fast pass is [`EmbedMode::Deferred`] — code map +
    /// keyword lane, no ONNX — so the boot handshake is never blocked on the embedder. Front-ends
    /// request `embed == true` for the detached vector-fill follow-up: an [`EmbedMode::Inline`] pass
    /// so documents and code chunks get their LanceDB vectors. The daemon is the sole fjall writer,
    /// so this embed write must be owned here (a `Deferred`-only daemon would leave `search_documents`
    /// permanently empty for repo documents).
    pub(crate) fn rescan(
        &self,
        root: &Path,
        paths: Option<Vec<PathBuf>>,
        full: bool,
        embed: bool,
    ) -> Result<ScanStats, WorkspacePoolError> {
        let entry = self.get_or_open(root)?;
        entry.touch();

        let mode = if embed { EmbedMode::Inline } else { EmbedMode::Deferred };
        let mut store = entry.store.lock().unwrap_or_else(PoisonError::into_inner);
        let report = match paths {
            Some(ref paths) if !full && !paths.is_empty() => {
                scanner::scan_paths(&entry.root, &mut store, &entry.config, paths, mode)?
            }
            _ => scanner::scan(&entry.root, &mut store, &entry.config, ScanSource::WorkingTree, mode)?,
        };
        Ok(report.stats)
    }

    /// Run `f` against a workspace's open read-write [`Store`] (immutable borrow), opening it into
    /// the pool if cold. Reads that only need the fjall index — the forwarded resolved-reference
    /// lookups (`references_to` / `definition_of`) — use this; it shares the same per-workspace
    /// open-and-LRU path as [`Self::with_workspace_mut`]. The store `Mutex` is held for the closure,
    /// so it briefly serializes against a same-workspace scan, fine for a fast prefix scan.
    pub(crate) fn with_workspace<R>(&self, root: &Path, f: impl FnOnce(&Store) -> R) -> Result<R, WorkspacePoolError> {
        let entry = self.get_or_open(root)?;
        entry.touch();
        let store = entry.store.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(f(&store))
    }

    /// Run `f` against a workspace's open read-write [`Store`], opening it into the pool if cold.
    ///
    /// The per-workspace store `Mutex` is held for the whole closure, so same-workspace callers
    /// serialize here (one writer) while distinct workspaces proceed in parallel. This is what makes
    /// a forwarded `memory_put` read-modify-write atomic without any per-key lock daemon-side.
    #[cfg(feature = "memory")]
    pub(crate) fn with_workspace_mut<R>(
        &self,
        root: &Path,
        f: impl FnOnce(&mut Store) -> R,
    ) -> Result<R, WorkspacePoolError> {
        let entry = self.get_or_open(root)?;
        entry.touch();
        let mut store = entry.store.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(f(&mut store))
    }

    /// Fetch the entry for `root`, opening it read-write and inserting it (evicting LRU past the
    /// cap) if cold. The returned `Arc` lets the caller run the scan after the map lock is dropped.
    fn get_or_open(&self, root: &Path) -> Result<std::sync::Arc<WorkspaceEntry>, WorkspacePoolError> {
        let key = store::workspace_key(root);
        {
            let map = self.lock_map();
            if let Some(entry) = map.get(&key) {
                return Ok(entry.clone());
            }
        }
        // Serialize the cold open. fjall's exclusive index lock means two concurrent opens of the
        // same key would leave the loser failing on the lock (inside `Store::open`, before the
        // post-open reconciliation), so the open itself — not just the insert — must be single.
        // Take the open lock OUTSIDE the map lock (opening touches the filesystem and can block; the
        // map lock must stay free for hot-path lookups), then re-check the map under it: a peer that
        // just opened this key while we waited already published its entry, and we return that.
        let _opening = self.open_lock.lock().unwrap_or_else(PoisonError::into_inner);
        {
            let map = self.lock_map();
            if let Some(entry) = map.get(&key) {
                return Ok(entry.clone());
            }
        }
        let store = Store::open_with_holder(root, VIEW_WORKING, LockHolder::Rescan)?;
        let config = load_config(root)?;
        let entry = std::sync::Arc::new(WorkspaceEntry {
            store: Mutex::new(store),
            config,
            root: root.to_path_buf(),
            key: key.clone(),
            last_used: Mutex::new(Instant::now()),
        });

        // We hold `open_lock`, so no peer can have opened this key while we did; insert directly.
        let mut map = self.lock_map();
        while map.len() >= self.cap {
            let victim = map.values().min_by_key(|e| e.last_used()).map(|e| e.key.clone());
            match victim {
                Some(victim) => {
                    map.remove(&victim);
                }
                None => break,
            }
        }
        map.insert(key, entry.clone());
        Ok(entry)
    }

    /// Snapshot the hot workspaces for the statusline, most-recently-used first.
    pub(crate) fn accessed(&self) -> Vec<AccessedWorkspace> {
        let map = self.lock_map();
        let mut rows: Vec<AccessedWorkspace> = map
            .values()
            .map(|e| AccessedWorkspace {
                root: e.root.clone(),
                key: e.key.clone(),
                idle_secs: e.last_used().elapsed().as_secs(),
            })
            .collect();
        rows.sort_by_key(|r| r.idle_secs);
        rows
    }

    /// Evict every entry idle for at least `idle`, returning the count dropped. The staleness
    /// collector calls this to shed cold workspaces from RAM (their on-disk cache survives).
    pub(crate) fn evict_idle(&self, idle: Duration) -> usize {
        let mut map = self.lock_map();
        let stale: Vec<String> = map
            .values()
            .filter(|e| e.last_used().elapsed() >= idle)
            .map(|e| e.key.clone())
            .collect();
        for key in &stale {
            map.remove(key);
        }
        stale.len()
    }

    /// Number of hot workspaces currently held. Exposed for tests and diagnostics.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.lock_map().len()
    }
}

/// Resolve a workspace's config, mirroring the CLI's `load_or_default`: a missing `basemind.toml`
/// falls back to per-root defaults; only a genuine parse/IO error propagates.
fn load_config(root: &Path) -> Result<Config, WorkspacePoolError> {
    match config::load_with_overrides(root, None, None) {
        Ok(loaded) => Ok(loaded.config),
        Err(config::ConfigError::NotFound(_)) => Ok(config::default_for_root(root)),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
#[path = "workspace_pool_tests.rs"]
mod tests;
