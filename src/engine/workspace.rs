//! The shared, workspace-level read stack ([`Workspace`]) behind [`crate::mcp::ServerState`].
//!
//! Everything here is scoped to a `(root, view)` and is safe to share — via an [`Arc`] — across
//! many client connections at once. That is the pivot for a single-daemon architecture: one heavy
//! read stack (store, in-RAM code map, git indexes, config) built once and pointed at by
//! every per-connection [`ServerState`](crate::mcp::ServerState), which keeps only its own identity.
//!
//! Split out of `state.rs` so the per-connection type and the shared type each stay well under the
//! per-file size budget.

use std::ops::Deref;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};

use arc_swap::ArcSwap;
use parking_lot::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::engine::types::LifecycleNotice;
use crate::engine::{MapCache, outline_cache::OutlineCache};
use crate::index::view::IndexView;
use crate::store::Store;

/// Heavy, workspace-level read state shared across every connection to one `(root, view)`.
///
/// Held behind an [`Arc`] on each [`ServerState`](crate::mcp::ServerState). In the in-process serve/CLI
/// path exactly one `ServerState` points at a given `Workspace`, so behavior is identical to
/// when these fields lived directly on `ServerState`; a future daemon can point many connections at
/// one stack.
pub struct Workspace {
    /// The opened store ([`StoreSlot`]): behind a lock a scan writes through, or, for a daemon-hosted
    /// workspace, behind an `ArcSwap` a publish replaces without waiting on any reader.
    store: StoreSlot,
    pub(crate) root: PathBuf,
    /// In-RAM mirror of every indexed file's L1 blob.
    ///
    /// Cross-file queries (`search_symbols`, `dependents`) otherwise re-read 1 blob per file
    /// per call — for a 39k-file repo that's seconds. With the preload they're pure-RAM scans.
    /// Wrapped in `ArcSwap` so the filesystem watcher can publish a new snapshot without
    /// blocking readers. Read-path tools do `.load_full()` once at the top to take a stable
    /// `Arc<MapCache>` for the duration of the call.
    pub(crate) cache: ArcSwap<MapCache>,
    /// Discovered git repository, or `None` when serving against a non-git directory.
    /// All git-aware tools (`working_tree_status`, `recent_changes`, …) check this and
    /// return an MCP error if `None`.
    pub(crate) repo: Option<Arc<crate::git::Repo>>,
    /// Sha-keyed cache for commit-files diffs, log walks, and blame results.
    pub(crate) git_cache: Arc<crate::git_cache::GitCache>,
    /// Precomputed git-history index (posting lists `path → [commit]`). `Some` only on a writable
    /// serve in a git repo with the index enabled; a read-only serve or a Fjall-lock collision
    /// leaves it `None`, and the git tools fall back to the live walk. Used by the history tools
    /// only when HEAD is an indexed tip (the freshness gate), so it never serves stale results.
    pub(crate) git_history: Option<Arc<crate::git_history::GitHistoryIndex>>,
    /// `(blob_oid, lang) -> Arc<OutlineEntry>` cache that keeps `symbol_history` fast on
    /// hot files even when the symbol's source blob shows up in many adjacent commits.
    pub(crate) outline_cache: Arc<OutlineCache>,
    /// The `[languages]` overrides of `config`, compiled on first use: the git tools and a stale
    /// re-parse detect a language per call, and the table never changes while the server runs.
    lang_rules: std::sync::OnceLock<crate::lang_rules::LangRules>,
    /// Scanner config (include / exclude globs, eager_l2, document tier knobs, …).
    /// Held on the server so the `rescan` MCP tool can re-run a scan in-process
    /// without re-reading `basemind.toml`.
    pub(crate) config: Arc<crate::config::Config>,
    /// The host's outline caches shared between its read stacks, for a daemon-hosted workspace;
    /// `None` gives every map a cache of its own ([`Workspace::outlines`]).
    shared_outlines: Option<Arc<crate::engine::l1_shared::SharedOutlines>>,
    /// Sum of `size_bytes` across every indexed file. Captured at boot and
    /// after each `rescan`. Feeds the corpus-baseline cost in
    /// [`crate::engine::savings::estimate_from_text`].
    pub(crate) corpus_bytes: AtomicU64,
    /// Monotonic counter bumped every time `cache` is swapped (boot, rescan, view watcher).
    /// In-memory pagination cursors embed this value as a snapshot id so a resume call
    /// against a stale generation can be detected and reported back as
    /// `cursor_invalidated = true`.
    pub(crate) cache_generation: AtomicU32,
    /// Moves with `cache_generation` when the index may have changed (rescan, view watcher, daemon
    /// refresh), but not when the map of the index the server opened is published (the boot warm,
    /// the lazy build). Lists that never come from the empty boot placeholder stamp their cursors
    /// with it — `code_search` / `docs_search` rank out of LanceDB, `code_outline` reads the map or
    /// the one blob alike — so that publish landing between two pages is not a changed index.
    pub(crate) index_generation: AtomicU32,
    /// Ranked lists of recent `code_search` / `docs_search` retrievals, keyed by query and
    /// `cache_generation`, so a later page does not rank the whole retrieval again.
    #[cfg_attr(
        not(any(feature = "code-search", feature = "documents")),
        allow(dead_code, reason = "read only by the ranked tools, which this build lacks")
    )]
    pub(crate) ranked: crate::engine::ranked_cache::RankedCache,
    /// The file-level import graph of the live view, kept for one `cache_generation`
    /// ([`crate::engine::import_graph::import_graph`]).
    #[cfg_attr(
        not(all(test, any(feature = "code-intel-js", feature = "code-intel-stack"))),
        allow(dead_code, reason = "the import graph's commands and arguments land in later waves")
    )]
    pub(crate) import_graph: crate::engine::import_graph::GraphCache,
    /// Per-repo scope key for LanceDB tables and `memory_by_key` Fjall keyspace.
    /// Computed once at boot. Do NOT recompute per-call.
    #[cfg_attr(
        not(feature = "memory"),
        allow(dead_code, reason = "read only by memory notes, which this build lacks")
    )]
    pub(crate) scope: String,
    /// LanceDB vector store. Lazy-init on first memory/document/code-search call.
    #[cfg(feature = "code-search")]
    pub(crate) lance: crate::engine::lance_cell::LanceCell,
    /// Owner of this workspace's `individual` memory notes, which is the same at every server start
    /// ([`OpenOptions::memory_owner`]).
    #[cfg(feature = "memory")]
    pub(crate) memory_owner: String,
    /// The repo group's memory vector store (see `engine::memory::vectors`). Lazy-init on first use.
    #[cfg(feature = "memory")]
    pub(crate) memory_lance: crate::engine::lance_cell::LanceCell,
    /// Memory namespaces `(vis_byte, owner)` whose vector rows a search in this process is bringing
    /// up to date with the notes, or has.
    #[cfg(feature = "memory")]
    pub(crate) memory_backfill:
        std::sync::Mutex<ahash::AHashMap<(u8, String), crate::engine::memory::vectors::Backfill>>,
    /// Whether this process has filed the group's notes under [`Workspace::scope`]
    /// ([`Workspace::memory_index`]).
    #[cfg(feature = "memory")]
    pub(crate) memory_rescoped: AtomicBool,
    /// Shared embedding engine. Lazy-init on first embed call ([`Workspace::shared_embedder`]).
    #[cfg(any(feature = "memory", feature = "documents", feature = "code-search"))]
    pub(crate) embedder: std::sync::OnceLock<Arc<crate::embeddings::SharedEmbedder>>,
    /// Held by the one embedder load running, so the calls that arrive during it wait for it.
    #[cfg(any(feature = "memory", feature = "documents", feature = "code-search"))]
    pub(super) embedder_loading: Mutex<()>,
    /// True while the boot-time initial scan (auto-scan of an empty index) is running. Lets a
    /// client polling `status` distinguish "index still building" from "index empty / no matches"
    /// so the build cost is not silently folded into the first query's latency.
    pub(crate) initial_scan_active: AtomicBool,
    /// Wall-clock duration of the boot-time initial scan, in milliseconds, once it completes
    /// (`0` = no initial scan happened this session, or it is still running). Surfaced on `status`
    /// as `index_build_ms` to report indexing time separately from query time.
    pub(crate) initial_scan_ms: AtomicU64,
    /// True while the boot-time in-RAM code-map preload (`MapCache::build` over the existing blobs)
    /// is still running. Deferring that build off the startup path is what lets `serve` answer the
    /// MCP `initialize`/`tools/list` handshake immediately instead of blocking on a rayon `par_iter`
    /// that can be starved for minutes by other sessions' scans. Cache-reading tools await
    /// [`wait_for_warm`](Self::wait_for_warm) while this is set (see [`crate::mcp::ServerState::await_cache_ready`]).
    pub(crate) cache_warming: AtomicBool,
    /// Wall-clock duration of the boot-time cache preload, in milliseconds, once it completes
    /// (`0` = still warming or no deferred preload this session). Surfaced on `status` as `warm_ms`.
    pub(crate) cache_warm_ms: AtomicU64,
    /// Opened once when the deferred preload finishes and the full map is swapped in. Tools that read
    /// the cache wait on it (bounded by [`CACHE_WARM_WAIT_CAP`]) so a query issued during the warmup
    /// window returns COMPLETE data rather than an empty snapshot.
    warm_gate: WarmGate,
    /// Background rescans in flight: a watcher batch, or the catch-up pass of a stack opened over a
    /// populated index ([`BootScan::CatchUp`]). Non-zero surfaces as the `Rescanning` lifecycle so a
    /// client sees "results may be a moment stale" rather than treating a mid-rescan snapshot as
    /// final. A count, not a flag: a watcher batch that ends must not clear a catch-up still running.
    /// Held through [`RescanMark`], which shares the counter rather than the workspace, so a pass
    /// that drops its mark last holds no reference once the workspace reads settled.
    rescans_active: Arc<AtomicUsize>,
    /// Watcher batches of a daemon-hosted workspace waiting for a scan slot, merged into one pass
    /// (see [`crate::engine::scan_queue`]). `index_status` reports what waits.
    pub(crate) scan_queue: crate::engine::scan_queue::ScanQueue,
    /// True when the in-RAM code map is built ON DEMAND — at the first
    /// [`await_cache_ready`](crate::mcp::ServerState::await_cache_ready) barrier — instead of at construction.
    ///
    /// Set only for the one-shot CLI. A CLI process answers exactly one tool call and exits, and
    /// most tools (`repo_info`, `status`, every git tool) never read the map at all — yet
    /// [`MapCache::build`] deserializes EVERY indexed file's L1 blob, so those tools were paying
    /// seconds of whole-corpus startup for data they never touch. `serve` keeps eager/background
    /// warming: it is long-lived, so the build amortizes over the session.
    pub(crate) lazy_cache: bool,
    /// Gates the one-time on-demand build under [`lazy_cache`](Self::lazy_cache). Concurrent
    /// callers of the barrier all wait for the single build rather than racing to rebuild the map.
    pub(crate) lazy_cache_built: std::sync::OnceLock<()>,
    /// True when this serve fell back to a read-only store because another serve owns the
    /// write lock for this repo (issue #27). The single in-process writer (`Workspace::rescan`,
    /// behind the `rescan` tool) checks this and returns a clean error rather than writing
    /// without the lock.
    pub(crate) read_only: bool,
    /// True on a long-lived serve, which keeps the git-history index caught up with HEAD: a history
    /// call that finds the index behind asks for a sync. The one-shot CLI never syncs.
    pub(crate) history_catch_up: bool,
    /// True while a git-history sync this stack asked for is running, so a burst of history calls
    /// behind a moved HEAD starts one sync, not one per call.
    /// `index_status` reports it: a paged git walk that spans the end of a sync is refused,
    /// because the index orders commits differently from the live walk.
    pub(crate) history_syncing: Arc<AtomicBool>,
    /// The working-tree watcher's watch counts, which `index_status` reports. Stays empty on a host
    /// that runs no per-directory watcher.
    pub(crate) watch_health: crate::watch_health::WatchHealth,
    /// The host's git-history sync ([`OpenOptions::history_sync`]).
    pub(crate) history_sync: Option<fn(&Workspace)>,
    /// Keeps the host process up while a git-history sync of this workspace runs detached. Set for a
    /// daemon-hosted workspace, whose syncs no client link waits on.
    pub(crate) work_pin: Option<WorkPin>,
    /// The git index's file list as `find_files` last read it ([`crate::engine::ops::find_files`]).
    pub(crate) tracked: std::sync::Mutex<crate::engine::ops::find_files::TrackedCache>,
}

