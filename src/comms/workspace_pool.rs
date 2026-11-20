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
use crate::scanner::{self, EmbedMode, ScanCancel, ScanSource, ScanStats};
use crate::store::{self, LockHolder, Store, VIEW_WORKING};

/// Default number of workspaces the daemon keeps hot in RAM at once. A cold workspace opened past
/// this evicts the least-recently-used entry; it re-opens lazily on its next request.
pub(crate) const DEFAULT_HOT_CAP: usize = 4;

/// Default number of daemon-hosted READ STACKS kept warm at once — deliberately far below
/// [`DEFAULT_HOT_CAP`], which governs something much cheaper.
///
/// A hot entry is an open `Store` handle. A warm read stack is an O(corpus) resident structure per
/// workspace (the in-RAM code map and its L1 outline cache, git caches, LanceDB, ONNX), so letting
/// one grow per hot entry multiplies the whole read stack by 16 — the daemon-specific multiplier
/// behind issue #62's 43.8 GiB. Three lets a developer alternate between a couple of repos without
/// paying a rebuild, and a rebuild is transparent anyway: dropping a stack changes latency, not
/// answers.
pub(crate) const DEFAULT_WARM_READ_STACK_CAP: usize = 3;

/// Env override for [`DEFAULT_WARM_READ_STACK_CAP`]. Zero or unparseable falls back to the default.
///
/// A const + env knob rather than a `[resources]` config key: the pool is machine-global and is
/// built before any workspace is known ([`super::daemon::Broker::with_registry`]), so a
/// per-workspace config key would have no answer to "which workspace's number governs the shared
/// daemon". This mirrors the other machine-global daemon knobs
/// ([`crate::daemon_lock::MAX_LIVE_DAEMONS_ENV`], `WORKSPACE_HOT_TTL`).
pub(crate) const WARM_READ_STACK_CAP_ENV: &str = "BASEMIND_WARM_READ_STACKS";

/// How long a hosted read stack may sit unrequested before the daemon drops it while KEEPING the
/// workspace's hot store entry.
///
/// Shorter than [`super::daemon::WORKSPACE_HOT_TTL`], because the two things are not comparable: the
/// store handle is a file descriptor and an index lock that cannot be reacquired for free, while the
/// read stack is a whole-corpus projection that any later connection rebuilds from the same on-disk
/// blobs. Shedding the expensive, rebuildable half early is what keeps a long-lived daemon's
/// resident set proportional to what is actually being served rather than to everything it has ever
/// served.
///
/// Two thirds rather than the more aggressive third it started at, because this clock is NOT what
/// bounds memory — [`DEFAULT_WARM_READ_STACK_CAP`] is. The cap already holds residency to three
/// stacks no matter how many workspaces the daemon has served; the TTL only reclaims *below* the
/// cap, so shortening it further buys at most three stacks. It is not free, either: dropping a
/// stack drops its `WatcherGuard` (they cannot be separated — the watcher task holds the read stack
/// it would keep fresh), and a rebuild must then run a catch-up scan over everything edited while
/// the workspace was shed. There is no reason to triple how often that is paid in exchange for
/// memory the cap has already bounded.
pub(crate) const READ_STACK_IDLE_TTL: Duration = Duration::from_secs(10 * 60);

/// The effective warm-stack cap: [`DEFAULT_WARM_READ_STACK_CAP`] unless [`WARM_READ_STACK_CAP_ENV`]
/// overrides it.
pub(crate) fn warm_read_stack_cap() -> usize {
    match std::env::var(WARM_READ_STACK_CAP_ENV) {
        Ok(raw) => match raw.trim().parse::<usize>() {
            Ok(n) if n > 0 => n,
            _ => DEFAULT_WARM_READ_STACK_CAP,
        },
        Err(_) => DEFAULT_WARM_READ_STACK_CAP,
    }
}

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
    /// The root is not a project basemind will index (issue #62). `message` is the full
    /// operator-facing guidance from [`config::root_guard::refusal_message`], carried verbatim so
    /// the daemon's generic `Err` arm relays the real explanation instead of a status word;
    /// `root` stays a structured field so a caller can key on the path without parsing the prose.
    #[error("{message}")]
    RootRefused { root: PathBuf, message: String },
}

