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
#[cfg(feature = "code-search")]
mod helpers_code;
#[cfg(all(feature = "comms", any(unix, windows)))]
mod helpers_comms;
mod helpers_compress;
#[cfg(feature = "documents")]
mod helpers_documents;
mod helpers_files;
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
#[cfg(any(feature = "memory", feature = "documents", feature = "code-search"))]
mod memory;
#[cfg(feature = "memory")]
pub(crate) mod memory_ops;
mod notifications;
mod prompts;
mod savings;
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
mod types_governance;
mod types_graph;
mod types_impls;
mod types_memory;
#[cfg(all(feature = "comms", any(unix, windows)))]
mod types_registry;
#[cfg(all(feature = "shells", any(unix, windows)))]
mod types_shells;

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use lru::LruCache;
use rmcp::ServerHandler;
use rmcp::handler::server::router::prompt::PromptRouter;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::model::{
    CompleteRequestParams, CompleteResult, GetPromptRequestParams, GetPromptResult, ListPromptsResult,
    PaginatedRequestParams, ServerCapabilities, ServerInfo,
};
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
    /// Precomputed git-history index (posting lists `path → [commit]`). `Some` only on a writable
    /// serve in a git repo with the index enabled; a read-only serve or a Fjall-lock collision
    /// leaves it `None`, and the git tools fall back to the live walk. Used by the history tools
    /// only when `last_indexed_head == HEAD` (the freshness gate), so it never serves stale results.
    pub(crate) git_history: Option<Arc<crate::git_history::GitHistoryIndex>>,
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
    /// [`super::savings::estimate_from_text`].
    pub(crate) corpus_bytes: std::sync::atomic::AtomicU64,
    /// Monotonic counter bumped every time `cache` is swapped (boot, rescan, view watcher).
    /// In-memory pagination cursors embed this value as a snapshot id so a resume call
    /// against a stale generation can be detected and reported back as
    /// `cursor_invalidated = true`.
    pub(crate) cache_generation: std::sync::atomic::AtomicU32,
    /// Per-repo scope key for LanceDB tables and `memory_by_key` Fjall keyspace.
    /// Computed once at boot. Do NOT recompute per-call.
    #[allow(dead_code)]
    pub(crate) scope: String,
    /// Owner segment for the individual-memory tier. Resolved once at boot from
    /// `BASEMIND_AGENT_ID` (validated through [`crate::comms::ids::AgentId`] so it is
    /// NUL-free) or `"anon"` when unset/invalid. Group-tier writes ignore it.
    #[allow(dead_code)]
    pub(crate) agent_id: String,
    /// LanceDB vector store. Lazy-init on first memory/document/code-search call.
    #[cfg(any(feature = "memory", feature = "documents", feature = "code-search"))]
    pub(crate) lance: tokio::sync::OnceCell<Arc<crate::lance::LanceStore>>,
    /// Shared embedding engine. Lazy-init on first embed call.
    #[cfg(feature = "intelligence")]
    pub(crate) embedder: tokio::sync::OnceCell<Arc<crate::embeddings::SharedEmbedder>>,
    /// Shared crawlberg engine. Initialised at server boot from the `[crawl]`
    /// config section; `None` if engine construction failed (the web_* tools
    /// will return an MCP error rather than crash).
    #[cfg(feature = "crawl")]
    pub(crate) crawl_engine: Option<crawlberg::CrawlEngineHandle>,
    /// Per-identity registry of lazily-connected comms-broker clients, keyed by `AgentId`. The
    /// server's own identity (`agent_id`) connects directly; a sub-identity (driven via a tool's
    /// `as_agent` param) gets its own broker connection, so one `serve` process can act as many
    /// named agents. Entries are created on first use; a connect failure surfaces as an MCP error
    /// on the triggering call, never at server boot.
    #[cfg(all(feature = "comms", any(unix, windows)))]
    pub(crate) comms_clients: tokio::sync::Mutex<
        ahash::AHashMap<
            crate::comms::ids::AgentId,
            std::sync::Arc<tokio::sync::Mutex<crate::comms::client::CommsClient>>,
        >,
    >,
    /// Embedded rmux-backed headless shell runtime. Lazily connects to (or
    /// starts) the embedded daemon on the first `shell_*` tool call; cheap to
    /// hold otherwise (no daemon spawn until first use).
    #[cfg(all(feature = "shells", any(unix, windows)))]
    pub(crate) shell_runtime: crate::shells::ShellRuntime,
    /// Minimum logging severity the client asked for via `logging/setLevel`, as an ordinal
    /// (see [`notifications::level_ordinal`]). Defaults to `Info`. Checked before every log emit so
    /// the server honors the client's verbosity preference.
    pub(crate) log_level: std::sync::atomic::AtomicU8,
    /// True while the boot-time initial scan (auto-scan of an empty index) is running. Lets a
    /// client polling `status` distinguish "index still building" from "index empty / no matches"
    /// so the build cost is not silently folded into the first query's latency.
    pub(crate) initial_scan_active: std::sync::atomic::AtomicBool,
    /// Wall-clock duration of the boot-time initial scan, in milliseconds, once it completes
    /// (`0` = no initial scan happened this session, or it is still running). Surfaced on `status`
    /// as `index_build_ms` to report indexing time separately from query time.
    pub(crate) initial_scan_ms: std::sync::atomic::AtomicU64,
    /// True while the boot-time in-RAM code-map preload (`MapCache::build` over the existing blobs)
    /// is still running. Deferring that build off the startup path is what lets `serve` answer the
    /// MCP `initialize`/`tools/list` handshake immediately instead of blocking on a rayon `par_iter`
    /// that can be starved for minutes by other sessions' scans. Cache-reading tools await
    /// [`cache_ready`](Self::cache_ready) while this is set (see [`ServerState::await_cache_ready`]).
    pub(crate) cache_warming: std::sync::atomic::AtomicBool,
    /// Wall-clock duration of the boot-time cache preload, in milliseconds, once it completes
    /// (`0` = still warming or no deferred preload this session). Surfaced on `status` as `warm_ms`.
    pub(crate) cache_warm_ms: std::sync::atomic::AtomicU64,
    /// Fired once when the deferred preload finishes and the full map is swapped in. Tools that read
    /// the cache `notified().await` on this (bounded by [`CACHE_WARM_WAIT_CAP`]) so a query issued
    /// during the warmup window returns COMPLETE data rather than an empty snapshot.
    pub(crate) cache_ready: tokio::sync::Notify,
    /// True while a watcher-driven incremental rescan (`scan_and_refresh` from the active filesystem
    /// watcher) is in flight. Surfaced as the `Rescanning` lifecycle so a client sees "results may be
    /// a moment stale" rather than treating a mid-rescan snapshot as final.
    pub(crate) rescan_active: std::sync::atomic::AtomicBool,
    /// True when this serve fell back to a read-only store because another serve owns the
    /// write lock for this repo (issue #27). The single in-process writer (`scan_and_refresh`,
    /// behind the `rescan` tool) checks this and returns a clean error rather than writing
    /// without the lock.
    pub(crate) read_only: bool,
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