/// Pins the host process while long detached work runs: the guard it returns holds the pin until
/// dropped.
pub type WorkPin = Arc<dyn Fn() -> Box<dyn Send> + Send + Sync>;

/// Whether the workspace's `status.json` records a partial or unresolved last scan. A missing or
/// unreadable sidecar is not degraded: the empty-index checks above own the never-scanned case.
fn last_scan_degraded(store: &Store) -> bool {
    crate::store::read_status_sidecar(&store.basemind_dir).is_some_and(|status| status.is_degraded())
}

/// How a [`Workspace`] is opened: which background facilities its host runs and how the index is
/// written. The MCP adapter converts its `ServerOptions` into this.
#[derive(Debug, Clone, Copy)]
pub struct OpenOptions {
    /// The host keeps the workspace fresh in the background: a deferred map warm, the git-history
    /// catch-up.
    pub(crate) background: bool,
    /// The store was opened without the write lock; this process must not scan into it.
    pub(crate) read_only: bool,
    /// Another process (the machine daemon) writes the index for this one, so an empty index still
    /// asks for an initial scan although the store is read-only.
    pub(crate) forwards_writes: bool,
    /// Build the in-RAM map on the first call that reads it instead of at construction.
    pub(crate) lazy_cache: bool,
    /// How the host brings the git-history index up to HEAD when a history read finds it behind;
    /// `None` reads the live walk until something else syncs it.
    pub(crate) history_sync: Option<fn(&Workspace)>,
    /// The owner of the workspace's `individual` memory notes, from its root: the host's durable
    /// principal for the checkout (the MCP adapter resolves the comms v2 identity).
    #[cfg(feature = "memory")]
    pub(crate) memory_owner: fn(&std::path::Path) -> String,
}

