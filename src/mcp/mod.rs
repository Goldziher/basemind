//! MCP server exposing the basemind code map + git context to AI agents.
//!
//! The server opens the store writably and is the canonical Fjall owner: it holds the exclusive
//! lock so the in-process `rescan` tool (and the background watcher) can refresh the index. While
//! a server is running, standalone `basemind scan` / `basemind watch` against the same repo fail
//! fast with a lock error rather than racing it. Tools return JSON so the agent can navigate by
//! file path + line numbers without opening source files.
//!
//! Transport: stdio (the canonical MCP transport). Spawn via `basemind serve`.

mod budget;
pub(crate) mod cursor;
mod helpers;
mod helpers_admin;
mod helpers_calls;
#[cfg(all(feature = "comms", unix))]
mod helpers_comms;
mod helpers_compress;
#[cfg(feature = "documents")]
mod helpers_documents;
#[cfg(feature = "memory")]
mod helpers_governance;
mod helpers_graph;
mod helpers_grep;
mod helpers_impls;
#[cfg(feature = "memory")]
mod helpers_proposals;
#[cfg(feature = "crawl")]
mod helpers_web;
mod lean;
mod lenient;
#[cfg(any(feature = "memory", feature = "documents"))]
mod memory;
mod savings;
mod telemetry;
mod tools;
mod tools_admin;
#[cfg(all(feature = "comms", unix))]
mod tools_comms;
mod tools_compress;
mod tools_git;
mod tools_governance;
mod tools_memory;
#[cfg(feature = "crawl")]
mod tools_web;
mod toon;
mod types;
mod types_admin;
#[cfg(all(feature = "comms", unix))]
mod types_comms;
mod types_compress;
mod types_documents;
mod types_git;
mod types_governance;
mod types_graph;
mod types_impls;
mod types_memory;

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use lru::LruCache;
use rmcp::ServerHandler;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::tool_handler;
use tokio::sync::RwLock;

use crate::extract::{FileMapL1, Import};
use crate::lang::LangId;
use crate::store::Store;

/// Public re-export of every tool `*Params` type plus the `Parameters` wrapper, so the
/// in-process CLI (`src/cli/`) can build tool arguments and call the `#[tool]` methods
/// directly. This is the parity-by-construction surface: the CLI runs the identical tool
/// code an MCP client would dispatch.
pub mod params {
    pub use rmcp::handler::server::wrapper::Parameters;

    pub(crate) use super::lenient::Lenient;

    pub use super::types::{
        BlameFileParams, BlameSymbolParams, CommitsTouchingParams, DependentsParams,
        DiffFileParams, DiffOutlineParams, FindCallersParams, FindCommitsByPathParams,
        FindReferencesParams, HotFilesParams, ListFilesParams, OutlineParams, RecentChangesParams,
        RepoInfoParams, RescanParams, SearchDocumentsParams, SearchSymbolsParams, StatusParams,
        SymbolHistoryParams, TelemetrySummaryParams, WorkingTreeStatusParams, WorkspaceGrepParams,
    };
    #[cfg(feature = "crawl")]
    pub use super::types::{WebCrawlParams, WebMapParams, WebScrapeParams};
    pub use super::types_admin::{CacheClearParams, CacheGcParams, CacheStatsParams};
    pub use super::types_governance::{
        MemoryAuditParams, ProposalAcceptParams, ProposalRejectParams, ProposalsListParams,
        ProposalsMineParams,
    };
    pub use super::types_graph::CallGraphParams;
    pub use super::types_impls::FindImplementationsParams;
    pub use super::types_memory::{
        MemoryDeleteParams, MemoryGetParams, MemoryListParams, MemoryPutParams, MemorySearchParams,
        Visibility,
    };
}

pub use params::Parameters;

/// In-memory cache for `symbol_history`-style workflows: given a blob's git OID and the
/// language we'd extract with, hold onto the parsed `FileMapL1` and the source bytes so
/// repeated visits to the same blob (across commits, modes, or tool calls) skip the
/// tree-sitter parse entirely. Memory-only — blob OIDs are content-addressed and immutable,
/// so cache invalidation is implicit (a new blob = a new key).
///
/// Cap chosen to bound steady-state memory at a few MB for typical repositories: 512
/// entries × ~few KiB per `FileMapL1` + Arc'd source = on the order of 1–10 MiB.
pub(crate) const OUTLINE_CACHE_CAP: usize = 512;

