//! The shared, workspace-level read stack ([`SharedReadStack`]) behind [`super::ServerState`].
//!
//! Everything here is scoped to a `(root, view)` and is safe to share — via an [`Arc`] — across
//! many client connections at once. That is the pivot for a single-daemon architecture: one heavy
//! read stack (store, in-RAM code map, git indexes, config, telemetry) built once and pointed at by
//! every per-connection [`ServerState`](super::ServerState), which keeps only its own identity.
//!
//! Split out of `state.rs` so the per-connection type and the shared type each stay well under the
//! per-file size budget.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64};

use arc_swap::ArcSwap;
use tokio::sync::{Notify, RwLock};

use super::{MapCache, OutlineCache, ServerOptions, telemetry};
use crate::index::view::IndexView;
use crate::store::Store;

/// Heavy, workspace-level read state shared across every connection to one `(root, view)`.
///
/// Held behind an [`Arc`] on each [`ServerState`](super::ServerState). In the in-process serve/CLI
/// path exactly one `ServerState` points at a given `SharedReadStack`, so behavior is identical to
/// when these fields lived directly on `ServerState`; a future daemon can point many connections at
/// one stack.
pub(crate) struct SharedReadStack {
    pub(crate) store: RwLock<Store>,
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
    /// Per-tool-call telemetry writer; appends to `.basemind/telemetry.jsonl`.
    /// Always present (best-effort writes); the dashboard surfaces / statusline
    /// read from the same file.
    pub(crate) telemetry: Arc<telemetry::Telemetry>,
    /// Sum of `size_bytes` across every indexed file. Captured at boot and
    /// after each `rescan`. Feeds the corpus-baseline cost in
    /// [`super::savings::estimate_from_text`].
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
    pub(crate) ranked: super::ranked_cache::RankedCache,
    /// Per-repo scope key for LanceDB tables and `memory_by_key` Fjall keyspace.
    /// Computed once at boot. Do NOT recompute per-call.
    #[allow(dead_code)]
    pub(crate) scope: String,
    /// LanceDB vector store. Lazy-init on first memory/document/code-search call.
    #[cfg(any(feature = "memory", feature = "documents", feature = "code-search"))]
    pub(crate) lance: super::lance_cell::LanceCell,
    /// The repo group's memory vector store (see `memory_vectors`). Lazy-init on first use.
    #[cfg(feature = "memory")]
    pub(crate) memory_lance: super::lance_cell::LanceCell,
    /// Memory namespaces `(vis_byte, owner)` whose vector rows a search in this process is bringing
    /// up to date with the notes, or has.
    #[cfg(feature = "memory")]
    pub(crate) memory_backfill: std::sync::Mutex<ahash::AHashMap<(u8, String), super::memory_vectors::Backfill>>,
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
    /// [`cache_ready`](Self::cache_ready) while this is set (see [`super::ServerState::await_cache_ready`]).
    pub(crate) cache_warming: AtomicBool,
    /// Wall-clock duration of the boot-time cache preload, in milliseconds, once it completes
    /// (`0` = still warming or no deferred preload this session). Surfaced on `status` as `warm_ms`.
    pub(crate) cache_warm_ms: AtomicU64,
    /// Fired once when the deferred preload finishes and the full map is swapped in. Tools that read
    /// the cache `notified().await` on this (bounded by [`super::state::CACHE_WARM_WAIT_CAP`]) so a
    /// query issued during the warmup window returns COMPLETE data rather than an empty snapshot.
    pub(crate) cache_ready: Notify,
    /// True while a watcher-driven incremental rescan (`scan_and_refresh` from the active filesystem
    /// watcher) is in flight. Surfaced as the `Rescanning` lifecycle so a client sees "results may be
    /// a moment stale" rather than treating a mid-rescan snapshot as final.
    pub(crate) rescan_active: AtomicBool,
    /// True when the in-RAM code map is built ON DEMAND — at the first
    /// [`await_cache_ready`](super::ServerState::await_cache_ready) barrier — instead of at construction.
    ///
    /// Set only for the one-shot CLI. A CLI process answers exactly one tool call and exits, and
    /// most tools (`repo_info`, `status`, every git tool) never read the map at all — yet
    /// [`MapCache::build`] deserializes EVERY indexed file's L1 blob, so those tools were paying
    /// seconds of whole-corpus startup for data they never touch. `serve` keeps eager/background
    /// warming: it is long-lived, so the build amortizes over the session.
    pub(crate) lazy_cache: bool,
    /// Gates the one-time on-demand build under [`lazy_cache`](Self::lazy_cache). Concurrent
    /// callers of the barrier all await the single build rather than racing to rebuild the map.
    pub(crate) lazy_cache_built: tokio::sync::OnceCell<()>,
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
    /// The git index's file list as `find_files` last read it ([`super::helpers_files`]).
    pub(super) tracked: std::sync::Mutex<super::helpers_files::TrackedCache>,
    /// True when this serve delegates every write to the machine daemon (the sole fjall writer)
    /// instead of writing locally. The store is opened read-only, but — unlike a plain
    /// [`read_only`](Self::read_only) fallback — the empty-index auto-scan, the filesystem
    /// watcher, and the `rescan` tool FORWARD their scans to the daemon over the socket and then
    /// rebuild the in-RAM map from the daemon-written `index.msgpack`. Only ever true on a
    /// `comms`-enabled build; always false otherwise, so the local-writer paths are unchanged.
    ///
    /// Only exists on a `comms` build: every read of this field lives behind the same `cfg`, so on
    /// a non-`comms` build there is no daemon to forward to and the field would be dead.
    #[cfg(all(feature = "comms", any(unix, windows)))]
    pub(crate) daemon_writer: bool,
    /// In-process host seam for a DAEMON-HOSTED read stack: the daemon's own workspace pool (the
    /// machine's sole fjall writer). When `Some`, the writer / resolved-refs branch sites route
    /// directly through it instead of forwarding to the daemon over a socket — closing the
    /// daemon-dials-itself loopback and, crucially, serving resolved-refs from the pool's read-write
    /// fjall index (which a `daemon_writer` serve's index-less store cannot see). `None` on every
    /// non-hosted stack (in-process serve / CLI), where the `daemon_writer` FORWARD path stays live.
    #[cfg(all(feature = "comms", any(unix, windows)))]
    pub(crate) host: Option<std::sync::Arc<dyn crate::mcp::HostBackend>>,
}

