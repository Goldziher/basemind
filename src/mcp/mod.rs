//! MCP server exposing the basemind code map + git context to AI agents.
//!
//! The server opens the store writably and is the canonical Fjall owner: it holds the exclusive
//! lock so the in-process `rescan` tool (and the background watcher) can refresh the index. While
//! a server is running, standalone `basemind scan` / `basemind watch` against the same repo fail
//! fast with a lock error rather than racing it. Tools return JSON so the agent can navigate by
//! file path + line numbers without opening source files.
//!
//! Transport: stdio (the canonical MCP transport). Spawn via `basemind serve`.

mod adapter;
pub(crate) mod admission;
/// Who a daemon-hosted session is to the agent tools.
#[cfg(all(feature = "comms", any(unix, windows)))]
mod agent_context;
pub mod agent_tools;
mod background;
mod cli_only;
mod completions;
pub mod lead_tools;
pub(crate) use crate::engine::cursor;
mod cursor_bind;
#[cfg(all(feature = "comms", any(unix, windows)))]
mod daemon_forward;
mod error_kind;
mod helpers;
pub(crate) use adapter::BUSY_DATA;

/// The overload refusal a CLI session answers with ([`admission::ServerBusy::into_cli_error`]).
#[cfg(all(test, feature = "comms", any(unix, windows)))]
pub(crate) fn server_busy_for_cli() -> rmcp::ErrorData {
    admission::ServerBusy.into_cli_error()
}
#[cfg(all(test, feature = "comms", feature = "code-intel-js", any(unix, windows)))]
mod code_refs_hosted_tests;
#[cfg(test)]
mod code_refs_tests;
mod helpers_telemetry;
mod history_sync;
pub(crate) mod identity;
#[cfg(feature = "test-support")]
pub mod in_memory;
#[cfg(test)]
mod index_view_tests;
pub(crate) use crate::engine::l1_cache;
#[cfg(all(test, feature = "memory"))]
mod memory_tests;
mod notifications;
pub mod offline;
mod prompts;
pub mod replay;
mod resources;
mod router_cache;
pub(crate) mod savings;
mod server_handler;
mod state;
mod strict;
mod tasks;
pub(crate) mod telemetry;
pub(crate) use crate::engine::tokens;
mod tool_contract;
mod tools;
mod tools_admin;
#[cfg(all(feature = "comms", any(unix, windows)))]
mod tools_agents;
mod tools_git;
mod tools_memory;
mod types;
mod types_tools;

use std::path::PathBuf;
use std::sync::Arc;

use rmcp::handler::server::router::prompt::PromptRouter;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::task_manager::TaskManager;

use crate::engine::workspace::BootScan;
use crate::store::Store;

pub(crate) use crate::engine::MapCache;
pub(crate) use crate::engine::Workspace;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub use agent_context::AgentContext;
#[cfg(feature = "test-support")]
pub use in_memory::{serve_in_memory, serve_in_memory_with_task_threshold};
pub(crate) use state::{ServedWorkspace, ServerState};

#[cfg(all(feature = "comms", any(unix, windows)))]
/// A daemon-owned read stack and the watcher lifetime that keeps it fresh.
pub(crate) struct HostedReadStack {
    _watcher: background::WatcherGuard,
    shared: ServedWorkspace,
}

#[cfg(all(feature = "comms", any(unix, windows)))]
impl HostedReadStack {
    /// Clone the read stack shared by every connection to this hosted workspace.
    pub(crate) fn shared(&self) -> ServedWorkspace {
        self.shared.clone()
    }

    /// Resolves once this stack's watcher thread has exited, after the stack is dropped.
    #[cfg(test)]
    pub(crate) fn watcher_exited(&self) -> background::WatcherExited {
        self._watcher.exited()
    }

