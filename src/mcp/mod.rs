//! MCP server exposing the basemind code map + git context to AI agents.
//!
//! The server opens the store writably and is the canonical Fjall owner: it holds the exclusive
//! lock so the in-process `rescan` tool (and the background watcher) can refresh the index. While
//! a server is running, standalone `basemind scan` / `basemind watch` against the same repo fail
//! fast with a lock error rather than racing it. Tools return JSON so the agent can navigate by
//! file path + line numbers without opening source files.
//!
//! Transport: stdio (the canonical MCP transport). Spawn via `basemind serve`.

pub(crate) mod admission;
pub mod agent_api;
mod background;
mod budget;
mod codegraph;
mod community;
mod completions;
pub(crate) mod cursor;
#[cfg(all(feature = "comms", any(unix, windows)))]
mod daemon_forward;
mod doc_links_cache;
mod file_url;
mod graph_html;
mod graph_svg;
mod graph_view;
mod helpers;
mod helpers_admin;
mod helpers_archmap;
mod helpers_calls;
mod helpers_calls_scan;
mod helpers_code;
#[cfg(feature = "code-search")]
mod helpers_code_search;
#[cfg(all(feature = "comms", any(unix, windows)))]
mod helpers_comms;
mod helpers_community;
mod helpers_compress;
#[cfg(feature = "documents")]
mod helpers_documents;
mod helpers_files;
mod helpers_fingerprint;
mod helpers_git;
mod helpers_git_file;
#[cfg(feature = "memory")]
mod helpers_governance;
mod helpers_graph;
mod helpers_graphview;
mod helpers_grep;
mod helpers_impls;
mod helpers_intel;
mod helpers_memory;
#[cfg(feature = "memory")]
mod helpers_proposals;
#[cfg(all(feature = "comms", any(unix, windows)))]
mod helpers_registry;
#[cfg(all(feature = "shells", any(unix, windows)))]
mod helpers_shells;
mod helpers_telemetry;
mod helpers_traverse;
#[cfg(feature = "crawl")]
mod helpers_web;
#[cfg(all(feature = "comms", any(unix, windows)))]
mod host;
mod identity;
#[cfg(feature = "test-support")]
pub mod in_memory;
mod kneedle;
mod lean;
mod lenient;
mod map_fingerprint;
#[cfg(any(feature = "memory", feature = "documents", feature = "code-search"))]
mod memory;
#[cfg(feature = "memory")]
pub(crate) mod memory_ops;
pub mod mode;
mod notifications;
mod prompts;
#[cfg(feature = "memory")]
pub(crate) mod proposals_ops;
mod router_cache;
mod savings;
mod server_handler;
mod shared_state;
mod state;
mod tasks;
mod telemetry;
mod tokens;
mod tools;
mod tools_admin;
#[cfg(all(feature = "comms", any(unix, windows)))]
mod tools_comms;
mod tools_git;
mod tools_graph;
mod tools_memory;
#[cfg(all(feature = "comms", any(unix, windows)))]
mod tools_registry;
#[cfg(all(feature = "shells", any(unix, windows)))]
mod tools_shells;
#[cfg(feature = "crawl")]
mod tools_web;
mod toon;
mod traverse;
mod types;
mod types_admin;
mod types_archmap;
mod types_code;
#[cfg(all(feature = "comms", any(unix, windows)))]
mod types_comms;
mod types_community;
mod types_compress;
mod types_documents;
mod types_git;
pub(crate) mod types_governance;
mod types_graph;
mod types_graphview;
mod types_impls;
pub(crate) mod types_memory;
#[cfg(all(feature = "comms", any(unix, windows)))]
mod types_registry;
#[cfg(all(feature = "shells", any(unix, windows)))]
mod types_shells;
mod types_traverse;
#[cfg(feature = "crawl")]
mod types_web;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use lru::LruCache;
use rmcp::handler::server::router::prompt::PromptRouter;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::task_manager::TaskManager;

use crate::extract::FileMapL1;
use crate::lang::LangId;
use crate::store::Store;

