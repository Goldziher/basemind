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
    /// Maximum hot entries; opening past this evicts the least-recently-used.
    cap: usize,
}

impl WorkspacePool {
    /// Construct an empty pool bounded at `cap` hot workspaces.
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            map: Mutex::new(AHashMap::new()),
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
    /// working-tree scan. Embeddings are [`EmbedMode::Deferred`] — the daemon writes the code map +
    /// keyword lane; vector fill is a serve/CLI concern and must not block the writer on ONNX.
    pub(crate) fn rescan(
        &self,
        root: &Path,
        paths: Option<Vec<PathBuf>>,
        full: bool,
    ) -> Result<ScanStats, WorkspacePoolError> {
        let entry = self.get_or_open(root)?;
        entry.touch();

        let mut store = entry.store.lock().unwrap_or_else(PoisonError::into_inner);
        let report = match paths {
            Some(ref paths) if !full && !paths.is_empty() => {
                scanner::scan_paths(&entry.root, &mut store, &entry.config, paths, EmbedMode::Deferred)?
            }
            _ => scanner::scan(
                &entry.root,
                &mut store,
                &entry.config,
                ScanSource::WorkingTree,
                EmbedMode::Deferred,
            )?,
        };
        Ok(report.stats)
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
        // Open outside the lock: opening touches the filesystem and can block. A concurrent opener ~keep
        // of the same key may win the race; we resolve that below by preferring the stored entry. ~keep
        let store = Store::open_with_holder(root, VIEW_WORKING, LockHolder::Rescan)?;
        let config = load_config(root)?;
        let entry = std::sync::Arc::new(WorkspaceEntry {
            store: Mutex::new(store),
            config,
            root: root.to_path_buf(),
            key: key.clone(),
            last_used: Mutex::new(Instant::now()),
        });

        let mut map = self.lock_map();
        // Lost the open race — a peer inserted this key first. Drop our fresh store (releasing the ~keep
        // duplicate lock) and use theirs. ~keep
        if let Some(existing) = map.get(&key) {
            return Ok(existing.clone());
        }
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