    /// Serve the workspace the daemon's pool opened, and start its single "warden": a
    /// background-running [`ServerState`] that owns freshness for the whole workspace (the
    /// empty-index auto-scan, the live filesystem watcher, cache warming and the git-history sync).
    /// The warden shares the [`Workspace`] by `Arc`, so when its watcher refreshes the in-RAM map
    /// every hosted connection sees the new snapshot at once; per-connection servers
    /// ([`BasemindServer::from_shared`]) spawn nothing.
    ///
    /// `store` is read-only and borrows the pool writer's index handle per call, so reads never wait
    /// on a scan. Scans run through `pool`, which holds the writer store.
    pub(crate) fn start(
        store: Store,
        config: Arc<crate::config::Config>,
        repo: Option<Arc<crate::git::Repo>>,
        git_cache: Arc<crate::git_cache::GitCache>,
        git_history: Option<crate::comms::git_history_ops::HostedHistory>,
        pool: Arc<crate::comms::workspace_pool::WorkspacePool>,
    ) -> Self {
        let root = store.root.clone();
        let options = ServerOptions {
            background: true,
            watch: true,
            read_only: true,
            lazy_cache: false,
        };
        // ~keep The pool writes the index, so an empty one still asks for an initial scan.
        let open = crate::engine::workspace::OpenOptions {
            forwards_writes: true,
            ..options.into()
        };
        let plan = crate::engine::workspace::boot_plan(&store, &open);
        let agent_id = identity::resolve_agent_id(&store.root);
        let telemetry_dir = store.basemind_dir.clone();
        let (git_history, work_pin) =
            match git_history.filter(|_| repo.is_some() && crate::git_history::index_enabled()) {
                Some(history) => (Some(history.index), Some(history.pin)),
                None => (None, None),
            };
        let mut workspace = Workspace::new(store, root.clone(), config, repo, git_cache, git_history, open);
        workspace.work_pin = work_pin;
        let workspace = Arc::new(workspace);
        let mut shared = ServedWorkspace::new(workspace, &telemetry_dir);
        shared.pool = Some(pool);
        let warden = Arc::new(ServerState::for_connection(shared.clone(), agent_id));
        let watcher = background::spawn_serve_watcher(Arc::clone(&warden));
        history_sync::request(&warden.shared);
        match plan.scan {
            BootScan::Initial => background::spawn_initial_scan(Arc::clone(&warden)),
            BootScan::CatchUp => background::spawn_catch_up(Arc::clone(&warden), plan.defer_warm),
            BootScan::None if plan.defer_warm => background::spawn_cache_warm(Arc::clone(&warden)),
            BootScan::None => {}
        }
        tracing::info!(root = %root.display(), scan = ?plan.scan, "daemon hosting read stack for workspace");
        Self {
            _watcher: watcher,
            shared,
        }
    }
}

/// Public re-export of every tool `*Params` type, so the in-process CLI (`src/cli/`) can build
/// tool arguments and call the same `*_call` bodies the `#[tool]` shims call. This is the
/// parity-by-construction surface: the CLI runs the identical tool code an MCP client would reach.
pub mod params {
    pub use rmcp::handler::server::wrapper::Parameters;

    pub(crate) use super::strict::parse as parse_args;
    pub use super::types::{FindCommitsByPathParams, HotFilesParams, HotspotsParams, TelemetrySummaryParams};
    #[cfg(feature = "test-support")]
    pub use super::types_tools::check_arguments;
    pub use super::types_tools::{
        CodeGrepParams, CodeOutlineParams, CodeReadParams, CodeRefsParams, CodeSearchParams, CodeSymbolsParams,
        DocsSearchParams, FindFilesToolParams, GitBlameParams, GitDiffParams, GitLogParams, IndexRescanParams,
        IndexStatusParams, MemorySaveParams, MemorySearchToolParams, RETIRED_TOOLS, VERB_TOOLS,
    };
    pub use crate::engine::types::memory::{MemoryDeleteParams, Visibility};
    pub use crate::engine::types::search::GetChunkParams;
}

pub use params::Parameters;

