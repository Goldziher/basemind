//! MCP server exposing the gitmind code map + git context to AI agents.
//!
//! The server is read-only and opens the store without taking the exclusive lock so it can
//! coexist with `gitmind watch` running in another terminal. Tools return JSON so the agent
//! can navigate by file path + line numbers without opening source files.
//!
//! Transport: stdio (the canonical MCP transport). Spawn via `gitmind serve`.

mod helpers;
mod tools;
mod types;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use rmcp::ServerHandler;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::tool_handler;
use tokio::sync::RwLock;

use crate::extract::{FileMapL1, Import};
use crate::store::Store;

/// Shared MCP server state. `ToolRouter<Self>` is Clone (Arc inside), so we hold it directly
/// on the struct as the `#[tool_handler]` macro expects.
#[derive(Clone)]
pub struct GitmindServer {
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
}

pub(crate) struct MapCache {
    /// path → L1 (kept sorted by path; iteration order matches `list_files`)
    pub(crate) by_path: BTreeMap<String, FileMapL1>,
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
            .map(|(p, l1)| (PathBuf::from(p), l1.imports.clone()))
            .collect();
        Self {
            by_path,
            imports_index,
        }
    }
}

impl GitmindServer {
    pub fn new(
        store: Store,
        root: PathBuf,
        repo: Option<Arc<crate::git::Repo>>,
        git_cache: Arc<crate::git_cache::GitCache>,
    ) -> Self {
        let cache = Arc::new(MapCache::build(&store));
        tracing::info!(
            files = cache.by_path.len(),
            git = repo.is_some(),
            "preloaded code map into RAM for MCP server"
        );
        let state = Arc::new(ServerState {
            store: RwLock::new(store),
            root,
            cache: ArcSwap::from(cache),
            repo,
            git_cache,
        });
        spawn_view_watcher(Arc::clone(&state));
        Self {
            state,
            tool_router: Self::tool_router(),
        }
    }
}

fn spawn_view_watcher(state: Arc<ServerState>) {
    let (gitmind_dir, view) = {
        let store = match state.store.try_read() {
            Ok(g) => g,
            Err(_) => return,
        };
        (store.gitmind_dir.clone(), store.view.clone())
    };
    let view_dir = gitmind_dir.join(crate::store::VIEWS_DIR).join(&view);
    let target = view_dir.join(crate::store::INDEX_FILE);

    std::thread::Builder::new()
        .name("gitmind-mcp-view-watcher".to_string())
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
            }
            tracing::info!("view watcher: channel closed; exiting");
        })
        .ok();
}

#[tool_handler]
impl ServerHandler for GitmindServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "gitmind exposes a tree-sitter-backed code map plus git context. \
             Code-map tools: `outline`, `search_symbols`, `list_files`, `dependents`, `status`. \
             Git tools (inside a repo): `working_tree_status`, `recent_changes`, `commits_touching`, \
             `find_commits_by_path`, `hot_files`, `diff_outline`, `diff_file`, `blame_file`, \
             `blame_symbol`, `symbol_history`, `repo_info`. All paths are repository-relative \
             with forward-slash separators.",
        )
    }
}
