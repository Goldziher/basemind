//! The shared, workspace-level read stack ([`Workspace`]) behind [`crate::mcp::ServerState`].
//!
//! Everything here is scoped to a `(root, view)` and is safe to share — via an [`Arc`] — across
//! many client connections at once. That is the pivot for a single-daemon architecture: one heavy
//! read stack (store, in-RAM code map, git indexes, config) built once and pointed at by
//! every per-connection [`ServerState`](crate::mcp::ServerState), which keeps only its own identity.
//!
//! Split out of `state.rs` so the per-connection type and the shared type each stay well under the
//! per-file size budget.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
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
pub(crate) struct Workspace {
    /// The opened store. A synchronous, task-fair lock, so the engine names no runtime: a scan holds
    /// the write side for its whole run, and readers wait for it as they did on the async lock this
    /// replaces (a waiting writer holds back new readers, and a panic poisons nothing). Shared by
    /// `Arc` so an async caller can hold an owned, `Send` read guard ([`store_lock`](Self::store_lock)).
    store: Arc<RwLock<Store>>,
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
    /// only when `last_indexed_head == HEAD` (the freshness gate), so it never serves stale results.
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
    /// Sum of `size_bytes` across every indexed file. Captured at boot and
    /// after each `rescan`. Feeds the corpus-baseline cost in
    /// [`crate::mcp::savings::estimate_from_text`].
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
    /// Per-repo scope key for LanceDB tables and `memory_by_key` Fjall keyspace.
    /// Computed once at boot. Do NOT recompute per-call.
    #[allow(dead_code)]
    pub(crate) scope: String,
    /// LanceDB vector store. Lazy-init on first memory/document/code-search call.
    #[cfg(feature = "code-search")]
    pub(crate) lance: crate::mcp::lance_cell::LanceCell,
    /// Owner of this workspace's `individual` memory notes: the checkout's comms v2 principal
    /// (`identity::resolve_agent_id`), which is the same at every server start.
    #[cfg(feature = "memory")]
    pub(crate) memory_owner: String,
    /// The repo group's memory vector store (see `memory_vectors`). Lazy-init on first use.
    #[cfg(feature = "memory")]
    pub(crate) memory_lance: crate::mcp::lance_cell::LanceCell,
    /// Memory namespaces `(vis_byte, owner)` whose vector rows a search in this process is bringing
    /// up to date with the notes, or has.
    #[cfg(feature = "memory")]
    pub(crate) memory_backfill: std::sync::Mutex<ahash::AHashMap<(u8, String), crate::mcp::memory_vectors::Backfill>>,
    /// Shared embedding engine. Lazy-init on first embed call.
    #[cfg(any(feature = "memory", feature = "documents", feature = "code-search"))]
    pub(crate) embedder: tokio::sync::OnceCell<Arc<crate::embeddings::SharedEmbedder>>,
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
    /// True while a watcher-driven incremental rescan (`scan_and_refresh` from the active filesystem
    /// watcher) is in flight. Surfaced as the `Rescanning` lifecycle so a client sees "results may be
    /// a moment stale" rather than treating a mid-rescan snapshot as final.
    pub(crate) rescan_active: AtomicBool,
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
    /// write lock for this repo (issue #27). The single in-process writer (`scan_and_refresh`,
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
    /// The host's git-history sync ([`OpenOptions::history_sync`]).
    pub(crate) history_sync: Option<fn(&Workspace)>,
    /// The git index's file list as `find_files` last read it ([`crate::mcp::helpers_files`]).
    pub(crate) tracked: std::sync::Mutex<crate::mcp::helpers_files::TrackedCache>,
}

/// Whether the workspace's `status.json` records a partial or unresolved last scan. A missing or
/// unreadable sidecar is not degraded: the empty-index checks above own the never-scanned case.
fn last_scan_degraded(store: &Store) -> bool {
    crate::store::read_status_sidecar(&store.basemind_dir).is_some_and(|status| status.is_degraded())
}

/// How a [`Workspace`] is opened: which background facilities its host runs and how the index is
/// written. The MCP adapter converts its `ServerOptions` into this.
#[derive(Debug, Clone, Copy)]
pub(crate) struct OpenOptions {
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
}