/// Per-category LRU capacity for a daemon-hosted workspace's git cache (commit_files, log, blame).
/// Matches the `basemind serve --git-cache-mem` default (1024): the daemon holds one git cache per
/// hot workspace, shared across all connections, so the same budget an in-process serve used per
/// process now serves every client of that workspace.
#[cfg(all(feature = "comms", any(unix, windows)))]
pub(crate) const HOSTED_GIT_CACHE_MEM: usize = 1024;

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
    /// SEP-2663 Tasks extension executor. Slow tools (see [`tasks::SLOW_CALLS`]) offload their work
    /// here for task-capable clients; cheaply cloneable and shared across every clone of the server.
    tasks: TaskManager,
    /// Bounds allocation-heavy calls while allowing control/comms traffic to bypass the queue.
    heavy_admission: Arc<admission::HeavyAdmission>,
    /// A `basemind` CLI command's relay session ([`for_cli`](Self::for_cli)).
    cli: bool,
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
    /// `Workspace::rescan` so the in-RAM map stays current as the agent edits.
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
    /// When true, defer [`MapCache::build`] to the first tool that actually reads the map, instead
    /// of running it at construction. Set only for the one-shot CLI — see
    /// [`ServerState::lazy_cache`].
    pub lazy_cache: bool,
}

