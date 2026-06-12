//! MCP server exposing the basemind code map + git context to AI agents.
//!
//! The server is read-only and opens the store without taking the exclusive lock so it can
//! coexist with `basemind watch` running in another terminal. Tools return JSON so the agent
//! can navigate by file path + line numbers without opening source files.
//!
//! Transport: stdio (the canonical MCP transport). Spawn via `basemind serve`.

pub(crate) mod cursor;
mod helpers;
mod helpers_calls;
mod helpers_grep;
mod helpers_impls;
#[cfg(feature = "crawl")]
mod helpers_web;
#[cfg(any(feature = "memory", feature = "documents"))]
mod memory;
mod savings;
mod telemetry;
mod tools;
mod tools_admin;
mod tools_git;
mod tools_memory;
#[cfg(feature = "crawl")]
mod tools_web;
mod types;
mod types_impls;

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

impl BasemindServer {
    pub fn new(
        store: Store,
        root: PathBuf,
        config: Arc<crate::config::Config>,
        repo: Option<Arc<crate::git::Repo>>,
        git_cache: Arc<crate::git_cache::GitCache>,
    ) -> Self {
        let scope = repo
            .as_ref()
            .map(|r| crate::git::scope_key(r))
            .unwrap_or_else(|| format!("path:{}", root.display()));
        let cache = Arc::new(MapCache::build(&store));
        let corpus_bytes: u64 = store.index.files.values().map(|e| e.size_bytes).sum();
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
            #[cfg(any(feature = "memory", feature = "documents"))]
            lance: tokio::sync::OnceCell::new(),
            #[cfg(feature = "intelligence")]
            embedder: tokio::sync::OnceCell::new(),
            #[cfg(feature = "crawl")]
            crawl_engine,
        });
        spawn_view_watcher(Arc::clone(&state));
        #[allow(unused_mut)]
        let mut router = Self::tool_router_core()
            + Self::tool_router_git()
            + Self::tool_router_memory()
            + Self::tool_router_admin();
        #[cfg(feature = "crawl")]
        {
            router += Self::tool_router_web();
        }
        Self {
            state,
            tool_router: router,
        }
    }
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
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "basemind is a tree-sitter-backed code map + git context. Prefer these tools over \
             reading files when navigating large or unfamiliar codebases.\n\
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
             to limit to changed files).\n\
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
             All paths are repository-relative with forward-slash separators. \
             If a tool reports \"no indexed files\", run `basemind scan` in the repo first.",
        )
    }
}