/// One hot workspace: an open read-write store plus the resolved config and LRU bookkeeping.
struct WorkspaceEntry {
    /// The open read-write store. Behind its own lock so concurrent scans of the SAME workspace
    /// serialize here (one writer) while different workspaces proceed in parallel.
    store: Mutex<Store>,
    /// The writer store's index handle, cloned at open and after a reopen. A hosted read store
    /// borrows it per call ([`Store::index_source`]) instead of waiting on a scan for the store
    /// lock (fjall's directory lock admits one holder per database, so a second open is not an
    /// option). Cleared before a reopen and on eviction, so no read takes the handle afterwards.
    index: std::sync::RwLock<Option<crate::index::IndexDb>>,
    /// Resolved config for this workspace, replaced in place when `basemind.toml` changes on disk
    /// (see [`WorkspacePool::refresh_config`]). A scan takes a snapshot, so one scan never sees a
    /// config change halfway through.
    config: std::sync::RwLock<std::sync::Arc<Config>>,
    /// Stamp of the config file the current `config` was loaded from.
    config_stamp: Mutex<config::daemon::ConfigStamp>,
    /// Canonical workspace root.
    root: PathBuf,
    /// Stable workspace key (blake3 of the canonical root).
    key: String,
    /// Last time a request touched this entry; drives LRU eviction and the statusline idle report.
    last_used: Mutex<Instant>,
    /// Monotonic count of COMPLETED (non-cancelled) full scans of this workspace. A full-rescan
    /// request captures it before blocking on the store lock; if it advanced while the request
    /// waited, an identical-or-stronger full scan just walked the same tree and the queued one is
    /// redundant. This is what stops N sessions' back-to-back full rescans (issue #44) from
    /// re-walking a monorepo N times.
    full_scan_gen: std::sync::atomic::AtomicU64,
    /// Set at open when the fjall index was wiped or is empty over a populated file map; every
    /// rescan runs full until one completes, which clears it. Without it the daemon would keep
    /// serving empty reference reads for a workspace nobody asked to rescan.
    rebuild_pending: std::sync::atomic::AtomicBool,
    /// The most recent completed full scan: (generation, embed mode, stats). Served to coalesced
    /// requests instead of re-scanning; an `Inline` result satisfies a `Deferred` request but not
    /// vice versa.
    last_full: Mutex<Option<(u64, EmbedMode, ScanStats)>>,
    /// The daemon-hosted shared read stack for this workspace, built on the first relay connection
    /// and shared (by `Arc`) across every connection thereafter — so the heavy read state (in-RAM
    /// `MapCache`, LanceDB, ONNX, git caches) is resident once per workspace, not once per client.
    /// `None` until the first relay connection; a pure comms build without any relay client never
    /// pays for it.
    ///
    /// A `Mutex<Option<_>>` rather than a `OnceCell`: it keeps the build-once-under-concurrency
    /// property (the lock is held across the build, so racing callers await the one build) while
    /// staying RESETTABLE, which a `OnceCell` behind an `Arc` is not. Resettable is the whole point
    /// — the stack is the O(corpus) half of a hot workspace and must be sheddable independently of
    /// the store handle it sits next to (see [`WorkspacePool::evict_idle_read_stacks`]).
    #[cfg(all(feature = "comms", any(unix, windows)))]
    serve_state: tokio::sync::Mutex<Option<crate::mcp::HostedReadStack>>,
    /// Count of relay connections currently being served against this workspace's shared read
    /// stack. Eviction (LRU + idle sweep) skips any entry with a live connection so a hosted
    /// workspace is never dropped from under an in-flight rmcp session.
    active_conns: std::sync::atomic::AtomicUsize,
    /// Count of in-flight operations (scans, store reads / writes) holding this entry's store.
    /// Eviction skips a busy entry: dropping it from the map while its `Arc` lives on in a scan would
    /// leave the store's index lock held, so the next open of the same root would fail on the lock.
    busy: std::sync::atomic::AtomicUsize,
}

/// RAII marker for one in-flight operation on a [`WorkspaceEntry`]; see [`WorkspaceEntry::busy`].
struct BusyGuard(std::sync::Arc<WorkspaceEntry>);

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.busy.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

impl WorkspaceEntry {
    /// Mark the entry busy until the returned guard drops.
    fn begin_busy(self: &std::sync::Arc<Self>) -> BusyGuard {
        self.busy.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        BusyGuard(std::sync::Arc::clone(self))
    }