impl From<ServerOptions> for crate::engine::workspace::OpenOptions {
    fn from(options: ServerOptions) -> Self {
        Self {
            background: options.background,
            read_only: options.read_only,
            forwards_writes: false,
            lazy_cache: options.lazy_cache,
            history_sync: Some(history_sync::request),
            #[cfg(feature = "memory")]
            memory_owner: identity::resolve_agent_id,
        }
    }
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            background: true,
            watch: true,
            read_only: false,
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
        let agent_id = identity::resolve_agent_id(&store.root);
        let history_dir = crate::git_history::shared_history_basemind_dir(&root);
        let git_history = Self::open_git_history(&root, &history_dir, repo.is_some(), &agent_id, &options);
        // ~keep Boot decisions depend on the opened store, so compute them before it moves into the stack.
        let plan = crate::engine::workspace::boot_plan(&store, &options.into());
        let basemind_dir = store.basemind_dir.clone();
        let shared = Arc::new(Workspace::new(
            store,
            root,
            config,
            repo,
            git_cache,
            git_history,
            options.into(),
        ));
        let served = ServedWorkspace::new(shared, &basemind_dir);
        let state = Arc::new(ServerState::for_connection(served, agent_id));
        let mut watcher = None;
        if options.background {
            let view_is_working = {
                match state.shared.try_store_read() {
                    Ok(g) => g.view == crate::store::VIEW_WORKING,
                    Err(_) => false,
                }
            };
            let watcher_guard = if options.watch && !options.read_only && view_is_working {
                background::spawn_serve_watcher(Arc::clone(&state))
            } else {
                background::spawn_view_watcher(Arc::clone(&state))
            };
            watcher = Some(Arc::new(watcher_guard));
            history_sync::request(&state.shared);
            if plan.scan == BootScan::Initial {
                background::spawn_initial_scan(Arc::clone(&state));
            } else {
                if plan.defer_warm {
                    background::spawn_cache_warm(Arc::clone(&state));
                }
                background::spawn_background_gc(Arc::clone(&state.shared));
            }
        }
        Self {
            state,
            _watcher: watcher,
            tool_router: router_cache::cached_tool_router(),
            prompt_router: router_cache::cached_prompt_router(),
            tasks: TaskManager::new(),
            heavy_admission: Arc::new(admission::HeavyAdmission::default()),
            cli: false,
        }
    }

    /// How long an offload-eligible call may run before it is answered with a task handle.
    pub(crate) fn task_threshold(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.state.task_threshold_ms.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// Assemble the full tool router (every feature-gated tool group). Pure and argument-free —
    /// its output is invariant for the life of the process, so [`new_with_options`]
    /// (Self::new_with_options) and [`from_shared`](Self::from_shared) never call this directly;
    /// they go through [`router_cache::cached_tool_router`], which builds it once behind a
    /// `OnceLock` and hands back a clone. This is the one-time builder the cache calls into.
    fn assemble_router() -> ToolRouter<Self> {
        #[allow(unused_mut)] // ~keep only a `comms` build adds the agent tools
        let mut router =
            Self::tool_router_core() + Self::tool_router_git() + Self::tool_router_memory() + Self::tool_router_admin();
        #[cfg(all(feature = "comms", any(unix, windows)))]
        {
            router += Self::tool_router_agents();
        }
        tool_contract::finalize_router(router)
    }

    /// Build a per-connection server that SHARES a workspace's [`Workspace`] rather than
    /// opening its own. This is the daemon-hosted seam: the daemon builds one shared read stack per
    /// hot workspace (see [`HostedReadStack::start`]) and hands every
    /// accepted relay connection its own `BasemindServer` over that one stack — so the heavy state
    /// (in-RAM `MapCache`, LanceDB, ONNX, git caches) is resident once per workspace, not once per
    /// client. Spawns NO background facilities: freshness is owned by the workspace's single warden
    /// (built alongside the shared stack), never per connection. `agent_id` comes from the
    /// connection's [`RelayHello`](crate::comms::relay::RelayHello), so each client keeps its own
    /// memory-owner identity even while sharing the read stack. `agents` is the session's identity
    /// to the agent tools; a server without one (the HTTP front-end, ADR-0021 rule 17) neither
    /// advertises nor serves them.
    #[cfg(all(feature = "comms", any(unix, windows)))]
    pub(crate) fn from_shared(
        shared: ServedWorkspace,
        agent_id: String,
        heavy_admission: Arc<admission::HeavyAdmission>,
        agents: Option<Arc<AgentContext>>,
    ) -> Self {
        let mut tool_router = router_cache::cached_tool_router();
        if agents.is_none() {
            for name in agent_tools::AGENT_TOOLS {
                tool_router.remove_route(name);
            }
        }
        let mut state = ServerState::for_connection(shared, agent_id);
        state.agents = agents;
        let state = Arc::new(state);
        Self {
            state,
            _watcher: None,
            tool_router,
            prompt_router: router_cache::cached_prompt_router(),
            tasks: TaskManager::new(),
            heavy_admission,
            cli: false,
        }
    }

    /// This session forwards a `basemind` CLI command: a failed call is answered as a JSON-RPC error
    /// carrying the code the CLI turns into its exit status, and the CLI-only commands
    /// ([`cli_only::CLI_ONLY`]) are answered too, off `tools/list`. Only a CLI's hello on the
    /// daemon's local socket or pipe asks for it.
    #[cfg(all(feature = "comms", any(unix, windows)))]
    pub(crate) fn for_cli(mut self) -> Self {
        self.cli = true;
        self
    }

    /// The git-history handle this session gets, if any. Fjall's directory lock is exclusive — even a
    /// read-only open takes it — so `git-history.fjall/` has exactly one holder machine-wide, and the
    /// only question here is whether that holder is us or the daemon:
    ///
    /// * **a daemon is up**: the DAEMON holds the database. A session it hosts reads it in process
    ///   through the daemon's handle. Any other session (a CLI command on a view the daemon does not
    ///   host) gets no handle and live-walks: it cannot win the lock, and trying would burn the retry
    ///   ladder on every invocation. The CLI asks whether a daemon is up, which is one `stat` when
    ///   there is none (see [`crate::comms::singleton::daemon_is_up`]).
    /// * **no daemon** (a standalone process, or a non-`comms` build): this process holds the
    ///   database and builds it in-process, as before. Nobody else can, and history tools would
    ///   otherwise permanently live-walk.
    /// * **read-only fallback** (another process owns the write lock, no daemon): no handle — history
    ///   tools live-walk, visibly (`partial: true`). A one-shot CLI command beside such a process
    ///   does not try the database at all, and tries a free one once, never the retry ladder.
    fn open_git_history(
        root: &std::path::Path,
        history_dir: &std::path::Path,
        has_repo: bool,
        agent_id: &str,
        options: &ServerOptions,
    ) -> Option<Arc<crate::git_history::GitHistoryIndex>> {
        if !has_repo || !crate::git_history::index_enabled() {
            return None;
        }
        #[cfg(all(feature = "comms", any(unix, windows)))]
        if crate::comms::singleton::daemon_is_up() {
            // ~keep The daemon holds the database and fjall's lock is exclusive even to a reader
            // ~keep (fjall `locked_file.rs` `try_lock`), so a session it does not host live-walks.
            return None;
        }
        let _ = (root, agent_id);
        // ~keep A one-shot query never syncs, so an index it would open empty would stay empty.
        if options.read_only || (!options.background && !crate::git_history::GitHistoryIndex::exists(history_dir)) {
            return None;
        }
        // ~keep A one-shot query holds no writer lock, so a held index is another process's for as
        // ~keep long as it runs: it live-walks at once instead of climbing the retry ladder (~1.5 s).
        let opened = if options.background {
            crate::git_history::GitHistoryIndex::open(history_dir)
        } else if crate::store_lock::writer_lock_is_held(history_dir) {
            return None;
        } else {
            crate::git_history::GitHistoryIndex::open_once(history_dir)
        };
        match opened {
            Ok(index) => Some(Arc::new(index)),
            Err(error) => {
                tracing::warn!(?error, "git-history index unavailable; tools will live-walk");
                None
            }
        }
    }

    /// Names of every tool this server advertises via `tools/list`. Exposed for the
    /// `tests/cli_parity/` guard, which asserts each advertised tool has a CLI counterpart. The set
    /// follows the compiled feature flags.
    pub fn tool_names(&self) -> Vec<String> {
        self.tool_router
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect()
    }

    /// Every advertised tool paired with its JSON input schema (the `inputSchema` of `tools/list`).
    /// Exposed for the `tests/cli_parity/` parameter-parity layer.
    pub fn tool_input_schemas(&self) -> Vec<(String, serde_json::Value)> {
        self.tool_router
            .list_all()
            .into_iter()
            .map(|tool| {
                (
                    tool.name.to_string(),
                    serde_json::Value::Object((*tool.input_schema).clone()),
                )
            })
            .collect()
    }
}