/// The scan a workspace owes when it is opened, per [`boot_plan`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootScan {
    /// None: the host keeps the index fresh from here (a watcher), or never writes it.
    None,
    /// A from-scratch scan: the index is empty, or its last full scan was cancelled or aborted
    /// before covering the workspace or finishing its resolve pass (issues #79, #84).
    Initial,
    /// The index is populated, but nothing watched the tree since it was written: a daemon-hosted
    /// stack is built after the daemon started, after its stack was shed, or after the daemon exited,
    /// and a `git checkout` or an edit in between is in no index. A whole-tree incremental pass (the
    /// stat-and-hash compare of `index_rescan {}`) brings it up to date.
    CatchUp,
}

/// Boot decisions derived purely from the opened store + [`OpenOptions`]: the scan the workspace
/// owes ([`BootScan`]), and whether the in-RAM cache preload is deferred to the background.
///
/// A catch-up is owed only by a daemon-hosted stack (`forwards_writes`): an in-process server is
/// the test-support in-memory one or a one-shot CLI call, since `basemind serve` relays to the
/// daemon and has no in-process path.
///
/// Factored out so [`Workspace::new`] (which sets the `cache_warming` atomic + eager/deferred
/// cache) and `BasemindServer::new_with_options` (which spawns the matching background tasks) agree
/// by construction rather than by duplicated expressions.
pub fn boot_plan(store: &Store, options: &OpenOptions) -> BootPlan {
    let view_is_working = store.view == crate::store::VIEW_WORKING;
    let fjall_index_empty = store
        .index_handle()
        .map(|db| db.view().symbols_index_is_empty())
        .unwrap_or(false);
    let writes = (options.forwards_writes || !options.read_only) && view_is_working;
    let scan = if writes && (store.index.files.is_empty() || fjall_index_empty || last_scan_degraded(store)) {
        BootScan::Initial
    } else if writes && options.forwards_writes && options.background {
        BootScan::CatchUp
    } else {
        BootScan::None
    };
    BootPlan {
        scan,
        defer_warm: options.background && scan != BootScan::Initial,
    }
}