/// Boot decisions derived purely from the opened store + [`OpenOptions`]: whether an initial scan
/// is needed (an empty index, or one whose last full scan was cancelled / aborted before covering the
/// workspace or finishing its resolve pass: issues #79, #84), and whether the in-RAM cache preload is deferred to the background.
///
/// Factored out so [`Workspace::new`] (which sets the `cache_warming` atomic + eager/deferred
/// cache) and `BasemindServer::new_with_options` (which spawns the matching background tasks) agree
/// by construction rather than by duplicated expressions.
pub(crate) fn boot_plan(store: &Store, options: &OpenOptions) -> (bool, bool) {
    let view_is_working = store.view == crate::store::VIEW_WORKING;
    let fjall_index_empty = store
        .index_view()
        .map(|view| view.symbols_index_is_empty())
        .unwrap_or(false);
    let needs_initial_scan = (options.forwards_writes || !options.read_only)
        && view_is_working
        && (store.index.files.is_empty() || fjall_index_empty || last_scan_degraded(store));
    let defer_warm = options.background && !needs_initial_scan;
    (needs_initial_scan, defer_warm)
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
        let scope = repo
            .as_ref()
            .map(|r| crate::git::scope_key(r))
            .unwrap_or_else(|| format!("path:{}", root.display()));
        let corpus_bytes: u64 = store.index.files.values().map(|e| e.size_bytes).sum();
        let (_needs_initial_scan, defer_warm) = boot_plan(&store, &options);
        let cache = if defer_warm || options.lazy_cache {
            Arc::new(MapCache::empty())
        } else {
            Arc::new(MapCache::build(
                &store,
                crate::engine::l1_cache::budget_bytes_from(&config.resources),
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
        let memory_owner = crate::mcp::identity::resolve_agent_id(&store.root);
        let outline_cache: Arc<OutlineCache> = Arc::new(OutlineCache::default());
        Self {
            #[cfg(feature = "memory")]
            memory_owner,
            store: Arc::new(RwLock::new(store)),
            root,
            cache: ArcSwap::from(cache),
            repo,
            git_cache,
            git_history,
            outline_cache,
            lang_rules: std::sync::OnceLock::new(),
            config,
            corpus_bytes: AtomicU64::new(corpus_bytes),
            cache_generation: AtomicU32::new(1),
            index_generation: AtomicU32::new(1),
            ranked: Default::default(),
            scope,
            #[cfg(feature = "code-search")]
            lance: Default::default(),
            #[cfg(any(feature = "memory", feature = "documents", feature = "code-search"))]
            embedder: tokio::sync::OnceCell::new(),
            #[cfg(feature = "memory")]
            memory_lance: Default::default(),
            #[cfg(feature = "memory")]
            memory_backfill: std::sync::Mutex::new(ahash::AHashMap::new()),
            initial_scan_active: AtomicBool::new(false),
            initial_scan_ms: AtomicU64::new(0),
            cache_warming: AtomicBool::new(defer_warm),
            cache_warm_ms: AtomicU64::new(0),
            warm_gate: WarmGate::default(),
            rescan_active: AtomicBool::new(false),
            lazy_cache: options.lazy_cache,
            lazy_cache_built: std::sync::OnceLock::new(),
            read_only: options.read_only,
            history_catch_up: options.background,
            history_syncing: Arc::new(AtomicBool::new(false)),
            history_sync: options.history_sync,
            tracked: std::sync::Mutex::default(),
        }
    }
}

impl Workspace {
    /// The compiled `[languages]` overrides. A table that does not compile was already reported
    /// when the config was loaded; detection then falls back to the built-in rules.
    pub(crate) fn lang_rules(&self) -> &crate::lang_rules::LangRules {
        self.lang_rules
            .get_or_init(|| crate::lang_rules::LangRules::from_config(&self.config.languages).unwrap_or_default())
    }

    /// Read the store, waiting while a scan holds it.
    pub(crate) fn store_read(&self) -> RwLockReadGuard<'_, Store> {
        self.store.read()
    }

    /// Write the store, waiting for every reader to finish.
    pub(crate) fn store_write(&self) -> RwLockWriteGuard<'_, Store> {
        self.store.write()
    }

    /// Read the store if no writer holds it right now.
    pub(crate) fn try_store_read(&self) -> Result<RwLockReadGuard<'_, Store>, StoreBusy> {
        self.store.try_read().ok_or(StoreBusy)
    }

    /// The lock itself, for a caller that needs an owned guard (`read_arc`, `write_arc`): one it
    /// can move to another thread or hold across an `.await`.
    pub(crate) fn store_lock(&self) -> &Arc<RwLock<Store>> {
        &self.store
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
            let budget = crate::engine::l1_cache::budget_bytes_from(&self.config.resources);
            let cache = Arc::new(MapCache::build(&store, budget));
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

    /// Current [`Lifecycle`] derived from the boot/rescan atomics, applying the documented precedence.
    pub(crate) fn lifecycle(&self) -> Lifecycle {
        Lifecycle::from_flags(
            self.initial_scan_active.load(Ordering::Relaxed),
            self.cache_warming.load(Ordering::Relaxed),
            self.rescan_active.load(Ordering::Relaxed),
        )
    }

    /// The `index_rebuilding` notice while the workspace's `status.json` records a partial scan or an
    /// uncommitted resolve pass. The daemon marks it when it opens a workspace whose fjall index was
    /// wiped or is empty over a populated file map, and a session that does not hold the index
    /// (`daemon_writer`, daemon-hosted) cannot see that any other way.
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
            .or_else(|| {
                // A read-only session whose projected call/impl indexes were capped answers reference
                // queries from a truncated set. Reported through the same channel as the warming/
                // rescanning states so a caller never reads an incomplete result as "no matches".
                self.cache
                    .load()
                    .projections_capped()
                    .then(LifecycleNotice::projections_capped)
            })
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
pub(crate) enum Lifecycle {
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