#[cfg(all(feature = "comms", any(unix, windows)))]
pub(crate) use host::HostBackend;
#[cfg(all(feature = "test-support", feature = "comms", any(unix, windows)))]
pub use in_memory::serve_in_memory_daemon_writer;
#[cfg(feature = "test-support")]
pub use in_memory::{serve_in_memory, serve_in_memory_lean};
pub(crate) use shared_state::SharedReadStack;
pub(crate) use state::{Lifecycle, MapCache, ServerState};

#[cfg(all(feature = "comms", any(unix, windows)))]
/// A daemon-owned read stack and the watcher lifetime that keeps it fresh.
pub(crate) struct HostedReadStack {
    _watcher: background::WatcherGuard,
    shared: Arc<SharedReadStack>,
}

#[cfg(all(feature = "comms", any(unix, windows)))]
impl HostedReadStack {
    /// Clone the read stack shared by every connection to this hosted workspace.
    pub(crate) fn shared(&self) -> Arc<SharedReadStack> {
        Arc::clone(&self.shared)
    }
}

/// Public re-export of every tool `*Params` type plus the `Parameters` wrapper, so the
/// in-process CLI (`src/cli/`) can build tool arguments and call the `#[tool]` methods
/// directly. This is the parity-by-construction surface: the CLI runs the identical tool
/// code an MCP client would dispatch.
pub mod params {
    pub use rmcp::handler::server::wrapper::Parameters;

    pub(crate) use super::lenient::Lenient;