#[cfg(test)]
#[path = "lazy_cache_tests.rs"]
mod lazy_cache_tests;

#[cfg(test)]
mod tool_contract_tests;

#[cfg(test)]
mod map_cache_tests {
    use super::*;
    use crate::engine::Lifecycle;
    use std::fs;

    fn sym_names(cache: &MapCache, rel: &str) -> Vec<String> {
        let key = crate::path::RelPath::from(rel);
        cache
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
        assert!(crate::engine::types::LifecycleNotice::for_state(Lifecycle::Ready).is_none());
        let warming = crate::engine::types::LifecycleNotice::for_state(Lifecycle::WarmingUp).expect("warming notice");
        assert_eq!(warming.state, "warming_up");
        assert!(warming.retry, "warming asks the caller to retry for complete results");
        let building =
            crate::engine::types::LifecycleNotice::for_state(Lifecycle::BuildingIndex).expect("building notice");
        assert_eq!(building.state, "building_index");
        assert!(building.retry);
        let rescanning =
            crate::engine::types::LifecycleNotice::for_state(Lifecycle::Rescanning).expect("rescan notice");
        assert_eq!(rescanning.state, "rescanning");
        assert!(!rescanning.retry, "rescan results are usable, no retry required");
    }

    /// A degraded `status.json` (the daemon's mark on a workspace whose index it is rebuilding) puts
    /// the "index rebuilding" notice on reads; a clean sidecar or none at all does not.
    #[test]
    fn degraded_scan_status_yields_the_index_rebuilding_notice() {
        use crate::store::{ScanHealth, StatusSidecar};
        let dir = tempfile::tempdir().expect("tempdir");
        crate::store::write_status_sidecar(dir.path(), 5, 5, &ScanHealth::partial("index rebuilding", 0, 0));
        let degraded = crate::store::read_status_sidecar(dir.path());
        let notice =
            crate::engine::types::LifecycleNotice::for_scan_status(degraded.as_ref()).expect("degraded notice");
        assert_eq!(notice.state, "index_rebuilding");
        assert!(notice.retry && notice.message.contains("index rebuilding"));

        crate::store::write_status_sidecar(dir.path(), 5, 5, &ScanHealth::complete());
        let clean: Option<StatusSidecar> = crate::store::read_status_sidecar(dir.path());
        assert!(crate::engine::types::LifecycleNotice::for_scan_status(clean.as_ref()).is_none());
        assert!(crate::engine::types::LifecycleNotice::for_scan_status(None).is_none());
    }