/// Whether the workspace's `status.json` records a partial or unresolved last scan. A missing or
/// unreadable sidecar is not degraded: the empty-index checks above own the never-scanned case.
fn last_scan_degraded(store: &Store) -> bool {
    crate::store::read_status_sidecar(&store.basemind_dir).is_some_and(|status| status.is_degraded())
}

/// Boot decisions derived purely from the opened store + [`ServerOptions`]: whether an initial scan
/// is needed (an empty index, or one whose last full scan was cancelled / aborted before covering the
/// workspace or finishing its resolve pass: issues #79, #84), and whether the in-RAM cache preload is deferred to the background.
///
/// Factored out so [`SharedReadStack::new`] (which sets the `cache_warming` atomic + eager/deferred
/// cache) and `BasemindServer::new_with_options` (which spawns the matching background tasks) agree
/// by construction rather than by duplicated expressions.
pub(crate) fn boot_plan(store: &Store, options: &ServerOptions) -> (bool, bool) {
    let view_is_working = store.view == crate::store::VIEW_WORKING;
    let fjall_index_empty = store
        .index_view()
        .map(|view| view.symbols_index_is_empty())
        .unwrap_or(false);
    let needs_initial_scan = store.layered_read.is_none()
        && (options.daemon_writer || !options.read_only)
        && view_is_working
        && (store.index.files.is_empty() || fjall_index_empty || last_scan_degraded(store));
    let defer_warm = options.background && !needs_initial_scan;
    (needs_initial_scan, defer_warm)
}

