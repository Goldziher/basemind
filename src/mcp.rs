//! MCP server exposing the gitmind code map to AI agents.
//!
//! The server is read-only and opens the store without taking the exclusive lock, so it can
//! coexist with `gitmind watch` running in another terminal. Tools all return JSON so the
//! agent can navigate the codebase by file path + line numbers without opening source files.
//!
//! Transport: stdio (the canonical MCP transport). Spawn this from an MCP-aware host.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use rmcp::ServerHandler;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, ServerCapabilities, ServerInfo};
use rmcp::schemars;
use rmcp::{ErrorData as McpError, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::extract::{FileMapL1, Import, SymbolKind};
use crate::query;
use crate::store::Store;

/// Shared MCP server state. Wraps a read-only `Store` plus the repo root path.
///
/// `ToolRouter<Self>` is Clone (cheap — Arc inside), so we hold it directly on the struct as
/// the `#[tool_handler]` macro expects.
#[derive(Clone)]
pub struct GitmindServer {
    state: Arc<ServerState>,
    // Touched by macro-generated dispatch; dead_code can't see that.
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

struct ServerState {
    store: RwLock<Store>,
    root: PathBuf,
    /// In-RAM mirror of every indexed file's L1 blob, built once at startup.
    ///
    /// Cross-file queries (`search_symbols`, `dependents`) otherwise re-read 1 blob per file
    /// per call — for a 39k-file repo that's seconds. With the preload they're pure-RAM scans.
    /// `outline` keeps reading via the store so it always sees fresh blobs (e.g. if `gitmind
    /// watch` rewrote a file in another process), and single-file reads are already cheap.
    cache: Arc<MapCache>,
    /// Discovered git repository, or `None` when serving against a non-git directory.
    /// All git-aware tools (`working_tree_status`, `recent_changes`, …) check this and
    /// return an MCP error if `None`.
    repo: Option<Arc<crate::git::Repo>>,
}

struct MapCache {
    /// path → L1 (kept sorted by path; iteration order matches `list_files`)
    by_path: BTreeMap<String, FileMapL1>,
    /// Pre-flattened `(path, imports)` view used by the `dependents` tool.
    ///
    /// Without this, every `dependents` call rebuilds the same `HashMap<PathBuf, Vec<Import>>`
    /// from scratch — that's one `Vec<Import>::clone()` per indexed file, ~1500 allocations on
    /// the TypeScript repo (6.5 ms wall). Precomputing once at server boot drops that to
    /// pure pointer-chase.
    imports_index: Vec<(PathBuf, Vec<Import>)>,
}

impl MapCache {
    /// Walks the store index once, loading every L1 blob into RAM. Silently skips entries
    /// whose blob is missing — a fresh `gitmind scan` will reconstruct them.
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
        // BTreeMap iteration is already path-sorted, so the imports_index ends up sorted
        // by path too — which is what `l3::dependents_of` sorts to anyway.
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
    pub fn new(store: Store, root: PathBuf, repo: Option<Arc<crate::git::Repo>>) -> Self {
        let cache = Arc::new(MapCache::build(&store));
        tracing::info!(
            files = cache.by_path.len(),
            git = repo.is_some(),
            "preloaded code map into RAM for MCP server"
        );
        Self {
            state: Arc::new(ServerState {
                store: RwLock::new(store),
                root,
                cache,
                repo,
            }),
            tool_router: Self::tool_router(),
        }
    }
}

// ─── Parameter / response shapes ─────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct OutlineParams {
    /// Repository-relative path (forward-slash). Must be a file gitmind has scanned.
    pub path: String,
    /// When true, also include calls + doc comments (L2). Falls back to empty
    /// arrays if no L2 blob exists for the file's current content.
    #[serde(default)]
    pub l2: bool,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SearchSymbolsParams {
    /// Substring matched against symbol name (case-sensitive).
    pub needle: String,
    /// Optional kind filter: function, method, struct, enum, class, interface,
    /// trait, type, const, module, macro.
    #[serde(default)]
    pub kind: Option<String>,
    /// Cap the number of results returned. Default 100, max 1000.
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ListFilesParams {
    /// Optional substring matched against the path. Cheaper than reading a glob crate.
    #[serde(default)]
    pub path_contains: Option<String>,
    /// Filter by language (e.g. "rust", "python").
    #[serde(default)]
    pub language: Option<String>,
    /// Cap. Default 200, max 5000.
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct DependentsParams {
    /// Module / import target (e.g. "tokio::sync" or "react").
    pub module: String,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct StatusParams {}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct WorkingTreeStatusParams {}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct RecentChangesParams {
    /// Number of commits to walk back from HEAD. Default 20, max 100.
    #[serde(default)]
    pub limit: Option<u32>,
    /// When true, include the per-file change list for each commit. Default true.
    #[serde(default = "default_true")]
    pub include_files: bool,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct CommitsTouchingParams {
    /// Repository-relative path (forward-slash) of the file to follow.
    pub path: String,
    /// Number of commits returned, newest first. Default 20, max 100.
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct DiffOutlineParams {
    /// Repository-relative path of the file to diff.
    pub path: String,
    /// Revision to compare against the *current view*. Defaults to "HEAD".
    #[serde(default)]
    pub rev: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct RepoInfoParams {}

fn default_true() -> bool {
    true
}

// ─── Response shapes (JSON-clean copies of the extract types) ────────────────

#[derive(Debug, Serialize)]
struct OutlineResponse {
    path: String,
    language: String,
    size_bytes: u64,
    had_errors: bool,
    error_count: u32,
    symbols: Vec<SymbolView>,
    imports: Vec<ImportView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    calls: Option<Vec<CallView>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    docs: Option<Vec<DocView>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    l2_status: Option<&'static str>,
}

#[derive(Debug, Serialize)]
struct SymbolView {
    name: String,
    kind: String,
    start_row: u32,
    start_col: u32,
    start_byte: u32,
    end_byte: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    signature: Option<String>,
}

#[derive(Debug, Serialize)]
struct ImportView {
    #[serde(skip_serializing_if = "Option::is_none")]
    module: Option<String>,
    raw: String,
    start_byte: u32,
}

#[derive(Debug, Serialize)]
struct CallView {
    callee: String,
    start_byte: u32,
}

#[derive(Debug, Serialize)]
struct DocView {
    text: String,
    start_byte: u32,
}

#[derive(Debug, Serialize)]
struct SearchHitView {
    path: String,
    name: String,
    kind: String,
    start_row: u32,
    start_col: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    signature: Option<String>,
}

#[derive(Debug, Serialize)]
struct SearchResponse {
    total: usize,
    truncated: bool,
    results: Vec<SearchHitView>,
}

#[derive(Debug, Serialize)]
struct ListFilesEntry {
    path: String,
    language: String,
    size_bytes: u64,
}

#[derive(Debug, Serialize)]
struct ListFilesResponse {
    total: usize,
    returned: usize,
    truncated: bool,
    files: Vec<ListFilesEntry>,
}

#[derive(Debug, Serialize)]
struct DependentsResponse {
    module: String,
    paths: Vec<String>,
}

#[derive(Debug, Serialize)]
struct StatusResponse {
    file_count: usize,
    total_size_bytes: u64,
    languages: BTreeMap<String, usize>,
    cache_dir: String,
    schema_version: u16,
    root: String,
}

#[derive(Debug, Serialize)]
struct CommitView {
    sha: String,
    short_sha: String,
    summary: String,
    author: String,
    author_time_unix: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    files: Option<Vec<CommitFileView>>,
}

#[derive(Debug, Serialize)]
struct CommitFileView {
    path: String,
    change: &'static str,
}

#[derive(Debug, Serialize)]
struct WorkingTreeStatusView {
    staged_added: Vec<String>,
    staged_modified: Vec<String>,
    staged_deleted: Vec<String>,
    modified: Vec<String>,
    untracked: Vec<String>,
    is_clean: bool,
}

#[derive(Debug, Serialize)]
struct RecentChangesResponse {
    commits: Vec<CommitView>,
}

#[derive(Debug, Serialize)]
struct CommitsTouchingResponse {
    path: String,
    commits: Vec<CommitView>,
}

#[derive(Debug, Serialize)]
struct DiffSymbolView {
    name: String,
    kind: String,
}

#[derive(Debug, Serialize)]
struct DiffOutlineResponse {
    path: String,
    rev: String,
    /// In the current view but not at `rev`.
    added: Vec<DiffSymbolView>,
    /// At `rev` but not in the current view.
    removed: Vec<DiffSymbolView>,
    /// In both. Useful so the agent can see context without re-querying outline.
    common: Vec<DiffSymbolView>,
    /// Set when one side doesn't contain the file at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

#[derive(Debug, Serialize)]
struct RepoInfoResponse {
    workdir: String,
    head_sha: Option<String>,
    head_short_sha: Option<String>,
    branch: Option<String>,
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn kind_to_str(k: SymbolKind) -> &'static str {
    match k {
        SymbolKind::Function => "function",
        SymbolKind::Method => "method",
        SymbolKind::Struct => "struct",
        SymbolKind::Enum => "enum",
        SymbolKind::Class => "class",
        SymbolKind::Interface => "interface",
        SymbolKind::Trait => "trait",
        SymbolKind::Type => "type",
        SymbolKind::Const => "const",
        SymbolKind::Module => "module",
        SymbolKind::Macro => "macro",
        SymbolKind::Unknown => "unknown",
    }
}

fn parse_kind(s: &str) -> Result<SymbolKind, McpError> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "function" => SymbolKind::Function,
        "method" => SymbolKind::Method,
        "struct" => SymbolKind::Struct,
        "enum" => SymbolKind::Enum,
        "class" => SymbolKind::Class,
        "interface" => SymbolKind::Interface,
        "trait" => SymbolKind::Trait,
        "type" => SymbolKind::Type,
        "const" => SymbolKind::Const,
        "module" => SymbolKind::Module,
        "macro" => SymbolKind::Macro,
        other => {
            return Err(McpError::invalid_params(
                format!("unknown symbol kind: {other}"),
                None,
            ));
        }
    })
}

fn json_result<T: Serialize>(value: &T) -> Result<CallToolResult, McpError> {
    let content = Content::json(value)
        .map_err(|e| McpError::internal_error(format!("serialize response: {e}"), None))?;
    Ok(CallToolResult::success(vec![content]))
}

const SEARCH_LIMIT_DEFAULT: u32 = 100;
const SEARCH_LIMIT_MAX: u32 = 1000;
const LIST_LIMIT_DEFAULT: u32 = 200;
const LIST_LIMIT_MAX: u32 = 5000;
const LOG_LIMIT_DEFAULT: u32 = 20;
const LOG_LIMIT_MAX: u32 = 100;

fn commit_to_view(c: crate::git::CommitInfo, include_files: bool) -> CommitView {
    let files = if include_files {
        Some(
            c.files
                .into_iter()
                .map(|(path, kind)| CommitFileView {
                    path,
                    change: kind.as_str(),
                })
                .collect(),
        )
    } else {
        None
    };
    CommitView {
        sha: c.sha,
        short_sha: c.short_sha,
        summary: c.summary,
        author: c.author,
        author_time_unix: c.author_time_unix,
        files,
    }
}

fn require_git_repo(state: &ServerState) -> Result<&Arc<crate::git::Repo>, McpError> {
    state.repo.as_ref().ok_or_else(|| {
        McpError::invalid_request(
            "this tool requires `gitmind serve` to be run inside a git repository",
            None,
        )
    })
}

// ─── Tools ───────────────────────────────────────────────────────────────────

#[tool_router]
impl GitmindServer {
    /// File outline: symbols + imports (L1), optionally calls + docs (L2).
    #[tool(
        description = "Return the structural outline of a file: every symbol with name, kind, \
                       and start row/column, plus imports. Set `l2: true` to also include calls \
                       and doc comments (only returned if an L2 blob already exists for the \
                       file's current content)."
    )]
    async fn outline(
        &self,
        Parameters(params): Parameters<OutlineParams>,
    ) -> Result<CallToolResult, McpError> {
        let store = self.state.store.read().await;
        let l1 = query::file_outline(&store, &params.path).map_err(|e| {
            McpError::invalid_params(format!("file_outline({}): {e}", params.path), None)
        })?;

        let symbols = l1
            .symbols
            .iter()
            .map(|s| SymbolView {
                name: s.name.clone(),
                kind: kind_to_str(s.kind).to_string(),
                start_row: s.start_row,
                start_col: s.start_col,
                start_byte: s.start_byte,
                end_byte: s.end_byte,
                signature: s.signature.clone(),
            })
            .collect();
        let imports = l1
            .imports
            .iter()
            .map(|i| ImportView {
                module: i.module.clone(),
                raw: i.raw.clone(),
                start_byte: i.start_byte,
            })
            .collect();

        let mut response = OutlineResponse {
            path: params.path.clone(),
            language: l1.language.clone(),
            size_bytes: l1.size_bytes,
            had_errors: l1.had_errors,
            error_count: l1.error_count,
            symbols,
            imports,
            calls: None,
            docs: None,
            l2_status: None,
        };

        if params.l2 {
            // Look up the L2 blob by hash without doing live extraction (we are read-only).
            let entry = store.lookup(&params.path).ok_or_else(|| {
                McpError::internal_error("file not indexed after outline succeeded", None)
            })?;
            match store.read_l2_by_hex(&entry.hash_hex) {
                Ok(Some(l2)) => {
                    response.calls = Some(
                        l2.calls
                            .iter()
                            .map(|c| CallView {
                                callee: c.callee.clone(),
                                start_byte: c.start_byte,
                            })
                            .collect(),
                    );
                    response.docs = Some(
                        l2.docs
                            .iter()
                            .map(|d| DocView {
                                text: d.text.clone(),
                                start_byte: d.start_byte,
                            })
                            .collect(),
                    );
                }
                Ok(None) => {
                    response.l2_status =
                        Some("missing — run `gitmind query outline <path> --l2` to materialize");
                }
                Err(e) => {
                    response.l2_status = Some("error");
                    return Err(McpError::internal_error(format!("read_l2: {e}"), None));
                }
            }
        }

        json_result(&response)
    }

    /// Substring search across symbol names, optionally filtered by kind.
    #[tool(
        description = "Search every indexed file for symbols whose name contains `needle`. \
                       Optional `kind` filter (function/struct/class/...). Returns up to `limit` \
                       (default 100, max 1000) results, each with path + line/column + signature."
    )]
    async fn search_symbols(
        &self,
        Parameters(params): Parameters<SearchSymbolsParams>,
    ) -> Result<CallToolResult, McpError> {
        let kind = params.kind.as_deref().map(parse_kind).transpose()?;
        let limit = params
            .limit
            .unwrap_or(SEARCH_LIMIT_DEFAULT)
            .min(SEARCH_LIMIT_MAX) as usize;

        // Pure-RAM scan over the preloaded code map. We collect into `results` until `limit`
        // hits, but keep counting `total` so the agent knows whether their needle was specific
        // enough. Hard cap on `total` iterations so a too-broad needle (e.g. "a") doesn't pin
        // a CPU on counting.
        //
        // memmem::Finder amortizes the bad-character table across every candidate symbol —
        // `str::contains` rebuilds it on each call.
        let finder = memchr::memmem::Finder::new(params.needle.as_bytes());
        let max_total = limit.saturating_mul(64).max(2_000);
        let mut results: Vec<SearchHitView> = Vec::with_capacity(limit);
        let mut total: usize = 0;
        let mut total_is_partial = false;
        'outer: for (path, l1) in &self.state.cache.by_path {
            for sym in &l1.symbols {
                if finder.find(sym.name.as_bytes()).is_none() {
                    continue;
                }
                if let Some(k) = kind
                    && sym.kind != k
                {
                    continue;
                }
                total += 1;
                if results.len() < limit {
                    results.push(SearchHitView {
                        path: path.clone(),
                        name: sym.name.clone(),
                        kind: kind_to_str(sym.kind).to_string(),
                        start_row: sym.start_row,
                        start_col: sym.start_col,
                        signature: sym.signature.clone(),
                    });
                }
                if total >= max_total {
                    total_is_partial = true;
                    break 'outer;
                }
            }
        }
        let truncated = total > limit || total_is_partial;
        json_result(&SearchResponse {
            total,
            truncated,
            results,
        })
    }

    /// List indexed files, optionally filtered by path substring and/or language.
    #[tool(
        description = "List indexed files with their language and size. Optional `path_contains` \
                       substring filter and `language` filter (rust/python/typescript/tsx/javascript/go). \
                       Default limit 200, max 5000."
    )]
    async fn list_files(
        &self,
        Parameters(params): Parameters<ListFilesParams>,
    ) -> Result<CallToolResult, McpError> {
        let limit = params
            .limit
            .unwrap_or(LIST_LIMIT_DEFAULT)
            .min(LIST_LIMIT_MAX) as usize;
        let store = self.state.store.read().await;

        let path_finder = params
            .path_contains
            .as_ref()
            .map(|n| memchr::memmem::Finder::new(n.as_bytes()));
        let lang_filter = params.language.as_deref();

        // BTreeMap iteration is already path-sorted, so we can walk in order and stop after
        // `limit` matches — no full collect, no resort.
        let mut files: Vec<ListFilesEntry> = Vec::with_capacity(limit.min(256));
        let mut total: usize = 0;
        for (p, e) in &store.index.files {
            let path_ok = path_finder
                .as_ref()
                .is_none_or(|f| f.find(p.as_bytes()).is_some());
            let lang_ok = lang_filter.is_none_or(|l| e.language == l);
            if !(path_ok && lang_ok) {
                continue;
            }
            total += 1;
            if files.len() < limit {
                files.push(ListFilesEntry {
                    path: p.clone(),
                    language: e.language.clone(),
                    size_bytes: e.size_bytes,
                });
            }
        }
        let truncated = total > limit;

        json_result(&ListFilesResponse {
            total,
            returned: files.len(),
            truncated,
            files,
        })
    }

    /// Heuristic reverse-dependency lookup via import statements.
    #[tool(
        description = "Return the list of indexed files whose imports mention `module`. \
                       Heuristic — matches by substring against the recorded module path of each import."
    )]
    async fn dependents(
        &self,
        Parameters(params): Parameters<DependentsParams>,
    ) -> Result<CallToolResult, McpError> {
        // Pure-RAM scan against the preloaded `imports_index`. No HashMap rebuild, no
        // per-file Vec<Import>::clone — the L3 heuristic borrows directly from cache.
        let paths: Vec<String> =
            crate::extract::l3::dependents_of(&params.module, &self.state.cache.imports_index)
                .into_iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect();
        json_result(&DependentsResponse {
            module: params.module.clone(),
            paths,
        })
    }

    /// High-level repo + cache state.
    #[tool(
        description = "Quick report on the repo gitmind has indexed: file count, total bytes, \
                       per-language breakdown, root path, grammar cache directory, schema version."
    )]
    async fn status(
        &self,
        Parameters(_): Parameters<StatusParams>,
    ) -> Result<CallToolResult, McpError> {
        let store = self.state.store.read().await;
        let mut by_lang: BTreeMap<String, usize> = BTreeMap::new();
        let mut total_size: u64 = 0;
        for entry in store.index.files.values() {
            *by_lang.entry(entry.language.clone()).or_insert(0) += 1;
            total_size = total_size.saturating_add(entry.size_bytes);
        }
        let cache_dir = crate::lang::grammar_cache_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(unresolved)".to_string());
        json_result(&StatusResponse {
            file_count: store.index.files.len(),
            total_size_bytes: total_size,
            languages: by_lang,
            cache_dir,
            schema_version: crate::extract::SCHEMA_VER,
            root: self.state.root.display().to_string(),
        })
    }

    /// `git status --porcelain` shape for an agent.
    #[tool(
        description = "Return what's dirty in the working tree: staged adds/modifies/deletes, \
                       working-tree modifications, and untracked files. `is_clean: true` if all five \
                       buckets are empty. Requires `gitmind serve` to be run inside a git repository."
    )]
    async fn working_tree_status(
        &self,
        Parameters(_): Parameters<WorkingTreeStatusParams>,
    ) -> Result<CallToolResult, McpError> {
        let repo = require_git_repo(&self.state)?;
        let s = repo
            .status_porcelain()
            .map_err(|e| McpError::internal_error(format!("git status: {e}"), None))?;
        let is_clean = s.staged_added.is_empty()
            && s.staged_modified.is_empty()
            && s.staged_deleted.is_empty()
            && s.modified.is_empty()
            && s.untracked.is_empty();
        json_result(&WorkingTreeStatusView {
            staged_added: s.staged_added,
            staged_modified: s.staged_modified,
            staged_deleted: s.staged_deleted,
            modified: s.modified,
            untracked: s.untracked,
            is_clean,
        })
    }

    /// Walk HEAD ancestry and return the last N commits.
    #[tool(
        description = "Last N commits on the current branch, newest first. Each commit comes with \
                       sha, summary (first line of message), author, unix timestamp, and — when \
                       `include_files=true` (default) — the per-file change list relative to its first \
                       parent. Default 20 commits, max 100."
    )]
    async fn recent_changes(
        &self,
        Parameters(params): Parameters<RecentChangesParams>,
    ) -> Result<CallToolResult, McpError> {
        let repo = require_git_repo(&self.state)?;
        let limit = params.limit.unwrap_or(LOG_LIMIT_DEFAULT).min(LOG_LIMIT_MAX) as usize;
        let commits = repo
            .log_paths(limit, params.include_files)
            .map_err(|e| McpError::internal_error(format!("log: {e}"), None))?;
        let view = commits
            .into_iter()
            .map(|c| commit_to_view(c, params.include_files))
            .collect();
        json_result(&RecentChangesResponse { commits: view })
    }

    /// Filter the log to commits whose tree differs from the parent at `path`.
    #[tool(
        description = "Commits that modified `path`, newest first. Returns the same per-commit \
                       shape as `recent_changes` without the per-file list (the path is implicit). \
                       Default 20 commits, max 100."
    )]
    async fn commits_touching(
        &self,
        Parameters(params): Parameters<CommitsTouchingParams>,
    ) -> Result<CallToolResult, McpError> {
        let repo = require_git_repo(&self.state)?;
        let limit = params.limit.unwrap_or(LOG_LIMIT_DEFAULT).min(LOG_LIMIT_MAX) as usize;
        let commits = repo
            .log_for_path(&params.path, limit)
            .map_err(|e| McpError::internal_error(format!("log: {e}"), None))?;
        let view = commits
            .into_iter()
            .map(|c| commit_to_view(c, false))
            .collect();
        json_result(&CommitsTouchingResponse {
            path: params.path,
            commits: view,
        })
    }

    /// Symbol-level diff between the served view and another rev.
    ///
    /// We extract the rev side live (it isn't necessarily indexed) and compare against the
    /// preloaded cache by `(name, kind)`. No textual diff, no signature diff — outline-level only.
    #[tool(
        description = "Diff the symbol set of `path` between the current view and another revision \
                       (`rev`, defaults to HEAD). Returns three lists: `added` (in the current view, \
                       not at `rev`), `removed` (at `rev`, not in current view), and `common`. Useful \
                       for 'what symbols did this branch add' style questions without reading source."
    )]
    async fn diff_outline(
        &self,
        Parameters(params): Parameters<DiffOutlineParams>,
    ) -> Result<CallToolResult, McpError> {
        let repo = require_git_repo(&self.state)?;
        let rev_spec = params.rev.as_deref().unwrap_or("HEAD");
        let rev_sha = repo
            .resolve_rev(rev_spec)
            .map_err(|e| McpError::invalid_params(format!("resolve_rev({rev_spec}): {e}"), None))?;

        let here = self.state.cache.by_path.get(&params.path).map(|l1| {
            l1.symbols
                .iter()
                .map(|s| (s.name.clone(), kind_to_str(s.kind)))
                .collect::<Vec<(String, &'static str)>>()
        });

        let rev_blob = repo.read_blob_at_rev(&rev_sha, &params.path).map_err(|e| {
            McpError::internal_error(format!("read blob {rev_sha}:{}: {e}", params.path), None)
        })?;

        let there: Option<Vec<(String, &'static str)>> = match rev_blob {
            Some(bytes) => {
                let lang =
                    crate::lang::detect(std::path::Path::new(&params.path)).ok_or_else(|| {
                        McpError::invalid_params(
                            format!("unsupported language for {}", params.path),
                            None,
                        )
                    })?;
                let l1 = crate::extract::l1::extract_l1(lang, &bytes).map_err(|e| {
                    McpError::internal_error(
                        format!("extract {rev_sha}:{}: {e}", params.path),
                        None,
                    )
                })?;
                Some(
                    l1.symbols
                        .into_iter()
                        .map(|s| (s.name, kind_to_str(s.kind)))
                        .collect(),
                )
            }
            None => None,
        };

        let (added, removed, common, note) = match (here, there) {
            (Some(h), Some(t)) => {
                let hs: ahash::AHashSet<(String, &'static str)> = h.iter().cloned().collect();
                let ts: ahash::AHashSet<(String, &'static str)> = t.iter().cloned().collect();
                let added = h
                    .iter()
                    .filter(|p| !ts.contains(*p))
                    .cloned()
                    .map(|(n, k)| DiffSymbolView {
                        name: n,
                        kind: k.to_string(),
                    })
                    .collect();
                let removed = t
                    .iter()
                    .filter(|p| !hs.contains(*p))
                    .cloned()
                    .map(|(n, k)| DiffSymbolView {
                        name: n,
                        kind: k.to_string(),
                    })
                    .collect();
                let common = h
                    .iter()
                    .filter(|p| ts.contains(*p))
                    .cloned()
                    .map(|(n, k)| DiffSymbolView {
                        name: n,
                        kind: k.to_string(),
                    })
                    .collect();
                (added, removed, common, None)
            }
            (Some(h), None) => (
                h.into_iter()
                    .map(|(n, k)| DiffSymbolView {
                        name: n,
                        kind: k.to_string(),
                    })
                    .collect(),
                Vec::new(),
                Vec::new(),
                Some(
                    format!("path absent at {rev_spec}; entire file treated as added").to_string(),
                ),
            ),
            (None, Some(t)) => (
                Vec::new(),
                t.into_iter()
                    .map(|(n, k)| DiffSymbolView {
                        name: n,
                        kind: k.to_string(),
                    })
                    .collect(),
                Vec::new(),
                Some(
                    "path not indexed in the current view; entire file treated as removed"
                        .to_string(),
                ),
            ),
            (None, None) => {
                return Err(McpError::invalid_params(
                    format!(
                        "path not present in current view or at {rev_spec}: {}",
                        params.path
                    ),
                    None,
                ));
            }
        };

        json_result(&DiffOutlineResponse {
            path: params.path,
            rev: rev_sha,
            added,
            removed,
            common,
            note,
        })
    }

    /// Workdir + branch + HEAD sha. No params.
    #[tool(
        description = "Repository identity: workdir path, current branch name (if HEAD is on one), \
                       full HEAD sha, short HEAD sha. Pairs well with `working_tree_status`."
    )]
    async fn repo_info(
        &self,
        Parameters(_): Parameters<RepoInfoParams>,
    ) -> Result<CallToolResult, McpError> {
        let repo = require_git_repo(&self.state)?;
        let info = repo
            .info()
            .map_err(|e| McpError::internal_error(format!("repo info: {e}"), None))?;
        json_result(&RepoInfoResponse {
            workdir: info.workdir.display().to_string(),
            head_sha: info.head_sha,
            head_short_sha: info.head_short_sha,
            branch: info.branch,
        })
    }
}

#[tool_handler]
impl ServerHandler for GitmindServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "gitmind exposes a tree-sitter-backed code map plus git context. \
             Code-map tools: `outline` for one file, `search_symbols` for cross-repo name lookup, \
             `list_files` to enumerate what's indexed, `dependents` for reverse imports, `status` for \
             cache stats. \
             Git tools (need to be inside a git repo): `working_tree_status` (dirty/staged/untracked), \
             `recent_changes` (HEAD ancestry), `commits_touching` (log filtered to a path), \
             `diff_outline` (symbol-level diff between current view and a rev), `repo_info` \
             (workdir/branch/HEAD). All paths are repository-relative with forward-slash separators.",
        )
    }
}