/// What [`boot_plan`] decided.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BootPlan {
    pub(crate) scan: BootScan,
    pub(crate) defer_warm: bool,
}

/// One background rescan in flight on a [`Workspace`], counted in its lifecycle until dropped.
pub(crate) struct RescanMark(Arc<AtomicUsize>);

impl Drop for RescanMark {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Workspace {
    /// Build the heavy read stack for one `(root, view)`. Owns the opened `store` and the already
    /// resolved `git_history` handle; computes the scope key, corpus size, and the initial in-RAM
    /// code map (eager, deferred-warm placeholder, or lazy) per [`boot_plan`] + [`OpenOptions`].
    ///
    /// The single sanctioned constructor for the shared stack — `BasemindServer::new_with_options`
    /// calls it, and a future daemon-hosted path can reuse it to build one stack shared across many
    /// [`ServerState`](crate::mcp::ServerState) connections.
    pub(crate) fn new(
        store: Store,
        root: PathBuf,
        config: Arc<crate::config::Config>,
        repo: Option<Arc<crate::git::Repo>>,
        git_cache: Arc<crate::git_cache::GitCache>,
        git_history: Option<Arc<crate::git_history::GitHistoryIndex>>,
        options: OpenOptions,
    ) -> Self {
        Self::sharing_outlines(store, root, config, repo, git_cache, git_history, options, None)
    }