impl ServerState {
    /// Current [`Lifecycle`] derived from the boot/rescan atomics, applying the documented precedence.
    pub(crate) fn lifecycle(&self) -> Lifecycle {
        use std::sync::atomic::Ordering::Relaxed;
        Lifecycle::from_flags(
            self.initial_scan_active.load(Relaxed),
            self.cache_warming.load(Relaxed),
            self.rescan_active.load(Relaxed),
        )
    }

    /// Block a cache-reading tool until the deferred boot preload has published the full map, bounded by
    /// [`CACHE_WARM_WAIT_CAP`]. No-op once warm (the common path). Does NOT wait on
    /// [`Lifecycle::BuildingIndex`] — a from-scratch scan can run for minutes, so those tools return the
    /// partial index plus a [`lifecycle_notice`](Self::lifecycle_notice) telling the client to poll.
    pub(crate) async fn await_cache_ready(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        if !self.cache_warming.load(Relaxed) {
            return;
        }
        let notified = self.cache_ready.notified();
        if !self.cache_warming.load(Relaxed) {
            return;
        }
        let _ = tokio::time::timeout(CACHE_WARM_WAIT_CAP, notified).await;
    }

    /// A [`LifecycleNotice`](types::LifecycleNotice) to attach to a tool response, or `None` when
    /// [`Ready`](Lifecycle::Ready). Lets every read tool label a possibly-incomplete result with the
    /// server state + an actionable message, so an agent knows to retry rather than concluding "empty".
    pub(crate) fn lifecycle_notice(&self) -> Option<types::LifecycleNotice> {
        types::LifecycleNotice::for_state(self.lifecycle())
    }
}

