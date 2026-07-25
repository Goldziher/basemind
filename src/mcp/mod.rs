//! MCP server exposing the basemind code map + git context to AI agents.
//!
//! The server opens the store writably and is the canonical Fjall owner: it holds the exclusive
//! lock so the in-process `rescan` tool (and the background watcher) can refresh the index. While
//! a server is running, standalone `basemind scan` / `basemind watch` against the same repo fail
//! fast with a lock error rather than racing it. Tools return JSON so the agent can navigate by
//! file path + line numbers without opening source files.
//!
//! Transport: stdio (the canonical MCP transport). Spawn via `basemind serve`.

mod background;
mod budget;
mod completions;
pub(crate) mod cursor;
#[cfg(all(feature = "comms", any(unix, windows)))]
mod daemon_forward;
mod helpers;
mod helpers_admin;
mod helpers_archmap;
mod helpers_calls;
mod helpers_calls_scan;
#[cfg(feature = "code-search")]
mod helpers_code;
#[cfg(all(feature = "comms", any(unix, windows)))]
mod helpers_comms;
mod helpers_compress;
#[cfg(feature = "documents")]
mod helpers_documents;
mod helpers_files;
mod helpers_fingerprint;
mod helpers_git;
#[cfg(feature = "memory")]
mod helpers_governance;
mod helpers_graph;
mod helpers_grep;
mod helpers_impls;
mod helpers_intel;
#[cfg(feature = "memory")]
mod helpers_proposals;
#[cfg(all(feature = "comms", any(unix, windows)))]
mod helpers_registry;
#[cfg(all(feature = "shells", any(unix, windows)))]
mod helpers_shells;
mod helpers_telemetry;
#[cfg(feature = "crawl")]
mod helpers_web;
mod identity;
mod kneedle;
mod lean;
mod lenient;
mod map_fingerprint;
#[cfg(any(feature = "memory", feature = "documents", feature = "code-search"))]
mod memory;
#[cfg(feature = "memory")]
pub(crate) mod memory_ops;
mod notifications;
mod prompts;
#[cfg(feature = "memory")]
pub(crate) mod proposals_ops;
mod savings;
mod server_handler;
mod shared_state;
mod state;
mod telemetry;
mod tokens;
mod tools;
mod tools_admin;
mod tools_archmap;
mod tools_code;
#[cfg(all(feature = "comms", any(unix, windows)))]
mod tools_comms;
mod tools_compress;
mod tools_git;
mod tools_governance;
mod tools_memory;
#[cfg(all(feature = "comms", any(unix, windows)))]
mod tools_registry;
#[cfg(all(feature = "shells", any(unix, windows)))]
mod tools_shells;
#[cfg(feature = "crawl")]
mod tools_web;
mod toon;
mod types;
mod types_admin;
mod types_archmap;
mod types_code;
#[cfg(all(feature = "comms", any(unix, windows)))]
mod types_comms;
mod types_compress;
mod types_documents;
mod types_git;
pub(crate) mod types_governance;
mod types_graph;
mod types_impls;
pub(crate) mod types_memory;
#[cfg(all(feature = "comms", any(unix, windows)))]
mod types_registry;
#[cfg(all(feature = "shells", any(unix, windows)))]
mod types_shells;
#[cfg(feature = "crawl")]
mod types_web;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use lru::LruCache;
use rmcp::handler::server::router::prompt::PromptRouter;
use rmcp::handler::server::tool::ToolRouter;

use crate::extract::FileMapL1;
use crate::lang::LangId;
use crate::store::Store;

pub(crate) use shared_state::SharedReadStack;
pub(crate) use state::{Lifecycle, MapCache, ServerState};

/// Public re-export of every tool `*Params` type plus the `Parameters` wrapper, so the
/// in-process CLI (`src/cli/`) can build tool arguments and call the `#[tool]` methods
/// directly. This is the parity-by-construction surface: the CLI runs the identical tool
/// code an MCP client would dispatch.
pub mod params {
    pub use rmcp::handler::server::wrapper::Parameters;

    pub(crate) use super::lenient::Lenient;