impl SharedReadStack {
    /// Build the heavy read stack for one `(root, view)`. Owns the opened `store` and the already
    /// resolved `git_history` handle; computes the scope key, corpus size, and the initial in-RAM
    /// code map (eager, deferred-warm placeholder, or lazy) per [`boot_plan`] + [`ServerOptions`].
    ///
    /// The single sanctioned constructor for the shared stack — `BasemindServer::new_with_options`
    /// calls it, and a future daemon-hosted path can reuse it to build one stack shared across many
    /// [`ServerState`](super::ServerState) connections.
    // ~keep Each argument is a distinct pre-resolved dependency of the read stack (store, git handles,
    // ~keep config, options, host seam); bundling them into a params struct would only relocate the same
    // ~keep fields with no clarity gain, so the eighth (cfg-gated `host`) argument is accepted here.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        store: Store,
        root: PathBuf,
        config: Arc<crate::config::Config>,
        repo: Option<Arc<crate::git::Repo>>,
        git_cache: Arc<crate::git_cache::GitCache>,
        git_history: Option<Arc<crate::git_history::GitHistoryIndex>>,
        options: ServerOptions,
        #[cfg(all(feature = "comms", any(unix, windows)))] host: Option<std::sync::Arc<dyn crate::mcp::HostBackend>>,
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
                super::l1_cache::budget_bytes_from(&config.resources),
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
        let outline_cache: Arc<OutlineCache> = Arc::new(OutlineCache::default());
        let telemetry_handle = Arc::new(telemetry::Telemetry::new(&store.basemind_dir));
        Self {
            store: RwLock::new(store),
            root,
            cache: ArcSwap::from(cache),
            repo,
            git_cache,
            git_history,
            outline_cache,
            lang_rules: std::sync::OnceLock::new(),
            config,
            telemetry: telemetry_handle,
            corpus_bytes: AtomicU64::new(corpus_bytes),
            cache_generation: AtomicU32::new(1),
            index_generation: AtomicU32::new(1),
            ranked: Default::default(),
            scope,
            #[cfg(any(feature = "memory", feature = "documents", feature = "code-search"))]
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
            cache_ready: Notify::new(),
            rescan_active: AtomicBool::new(false),
            lazy_cache: options.lazy_cache,
            lazy_cache_built: tokio::sync::OnceCell::new(),
            read_only: options.read_only,
            history_catch_up: options.background,
            history_syncing: Arc::new(AtomicBool::new(false)),
            tracked: std::sync::Mutex::default(),
            #[cfg(all(feature = "comms", any(unix, windows)))]
            daemon_writer: options.daemon_writer,
            #[cfg(all(feature = "comms", any(unix, windows)))]
            host,
        }
    }
}

impl SharedReadStack {
    /// Whether a memory request crosses the daemon's socket, whose frame bounds its size, rather
    /// than being answered in this process.
    #[cfg(feature = "memory")]
    pub(crate) fn memory_is_framed(&self) -> bool {
        #[cfg(all(feature = "comms", any(unix, windows)))]
        return self.daemon_writer && self.host.is_none();
        #[cfg(not(all(feature = "comms", any(unix, windows))))]
        false
    }

    /// The compiled `[languages]` overrides. A table that does not compile was already reported
    /// when the config was loaded; detection then falls back to the built-in rules.
    pub(crate) fn lang_rules(&self) -> &crate::lang_rules::LangRules {
        self.lang_rules
            .get_or_init(|| crate::lang_rules::LangRules::from_config(&self.config.languages).unwrap_or_default())
    }

    /// Record that the index may have changed under the published map: both generations move.
    pub(crate) fn note_index_changed(&self) {
        self.cache_generation.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.index_generation.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
