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
pub(crate) const DEFAULT_HOT_CAP: usize = 16;

/// Default number of daemon-hosted READ STACKS kept warm at once — deliberately far below
/// [`DEFAULT_HOT_CAP`], which governs something much cheaper.
///
/// A hot entry is an open `Store` handle. A warm read stack is an O(corpus) resident structure per
/// workspace (the L1 outline cache, the projected call / implementation indexes, git caches,
/// LanceDB, ONNX), so letting one grow per hot entry multiplies the whole read stack by 16 — the
/// daemon-specific multiplier behind issue #62's 43.8 GiB. Three lets a developer alternate between
/// a couple of repos without paying a rebuild, and a rebuild is transparent anyway: dropping a stack
/// changes latency, not answers.
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
/// it would keep fresh), and a rebuild runs no catch-up scan, so edits made while a workspace is
/// shed are invisible until the next watcher event or an explicit `admin rescan`. That exposure
/// already existed at `WORKSPACE_HOT_TTL`; there is no reason to triple how often it is reached in
/// exchange for memory the cap has already bounded.
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
    /// Resolved config for this workspace, captured at open time.
    config: Config,
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
    /// The broker's drain token, shared so the in-process host-rescan seam
    /// ([`HostBackend::host_rescan`](crate::mcp::HostBackend::host_rescan)) cancels on drain exactly
    /// like the socket path does. The broker adopts this token (see [`Self::scan_cancel`]) rather
    /// than holding a separate one, so `begin_drain` trips a single flag that every scan path — socket
    /// or hosted — observes at per-file granularity.
    scan_cancel: ScanCancel,
}