    pub use super::mode::AdminMode;
    pub use super::mode::CodeMode;
    pub use super::mode::GitMode;
    pub use super::mode::GraphMode;
    #[cfg(all(feature = "shells", any(unix, windows)))]
    pub use super::mode::ShellMode;
    pub use super::mode::WebMode;
    pub use super::types::{
        BlameFileParams, BlameSymbolParams, CommitsTouchingParams, DependentsParams, DiffFileParams, DiffOutlineParams,
        FindCallersParams, FindCommitsByPathParams, FindFilesParams, FindReferencesParams, GotoDefinitionParams,
        HotFilesParams, ListFilesParams, OutlineParams, RecentChangesParams, RepoInfoParams, RescanParams,
        SearchDocumentsParams, SearchGitHistoryParams, SearchSymbolsParams, StatusParams, SymbolHistoryParams,
        TelemetrySummaryParams, WorkingTreeStatusParams, WorkspaceGrepParams,
    };
    pub use super::types_admin::{AdminParams, CacheClearParams, CacheGcParams, CacheStatsParams};
    pub use super::types_code::CodeParams;
    pub use super::types_git::GitParams;
    pub use super::types_governance::{
        MemoryAuditParams, ProposalAcceptParams, ProposalRejectParams, ProposalsListParams, ProposalsMineParams,
    };
    pub use super::types_graph::{CallGraphParams, GraphParams};
    pub use super::types_impls::FindImplementationsParams;
    pub use super::types_memory::{
        MemoryDeleteParams, MemoryGetParams, MemoryListParams, MemoryPutParams, MemorySearchParams, Visibility,
    };
    #[cfg(all(feature = "shells", any(unix, windows)))]
    pub use super::types_shells::{ShellEnv, ShellParams};
    #[cfg(feature = "crawl")]
    pub use super::types_web::WebParams;
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

/// Per-category LRU capacity for a daemon-hosted workspace's git cache (commit_files, log, blame).
/// Matches the `basemind serve --git-cache-mem` default (1024): the daemon holds one git cache per
/// hot workspace, shared across all connections, so the same budget an in-process serve used per
/// process now serves every client of that workspace.
#[cfg(all(feature = "comms", any(unix, windows)))]
pub(crate) const HOSTED_GIT_CACHE_MEM: usize = 1024;

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
    /// Shared by server clones so the last session handle shuts down its filesystem watcher.
    _watcher: Option<Arc<background::WatcherGuard>>,
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
    /// Reusable prompt templates (`prompts/list` + `prompts/get`). Built by the
    /// `#[prompt_router]` macro in [`prompts`]; `list_prompts` / `get_prompt` delegate here.
    prompt_router: PromptRouter<Self>,
    /// SEP-2663 Tasks extension executor. Slow tools (see [`tasks::SLOW_TOOLS`]) offload their work
    /// here for task-capable clients; cheaply cloneable and shared across every clone of the server.
    tasks: TaskManager,
    /// Bounds allocation-heavy calls while allowing control/comms traffic to bypass the queue.
    heavy_admission: Arc<admission::HeavyAdmission>,
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
    /// When true, the store was opened read-only (it does NOT hold the write
    /// lock) because another `serve` owns it for this repo (issue #27). The
    /// server still answers every read tool from the shared index, but it must
    /// not write: the empty-index auto-scan and the active filesystem watcher
    /// are suppressed, and the `rescan` tool returns a clean error instead of
    /// scanning. The passive view watcher still runs, so the in-RAM map tracks
    /// the lock-holding writer's `index.msgpack` updates.
    pub read_only: bool,
    /// When true, this serve forwards every write (auto-scan, watcher rescan, `rescan` tool) to
    /// the machine daemon (the sole fjall writer) rather than writing locally, and rebuilds its
    /// in-RAM map from the daemon-written `index.msgpack`. Set only by the real `serve` binary on
    /// a `comms` build; always false for the in-process one-shot and non-comms builds.
    pub daemon_writer: bool,
    /// When true, defer [`MapCache::build`] to the first tool that actually reads the map, instead
    /// of running it at construction. Set only for the one-shot CLI — see
    /// [`ServerState::lazy_cache`].
    pub lazy_cache: bool,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            background: true,
            watch: true,
            read_only: false,
            daemon_writer: false,
            lazy_cache: false,
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
        Self::new_with_options(store, root, config, repo, git_cache, ServerOptions::default())
    }

    /// Construct a one-shot server with every background facility disabled.
    ///
    /// Used by the `basemind` CLI to run a single MCP tool in-process and exit — no auto-scan, no
    /// view watcher, no background GC. The in-RAM map cache is built LAZILY: a CLI process answers
    /// one tool call, and most tools never read the map, so preloading it charged every invocation
    /// the whole-corpus blob-deserialization cost (seconds on a large monorepo) for nothing. The
    /// first tool that does read the map still builds it in full, so results are identical to what
    /// an MCP client would see.
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
                read_only: false,
                daemon_writer: false,
                lazy_cache: true,
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
        let agent_id = identity::resolve_agent_id(&config, &store);
        let history_dir = crate::git_history::shared_history_basemind_dir(&root);
        let git_history = Self::open_git_history(
            &root,
            &history_dir,
            repo.is_some(),
            &agent_id,
            &options,
            // ~keep In-process serve / CLI: no daemon hosting us, so history reads use the socket-forward
            // ~keep path (or a local handle) — never the in-process seam.
            #[cfg(all(feature = "comms", any(unix, windows)))]
            None,
        );
        // ~keep Boot decisions depend on the opened store, so compute them before it moves into the stack.
        let (needs_initial_scan, defer_warm) = shared_state::boot_plan(&store, &options);
        let shared = Arc::new(SharedReadStack::new(
            store,
            root,
            config,
            repo,
            git_cache,
            git_history,
            options,
            // ~keep In-process serve / CLI: no daemon-hosted pool, so the `daemon_writer` FORWARD path
            // ~keep (not this seam) handles writes when a daemon is up.
            #[cfg(all(feature = "comms", any(unix, windows)))]
            None,
        ));
        let state = Arc::new(ServerState {
            shared,
            agent_id,
            #[cfg(all(feature = "comms", any(unix, windows)))]
            comms_clients: tokio::sync::Mutex::new(ahash::AHashMap::new()),
            #[cfg(all(feature = "comms", any(unix, windows)))]
            delivered_notifications: tokio::sync::Mutex::new(lru::LruCache::new(
                std::num::NonZeroUsize::new(state::DELIVERED_NOTIFICATION_CAP)
                    .expect("notification cache capacity is non-zero"),
            )),
            log_level: std::sync::atomic::AtomicU8::new(notifications::DEFAULT_LOG_ORDINAL),
            lean: std::sync::atomic::AtomicBool::new(lean::lean_mode_enabled()),
        });
        Self::spawn_comms_presence(&state);
        let mut watcher = None;
        if options.background {
            let view_is_working = {
                match state.shared.store.try_read() {
                    Ok(g) => g.view == crate::store::VIEW_WORKING,
                    Err(_) => false,
                }
            };
            let watcher_guard = if options.watch && (options.daemon_writer || !options.read_only) && view_is_working {
                background::spawn_serve_watcher(Arc::clone(&state))
            } else {
                background::spawn_view_watcher(Arc::clone(&state))
            };
            watcher = Some(Arc::new(watcher_guard));
            Self::spawn_git_history_sync(&state, &history_dir);
            if needs_initial_scan {
                background::spawn_initial_scan(Arc::clone(&state));
            } else {
                if defer_warm {
                    background::spawn_cache_warm(Arc::clone(&state));
                }
                let gc_state = Arc::clone(&state);
                tokio::spawn(async move {
                    background::run_background_gc(gc_state).await;
                });
            }
        }
        Self {
            state,
            _watcher: watcher,
            tool_router: router_cache::cached_tool_router(),
            prompt_router: router_cache::cached_prompt_router(),
            tasks: TaskManager::new(),
            heavy_admission: Arc::new(admission::HeavyAdmission::default()),
        }
    }

    /// Whether this server advertises the lean three-tool surface. Resolved once at construction
    /// (see [`ServerState::lean`]) and read here per request, so the decision is per-server rather
    /// than a global-env read.
    pub(crate) fn lean_enabled(&self) -> bool {
        self.state.lean.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Assemble the full tool router (every feature-gated tool group). Pure and argument-free —
    /// its output is invariant for the life of the process, so [`new_with_options`]
    /// (Self::new_with_options) and [`from_shared`](Self::from_shared) never call this directly;
    /// they go through [`router_cache::cached_tool_router`], which builds it once behind a
    /// `OnceLock` and hands back a clone. This is the one-time builder the cache calls into.
    fn assemble_router() -> ToolRouter<Self> {
        #[allow(unused_mut)]
        let mut router = Self::tool_router_core()
            + Self::tool_router_graph()
            + Self::tool_router_git()
            + Self::tool_router_memory()
            + Self::tool_router_admin();
        #[cfg(feature = "crawl")]
        {
            router += Self::tool_router_web();
        }
        #[cfg(all(feature = "comms", any(unix, windows)))]
        {
            router += Self::tool_router_comms();
            router += Self::tool_router_registry();
        }
        #[cfg(all(feature = "shells", any(unix, windows)))]
        {
            router += Self::tool_router_shells();
        }
        router
    }

    /// Build a per-connection server that SHARES a workspace's [`SharedReadStack`] rather than
    /// opening its own. This is the daemon-hosted seam: the daemon builds one shared read stack per
    /// hot workspace (see [`build_hosted_read_stack`](Self::build_hosted_read_stack)) and hands every
    /// accepted relay connection its own `BasemindServer` over that one stack — so the heavy state
    /// (in-RAM `MapCache`, LanceDB, ONNX, git caches) is resident once per workspace, not once per
    /// client. Spawns NO background facilities: freshness is owned by the workspace's single warden
    /// (built alongside the shared stack), never per connection. `agent_id` comes from the
    /// connection's [`RelayHello`](crate::comms::relay::RelayHello), so each client keeps its own
    /// memory-owner identity even while sharing the read stack.
    #[cfg(all(feature = "comms", any(unix, windows)))]
    pub(crate) fn from_shared(
        shared: Arc<SharedReadStack>,
        agent_id: String,
        heavy_admission: Arc<admission::HeavyAdmission>,
    ) -> Self {
        let state = Arc::new(ServerState::for_connection(shared, agent_id));
        Self::spawn_comms_presence(&state);
        Self {
            state,
            _watcher: None,
            tool_router: router_cache::cached_tool_router(),
            prompt_router: router_cache::cached_prompt_router(),
            tasks: TaskManager::new(),
            heavy_admission,
        }
    }

    /// Establish broker presence at MCP connection startup. Registration remains an optional card
    /// update; the broker's `Hello` creates the routable session record.
    fn spawn_comms_presence(state: &Arc<ServerState>) {
        #[cfg(all(feature = "comms", any(unix, windows)))]
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let state = Arc::clone(state);
            runtime.spawn(async move {
                let connected = tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    helpers_comms::resolve_comms_client(&state, None),
                )
                .await;
                if !matches!(connected, Ok(Ok(_))) {
                    tracing::debug!("comms presence unavailable during MCP startup");
                }
            });
        }
        #[cfg(not(all(feature = "comms", any(unix, windows))))]
        let _ = state;
    }

    /// Build (once) the shared read stack the daemon hosts for one workspace, plus its single
    /// "warden" — a background-running [`ServerState`] that owns freshness for the whole workspace:
    /// the empty-index auto-scan, the live filesystem watcher, cache warming, and the git-history
    /// sync. The warden shares the returned [`SharedReadStack`] by `Arc`, so when its watcher
    /// rebuilds the in-RAM map every hosted connection sees the refreshed snapshot at once. This is
    /// the N-watchers-per-workspace → one-watcher-per-workspace consolidation: per-connection
    /// servers ([`from_shared`](Self::from_shared)) spawn nothing.
    ///
    /// The warden uses the same topology as today's thin `serve` (`read_only` + `daemon_writer`), so
    /// its scans forward to the daemon's sole-writer pool over the existing path — no new scan/write
    /// machinery. The store is [`open_read_only_no_index`](crate::store::Store::open_read_only_no_index)
    /// (no fjall lock), so the shared `MapCache` still builds its in-RAM call/impl indexes exactly as
    /// a thin serve does.
    #[cfg(all(feature = "comms", any(unix, windows)))]
    pub(crate) fn build_hosted_read_stack(
        root: &std::path::Path,
        host: Arc<dyn HostBackend>,
        git_history_host: Arc<dyn crate::git_history::remote::HistoryHost>,
    ) -> anyhow::Result<HostedReadStack> {
        use anyhow::Context as _;

        let view = crate::store::VIEW_WORKING;
        let store = crate::store::Store::open_read_only_no_index(root, view).context("open hosted read stack store")?;
        debug_assert!(
            store.index_db.is_none(),
            "hosted read stack must be index-less so MapCache builds in-RAM call/impl indexes"
        );
        let basemind_dir = crate::store::workspace_cache_dir(root);
        let config = Arc::new(match crate::config::load_with_overrides(root, None, None) {
            Ok(loaded) => loaded.config,
            Err(crate::config::ConfigError::NotFound(_)) => crate::config::default_for_root(root),
            Err(error) => return Err(anyhow::Error::new(error).context("load hosted workspace config")),
        });
        let repo = crate::git::Repo::discover(root).ok().map(Arc::new);
        let git_cache = Arc::new(
            crate::git_cache::GitCache::open(&basemind_dir, HOSTED_GIT_CACHE_MEM, true)
                .context("open hosted git cache")?,
        );
        let options = ServerOptions {
            background: true,
            watch: true,
            read_only: true,
            daemon_writer: true,
            lazy_cache: false,
        };
        let agent_id = identity::resolve_agent_id(&config, &store);
        let history_dir = crate::git_history::shared_history_basemind_dir(root);
        let git_history = Self::open_git_history(
            root,
            &history_dir,
            repo.is_some(),
            &agent_id,
            &options,
            Some(git_history_host),
        );
        let (needs_initial_scan, defer_warm) = shared_state::boot_plan(&store, &options);
        let shared = Arc::new(SharedReadStack::new(
            store,
            root.to_path_buf(),
            config,
            repo,
            git_cache,
            git_history,
            options,
            // ~keep The daemon-hosted seam: hosted writes / rescans / resolved-refs run directly through
            // ~keep the pool instead of looping back over the daemon's own socket.
            Some(host),
        ));
        // ~keep The warden shares the stack by Arc and owns every background facility for the workspace.
        let warden = Arc::new(ServerState::for_connection(Arc::clone(&shared), agent_id));
        let watcher = background::spawn_serve_watcher(Arc::clone(&warden));
        Self::spawn_git_history_sync(&warden, &history_dir);
        if needs_initial_scan {
            background::spawn_initial_scan(Arc::clone(&warden));
        } else if defer_warm {
            background::spawn_cache_warm(Arc::clone(&warden));
        }
        tracing::info!(root = %root.display(), needs_initial_scan, "daemon hosting read stack for workspace");
        Ok(HostedReadStack {
            _watcher: watcher,
            shared,
        })
    }

    /// The git-history handle this session gets, if any. Fjall's directory lock is exclusive — even a
    /// read-only open takes it — so `git-history.fjall/` has exactly one holder machine-wide, and the
    /// only question here is whether that holder is us or the daemon:
    ///
    /// * **a daemon is up**: the DAEMON holds the database, so every session — a `daemon_writer`
    ///   serve, and equally the one-shot CLI, which runs these same tool bodies in-process — takes a
    ///   forwarding handle. It must not try to open the index: it cannot win the lock, so it would
    ///   burn the retry ladder on every invocation and then silently live-walk, on the exact machine
    ///   where the index is built and fresh. Serve knows a daemon is up by construction (it brings
    ///   one up); the CLI has to ask, which is one `stat` when there is no daemon and one ping when
    ///   there is (see [`crate::git_history::remote::daemon_is_up`]).
    /// * **no daemon** (a standalone process, or a non-`comms` build): this process holds the
    ///   database and builds it in-process, as before. Nobody else can, and history tools would
    ///   otherwise permanently live-walk.
    /// * **read-only fallback** (another process owns the write lock, no daemon): no handle — history
    ///   tools live-walk, visibly (`partial: true`). Unchanged.
    fn open_git_history(
        root: &std::path::Path,
        history_dir: &std::path::Path,
        has_repo: bool,
        agent_id: &str,
        options: &ServerOptions,
        // ~keep A daemon-hosted connection passes the daemon itself as the in-process history host, so
        // ~keep its reads run through the shared handle instead of looping back over the daemon's socket.
        #[cfg(all(feature = "comms", any(unix, windows)))] git_history_host: Option<
            Arc<dyn crate::git_history::remote::HistoryHost>,
        >,
    ) -> Option<Arc<crate::git_history::GitHistoryIndex>> {
        if !has_repo || !crate::git_history::index_enabled() {
            return None;
        }
        #[cfg(all(feature = "comms", any(unix, windows)))]
        if options.daemon_writer || crate::git_history::remote::daemon_is_up() {
            let agent = crate::comms::ids::AgentId::parse(agent_id.to_string())
                .inspect_err(|error| tracing::warn!(%error, "git-history: bad agent id; tools will live-walk"))
                .ok()?;
            if let Some(host) = git_history_host {
                return Some(Arc::new(crate::git_history::GitHistoryIndex::hosted(
                    root.to_path_buf(),
                    agent,
                    host,
                )));
            }
            return Some(Arc::new(crate::git_history::GitHistoryIndex::remote(
                root.to_path_buf(),
                agent,
            )));
        }
        let _ = (root, agent_id);
        if options.read_only {
            return None;
        }
        match crate::git_history::GitHistoryIndex::open(history_dir) {
            Ok(index) => Some(Arc::new(index)),
            Err(error) => {
                tracing::warn!(?error, "git-history index unavailable; tools will live-walk");
                None
            }
        }
    }

    /// Kick the git-history index up to date, off the MCP thread. A session whose handle is
    /// daemon-backed ASKS the daemon to do it (which serializes the build per repo, so N sessions
    /// cause one walk); a session that holds the database does it in-process, as it always has.
    ///
    /// Keyed off the handle the routing above actually produced, not off `daemon_writer`: a
    /// non-`daemon_writer` session that found a live daemon also holds a forwarding handle, and
    /// asking the builder to write through it is a guaranteed
    /// [`NotLocal`](crate::git_history::GitHistoryError::NotLocal).
    ///
    /// Only ever reached with `background: true` (i.e. `serve`). The one-shot CLI requests no sync at
    /// all: it exits in milliseconds, and a first build on a deep repo is a minutes-long walk.
    fn spawn_git_history_sync(state: &Arc<ServerState>, history_dir: &std::path::Path) {
        let Some(index) = state.shared.git_history.as_deref() else {
            return;
        };
        let _ = index;
        // ~keep A daemon-hosted handle syncs in-process through the seam (the daemon's own build lock),
        // ~keep never over the socket back to itself.
        #[cfg(all(feature = "comms", any(unix, windows)))]
        if let Some(host) = index.history_host() {
            let root = state.shared.root.clone();
            tokio::spawn(async move {
                match host
                    .run_history(root, crate::git_history::proto::GitHistoryOp::Sync)
                    .await
                {
                    Ok(reply) => tracing::info!(?reply, "git-history index synced in-process (hosted)"),
                    Err(error) => tracing::warn!(%error, "git-history in-process sync failed; history tools live-walk"),
                }
            });
            return;
        }
        #[cfg(all(feature = "comms", any(unix, windows)))]
        if index.is_daemon_backed() {
            let root = state.shared.root.clone();
            let agent_id = state.agent_id.clone();
            tokio::spawn(async move {
                let Ok(agent) = crate::comms::ids::AgentId::parse(agent_id) else {
                    return;
                };
                match crate::git_history::remote::request_sync(root, agent).await {
                    Some(outcome) => tracing::info!(?outcome, "git-history index synced by the daemon"),
                    None => tracing::warn!("git-history index sync unavailable; history tools live-walk"),
                }
            });
            return;
        }
        if let (Some(git_history), Some(repo)) = (state.shared.git_history.clone(), state.shared.repo.clone()) {
            let history_dir = history_dir.to_path_buf();
            tokio::task::spawn_blocking(move || {
                match crate::git_history::builder::sync(&git_history, &repo, &history_dir) {
                    Ok(outcome) => tracing::info!(?outcome, "git-history index sync complete"),
                    Err(error) => tracing::warn!(%error, "git-history index sync failed; tools live-walk"),
                }
                // Fjall 3.1 can deadlock while dropping its final database handle after a
                // background sync (fjall-rs/fjall#260). This path is only used by a standalone
                // MCP process; the daemon-backed path above owns and closes the shared database.
                // Keep this clone alive until process exit so Tokio runtime shutdown cannot hang.
                std::mem::forget(git_history);
            });
        }
    }

    /// Names of every tool this server advertises via `tools/list` (the full router, ignoring the
    /// `BASEMIND_MCP_LEAN` wrapper mode). Exposed for the `tests/cli_parity.rs` guard, which asserts
    /// each advertised tool has a CLI counterpart. The set follows the compiled feature flags.
    pub fn tool_names(&self) -> Vec<String> {
        self.tool_router
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect()
    }
}