    /// [`Workspace::new`] whose maps take their decoded outlines from `shared_outlines`, the cache a
    /// daemon shares between every read stack it hosts ([`crate::engine::l1_shared`]).
    #[allow(
        clippy::too_many_arguments,
        reason = "Workspace::new's parts plus the one cache a host shares"
    )]
    pub(crate) fn sharing_outlines(
        store: Store,
        root: PathBuf,
        config: Arc<crate::config::Config>,
        repo: Option<Arc<crate::git::Repo>>,
        git_cache: Arc<crate::git_cache::GitCache>,
        git_history: Option<Arc<crate::git_history::GitHistoryIndex>>,
        options: OpenOptions,
        shared_outlines: Option<Arc<crate::engine::l1_shared::SharedOutlines>>,
    ) -> Self {
        let scope = repo
            .as_ref()
            .map(|r| crate::git::scope_key(r))
            .unwrap_or_else(|| crate::git::path_scope_key(&root));
        let corpus_bytes: u64 = store.index.files.values().map(|e| e.size_bytes).sum();
        let defer_warm = boot_plan(&store, &options).defer_warm;
        let cache = if defer_warm || options.lazy_cache {
            Arc::new(MapCache::empty())
        } else {
            Arc::new(MapCache::build(
                &store,
                outlines_of(shared_outlines.as_deref(), &config),
            ))
        };
        tracing::info!(
            files = store.index.files.len(),
            corpus_bytes,
            git = repo.is_some(),
            scope = %scope,
            deferred_warm = defer_warm,
            lazy_cache = options.lazy_cache,
            "code map ready for MCP server (preloaded, warming in background, or lazy)"
        );
        #[cfg(feature = "memory")]
        let memory_owner = (options.memory_owner)(&store.root);
        let outline_cache: Arc<OutlineCache> = Arc::new(OutlineCache::default());
        Self {
            #[cfg(feature = "memory")]
            memory_owner,
            store: StoreSlot::new(store),
            root,
            cache: ArcSwap::from(cache),
            repo,
            git_cache,
            git_history,
            outline_cache,
            lang_rules: std::sync::OnceLock::new(),
            config,
            shared_outlines,
            corpus_bytes: AtomicU64::new(corpus_bytes),
            cache_generation: AtomicU32::new(1),
            index_generation: AtomicU32::new(1),
            ranked: Default::default(),
            import_graph: Default::default(),
            scope,
            #[cfg(feature = "code-search")]
            lance: Default::default(),
            #[cfg(any(feature = "memory", feature = "documents", feature = "code-search"))]
            embedder: std::sync::OnceLock::new(),
            #[cfg(any(feature = "memory", feature = "documents", feature = "code-search"))]
            embedder_loading: Mutex::new(()),
            #[cfg(feature = "memory")]
            memory_lance: Default::default(),
            #[cfg(feature = "memory")]
            memory_backfill: std::sync::Mutex::new(ahash::AHashMap::new()),
            #[cfg(feature = "memory")]
            memory_rescoped: AtomicBool::new(false),
            initial_scan_active: AtomicBool::new(false),
            initial_scan_ms: AtomicU64::new(0),
            cache_warming: AtomicBool::new(defer_warm),
            cache_warm_ms: AtomicU64::new(0),
            warm_gate: WarmGate::default(),
            rescans_active: Arc::new(AtomicUsize::new(0)),
            scan_queue: Default::default(),
            lazy_cache: options.lazy_cache,
            lazy_cache_built: std::sync::OnceLock::new(),
            read_only: options.read_only,
            history_catch_up: options.background,
            history_syncing: Arc::new(AtomicBool::new(false)),
            watch_health: crate::watch_health::WatchHealth::default(),
            history_sync: options.history_sync,
            work_pin: None,
            tracked: std::sync::Mutex::default(),
        }
    }
}