pub(crate) struct OutlineEntry {
    pub map: Arc<FileMapL1>,
    pub source: Arc<Vec<u8>>,
}

pub(crate) type OutlineCache = Mutex<LruCache<(gix::ObjectId, LangId), Arc<OutlineEntry>>>;

/// Shared MCP server state. `ToolRouter<Self>` is Clone (Arc inside), so we hold it directly
/// on the struct as the `#[tool_handler]` macro expects.
#[derive(Clone)]
pub struct BasemindServer {
    pub(crate) state: Arc<ServerState>,
    // Touched by macro-generated dispatch; dead_code can't see that.
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

pub(crate) struct ServerState {
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
    /// `(blob_oid, lang) -> Arc<OutlineEntry>` cache that keeps `symbol_history` fast on
    /// hot files even when the symbol's source blob shows up in many adjacent commits.
    pub(crate) outline_cache: Arc<OutlineCache>,
    /// Scanner config (include / exclude globs, eager_l2, document tier knobs, …).
    /// Held on the server so the `rescan` MCP tool can re-run a scan in-process
    /// without re-reading `.basemind/basemind.toml`.
    pub(crate) config: Arc<crate::config::Config>,
    /// Per-tool-call telemetry writer; appends to `.basemind/telemetry.jsonl`.
    /// Always present (best-effort writes); the dashboard surfaces / statusline
    /// read from the same file.
    pub(crate) telemetry: Arc<telemetry::Telemetry>,
    /// Sum of `size_bytes` across every indexed file. Captured at boot and
    /// after each `rescan`. Feeds the corpus-baseline cost in
    /// [`super::savings::estimate`].
    pub(crate) corpus_bytes: std::sync::atomic::AtomicU64,
    /// Monotonic counter bumped every time `cache` is swapped (boot, rescan, view watcher).
    /// In-memory pagination cursors embed this value as a snapshot id so a resume call
    /// against a stale generation can be detected and reported back as
    /// `cursor_invalidated = true`.
    pub(crate) cache_generation: std::sync::atomic::AtomicU32,
    /// Per-repo scope key for LanceDB tables and `memory_by_key` Fjall keyspace.
    /// Computed once at boot. Do NOT recompute per-call.
    #[allow(dead_code)] // used by memory / documents feature tools
    pub(crate) scope: String,
    /// Owner segment for the individual-memory tier. Resolved once at boot from
    /// `BASEMIND_AGENT_ID` (validated through [`crate::comms::ids::AgentId`] so it is
    /// NUL-free) or `"anon"` when unset/invalid. Group-tier writes ignore it.
    #[allow(dead_code)] // used by the memory feature tools
    pub(crate) agent_id: String,
    /// LanceDB vector store. Lazy-init on first memory/document call.
    #[cfg(any(feature = "memory", feature = "documents"))]
    pub(crate) lance: tokio::sync::OnceCell<Arc<crate::lance::LanceStore>>,
    /// Shared embedding engine. Lazy-init on first embed call.
    #[cfg(feature = "intelligence")]
    pub(crate) embedder: tokio::sync::OnceCell<Arc<crate::embeddings::SharedEmbedder>>,
    /// Shared kreuzcrawl engine. Initialised at server boot from the `[crawl]`
    /// config section; `None` if engine construction failed (the web_* tools
    /// will return an MCP error rather than crash).
    #[cfg(feature = "crawl")]
    pub(crate) crawl_engine: Option<kreuzcrawl::CrawlEngineHandle>,
    /// Lazily-connected client to the user-global comms broker. The first comms tool call
    /// spawns the daemon if absent (via `ensure_and_connect`) and caches the connected client
    /// here; subsequent calls reuse it. `None` until the first call. Best-effort: a connect
    /// failure surfaces as an MCP error on the call, never at server boot.
    #[cfg(all(feature = "comms", unix))]
    pub(crate) comms_client: tokio::sync::Mutex<Option<crate::comms::client::CommsClient>>,
}

pub(crate) struct MapCache {
    /// path → L1 (kept sorted by path; iteration order matches `list_files`)
    pub(crate) by_path: BTreeMap<crate::path::RelPath, FileMapL1>,
    /// Pre-flattened `(path, imports)` view used by the `dependents` tool. Without this,
    /// every `dependents` call rebuilds the same `HashMap<PathBuf, Vec<Import>>` from
    /// scratch. Precomputing once at server boot drops that to pure pointer-chase.
    pub(crate) imports_index: Vec<(PathBuf, Vec<Import>)>,
}

impl MapCache {
    fn build(store: &Store) -> Self {
        let mut by_path = BTreeMap::new();
        for (path, entry) in &store.index.files {
            match store.read_l1_by_hex(&entry.hash_hex) {
                Ok(Some(l1)) => {
                    by_path.insert(path.clone(), l1);
                }
                Ok(None) | Err(_) => continue,
            }
        }
        let imports_index: Vec<(PathBuf, Vec<Import>)> = by_path
            .iter()
            .map(|(p, l1)| (p.to_path_buf(), l1.imports.clone()))
            .collect();
        Self {
            by_path,
            imports_index,
        }
    }
}

/// Construction-time switches for [`BasemindServer`].
///
/// `serve` wants every background facility running; a one-shot CLI query wants
/// none of them (no auto-scan, no view watcher, no background GC) so the process
/// exits the instant the single tool call returns.
///
/// NOTE: this is intentionally a struct of named bools rather than a bare flag —
/// a future workstream (the live FS watcher / `--no-watch`) will extend it with
/// finer-grained switches (e.g. `auto_scan` vs `watch` decoupled). Keep new knobs
/// additive and defaulted so callers that only care about `background` stay terse.
#[derive(Debug, Clone, Copy)]
pub struct ServerOptions {
    /// When true, spawn the empty-index auto-scan, the view watcher thread, and
    /// the background blob GC. When false, the server is a pure one-shot query
    /// handle: it preloads the in-RAM map cache and nothing else.
    pub background: bool,
    /// When true (and `background` is on, and the served view is the working
    /// view), spawn a live filesystem watcher that funnels changed paths into
    /// `scan_and_refresh` so the in-RAM map stays current as the agent edits.
    /// When false, fall back to the passive view watcher (which only reacts to
    /// external scans writing `index.msgpack`). Disabled for one-shot queries.
    ///
    /// `--no-watch` on `basemind serve` flips this off — useful for very large
    /// repos (e.g. the ~81k-file TypeScript tree) or CI, where the continuous
    /// incremental re-scan is not worth the cost.
    pub watch: bool,
}

impl Default for ServerOptions {
    fn default() -> Self {
        // Default mirrors `serve`: everything on.
        Self {
            background: true,
            watch: true,
        }
    }
}

impl BasemindServer {
    /// Construct a server with all background facilities running (the `serve` path).
    pub fn new(
        store: Store,
        root: PathBuf,
        config: Arc<crate::config::Config>,
        repo: Option<Arc<crate::git::Repo>>,
        git_cache: Arc<crate::git_cache::GitCache>,
    ) -> Self {
        Self::new_with_options(
            store,
            root,
            config,
            repo,
            git_cache,
            ServerOptions::default(),
        )
    }