    /// `with_delta` must re-read only the changed blobs, preserve untouched entries, and drop
    /// removed ones — the incremental refresh the serve watcher uses instead of a whole-corpus
    /// rebuild (issue #33).
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
            crate::retrieval_wiring::backend(),
        )
        .unwrap();

        let cache = MapCache::build(&store, 0);
        assert_eq!(sym_names(&cache, "a.rs"), vec!["alpha".to_string()]);
        assert_eq!(sym_names(&cache, "b.rs"), vec!["beta".to_string()]);

        fs::write(root.join("a.rs"), b"pub fn alpha2() {}\npub fn alpha3() {}\n").unwrap();
        let report = crate::scanner::scan_paths(
            root,
            &mut store,
            &cfg,
            &[root.join("a.rs")],
            crate::scanner::EmbedMode::Inline,
            crate::retrieval_wiring::backend(),
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
            !after.contains(&crate::path::RelPath::from("b.rs")),
            "removed path dropped from the file view"
        );
        assert!(after.contains(&crate::path::RelPath::from("a.rs")), "other path kept");
        assert!(
            sym_names(&after, "b.rs").is_empty(),
            "a removed path resolves to no outline"
        );
    }

    /// An extract-epoch refresh rewrites a file's blob under its unchanged content hash. The L1
    /// cache `with_delta` carries forward still holds the outline decoded from the replaced blob,
    /// and the derived snapshot must not serve it.
    #[test]
    fn with_delta_serves_an_outline_refreshed_under_the_same_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // Content no other test scans: the blob store is shared, and this test rewrites the blob.
        fs::write(root.join("a.rs"), b"pub fn epoch_refresh_probe() {}\n").unwrap();
        let cfg = crate::config::ConfigV1::with_defaults();
        let mut store = crate::store::Store::open(root, crate::store::VIEW_WORKING).unwrap();
        crate::scanner::scan(
            root,
            &mut store,
            &cfg,
            crate::scanner::ScanSource::WorkingTree,
            crate::scanner::EmbedMode::Inline,
            crate::retrieval_wiring::backend(),
        )
        .unwrap();

        let rel = crate::path::RelPath::from("a.rs");
        let hash_hex = store.lookup(&rel).unwrap().hash_hex.clone();
        let current = store.read_l1_by_hex(&hash_hex).unwrap().unwrap();
        let mut stale = current.clone();
        stale.extract_epoch = crate::extract::EXTRACT_EPOCH - 1;
        stale.symbols[0].name = "before_refresh".to_string();
        store.overwrite_filemap_hex(&hash_hex, &stale, None).unwrap();
        store.index.files.get_mut(&rel).unwrap().extract_epoch = stale.extract_epoch;

        let cache = MapCache::build(&store, 0);
        assert_eq!(sym_names(&cache, "a.rs"), vec!["before_refresh".to_string()]);

        store.overwrite_filemap_hex(&hash_hex, &current, None).unwrap();
        store.index.files.get_mut(&rel).unwrap().extract_epoch = crate::extract::EXTRACT_EPOCH;
        let next = cache.with_delta(&store, std::slice::from_ref(&rel), &[]);
        assert_eq!(
            sym_names(&next, "a.rs"),
            vec!["epoch_refresh_probe".to_string()],
            "the refreshed blob is read again"
        );
    }
}

#[cfg(all(test, feature = "documents"))]
#[path = "find_files_tests.rs"]
mod find_files_tests;