/// Where a map of a workspace configured by `config` takes its outline cache from: `shared`, else
/// a cache of its own, either charged against `[resources] max_map_cache_mb`.
fn outlines_of<'a>(
    shared: Option<&'a crate::engine::l1_shared::SharedOutlines>,
    config: &crate::config::Config,
) -> crate::engine::l1_shared::Outlines<'a> {
    let budget = crate::engine::l1_cache::budget_bytes_from(&config.resources);
    match shared {
        Some(shared) => crate::engine::l1_shared::Outlines::shared(shared, budget),
        None => crate::engine::l1_shared::Outlines::private(budget),
    }
}

impl Workspace {
    /// Where this workspace's next map takes its outline cache from: the host's shared one, else a
    /// cache of its own.
    pub(crate) fn outlines(&self) -> crate::engine::l1_shared::Outlines<'_> {
        outlines_of(self.shared_outlines.as_deref(), &self.config)
    }

    /// The compiled `[languages]` overrides. A table that does not compile was already reported
    /// when the config was loaded; detection then falls back to the built-in rules.
    pub(crate) fn lang_rules(&self) -> &crate::lang_rules::LangRules {
        self.lang_rules
            .get_or_init(|| crate::lang_rules::LangRules::from_config(&self.config.languages).unwrap_or_default())
    }

    /// Read the store: a snapshot for a hosted workspace, else the lock's read side, waiting while a
    /// scan holds it. A hosted snapshot stays valid for as long as the caller holds it, whatever is
    /// published meanwhile.
    pub(crate) fn store_read(&self) -> StoreRead<'_> {
        match &self.store {
            StoreSlot::Local(lock) => StoreRead::Local(lock.read()),
            StoreSlot::Hosted(current) => StoreRead::Snapshot(current.load_full()),
        }
    }

    /// Write the store, waiting for every reader to finish. `None` for a hosted workspace, whose
    /// store the daemon's pool writes and a publish replaces.
    pub(crate) fn store_write(&self) -> Option<RwLockWriteGuard<'_, Store>> {
        match &self.store {
            StoreSlot::Local(lock) => Some(lock.write()),
            StoreSlot::Hosted(_) => None,
        }
    }

    /// Read the store if no writer holds it right now. A hosted workspace has no writer here.
    pub(crate) fn try_store_read(&self) -> Result<StoreRead<'_>, StoreBusy> {
        match &self.store {
            StoreSlot::Local(lock) => lock.try_read().map(StoreRead::Local).ok_or(StoreBusy),
            StoreSlot::Hosted(current) => Ok(StoreRead::Snapshot(current.load_full())),
        }
    }

    /// Serve `store` from now on. A hosted workspace swaps its snapshot, so it waits for no reader
    /// and no reader waits for it; a local one takes the write side.
    pub(crate) fn replace_store(&self, store: Store) {
        match &self.store {
            StoreSlot::Local(lock) => *lock.write() = store,
            StoreSlot::Hosted(current) => current.store(Arc::new(store)),
        }
    }

    /// Mark the deferred preload finished and wake every caller of
    /// [`wait_for_warm`](Self::wait_for_warm).
    pub(crate) fn finish_warm(&self) {
        self.cache_warming.store(false, Ordering::Relaxed);
        self.warm_gate.open();
    }

    /// Wait for the deferred preload, at most [`CACHE_WARM_WAIT_CAP`]. Returns at once when no
    /// preload is running. Never call it holding a store guard: the preload reads the store.
    pub(crate) fn wait_for_warm(&self) {
        self.warm_gate
            .wait_while(CACHE_WARM_WAIT_CAP, || self.cache_warming.load(Ordering::Relaxed));
    }

    /// Wait until the in-RAM map can answer a query that reads the whole corpus: under
    /// [`lazy_cache`](Self::lazy_cache) build it now (once); otherwise wait out a deferred preload
    /// ([`wait_for_warm`](Self::wait_for_warm)). Never call it holding a store guard.
    pub(crate) fn wait_map_ready(&self) {
        if self.lazy_cache {
            if self.lazy_cache_built.get().is_none() {
                self.build_map_on_demand();
            }
            return;
        }
        if self.cache_warming.load(Ordering::Relaxed) {
            self.wait_for_warm();
        }
    }

    /// Build the whole-corpus in-RAM map and publish it, exactly once however many callers race.
    /// Returns the map when this call built it, so the caller can warm its term index.
    pub(crate) fn build_map_on_demand(&self) -> Option<Arc<MapCache>> {
        let mut built = None;
        self.lazy_cache_built.get_or_init(|| {
            let started = std::time::Instant::now();
            let store = self.store_read();
            let cache = Arc::new(MapCache::build(&store, self.outlines()));
            let files = cache.len();
            self.cache.store(Arc::clone(&cache));
            self.cache_generation.fetch_add(1, Ordering::Relaxed);
            let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            self.cache_warm_ms.store(elapsed_ms, Ordering::Relaxed);
            tracing::debug!(files, elapsed_ms, "in-RAM code map built on demand");
            built = Some(cache);
        });
        built
    }

    /// Ask the host to bring the git-history index up to HEAD, when it keeps one caught up
    /// ([`OpenOptions::history_sync`]).
    pub(crate) fn request_history_sync(&self) {
        if let Some(sync) = self.history_sync {
            sync(self);
        }
    }

    /// Record that the index may have changed under the published map: both generations move.
    pub(crate) fn note_index_changed(&self) {
        self.cache_generation.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.index_generation.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

impl Workspace {
    /// Grammar for `path`, honouring the workspace's `[languages]` overrides so the git-side tools
    /// and a stale re-parse agree with what the scanner indexed.
    pub(crate) fn detect_lang(&self, path: impl AsRef<std::ffi::OsStr>) -> Option<crate::lang::LangId> {
        self.lang_rules().detect(std::path::Path::new(path.as_ref())).lang()
    }

    /// Count a background rescan in the lifecycle until the returned mark is dropped.
    pub(crate) fn mark_rescan(&self) -> RescanMark {
        self.rescans_active.fetch_add(1, Ordering::Relaxed);
        RescanMark(Arc::clone(&self.rescans_active))
    }

    /// Current [`Lifecycle`] derived from the boot/rescan atomics, applying the documented precedence.
    pub(crate) fn lifecycle(&self) -> Lifecycle {
        Lifecycle::from_flags(
            self.initial_scan_active.load(Ordering::Relaxed),
            self.cache_warming.load(Ordering::Relaxed),
            self.rescans_active.load(Ordering::Relaxed) > 0,
        )
    }

    /// The `index_rebuilding` notice while the workspace's `status.json` records a partial scan or an
    /// uncommitted resolve pass. The daemon marks it when it opens a workspace whose fjall index was
    /// wiped or is empty over a populated file map, and a session that does not own the writer store
    /// (daemon-hosted) cannot see that any other way.
    fn index_rebuilding_notice(&self) -> Option<LifecycleNotice> {
        let status = crate::store::read_status_sidecar(&crate::store::workspace_cache_dir(&self.root));
        LifecycleNotice::for_scan_status(status.as_ref())
    }

    /// The notice for a workspace nothing has been scanned into, so an empty answer is never read
    /// as "no matches". Judged on the store's file map, not the in-RAM map, which is also empty
    /// before a lazy build. `None` while another task holds the store lock.
    fn empty_index_notice(&self) -> Option<LifecycleNotice> {
        let store = self.try_store_read().ok()?;
        (store.index.files.is_empty() && store.index.doc_files.is_empty()).then(LifecycleNotice::empty_index)
    }

    /// A [`LifecycleNotice`] to attach to a tool response, or `None` when
    /// [`Ready`](Lifecycle::Ready). Lets every read tool label a possibly-incomplete result with the
    /// server state + an actionable message, so an agent knows to retry rather than concluding "empty".
    pub(crate) fn lifecycle_notice(&self) -> Option<LifecycleNotice> {
        LifecycleNotice::for_state(self.lifecycle())
            .or_else(|| self.index_rebuilding_notice())
            .or_else(|| self.empty_index_notice())
    }
}

/// Where a workspace keeps its [`Store`].
///
/// A workspace that scans in this process mutates its store in place, so the store sits behind a
/// synchronous, task-fair lock (the engine names no runtime): a scan holds the write side for its
/// whole run, and a waiting writer holds back new readers. A daemon-hosted workspace never scans
/// here: the pool's writer does, and a publish hands over the file map it wrote. Its store is a
/// read-only one borrowing the writer's index per call, so it sits behind an [`ArcSwap`]: a read
/// takes the snapshot current when it starts, a publish swaps in the next one, and a long read
/// never holds a publish back nor a publish the reads that follow.
enum StoreSlot {
    Local(Box<RwLock<Store>>),
    Hosted(ArcSwap<Store>),
}

impl StoreSlot {
    /// A store that borrows its index from the daemon's pool ([`Store::index_source`]) is hosted.
    fn new(store: Store) -> Self {
        if store.index_source.is_some() {
            Self::Hosted(ArcSwap::from_pointee(store))
        } else {
            Self::Local(Box::new(RwLock::new(store)))
        }
    }
}

/// A read of the workspace's [`Store`]: the lock's read guard, or the snapshot a hosted workspace
/// served when the read began. Dereferences to the store either way.
pub(crate) enum StoreRead<'a> {
    Local(RwLockReadGuard<'a, Store>),
    Snapshot(Arc<Store>),
}

impl Deref for StoreRead<'_> {
    type Target = Store;

    fn deref(&self) -> &Store {
        match self {
            Self::Local(guard) => guard,
            Self::Snapshot(store) => store,
        }
    }
}