    /// Construct a one-shot server with every background facility disabled.
    ///
    /// Used by the `basemind` CLI to run a single MCP tool in-process and exit —
    /// no auto-scan, no view watcher, no background GC. The in-RAM map cache is
    /// still preloaded so the tool sees the same data an MCP client would.
    pub fn new_oneshot(
        store: Store,
        root: PathBuf,
        config: Arc<crate::config::Config>,
        repo: Option<Arc<crate::git::Repo>>,
        git_cache: Arc<crate::git_cache::GitCache>,
    ) -> Self {
        Self::new_with_options(
            store,
            root,
            config,
            repo,
            git_cache,
            ServerOptions {
                background: false,
                watch: false,
            },
        )
    }

    /// Shared constructor honoring [`ServerOptions`]. `new` / `new_oneshot` are
    /// the public entry points; this threads the `background` switch through the
    /// three spawn sites + the initial auto-scan.
    pub fn new_with_options(
        store: Store,
        root: PathBuf,
        config: Arc<crate::config::Config>,
        repo: Option<Arc<crate::git::Repo>>,
        git_cache: Arc<crate::git_cache::GitCache>,
        options: ServerOptions,
    ) -> Self {
        let scope = repo
            .as_ref()
            .map(|r| crate::git::scope_key(r))
            .unwrap_or_else(|| format!("path:{}", root.display()));
        // Resolve this server's stable agent identity once. Used as the individual-memory
        // owner segment AND the comms-broker handle, so it must be NUL-free — every candidate
        // is validated through `AgentId` and rejected candidates fall through to the next tier.
        let agent_id = resolve_agent_id(&config, &store);
        let cache = Arc::new(MapCache::build(&store));
        let corpus_bytes: u64 = store.index.files.values().map(|e| e.size_bytes).sum();
        // A fresh repo has no index yet. Auto-scan on startup (working view only)
        // so the agent never has to run `basemind scan` by hand — the scan runs
        // in-process below, after the server is up, so it never contends for the
        // Fjall lock this `serve` already holds.
        //
        // Two distinct "needs a scan" signals, both working-view only:
        //   1. The in-RAM map cache is empty — a fresh repo with no `index.msgpack`.
        //   2. The map cache is populated (msgpack survived) BUT the Fjall secondary
        //      index holds no symbols. `index.msgpack` and `index.fjall/` are separate
        //      on-disk artifacts written together by the scanner; they can diverge if the
        //      Fjall dir is removed or wiped out-of-band (manual `rm -rf`, a crash mid-wipe,
        //      or an independent index-schema bump) while the msgpack index stays current.
        //      In that state the RAM cache looks healthy but `find_references` /
        //      `search_symbols` silently return nothing. Detect it cheaply and rescan.
        let view_is_working = store.view == crate::store::VIEW_WORKING;
        // `None` index_db means Fjall failed to OPEN (not that it is empty); an auto-scan writes to
        // the same broken path and would just fail in a loop, so treat that as "not empty" and let
        // the failure surface elsewhere rather than triggering a futile rescan.
        let fjall_index_empty = store
            .index_db
            .as_ref()
            .map(|db| db.symbols_index_is_empty())
            .unwrap_or(false);
        // NOTE: a genuinely empty repo (zero source files) satisfies this every startup and
        // re-runs a trivially fast no-op scan; that is acceptable and not worth a freshness flag.
        let needs_initial_scan = view_is_working && (cache.by_path.is_empty() || fjall_index_empty);
        tracing::info!(
            files = cache.by_path.len(),
            corpus_bytes,
            git = repo.is_some(),
            scope = %scope,
            "preloaded code map into RAM for MCP server"
        );
        let outline_cache: Arc<OutlineCache> = Arc::new(Mutex::new(LruCache::new(
            NonZeroUsize::new(OUTLINE_CACHE_CAP).expect("OUTLINE_CACHE_CAP > 0"),
        )));
        let telemetry_handle = Arc::new(telemetry::Telemetry::new(&store.basemind_dir));
        #[cfg(feature = "crawl")]
        let crawl_engine = match crate::web::build_engine(&config.crawl) {
            Ok(e) => Some(e),
            Err(error) => {
                tracing::warn!(
                    ?error,
                    "crawl engine init failed; web_* tools will report errors"
                );
                None
            }
        };
        let state = Arc::new(ServerState {
            store: RwLock::new(store),
            root,
            cache: ArcSwap::from(cache),
            repo,
            git_cache,
            outline_cache,
            config,
            telemetry: telemetry_handle,
            corpus_bytes: std::sync::atomic::AtomicU64::new(corpus_bytes),
            cache_generation: std::sync::atomic::AtomicU32::new(1),
            scope,
            agent_id,
            #[cfg(any(feature = "memory", feature = "documents"))]
            lance: tokio::sync::OnceCell::new(),
            #[cfg(feature = "intelligence")]
            embedder: tokio::sync::OnceCell::new(),
            #[cfg(feature = "crawl")]
            crawl_engine,
            #[cfg(all(feature = "comms", unix))]
            comms_client: tokio::sync::Mutex::new(None),
        });
        // One-shot CLI queries skip ALL background facilities: no view watcher,
        // no auto-scan, no background GC. They preload the map cache (above) and
        // return immediately so the process can exit after a single tool call.
        if options.background {
            // Live FS watcher vs. passive view watcher are mutually exclusive for
            // the working view: the active watcher already triggers
            // `scan_and_refresh`, which writes `index.msgpack` — the exact event
            // the passive watcher reacts to. Running both would double-refresh.
            //
            // Non-working views (staged / rev-<sha>) are immutable snapshots, so
            // the active watcher is meaningless there; they always get the passive
            // watcher (which still picks up an external re-scan of that view).
            let view_is_working = {
                match state.store.try_read() {
                    Ok(g) => g.view == crate::store::VIEW_WORKING,
                    // Unable to read the view at boot is unexpected; fall back to the
                    // passive watcher rather than risk watching the wrong tree.
                    Err(_) => false,
                }
            };
            if options.watch && view_is_working {
                spawn_serve_watcher(Arc::clone(&state));
            } else {
                spawn_view_watcher(Arc::clone(&state));
            }
            // Background blob GC: reclaim orphaned blobs left behind by prior scans /
            // branch switches. Detached so it never blocks serve startup, and it never
            // crashes serve (all errors are warned + swallowed).
            if needs_initial_scan {
                // A fresh scan is what *creates* reclaimable orphans, so chain GC after it.
                let scan_state = Arc::clone(&state);
                tracing::info!("empty index on startup; running initial scan in background");
                tokio::spawn(async move {
                    match helpers::scan_and_refresh(Arc::clone(&scan_state), None).await {
                        Ok(report) => tracing::info!(
                            scanned = report.stats.scanned,
                            updated = report.stats.updated,
                            "initial background scan complete"
                        ),
                        Err(error) => {
                            tracing::warn!(%error, "initial background scan failed");
                        }
                    }
                    // Run GC after the scan settles, regardless of scan outcome.
                    run_background_gc(scan_state).await;
                });
            } else {
                // No initial scan — run GC shortly after startup to reclaim any
                // orphans from earlier sessions.
                let gc_state = Arc::clone(&state);
                tokio::spawn(async move {
                    run_background_gc(gc_state).await;
                });
            }
        }
        #[allow(unused_mut)]
        let mut router = Self::tool_router_core()
            + Self::tool_router_git()
            + Self::tool_router_memory()
            + Self::tool_router_governance()
            + Self::tool_router_admin()
            + Self::tool_router_compress();
        #[cfg(feature = "crawl")]
        {
            router += Self::tool_router_web();
        }
        #[cfg(all(feature = "comms", unix))]
        {
            router += Self::tool_router_comms();
        }
        Self {
            state,
            tool_router: router,
        }
    }
}

/// File under `.basemind/` holding the generated-and-persisted per-session agent id. Created
/// the first time identity resolution falls through to the generated tier, so two `serve`
/// sessions against different repos get distinct ids while a single repo stays stable.
const AGENT_ID_FILE: &str = "agent-id";

/// Resolve this server's stable agent identity. Tiered, each candidate validated through
/// [`crate::comms::ids::AgentId`] (an invalid candidate falls through, not fails):
///
/// 1. `BASEMIND_AGENT_ID` env — explicit per-process override.
/// 2. `config.comms.agent_id` — workspace config.
/// 3. A generated-and-persisted id at `.basemind/agent-id` — stable per repo across restarts,
///    distinct across repos so two windows differ.
/// 4. `"anon"` — the final fallback (itself a valid `AgentId`).
///
/// TODO: prefer the MCP `clientInfo.name` from rmcp's `initialize` handshake once it is
/// cleanly reachable at construction time; the persisted per-session id is the stand-in.
fn resolve_agent_id(config: &crate::config::Config, store: &Store) -> String {
    fn validated(candidate: Option<String>) -> Option<String> {
        candidate
            .and_then(|s| crate::comms::ids::AgentId::parse(s).ok())
            .map(|a| a.into_string())
    }

    if let Some(id) = validated(std::env::var("BASEMIND_AGENT_ID").ok()) {
        return id;
    }
    if let Some(id) = validated(config.comms.agent_id.clone()) {
        return id;
    }
    if let Some(id) = validated(load_or_create_persisted_agent_id(&store.basemind_dir)) {
        return id;
    }
    "anon".to_string()
}

/// Read the persisted per-session agent id from `<basemind_dir>/agent-id`, generating and
/// writing a fresh one when absent or unreadable. Best-effort: any io failure returns `None`
/// so the resolver falls through to `"anon"` rather than erroring at server boot.
fn load_or_create_persisted_agent_id(basemind_dir: &std::path::Path) -> Option<String> {
    let path = basemind_dir.join(AGENT_ID_FILE);
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    // Generate a short, id-alphabet-safe token. The low 64 bits of a nanosecond timestamp mixed
    // with the pid give a per-session-unique, NUL-free value within the `AgentId` alphabet.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let token = format!("session-{:x}-{:x}", std::process::id(), nanos);
    // Persist best-effort; a write failure still returns the in-memory token for this session.
    let _ = std::fs::write(&path, &token);
    Some(token)
}

/// Run an in-process blob GC once, logging the outcome and swallowing any error.
///
/// Uses the UNLOCKED `store_gc` primitives (`collect_referenced_hashes` + `gc_blobs`)
/// under a `blocking_read()` store guard — NEVER `store_gc::run_gc`, which re-acquires
/// the `.basemind/.lock` flock that `serve` already holds (that would deadlock). The
/// held read guard blocks the only in-process writer (`scan_and_refresh`) for the
/// mark+sweep; cross-process scans are impossible because serve holds the flock.
async fn run_background_gc(state: Arc<ServerState>) {
    let result = tokio::task::spawn_blocking(move || {
        let store = state.store.blocking_read();
        let referenced = crate::store_gc::collect_referenced_hashes(&store.basemind_dir)?;
        crate::store_gc::gc_blobs(&store.basemind_dir, &referenced)
    })
    .await;
    match result {
        Ok(Ok(report)) if report.removed > 0 => tracing::info!(
            removed = report.removed,
            bytes_freed = report.bytes_freed,
            "background blob GC reclaimed orphaned blobs"
        ),
        Ok(Ok(_)) => tracing::debug!("background blob GC: nothing to reclaim"),
        Ok(Err(error)) => tracing::warn!(%error, "background blob GC failed"),
        Err(error) => tracing::warn!(%error, "background blob GC task panicked"),
    }
}

/// Active filesystem watcher embedded in `serve` for the working view.
///
/// Unlike [`spawn_view_watcher`] (which is passive — it only reacts to an
/// external process writing `index.msgpack`), this watches the working tree
/// directly and funnels every debounced batch of changed paths into the
/// canonical in-process refresh, [`helpers::scan_and_refresh`]. That re-scans
/// under serve's already-open `Store` (its `RwLock`), so we never open a second
/// `.basemind/.lock` flock — the reason we cannot reuse `watcher::watch`, which
/// owns its own `Store`.
///
/// Threading bridge: `watcher::watch_paths` runs the debouncer on a blocking std
/// thread, but `scan_and_refresh` is async. We capture the current tokio runtime
/// `Handle` at spawn time and `handle.block_on(...)` the refresh inside the
/// callback. `block_on` is safe here because the callback runs on a plain OS
/// thread with no tokio runtime entered (it's `std::thread`, not a worker), so
/// the "cannot block the current thread from within a runtime" guard never trips.
///
/// Lifetime: the thread is detached and runs for the process lifetime, mirroring
/// `spawn_view_watcher`. The `shutdown` oneshot sender is dropped immediately, so
/// `watch_paths`'s `shutdown.try_recv()` returns `Disconnected` only if the loop
/// ever polls it after the sender drops — in practice the loop exits when the
/// process tears down stdio and the debouncer channel closes. A failed
/// incremental refresh is logged and swallowed so a transient scan error never
/// kills the watcher.
fn spawn_serve_watcher(state: Arc<ServerState>) {
    let root = state.root.clone();
    let config = Arc::clone(&state.config);
    let handle = tokio::runtime::Handle::current();
    // Keep the sender alive for the process lifetime by leaking it into the
    // detached closure-free slot: we never signal shutdown explicitly (the
    // process exit tears the watcher down), so hold the receiver and drop the
    // sender at the end of `serve`'s life via the thread owning it.
    let (_shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    std::thread::Builder::new()
        .name("basemind-mcp-serve-watcher".to_string())
        .spawn(move || {
            // Hold the sender for the whole watcher lifetime so the receiver never
            // sees `Disconnected` early; the watcher exits when the debouncer
            // channel closes at process teardown.
            let _keep_sender_alive = _shutdown_tx;
            tracing::info!(root = %root.display(), "serve watcher armed (live incremental rescan)");
            let result =
                crate::watcher::watch_paths(&root, &config, shutdown_rx, |paths, _kind| {
                    let refresh_state = Arc::clone(&state);
                    // Bridge the blocking watcher thread into the async refresh.
                    match handle.block_on(helpers::scan_and_refresh(refresh_state, Some(paths))) {
                        Ok(report) => tracing::debug!(
                            scanned = report.stats.scanned,
                            updated = report.stats.updated,
                            removed = report.stats.removed,
                            "serve watcher: incremental rescan complete"
                        ),
                        Err(error) => tracing::warn!(
                            %error,
                            "serve watcher: incremental rescan failed (watcher continues)"
                        ),
                    }
                });
            if let Err(error) = result {
                tracing::warn!(%error, "serve watcher exited with error");
            }
            tracing::info!("serve watcher: exiting");
        })
        .ok();
}

fn spawn_view_watcher(state: Arc<ServerState>) {
    let (basemind_dir, view) = {
        let store = match state.store.try_read() {
            Ok(g) => g,
            Err(_) => return,
        };
        (store.basemind_dir.clone(), store.view.clone())
    };
    let view_dir = basemind_dir.join(crate::store::VIEWS_DIR).join(&view);
    let target = view_dir.join(crate::store::INDEX_FILE);

    std::thread::Builder::new()
        .name("basemind-mcp-view-watcher".to_string())
        .spawn(move || {
            use notify_debouncer_full::new_debouncer;
            use std::time::Duration;

            let (tx, rx) = std::sync::mpsc::channel();
            let mut debouncer = match new_debouncer(Duration::from_millis(150), None, tx) {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(error = %e, "view watcher: failed to start debouncer");
                    return;
                }
            };
            if let Err(e) = debouncer.watch(&view_dir, notify::RecursiveMode::NonRecursive) {
                tracing::warn!(error = %e, dir = %view_dir.display(), "view watcher: failed to watch");
                return;
            }
            tracing::info!(target = %target.display(), "view watcher armed");

            while let Ok(result) = rx.recv() {
                let events = match result {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                let touches_index = events
                    .iter()
                    .any(|de| de.event.paths.iter().any(|p| p == &target));
                if !touches_index {
                    continue;
                }
                let new_store = match crate::store::Store::open_read_only(
                    state.root.as_path(),
                    &state
                        .store
                        .try_read()
                        .map(|g| g.view.clone())
                        .unwrap_or_default(),
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(error = %e, "view watcher: store reopen failed");
                        continue;
                    }
                };
                let new_cache = Arc::new(MapCache::build(&new_store));
                tracing::info!(
                    files = new_cache.by_path.len(),
                    "view watcher: rebuilt MapCache from refreshed index"
                );
                state.cache.store(new_cache);
                state
                    .cache_generation
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            tracing::info!("view watcher: channel closed; exiting");
        })
        .ok();
}

#[tool_handler(router = self.tool_router.clone())]
impl ServerHandler for BasemindServer {
    /// `tools/list`. Default (the overwhelming case): delegate to the static router exactly as
    /// the `#[tool_handler]` macro would, advertising every real tool. When `BASEMIND_MCP_LEAN`
    /// is set, advertise only the three lean wrapper tools instead. The macro detects this
    /// hand-written method and skips generating its own, so the default branch must remain a
    /// faithful copy of the generated body to keep the unset-flag surface byte-for-byte identical.
    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ListToolsResult, rmcp::ErrorData> {
        if lean::lean_mode_enabled() {
            return Ok(lean::lean_list_tools());
        }
        Ok(rmcp::model::ListToolsResult {
            tools: self.tool_router.list_all(),
            meta: None,
            next_cursor: None,
        })
    }

    /// `tools/call`. Default: dispatch through the static router exactly as the macro would.
    /// In lean mode, route the three wrapper tools through [`lean::lean_call_tool`], which itself
    /// delegates `invoke_tool` back to this same router — no tool logic is duplicated.
    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
        if lean::lean_mode_enabled() {
            return lean::lean_call_tool(self, &self.tool_router, request, context).await;
        }
        let tcc = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        self.tool_router.call(tcc).await
    }

    /// `get_tool` introspection. Default mirrors the macro (router lookup); in lean mode it
    /// reports the three wrapper tools so task-support validation matches the advertised surface.
    fn get_tool(&self, name: &str) -> Option<rmcp::model::Tool> {
        if lean::lean_mode_enabled() {
            return lean::lean_get_tool(name);
        }
        self.tool_router.get(name).cloned()
    }

    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "basemind is the indexed context layer for this repository, served over MCP: a \
             tree-sitter code map across 300+ languages (symbols, references, callers, call \
             graphs, implementations), git history + blame at symbol resolution, full-text + \
             semantic search, document RAG over 90+ file formats, and shared cross-session \
             memory. basemind first, shell/grep/git fallback: prefer these tools over reading \
             files, over grep, and over naked `git` — and use them for document extraction, web \
             crawling, and code parsing too. You may be one of several agents in this repo: on \
             start, check the comms room and post status as you work (see Agent comms below).\n\
             Context economy — these tools return paths, line numbers, and signatures, not \
             file bodies, so they cost a fraction of the tokens of reading source. Default to \
             them: `outline` a file before you open it (then read only the span you need); \
             `search_symbols` instead of grep for a definition; `find_references` / \
             `find_callers` instead of grepping call sites; `workspace_grep` instead of \
             shelling out to ripgrep; `rescan` after edits instead of reconnecting. Do not \
             re-read a file basemind already mapped. Same discipline beyond code: use the git \
             tools (`recent_changes` / `blame_*` / `diff_*` / `commits_touching`) instead of \
             shelling out to `git log`/`git blame`; `search_documents` and the documents \
             pipeline for extraction, RAG, keyword + entity (NER), and summary instead of \
             opening files; `web_scrape` / `web_crawl` / `web_map` for scraping, crawling, and \
             sitemaps.\n\
             Routing: \
             \"where is X defined?\" → `search_symbols`; \
             \"what calls X?\" → `find_references` (any name) or `find_callers` (specific def); \
             \"shape of this file?\" → `outline` (add `l2: true` for calls + docs); \
             \"what changed recently?\" → `recent_changes`, `commits_touching`, `symbol_history`; \
             \"who last touched this?\" → `blame_file` / `blame_symbol`; \
             \"where's the churn?\" → `hot_files`; \
             \"semantic search across PDFs/docs in the repo?\" → `search_documents`; \
             \"recall something the agent remembered earlier?\" → `memory_get` / `memory_list` / \
             `memory_search`; \
             \"remember this for later sessions?\" → `memory_put` (delete with `memory_delete`); \
             \"refresh the index after editing code?\" → `rescan` (or `rescan { paths: [...] }` \
             to limit to changed files); \
             \"any other agents working here / leave a note for the next session?\" → \
             `room_list` / `inbox_read` / `room_post`.\n\
             \"got a truncated result? fetch the next page?\" → pass `next_cursor` from the prior \
             response back as `cursor`.\n\
             \"need regex over file contents?\" → `workspace_grep`.\n\
             Code-map tools: `outline`, `search_symbols`, `find_references`, `find_callers`, \
             `list_files`, `workspace_grep`, `dependents`, `status`, `repo_info`, \
             `symbol_history`. \
             Git tools (inside a repo): `working_tree_status`, `recent_changes`, `commits_touching`, \
             `find_commits_by_path`, `hot_files`, `diff_outline`, `diff_file`, `blame_file`, \
             `blame_symbol`. \
             Intelligence tools (require build with `--features documents,memory`): \
             `search_documents`, `memory_put`, `memory_get`, `memory_list`, `memory_search`, \
             `memory_delete`. \
             Web tools (require build with `--features crawl`): `web_scrape` (one URL), \
             `web_crawl` (follow links from a seed URL), `web_map` (sitemap-only discovery). \
             Crawled pages land in the same LanceDB documents table as on-disk docs, scoped \
             under `web:<host>` — find them later with `search_documents`. \
             Agent comms (require build with `--features comms`): you may share this repo's \
             rooms with other agents working alongside you. On start, check `room_list` + \
             `inbox_read` (and recent `room_history`) for what's been said; `room_history` and \
             `inbox_read` return front-matter only (subject / from / id) — call `message_get` \
             with an id for a body. Post a concise `room_post {room, subject, body, reply_to?}` \
             when you begin, finish, or hit a decision, and reply (`reply_to`) to messages \
             about your work — do not stay silent when collaborating. Tools: `room_list`, \
             `room_join`, `room_post`, `room_history`, `inbox_read`, `message_get`, \
             `room_create`, `room_leave`, `agent_register`, `agent_list`. \
             All paths are repository-relative with forward-slash separators. \
             If a tool reports \"no indexed files\", run `basemind scan` in the repo first.",
        )
    }
}