pub(crate) struct MapCache {
    /// path → L1 (kept sorted by path; iteration order matches `list_files`)
    pub(crate) by_path: BTreeMap<crate::path::RelPath, FileMapL1>,
    /// Pre-flattened `(path, imports)` view used by the `dependents` tool. Without this,
    /// every `dependents` call rebuilds the same `HashMap<PathBuf, Vec<Import>>` from
    /// scratch. Precomputing once at server boot drops that to pure pointer-chase.
    pub(crate) imports_index: Vec<(PathBuf, Vec<Import>)>,
    /// In-RAM callee index, populated ONLY when the Fjall index is unavailable —
    /// i.e. a read-only `serve` session that lost the single-holder lock to another
    /// process. Lets `find_references` / `find_callers` / `call_graph` answer from
    /// the shared L2 blobs so multiple sessions can use one repo at once. `None` on
    /// a writer session, which uses the live Fjall index (no extra RAM/build cost).
    pub(crate) calls: Option<helpers_calls::InRamCallIndex>,
    /// In-RAM trait→impl index, same read-only-only gating as `calls`. Backs
    /// `find_implementations` from the L1 blobs when Fjall is held elsewhere.
    pub(crate) impls: Option<helpers_impls::InRamImplIndex>,
}

impl MapCache {
    fn build(store: &Store) -> Self {
        use rayon::prelude::*;

        let by_path: BTreeMap<crate::path::RelPath, FileMapL1> = store
            .index
            .files
            .par_iter()
            .filter_map(|(path, entry)| {
                store
                    .read_l1_by_hex(&entry.hash_hex)
                    .ok()
                    .flatten()
                    .map(|l1| (path.clone(), l1))
            })
            .collect();
        let imports_index: Vec<(PathBuf, Vec<Import>)> = by_path
            .par_iter()
            .map(|(p, l1)| (p.to_path_buf(), l1.imports.clone()))
            .collect();
        let (calls, impls) = if store.index_db.is_none() {
            (
                Some(helpers_calls::InRamCallIndex::build(store)),
                Some(helpers_impls::InRamImplIndex::build(&by_path)),
            )
        } else {
            (None, None)
        };
        Self {
            by_path,
            imports_index,
            calls,
            impls,
        }
    }

    /// An empty map cache: the placeholder a `serve` boots with while the real [`build`](Self::build)
    /// runs in the background (see [`background::spawn_cache_warm`]). Deferring the whole-corpus blob
    /// load off the startup path is what lets the MCP `initialize`/`tools/list` handshake answer
    /// immediately instead of blocking on a rayon `par_iter` that a loaded machine can starve for
    /// minutes. Cache-reading tools await [`ServerState::cache_ready`] before reading, so they observe
    /// the fully-built map, never this placeholder.
    fn empty() -> Self {
        Self {
            by_path: BTreeMap::new(),
            imports_index: Vec::new(),
            calls: None,
            impls: None,
        }
    }