/// The store is held by a writer (a scan) right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StoreBusy;

/// A one-way gate a preload opens and readers wait on. The condition itself lives with the caller
/// (an atomic flag); the mutex only orders a waiter's check against [`open`](Self::open), so a
/// waiter that saw the gate closed is already waiting when the opener notifies.
#[derive(Default)]
struct WarmGate {
    lock: Mutex<()>,
    opened: Condvar,
}

impl WarmGate {
    /// Wake every waiter. The caller clears its condition first.
    fn open(&self) {
        let _held = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        self.opened.notify_all();
    }

    /// Block while `closed` holds, at most `cap`.
    fn wait_while(&self, cap: std::time::Duration, closed: impl Fn() -> bool) {
        let held = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = self
            .opened
            .wait_timeout_while(held, cap, |_| closed())
            .unwrap_or_else(PoisonError::into_inner);
    }
}

/// Upper bound a cache-reading tool waits for the deferred boot preload to finish before serving from
/// whatever is loaded so far. Sized so a normal repo's preload (seconds) completes within the wait — a
/// query issued right after the handshake returns COMPLETE data — while a pathologically large tree
/// still can't hang a call indefinitely (it returns partial results labelled with a warming notice).
pub(crate) const CACHE_WARM_WAIT_CAP: std::time::Duration = std::time::Duration::from_secs(15);