impl WorkspacePool {
    /// Construct an empty pool bounded at `cap` hot workspaces.
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            map: Mutex::new(AHashMap::new()),
            open_lock: Mutex::new(()),
            cap: cap.max(1),
            scan_cancel: ScanCancel::new(),
        }
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
        let entry = self.get_or_open(root)?;
        entry.touch();

        let mode = if embed { EmbedMode::Inline } else { EmbedMode::Deferred };
        let incremental = matches!(paths, Some(ref p) if !full && !p.is_empty());
        let gen_before = entry.full_scan_gen.load(std::sync::atomic::Ordering::Acquire);
        let mut store = entry.store.lock().unwrap_or_else(PoisonError::into_inner);
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
        let report = if incremental {
            let paths = paths.as_deref().unwrap_or_default();
            scanner::scan_paths_with_cancel(&entry.root, &mut store, &entry.config, paths, mode, cancel)?
        } else {
            scanner::scan_with_cancel(
                &entry.root,
                &mut store,
                &entry.config,
                ScanSource::WorkingTree,
                mode,
                cancel,
            )?
        };
        if !incremental && !report.cancelled {
            let generation = entry.full_scan_gen.fetch_add(1, std::sync::atomic::Ordering::AcqRel) + 1;
            *entry.last_full.lock().unwrap_or_else(PoisonError::into_inner) = Some((generation, mode, report.stats));
        }
        Ok((report.stats, report.cancelled))
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

    /// Build (or fetch) the daemon-hosted shared read stack for `root`, opening the workspace into
    /// the pool if cold. The first caller runs
    /// [`build_hosted_read_stack`](crate::mcp::BasemindServer::build_hosted_read_stack) — which does
    /// a blocking whole-corpus `MapCache::build`, so it runs on a blocking thread — and spawns the
    /// workspace's single freshness warden; every later caller gets the same `Arc`. Concurrent
    /// callers all await the one build, because the slot lock is held across it.
    ///
    /// `host` is the in-process host seam handed to the built stack (the pool itself), so the hosted
    /// connection's writes / rescans / resolved-refs reach the pool directly instead of dialing the
    /// daemon over its own socket. `git_history_host` is the same seam for git-history reads (the
    /// daemon Broker, the sole holder of `git-history.fjall/`).
    ///
    /// Building one stack is what enforces the warm-stack cap: the trim runs here, AFTER the slot
    /// lock is released, so admitting a new stack is the moment the least-recently-used ones are
    /// shed.
    #[cfg(all(feature = "comms", any(unix, windows)))]
    pub(crate) async fn get_or_build_serve_state(
        &self,
        root: &Path,
        host: std::sync::Arc<dyn crate::mcp::HostBackend>,
        git_history_host: std::sync::Arc<dyn crate::git_history::remote::HistoryHost>,
    ) -> anyhow::Result<std::sync::Arc<crate::mcp::SharedReadStack>> {
        let entry = self.get_or_open(root).map_err(anyhow::Error::new)?;
        entry.touch();
        let (shared, built) = {
            let mut slot = entry.serve_state.lock().await;
            match slot.as_ref() {
                Some(hosted) => (hosted.shared(), false),
                None => {
                    let root_buf = root.to_path_buf();
                    let hosted = tokio::task::spawn_blocking(move || {
                        crate::mcp::BasemindServer::build_hosted_read_stack(&root_buf, host, git_history_host)
                    })
                    .await
                    .map_err(|join| anyhow::anyhow!("hosted read stack build panicked: {join}"))??;
                    let shared = hosted.shared();
                    *slot = Some(hosted);
                    (shared, true)
                }
            }
        };
        // Only an admission can push the warm set over the cap, and the stateless HTTP front-end
        // resolves a stack on every request — so trimming on the cache-hit path would walk the pool
        // once per request to discover there is nothing to do. ~keep
        if built {
            self.trim_warm_read_stacks(&entry.key);
        }
        Ok(shared)
    }

    /// Drop least-recently-used hosted read stacks until at most [`warm_read_stack_cap`] stay warm,
    /// leaving every workspace's hot store entry in place.
    ///
    /// Follows the store LRU in [`Self::get_or_open`] exactly — same `last_used` clock, same
    /// `active_conns` pin, same "exceed the cap rather than pull the rug from under a live session"
    /// rule — so there is one eviction policy here, expressed twice at two different costs.
    /// `just_built` is never a victim: the caller that paid for the build must be served from it.
    ///
    /// Non-blocking by construction: a slot whose lock is held is one being built or handed out
    /// right now, i.e. the most-recently-used, so skipping it is both cheaper and more correct than
    /// waiting for it.
    #[cfg(all(feature = "comms", any(unix, windows)))]
    fn trim_warm_read_stacks(&self, just_built: &str) {
        use std::sync::atomic::Ordering::Acquire;
        let cap = warm_read_stack_cap();
        let entries: Vec<std::sync::Arc<WorkspaceEntry>> = self.lock_map().values().cloned().collect();
        let mut warm = 0usize;
        let mut evictable: Vec<std::sync::Arc<WorkspaceEntry>> = Vec::new();
        for entry in entries {
            let is_warm = match entry.serve_state.try_lock() {
                Ok(slot) => slot.is_some(),
                Err(_) => continue,
            };
            if !is_warm {
                continue;
            }
            warm += 1;
            if entry.key != just_built && entry.active_conns.load(Acquire) == 0 {
                evictable.push(entry);
            }
        }
        let Some(mut over) = warm.checked_sub(cap).filter(|over| *over > 0) else {
            return;
        };
        evictable.sort_by_key(|entry| entry.last_used());
        for entry in evictable {
            if over == 0 {
                break;
            }
            if let Ok(mut slot) = entry.serve_state.try_lock()
                && slot.take().is_some()
            {
                over -= 1;
                tracing::info!(root = %entry.root.display(), cap, "daemon: shed a warm read stack over the cap");
            }
        }
    }

    /// Drop the hosted read stack of every workspace idle for at least `idle`, KEEPING its hot store
    /// entry, and return the count dropped.
    ///
    /// The counterpart to [`Self::evict_idle`] on a much shorter clock ([`READ_STACK_IDLE_TTL`]): a
    /// read stack is an O(corpus) projection any later connection rebuilds from the same blobs,
    /// whereas the store handle it sits next to holds the workspace's index lock. An entry with a
    /// live connection is skipped, exactly as in the entry sweep — dropping a stack out from under
    /// an in-flight rmcp session would leave that session's `Arc` alive AND let the next connection
    /// build a second stack for the same workspace, which is worse than keeping the first.
    #[cfg(all(feature = "comms", any(unix, windows)))]
    pub(crate) fn evict_idle_read_stacks(&self, idle: Duration) -> usize {
        use std::sync::atomic::Ordering::Acquire;
        let entries: Vec<std::sync::Arc<WorkspaceEntry>> = self.lock_map().values().cloned().collect();
        let mut dropped = 0;
        for entry in entries {
            if entry.active_conns.load(Acquire) != 0 || entry.last_used().elapsed() < idle {
                continue;
            }
            if let Ok(mut slot) = entry.serve_state.try_lock()
                && slot.take().is_some()
            {
                dropped += 1;
            }
        }
        dropped
    }

    /// Number of workspaces currently holding a warm hosted read stack. Exposed for tests: it is the
    /// quantity the cap bounds, and it is deliberately NOT [`Self::len`] — the two diverge on
    /// purpose.
    #[cfg(all(test, feature = "comms", any(unix, windows)))]
    pub(crate) fn warm_read_stacks(&self) -> usize {
        let entries: Vec<std::sync::Arc<WorkspaceEntry>> = self.lock_map().values().cloned().collect();
        entries
            .iter()
            .filter(|entry| entry.serve_state.try_lock().is_ok_and(|slot| slot.is_some()))
            .count()
    }

    /// Register one relay connection against `root`, returning a guard that decrements the live-count
    /// on drop. While any guard is held, eviction (LRU + idle sweep) skips this workspace, so its
    /// shared read stack is never dropped from under an in-flight rmcp session.
    #[cfg(all(feature = "comms", any(unix, windows)))]
    pub(crate) fn begin_conn(&self, root: &Path) -> Result<ServeConnGuard, WorkspacePoolError> {
        let entry = self.get_or_open(root)?;
        entry.touch();
        entry.active_conns.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Ok(ServeConnGuard { entry })
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
            let map = self.lock_map();
            if let Some(entry) = map.get(&key) {
                return Ok(entry.clone());
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
        let store = Store::open_with_holder(&resolved, VIEW_WORKING, LockHolder::Rescan)?;
        let config = load_config(&resolved)?;
        let entry = std::sync::Arc::new(WorkspaceEntry {
            store: Mutex::new(store),
            config,
            root: resolved,
            key: key.clone(),
            last_used: Mutex::new(Instant::now()),
            full_scan_gen: std::sync::atomic::AtomicU64::new(0),
            last_full: Mutex::new(None),
            #[cfg(all(feature = "comms", any(unix, windows)))]
            serve_state: tokio::sync::Mutex::new(None),
            active_conns: std::sync::atomic::AtomicUsize::new(0),
        });

        let mut map = self.lock_map();
        while map.len() >= self.cap {
            // ~keep Only evict entries with no live relay connection — a hosted workspace must not be
            // ~keep dropped from under an in-flight rmcp session. If every entry is busy, exceed the cap
            // ~keep rather than evict an active one (the sweep reclaims it once its connections drain).
            let victim = map
                .values()
                .filter(|e| e.active_conns.load(std::sync::atomic::Ordering::Acquire) == 0)
                .min_by_key(|e| e.last_used())
                .map(|e| e.key.clone());
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
        use std::sync::atomic::Ordering::Acquire;
        let mut map = self.lock_map();
        let stale: Vec<String> = map
            .values()
            .filter(|e| e.last_used().elapsed() >= idle && e.active_conns.load(Acquire) == 0)
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

/// The in-process host seam: a daemon-hosted read stack routes its writes / rescans / precise
/// resolved-reference reads straight through the pool (the machine's sole fjall writer) instead of
/// forwarding them over the daemon's own socket — the daemon would otherwise dial itself. Mirrors
/// the forwarded-op handlers in [`daemon_forward_handlers`](super::daemon_forward_handlers) exactly;
/// the pool's per-workspace store lock supplies the same serialization the socket path relied on.
///
/// Methods are synchronous (the pool's API is blocking); call sites run them under `spawn_blocking`.
#[cfg(all(feature = "comms", any(unix, windows)))]
impl crate::mcp::HostBackend for WorkspacePool {
    fn host_rescan(
        &self,
        root: &Path,
        paths: Option<Vec<PathBuf>>,
        full: bool,
        embed: bool,
    ) -> Result<ScanStats, String> {
        // Use the broker's shared drain token (not a throwaway `default()`), so a hosted mid-scan — ~keep
        // e.g. a full-corpus Inline embed pass — honors `comms stop` / SIGTERM within one file ~keep
        // instead of pinning the runtime until the 15s teardown backstop. The socket path already ~keep
        // threads this token; the host seam must too. ~keep
        self.rescan(root, paths, full, embed, &self.scan_cancel)
            .map(|(stats, _cancelled)| stats)
            .map_err(|error| error.to_string())
    }

    #[cfg(feature = "code-search")]
    fn host_code_search_lanes(
        &self,
        root: &Path,
        query: crate::comms::code_search_proto::CodeSearchLaneQuery,
    ) -> Result<crate::comms::code_search_proto::CodeSearchLaneResult, String> {
        self.with_workspace(root, |store| {
            super::daemon_forward_handlers::code_search_lanes_against(store, &query)
        })
        .map_err(|error| error.to_string())?
    }

    fn host_resolved_refs(
        &self,
        root: &Path,
        query: crate::comms::resolved_proto::ResolvedRefQuery,
    ) -> Result<crate::comms::resolved_proto::ResolvedRefResult, String> {
        self.with_workspace(root, |store| {
            super::daemon_forward_handlers::resolve_refs_against(store, &query)
        })
        .map_err(|error| error.to_string())
    }

    #[cfg(feature = "memory")]
    fn host_memory(
        &self,
        root: &Path,
        scope: &str,
        op: crate::comms::memory_proto::MemoryOp,
    ) -> Result<crate::comms::memory_proto::MemoryOutcome, String> {
        self.with_workspace_mut(root, |store| {
            let idx = store
                .index_db
                .as_ref()
                .ok_or(crate::mcp::memory_ops::MemoryOpError::IndexUnavailable)?;
            crate::mcp::memory_ops::run_memory_op(idx, scope, &op)
        })
        .map_err(|error| error.to_string())
        .and_then(|result| result.map_err(|error| error.to_string()))
    }

    #[cfg(feature = "memory")]
    fn host_governance(
        &self,
        root: &Path,
        scope: &str,
        op: crate::comms::proposals_proto::GovernanceOp,
    ) -> Result<crate::comms::proposals_proto::GovernanceOutcome, String> {
        self.with_workspace_mut(root, |store| {
            let idx = store
                .index_db
                .as_ref()
                .ok_or(crate::mcp::memory_ops::MemoryOpError::IndexUnavailable)?;
            crate::mcp::proposals_ops::run_governance_op(idx, scope, &op)
        })
        .map_err(|error| error.to_string())
        .and_then(|result| result.map_err(|error| error.to_string()))
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