    /// Incrementally derive a fresh cache from `self` for a **scoped** (watcher) rescan: clone the
    /// existing maps and patch only the changed entries — re-read L1 from disk for `updated` paths,
    /// drop `removed` paths. This avoids `build`'s whole-corpus blob I/O (read + msgpack-decode of
    /// every L1, the dominant cost) on every debounced batch, which is what pegged multi-core CPU on
    /// gitignored / nested-`.basemind` churn (issue #33). `imports_index` is rebuilt from the
    /// patched in-RAM `by_path` — a pure clone pass, no I/O.
    ///
    /// Only valid on a writer session, where `calls`/`impls` are `None` (a read-only fallback
    /// session serves those from the blobs and never reaches the rescan path — `scan_and_refresh`
    /// early-returns on `state.read_only`). If they are somehow present, fall back to a full rebuild
    /// rather than let the in-RAM call/impl indexes drift out of sync.
    fn with_delta(&self, store: &Store, updated: &[crate::path::RelPath], removed: &[crate::path::RelPath]) -> Self {
        use rayon::prelude::*;
        if self.calls.is_some() || self.impls.is_some() {
            return Self::build(store);
        }
        let mut by_path = self.by_path.clone();
        for p in removed {
            by_path.remove(p);
        }
        for p in updated {
            match store.index.files.get(p) {
                Some(entry) => {
                    if let Ok(Some(l1)) = store.read_l1_by_hex(&entry.hash_hex) {
                        by_path.insert(p.clone(), l1);
                    }
                }
                None => {
                    by_path.remove(p);
                }
            }
        }
        let imports_index: Vec<(PathBuf, Vec<Import>)> = by_path
            .par_iter()
            .map(|(p, l1)| (p.to_path_buf(), l1.imports.clone()))
            .collect();
        Self {
            by_path,
            imports_index,
            calls: None,
            impls: None,
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
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            background: true,
            watch: true,
            read_only: false,
            daemon_writer: false,
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
                read_only: false,
                daemon_writer: false,
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
        let basemind_dir = store.basemind_dir.clone();
        let git_history = if !options.read_only && repo.is_some() && crate::git_history::index_enabled() {
            match crate::git_history::GitHistoryIndex::open(&basemind_dir) {
                Ok(idx) => Some(Arc::new(idx)),
                Err(error) => {
                    tracing::warn!(?error, "git-history index unavailable; tools will live-walk");
                    None
                }
            }
        } else {
            None
        };
        let agent_id = identity::resolve_agent_id(&config, &store);
        let corpus_bytes: u64 = store.index.files.values().map(|e| e.size_bytes).sum();
        let view_is_working = store.view == crate::store::VIEW_WORKING;
        let fjall_index_empty = store
            .index_db
            .as_ref()
            .map(|db| db.symbols_index_is_empty())
            .unwrap_or(false);
        let needs_initial_scan = (options.daemon_writer || !options.read_only)
            && view_is_working
            && (store.index.files.is_empty() || fjall_index_empty);
        let defer_warm = options.background && !needs_initial_scan;
        let cache = if defer_warm {
            Arc::new(MapCache::empty())
        } else {
            Arc::new(MapCache::build(&store))
        };
        tracing::info!(
            files = store.index.files.len(),
            corpus_bytes,
            git = repo.is_some(),
            scope = %scope,
            deferred_warm = defer_warm,
            "code map ready for MCP server (preloaded, or warming in background)"
        );
        let outline_cache: Arc<OutlineCache> = Arc::new(Mutex::new(LruCache::new(
            NonZeroUsize::new(OUTLINE_CACHE_CAP).expect("OUTLINE_CACHE_CAP > 0"),
        )));
        let telemetry_handle = Arc::new(telemetry::Telemetry::new(&store.basemind_dir));
        #[cfg(feature = "crawl")]
        let crawl_engine = match crate::web::build_engine(&config.crawl) {
            Ok(e) => Some(e),
            Err(error) => {
                tracing::warn!(?error, "crawl engine init failed; web_* tools will report errors");
                None
            }
        };
        let state = Arc::new(ServerState {
            store: RwLock::new(store),
            root,
            cache: ArcSwap::from(cache),
            repo,
            git_cache,
            git_history,
            outline_cache,
            config,
            telemetry: telemetry_handle,
            corpus_bytes: std::sync::atomic::AtomicU64::new(corpus_bytes),
            cache_generation: std::sync::atomic::AtomicU32::new(1),
            scope,
            agent_id,
            #[cfg(any(feature = "memory", feature = "documents", feature = "code-search"))]
            lance: tokio::sync::OnceCell::new(),
            #[cfg(feature = "intelligence")]
            embedder: tokio::sync::OnceCell::new(),
            #[cfg(feature = "crawl")]
            crawl_engine,
            #[cfg(all(feature = "comms", any(unix, windows)))]
            comms_clients: tokio::sync::Mutex::new(ahash::AHashMap::new()),
            #[cfg(all(feature = "shells", any(unix, windows)))]
            shell_runtime: crate::shells::ShellRuntime::new(),
            log_level: std::sync::atomic::AtomicU8::new(notifications::DEFAULT_LOG_ORDINAL),
            initial_scan_active: std::sync::atomic::AtomicBool::new(false),
            initial_scan_ms: std::sync::atomic::AtomicU64::new(0),
            cache_warming: std::sync::atomic::AtomicBool::new(defer_warm),
            cache_warm_ms: std::sync::atomic::AtomicU64::new(0),
            cache_ready: tokio::sync::Notify::new(),
            rescan_active: std::sync::atomic::AtomicBool::new(false),
            read_only: options.read_only,
            #[cfg(all(feature = "comms", any(unix, windows)))]
            daemon_writer: options.daemon_writer,
        });
        if options.background {
            let view_is_working = {
                match state.store.try_read() {
                    Ok(g) => g.view == crate::store::VIEW_WORKING,
                    Err(_) => false,
                }
            };
            if options.watch && (options.daemon_writer || !options.read_only) && view_is_working {
                background::spawn_serve_watcher(Arc::clone(&state));
            } else {
                background::spawn_view_watcher(Arc::clone(&state));
            }
            if let (Some(git_history), Some(repo)) = (state.git_history.clone(), state.repo.clone()) {
                let basemind_dir = basemind_dir.clone();
                tokio::task::spawn_blocking(move || {
                    match crate::git_history::builder::sync(&git_history, &repo, &basemind_dir) {
                        Ok(outcome) => {
                            tracing::info!(?outcome, "git-history index sync complete")
                        }
                        Err(error) => {
                            tracing::warn!(%error, "git-history index sync failed; tools live-walk")
                        }
                    }
                });
            }
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
    /// In lean mode, route the three wrapper tools through `lean::lean_call_tool`, which itself
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

    /// `prompts/list`: advertise the reusable prompt templates. Delegates to the
    /// `#[prompt_router]`-built router (basemind can't use `#[prompt_handler]` — it would
    /// regenerate `get_info`).
    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<ListPromptsResult, rmcp::ErrorData> {
        Ok(ListPromptsResult {
            prompts: self.prompt_router.list_all(),
            meta: None,
            next_cursor: None,
        })
    }

    /// `prompts/get`: render one prompt template with its arguments, via the prompt router.
    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<GetPromptResult, rmcp::ErrorData> {
        let prompt_context =
            rmcp::handler::server::prompt::PromptContext::new(self, request.name, request.arguments, context);
        self.prompt_router.get_prompt(prompt_context).await
    }

    /// `logging/setLevel`: record the minimum severity the client wants. Subsequent log
    /// notifications (e.g. from `rescan`) are gated on this threshold.
    #[allow(deprecated)]
    async fn set_level(
        &self,
        request: rmcp::model::SetLevelRequestParams,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<(), rmcp::ErrorData> {
        self.state.log_level.store(
            notifications::level_ordinal(request.level),
            std::sync::atomic::Ordering::Relaxed,
        );
        Ok(())
    }

    /// `completion/complete`: autocomplete a prompt argument from the indexed code map (symbol
    /// names for `trace-symbol`, file paths for `explain-file`). Pure in-RAM prefix scan.
    async fn complete(
        &self,
        request: CompleteRequestParams,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<CompleteResult, rmcp::ErrorData> {
        Ok(self.complete_argument(&request))
    }

    #[allow(deprecated)]
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .enable_completions()
                .enable_logging()
                .build(),
        )
        .with_instructions(
            "basemind is the indexed context layer for this repository, served over MCP: a \
             tree-sitter code map across 300+ languages (symbols, references, callers, call \
             graphs, implementations), git history + blame at symbol resolution, full-text + \
             semantic search, document RAG over 90+ file formats, and shared cross-session \
             memory. The index lives in a machine-global cache (`~/.local/share/basemind/`, \
             override `BASEMIND_DATA_HOME`) keyed by workspace — nothing is written into the \
             repo — and a background daemon is the sole writer, so any number of sessions on \
             this repo read and write concurrently. basemind first, shell/grep/git fallback: \
             prefer these tools over reading files, over grep, and over naked `git` — and use \
             them for document extraction, web crawling, and code parsing too. You may be one of \
             several agents in this repo: on start, check your inbox and the threads scoped to \
             where you're working, and post status as you go (see Agent comms below).\n\
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
             `inbox_read` / `thread_list` / `thread_post`.\n\
             \"find a file by name/path when I only remember a fragment?\" → `find_files` \
             (fuzzy, fzf/fd-style).\n\
             \"which repos/worktrees/branches does the daemon know?\" → `workspaces` / \
             `worktrees` / `branches`; claim a worktree so sessions don't collide with \
             `worktree_claim` (release with `worktree_release`).\n\
             \"got a truncated result? fetch the next page?\" → pass `next_cursor` from the prior \
             response back as `cursor`.\n\
             \"need regex over file contents?\" → `workspace_grep`.\n\
             Code-map tools: `outline`, `search_symbols`, `find_references`, `find_callers`, \
             `list_files`, `find_files`, `workspace_grep`, `dependents`, `status`, `repo_info`, \
             `symbol_history`. \
             Coordination tools: `workspaces`, `worktrees`, `branches`, `worktree_claim`, \
             `worktree_release` (advisory claims across the daemon's known worktrees). \
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
             Agent comms (require build with `--features comms`): coordinate with other agents \
             via THREADS — scoped conversations addressed by at least two of {subject, \
             path-glob, members}. Threads are discovered by scope (you're a member, your cwd \
             matches the thread's path-glob, or a subject filter) — never globally — and you \
             must explicitly `thread_join` to participate; there is no auto-join. On start, \
             `inbox_read` (front-matter only: subject / from / id — call `message_get` with an \
             id for a body) and `thread_list` to see threads in scope; skim `thread_history` on \
             the relevant one. `thread_start {subject, path_glob?, members?}` opens a thread \
             (you're the creator/admin; a human is also admin); `thread_post {thread, subject, \
             body, reply_to?}` when you begin, finish, or hit a decision, and reply \
             (`reply_to`) to messages about your work — do not stay silent when collaborating. \
             `inbox_ack` clears read messages. Idle threads auto-archive; `thread_archive` \
             closes one explicitly. Tools: `thread_start`, `thread_list`, `thread_join`, \
             `thread_leave`, `thread_members`, `thread_add_member`, `thread_remove_member`, \
             `thread_archive`, `thread_post`, `thread_history`, `message_get`, `inbox_read`, \
             `inbox_ack`, `agent_register`, `agent_list`. \
             All paths are repository-relative with forward-slash separators. \
             If a tool reports \"no indexed files\", run `basemind scan` in the repo first.",
        )
    }
}

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