#[cfg(test)]
#[path = "lazy_cache_tests.rs"]
mod lazy_cache_tests;

#[cfg(test)]
mod map_cache_tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn sym_names(cache: &MapCache, rel: &str) -> Vec<String> {
        let key = crate::path::RelPath::from(rel);
        cache
            .by_path
            .get(&key)
            .map(|l1| l1.symbols.iter().map(|s| s.name.clone()).collect())
            .unwrap_or_default()
    }

    /// The lifecycle precedence is BuildingIndex > WarmingUp > Rescanning > Ready — a from-scratch
    /// scan outranks a preload, which outranks a watcher refresh. Guards the ordering a read tool
    /// relies on to label a possibly-incomplete result correctly.
    #[test]
    fn lifecycle_from_flags_applies_precedence() {
        assert_eq!(Lifecycle::from_flags(false, false, false), Lifecycle::Ready);
        assert_eq!(Lifecycle::from_flags(false, false, true), Lifecycle::Rescanning);
        assert_eq!(Lifecycle::from_flags(false, true, true), Lifecycle::WarmingUp);
        assert_eq!(Lifecycle::from_flags(true, true, true), Lifecycle::BuildingIndex);
        assert_eq!(Lifecycle::from_flags(true, false, false), Lifecycle::BuildingIndex);
    }

    /// A notice is emitted for every non-ready state (with the stable machine tag and the right retry
    /// hint) and suppressed when ready, so a healthy response carries no `notice` field.
    #[test]
    fn lifecycle_notice_maps_state_to_tag_and_retry() {
        assert!(types::LifecycleNotice::for_state(Lifecycle::Ready).is_none());
        let warming = types::LifecycleNotice::for_state(Lifecycle::WarmingUp).expect("warming notice");
        assert_eq!(warming.state, "warming_up");
        assert!(warming.retry, "warming asks the caller to retry for complete results");
        let building = types::LifecycleNotice::for_state(Lifecycle::BuildingIndex).expect("building notice");
        assert_eq!(building.state, "building_index");
        assert!(building.retry);
        let rescanning = types::LifecycleNotice::for_state(Lifecycle::Rescanning).expect("rescan notice");
        assert_eq!(rescanning.state, "rescanning");
        assert!(!rescanning.retry, "rescan results are usable, no retry required");
    }

    /// `with_delta` must re-read only the changed blobs, preserve untouched entries, drop removed
    /// ones, and keep `imports_index` consistent — the incremental refresh the serve watcher uses
    /// instead of a whole-corpus rebuild (issue #33).
    #[test]
    fn with_delta_patches_updated_and_removed_paths_only() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("a.rs"), b"pub fn alpha() {}\n").unwrap();
        fs::write(root.join("b.rs"), b"pub fn beta() {}\n").unwrap();
        let cfg = crate::config::ConfigV1::with_defaults();

        let mut store = crate::store::Store::open(root, crate::store::VIEW_WORKING).unwrap();
        crate::scanner::scan(
            root,
            &mut store,
            &cfg,
            crate::scanner::ScanSource::WorkingTree,
            crate::scanner::EmbedMode::Inline,
        )
        .unwrap();

        let cache = MapCache::build(&store);
        assert_eq!(sym_names(&cache, "a.rs"), vec!["alpha".to_string()]);
        assert_eq!(sym_names(&cache, "b.rs"), vec!["beta".to_string()]);
        assert!(cache.calls.is_none() && cache.impls.is_none());

        fs::write(root.join("a.rs"), b"pub fn alpha2() {}\npub fn alpha3() {}\n").unwrap();
        let report = crate::scanner::scan_paths(
            root,
            &mut store,
            &cfg,
            &[root.join("a.rs")],
            crate::scanner::EmbedMode::Inline,
        )
        .unwrap();
        assert_eq!(report.stats.updated, 1);

        let updated = vec![crate::path::RelPath::from("a.rs")];
        let next = cache.with_delta(&store, &updated, &[]);
        assert_eq!(
            sym_names(&next, "a.rs"),
            vec!["alpha2".to_string(), "alpha3".to_string()],
            "updated path reflects fresh L1"
        );
        assert_eq!(
            sym_names(&next, "b.rs"),
            vec!["beta".to_string()],
            "untouched path preserved without re-reading its blob"
        );

        let removed = vec![crate::path::RelPath::from("b.rs")];
        let after = next.with_delta(&store, &[], &removed);
        assert!(
            !after.by_path.contains_key(&crate::path::RelPath::from("b.rs")),
            "removed path dropped from by_path"
        );
        assert!(
            after.by_path.contains_key(&crate::path::RelPath::from("a.rs")),
            "other path kept"
        );
        assert!(
            !after.imports_index.iter().any(|(p, _)| p == Path::new("b.rs")),
            "imports_index must not retain a removed path"
        );
    }
}