    pub use super::types::{
        BlameFileParams, BlameSymbolParams, CommitsTouchingParams, DependentsParams, DiffFileParams, DiffOutlineParams,
        FindCallersParams, FindCommitsByPathParams, FindFilesParams, FindReferencesParams, GotoDefinitionParams,
        HotFilesParams, ListFilesParams, OutlineParams, RecentChangesParams, RepoInfoParams, RescanParams,
        SearchDocumentsParams, SearchGitHistoryParams, SearchSymbolsParams, StatusParams, SymbolHistoryParams,
        TelemetrySummaryParams, WorkingTreeStatusParams, WorkspaceGrepParams,
    };
    #[cfg(feature = "crawl")]
    pub use super::types::{WebCrawlParams, WebMapParams, WebScrapeParams};
    pub use super::types_admin::{CacheClearParams, CacheGcParams, CacheStatsParams};
    pub use super::types_archmap::ArchitectureMapParams;
    pub use super::types_code::{GetChunkParams, SearchCodeParams};
    pub use super::types_compress::ExpandParams;
    pub use super::types_governance::{
        MemoryAuditParams, ProposalAcceptParams, ProposalRejectParams, ProposalsListParams, ProposalsMineParams,
    };
    pub use super::types_graph::CallGraphParams;
    pub use super::types_impls::FindImplementationsParams;
    pub use super::types_memory::{
        MemoryDeleteParams, MemoryGetParams, MemoryListParams, MemoryPutParams, MemorySearchParams, Visibility,
    };
    #[cfg(all(feature = "shells", any(unix, windows)))]
    pub use super::types_shells::{
        ShellBroadcastParams, ShellCaptureParams, ShellEnv, ShellKillParams, ShellListParams, ShellSendParams,
        ShellSpawnParams,
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
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
    /// Reusable prompt templates (`prompts/list` + `prompts/get`). Built by the
    /// `#[prompt_router]` macro in [`prompts`]; `list_prompts` / `get_prompt` delegate here.
    prompt_router: PromptRouter<Self>,
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
        let git_history = Self::open_git_history(&root, &history_dir, repo.is_some(), &agent_id, &options);
        // Boot decisions depend on the opened store, so compute them before it moves into the stack.
        let (needs_initial_scan, defer_warm) = shared_state::boot_plan(&store, &options);
        let shared = Arc::new(SharedReadStack::new(
            store,
            root,
            config,
            repo,
            git_cache,
            git_history,
            options,
        ));
        let state = Arc::new(ServerState {
            shared,
            agent_id,
            #[cfg(all(feature = "comms", any(unix, windows)))]
            comms_clients: tokio::sync::Mutex::new(ahash::AHashMap::new()),
            log_level: std::sync::atomic::AtomicU8::new(notifications::DEFAULT_LOG_ORDINAL),
        });
        if options.background {
            let view_is_working = {
                match state.shared.store.try_read() {
                    Ok(g) => g.view == crate::store::VIEW_WORKING,
                    Err(_) => false,
                }
            };
            if options.watch && (options.daemon_writer || !options.read_only) && view_is_working {
                background::spawn_serve_watcher(Arc::clone(&state));
            } else {
                background::spawn_view_watcher(Arc::clone(&state));
            }
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
        #[allow(unused_mut)]
        let mut router = Self::tool_router_core()
            + Self::tool_router_archmap()
            + Self::tool_router_git()
            + Self::tool_router_memory()
            + Self::tool_router_code()
            + Self::tool_router_governance()
            + Self::tool_router_admin()
            + Self::tool_router_compress();
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
        Self {
            state,
            tool_router: router,
            prompt_router: Self::prompt_router(),
        }
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
    ) -> Option<Arc<crate::git_history::GitHistoryIndex>> {
        if !has_repo || !crate::git_history::index_enabled() {
            return None;
        }
        #[cfg(all(feature = "comms", any(unix, windows)))]
        if options.daemon_writer || crate::git_history::remote::daemon_is_up() {
            let agent = crate::comms::ids::AgentId::parse(agent_id.to_string())
                .inspect_err(|error| tracing::warn!(%error, "git-history: bad agent id; tools will live-walk"))
                .ok()?;
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