/// Coarse server lifecycle state surfaced to clients so an empty/partial result is never mistaken for
/// "no matches". Precedence (highest first): [`BuildingIndex`](Lifecycle::BuildingIndex) (a from-scratch
/// scan is populating the index) > [`WarmingUp`](Lifecycle::WarmingUp) (blobs are loading into RAM) >
/// [`Rescanning`](Lifecycle::Rescanning) (a watcher-driven incremental refresh is in flight) >
/// [`Ready`](Lifecycle::Ready).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifecycle {
    Ready,
    WarmingUp,
    BuildingIndex,
    Rescanning,
}

impl Lifecycle {
    /// Pure precedence classifier over the three lifecycle flags. Highest first: `building`
    /// (from-scratch index scan), then `warming` (blobs loading into RAM), then `rescanning`
    /// (watcher refresh), then `Ready`. Split out from [`ServerState::lifecycle`] so the precedence
    /// is unit-testable without constructing a server.
    pub(crate) fn from_flags(building: bool, warming: bool, rescanning: bool) -> Self {
        if building {
            Lifecycle::BuildingIndex
        } else if warming {
            Lifecycle::WarmingUp
        } else if rescanning {
            Lifecycle::Rescanning
        } else {
            Lifecycle::Ready
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{ScanHealth, VIEW_WORKING, write_status_sidecar};

    #[test]
    fn a_partial_or_unresolved_last_scan_is_degraded_and_a_clean_one_is_not() {
        crate::store::init_isolated_cache();
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(dir.path(), VIEW_WORKING).expect("open store");
        assert!(
            !last_scan_degraded(&store),
            "a workspace with no sidecar is not degraded"
        );
        for (health, degraded) in [
            (ScanHealth::complete(), false),
            (ScanHealth::resolving(), true),
            (ScanHealth::partial("cancelled", 1, 9), true),
        ] {
            write_status_sidecar(&store.basemind_dir, 1, 1, &health);
            assert_eq!(last_scan_degraded(&store), degraded, "{health:?}");
        }
    }
}

#[cfg(test)]
#[path = "workspace_store_tests.rs"]
mod store_tests;