    /// Whether eviction may drop this entry: no live relay connection and no in-flight operation.
    fn evictable(&self) -> bool {
        use std::sync::atomic::Ordering::Acquire;
        self.active_conns.load(Acquire) == 0 && self.busy.load(Acquire) == 0
    }

    /// The writer's current index handle.
    fn index(&self) -> Option<crate::index::IndexDb> {
        self.index.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Stop lending the writer's handle: a hosted read from here on finds no index, and the only
    /// clones left are those of reads already in flight.
    fn revoke_index(&self) {
        *self.index.write().unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// Snapshot of the current config.
    fn config(&self) -> std::sync::Arc<Config> {
        self.config.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

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

/// RAII guard for one live relay connection to a hosted workspace. Held for the lifetime of the
/// rmcp session; its [`Drop`] decrements the workspace's `active_conns` so the eviction sweep can
/// reclaim the entry once the last connection drains. Created by
/// [`WorkspacePool::begin_conn`](WorkspacePool::begin_conn).
#[cfg(all(feature = "comms", any(unix, windows)))]
pub(crate) struct ServeConnGuard {
    entry: std::sync::Arc<WorkspaceEntry>,
}

#[cfg(all(feature = "comms", any(unix, windows)))]
impl Drop for ServeConnGuard {
    fn drop(&mut self) {
        // A session uses its workspace until it ends, so the idle clocks start here, not at the
        // connect: a long session must not leave a stack that looks as old as the session.
        self.entry.touch();
        self.entry
            .active_conns
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
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
    /// The broker's drain token, shared so a hosted session's rescan ([`Self::rescan_as`]) cancels on
    /// drain exactly like the socket path does. The broker adopts this token (see [`Self::scan_cancel`]) rather
    /// than holding a separate one, so `begin_drain` trips a single flag that every scan path — socket
    /// or hosted — observes at per-file granularity.
    scan_cancel: ScanCancel,
    /// Raised whenever a completed pass displaced artifacts (tier migration or epoch re-extraction),
    /// so the daemon reclaims the orphaned blobs promptly instead of waiting out the maintenance
    /// interval. A stored permit coalesces any number of passes into one wake-up.
    cleanup_signal: std::sync::Arc<tokio::sync::Notify>,
    /// Held shared by every scan this pool runs, exclusively by the blob sweep.
    blob_gc: std::sync::Arc<super::blob_gc_gate::BlobGcGate>,
}

impl WorkspacePool {
    /// Construct an empty pool bounded at `cap` hot workspaces.
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            map: Mutex::new(AHashMap::new()),
            open_lock: Mutex::new(()),
            cap: cap.max(1),
            scan_cancel: ScanCancel::new(),
            cleanup_signal: std::sync::Arc::new(tokio::sync::Notify::new()),
            blob_gc: std::sync::Arc::default(),
        }
    }

    /// The gate the blob sweep must hold exclusively (see [`super::blob_gc_gate`]).
    pub(crate) fn blob_gc_gate(&self) -> std::sync::Arc<super::blob_gc_gate::BlobGcGate> {
        std::sync::Arc::clone(&self.blob_gc)
    }

    /// The signal raised after a pass that displaced artifacts; the daemon's cleanup task waits on it.
    pub(crate) fn cleanup_signal(&self) -> std::sync::Arc<tokio::sync::Notify> {
        std::sync::Arc::clone(&self.cleanup_signal)
    }

    /// The pool's drain token, for the broker to adopt as its own so a single `cancel()` stops
    /// scans on both the socket dispatch path and the in-process host seam.
    pub(crate) fn scan_cancel(&self) -> ScanCancel {
        self.scan_cancel.clone()
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
    ///
    /// `cancel` is the broker's drain token: a draining daemon trips it so a mid-flight scan stops
    /// at per-file granularity instead of pinning the runtime shutdown. The returned `bool` reports
    /// whether the pass was cancelled — the dispatch layer must surface a cancelled partial pass as
    /// an error, never as a completed rescan.
    pub(crate) fn rescan(
        &self,
        root: &Path,
        paths: Option<Vec<PathBuf>>,
        full: bool,
        embed: bool,
        cancel: &ScanCancel,
    ) -> Result<(ScanStats, bool), WorkspacePoolError> {
        self.rescan_as(root, paths, full, embed, false, cancel)
    }

    /// [`rescan`](Self::rescan), saying whether `paths` were named by a person or an agent
    /// (`verify`) or seen changing by a watcher: only named paths have their blobs validated.
    pub(crate) fn rescan_as(
        &self,
        root: &Path,
        paths: Option<Vec<PathBuf>>,
        full: bool,
        embed: bool,
        verify: bool,
        cancel: &ScanCancel,
    ) -> Result<(ScanStats, bool), WorkspacePoolError> {
        // Taken here, not by the callers: every way into a scan passes through this function.
        let _scanning = self.blob_gc.scanning();
        let entry = self.get_or_open(root)?;
        entry.touch();
        let _busy = entry.begin_busy();

        let mode = if embed { EmbedMode::Inline } else { EmbedMode::Deferred };
        let config = entry.config();
        let rebuild_pending = entry.rebuild_pending.load(std::sync::atomic::Ordering::Acquire);
        let gen_before = entry.full_scan_gen.load(std::sync::atomic::Ordering::Acquire);
        let mut store = entry.store.lock().unwrap_or_else(PoisonError::into_inner);
        let full = full || rebuild_pending || store.needs_full_scan();
        let incremental = matches!(paths, Some(ref p) if !full && !p.is_empty());
        if !incremental {
            let last = entry.last_full.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some((generation, last_mode, stats)) = *last
                && generation > gen_before
                && (last_mode == EmbedMode::Inline || mode == EmbedMode::Deferred)
            {
                return Ok((stats, false));
            }
        }
        if cancel.is_cancelled() {
            return Ok((ScanStats::default(), true));
        }
        // The daemon owns the embedding engine for forwarded and hosted passes alike: a full Inline
        // pass releases only an engine it loaded and no search used (see `EmbedPass`).
        #[cfg(feature = "intelligence")]
        let _embed_pass = (matches!(mode, EmbedMode::Inline) && !incremental).then(crate::embeddings::begin_embed_pass);
        let scanned = if incremental {
            let paths = paths.as_deref().unwrap_or_default();
            let (root, observer) = (&entry.root, &mut scanner::NullObserver);
            if verify {
                scanner::scan_named_paths_with_observer(
                    root,
                    &mut store,
                    &config,
                    paths,
                    mode,
                    cancel,
                    observer,
                    crate::retrieval_wiring::backend(),
                )
            } else {
                scanner::scan_paths_with_observer(
                    root,
                    &mut store,
                    &config,
                    paths,
                    mode,
                    cancel,
                    observer,
                    crate::retrieval_wiring::backend(),
                )
            }
        } else {
            scanner::scan_with_cancel(
                &entry.root,
                &mut store,
                &config,
                ScanSource::WorkingTree,
                mode,
                cancel,
                crate::retrieval_wiring::backend(),
            )
        };
        // Before the scan's error propagates: a poisoned handle fails every later commit for the life
        // of the daemon, so it has to be replaced whether or not this pass got to report it.
        self.recover_poisoned_index(&entry, &mut store);
        let report = scanned?;
        if report.stats.displaced_artifacts() || std::mem::take(&mut store.reset_stale_schema) {
            self.cleanup_signal.notify_one();
        }
        if !incremental && !report.cancelled {
            entry.rebuild_pending.store(false, std::sync::atomic::Ordering::Release);
            store.index_rebuild_needed = false;
            let generation = entry.full_scan_gen.fetch_add(1, std::sync::atomic::Ordering::AcqRel) + 1;
            *entry.last_full.lock().unwrap_or_else(PoisonError::into_inner) = Some((generation, mode, report.stats));
        }
        Ok((report.stats, report.cancelled))
    }

    /// A daemon-hosted session's scan of `root`: [`Self::rescan_as`] under the pool's own drain token,
    /// so `comms stop` / SIGTERM stops a hosted mid-scan within one file, as on the socket path. The
    /// `bool` says the pass was cancelled: it may have flushed a file map whose resolve pass never
    /// ran, which the caller must not present as a completed rescan.
    pub(crate) fn hosted_rescan(
        &self,
        root: &Path,
        paths: Option<Vec<PathBuf>>,
        full: bool,
        embed: bool,
        verify: bool,
    ) -> Result<(ScanStats, bool), WorkspacePoolError> {
        self.rescan_as(root, paths, full, embed, verify, &self.scan_cancel)
    }

    /// Replace a poisoned code-index handle (a failed flush or commit; see
    /// [`IndexDb::is_poisoned`](crate::index::IndexDb::is_poisoned)) by reopening the database,
    /// which replays the journal into a fresh handle. When the reopen fails the entry is evicted so
    /// the next request opens the workspace from scratch instead of serving a dead handle. Either
    /// way the status sidecar already reads partial, so the next open schedules a full scan.
    fn recover_poisoned_index(&self, entry: &WorkspaceEntry, store: &mut Store) {
        if !store.index_db.as_ref().is_some_and(crate::index::IndexDb::is_poisoned) {
            return;
        }
        tracing::warn!(root = %entry.root.display(), "index handle poisoned; reopening");
        // Hosted reads borrow the handle per call, so once it is revoked the reopen waits only for the
        // reads in flight. Another view of the group in this pool (a sibling worktree) keeps the
        // poisoned database open; the reopen then fails and this entry is evicted. ~keep
        entry.revoke_index();
        match store.reopen_index_db() {
            Ok(()) => *entry.index.write().unwrap_or_else(PoisonError::into_inner) = store.index_db.clone(),
            Err(error) => {
                tracing::error!(root = %entry.root.display(), %error, "index reopen failed; evicting the workspace");
                let mut map = self.lock_map();
                if map
                    .get(&entry.key)
                    .is_some_and(|current| std::ptr::eq(std::sync::Arc::as_ptr(current), entry))
                {
                    map.remove(&entry.key);
                }
            }
        }
    }

    /// Run `f` against a workspace's open read-write [`Store`], opening it into the pool if cold.
    #[cfg(test)]
    pub(crate) fn with_workspace<R>(&self, root: &Path, f: impl FnOnce(&Store) -> R) -> Result<R, WorkspacePoolError> {
        let entry = self.get_or_open(root)?;
        let store = entry.store.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(f(&store))
    }

    /// Reload `entry`'s config when its `basemind.toml` changed on disk since it was loaded.
    ///
    /// The stamp is advanced before the reload, so a file that fails to parse is reported once per
    /// edit rather than once per request, and the previous config stays in force until a good one
    /// lands. A hosted read stack with no live connection is dropped so its next build picks the new
    /// config up; live sessions keep theirs until they reconnect. The scanner's thread pool is sized
    /// once per process, so a changed `scan_threads` only warns.
    fn refresh_config(&self, entry: &WorkspaceEntry) {
        let now = config::daemon::ConfigStamp::of(&entry.root);
        {
            let mut stamp = entry.config_stamp.lock().unwrap_or_else(PoisonError::into_inner);
            if *stamp == now {
                return;
            }
            *stamp = now;
        }
        let new = match config::daemon::load_daemon(&entry.root) {
            Ok(new) => new,
            Err(error) => {
                tracing::warn!(root = %entry.root.display(), %error, "config changed but does not load; keeping the previous config");
                return;
            }
        };
        let old = entry.config();
        if old.resources.scan_threads != new.resources.scan_threads {
            tracing::warn!(
                root = %entry.root.display(),
                "config changed: resources.scan_threads is fixed for the process lifetime; restart the daemon for it to take effect"
            );
        }
        tracing::info!(root = %entry.root.display(), stamp = %now, "config changed; reloaded");
        *entry.config.write().unwrap_or_else(PoisonError::into_inner) = std::sync::Arc::new(new);
        #[cfg(all(feature = "comms", any(unix, windows)))]
        if entry.active_conns.load(std::sync::atomic::Ordering::Acquire) == 0
            && let Ok(mut slot) = entry.serve_state.try_lock()
        {
            *slot = None;
        }
    }

    /// Fetch the entry for `root`, opening it read-write and inserting it (evicting LRU past the
    /// cap) if cold. The returned `Arc` lets the caller run the scan after the map lock is dropped.
    ///
    /// The map key stays [`store::workspace_key`], which canonicalizes internally — so two spellings
    /// of one repo (`/tmp/x` and `/private/tmp/x`, `/repo` and `/repo/..//repo`) already collapse to
    /// one entry, and the root-guard resolution below cannot change which entry a caller lands on.
    /// The entry's `root`, however, is now the resolved path, because that is what gets opened.
    fn get_or_open(&self, root: &Path) -> Result<std::sync::Arc<WorkspaceEntry>, WorkspacePoolError> {
        let key = store::workspace_key(root);
        {
            let hit = self.lock_map().get(&key).cloned();
            if let Some(entry) = hit {
                self.refresh_config(&entry);
                return Ok(entry);
            }
        }
        let _opening = self.open_lock.lock().unwrap_or_else(PoisonError::into_inner);
        {
            let map = self.lock_map();
            if let Some(entry) = map.get(&key) {
                return Ok(entry.clone());
            }
        }
        // The daemon's choke point for issue #62: relay, HTTP, `host_read_stack`, `begin_conn` and
        // every forwarded rescan funnel through here, so refusing a non-project root here is what
        // actually stops the daemon opening `/` read-write and walking the whole filesystem.
        //
        // `root` arrives straight off the wire (`hello.root`), so everything below uses the
        // RESOLVED path the guard returns — never the raw one. Opening the raw path is how a client
        // sending `/..` got `/` opened read-write past a guard that had approved something else.
        let resolved = match config::root_guard::workspace_root_verdict(root) {
            Ok(resolved) => resolved,
            Err(refusal) => {
                return Err(WorkspacePoolError::RootRefused {
                    root: root.to_path_buf(),
                    message: config::root_guard::refusal_message(root, refusal),
                });
            }
        };
        let config_stamp = config::daemon::ConfigStamp::of(&resolved);
        let config = config::daemon::load_daemon(&resolved)?;
        let store = Store::open_with_holder(&resolved, VIEW_WORKING, LockHolder::Daemon)?;
        let rebuild_pending = store.index_rebuild_needed;
        if rebuild_pending {
            tracing::warn!(root = %resolved.display(), "index is empty over a populated file map; scheduling a full rebuild");
            store.mark_status_health(&crate::store::ScanHealth::partial("index rebuilding", 0, 0));
        }
        let entry = std::sync::Arc::new(WorkspaceEntry {
            index: std::sync::RwLock::new(store.index_db.clone()),
            store: Mutex::new(store),
            config: std::sync::RwLock::new(std::sync::Arc::new(config)),
            config_stamp: Mutex::new(config_stamp),
            root: resolved,
            key: key.clone(),
            last_used: Mutex::new(Instant::now()),
            full_scan_gen: std::sync::atomic::AtomicU64::new(0),
            rebuild_pending: std::sync::atomic::AtomicBool::new(rebuild_pending),
            last_full: Mutex::new(None),
            #[cfg(all(feature = "comms", any(unix, windows)))]
            serve_state: tokio::sync::Mutex::new(None),
            active_conns: std::sync::atomic::AtomicUsize::new(0),
            busy: std::sync::atomic::AtomicUsize::new(0),
        });

        // Victims are dropped AFTER the map lock is released: the last `Arc` to an entry closes its
        // fjall database, which must not happen while every other request waits on the map.
        let mut evicted = Vec::new();
        let mut map = self.lock_map();
        while map.len() >= self.cap {
            // ~keep Only evict entries with no live relay connection and no in-flight scan or store op
            // ~keep — a hosted workspace must not be dropped from under an rmcp session, and a scanning
            // ~keep one still holds its index lock. If every entry is busy, exceed the cap rather than
            // ~keep evict an active one (the sweep reclaims it once it drains).
            let victim = map
                .values()
                .filter(|e| e.evictable())
                .min_by_key(|e| e.last_used())
                .map(|e| e.key.clone());
            match victim.and_then(|key| map.remove(&key)) {
                Some(entry) => evicted.push(entry),
                None => break,
            }
        }
        map.insert(key, entry.clone());
        drop(map);
        evicted.iter().for_each(|victim| victim.revoke_index());
        drop(evicted);
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
            .filter(|e| e.last_used().elapsed() >= idle && e.evictable())
            .map(|e| e.key.clone())
            .collect();
        let removed: Vec<_> = stale.iter().filter_map(|key| map.remove(key)).collect();
        drop(map);
        removed.iter().for_each(|entry| entry.revoke_index());
        let count = removed.len();
        drop(removed);
        count
    }

    /// Number of hot workspaces currently held. Exposed for tests and diagnostics.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.lock_map().len()
    }
}

#[cfg(all(feature = "comms", any(unix, windows)))]
#[path = "workspace_pool_read_stack.rs"]
mod read_stack;

#[cfg(test)]
#[path = "workspace_pool_tests.rs"]
mod tests;
