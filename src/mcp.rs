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
use crate::hashing;
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
}

struct MapCache {
    /// path → L1 (kept sorted by path; iteration order matches `list_files`)
    by_path: BTreeMap<String, FileMapL1>,
}

impl MapCache {
    /// Walks the store index once, loading every L1 blob into RAM. Silently skips entries
    /// whose blob is missing — a fresh `gitmind scan` will reconstruct them.
    fn build(store: &Store) -> Self {
        let mut by_path = BTreeMap::new();
        for (path, entry) in &store.index.files {
            let Some(hash) = hashing::from_hex(&entry.hash_hex) else {
                continue;
            };
            match store.read_l1(&hash) {
                Ok(Some(l1)) => {
                    by_path.insert(path.clone(), l1);
                }
                Ok(None) | Err(_) => continue,
            }
        }
        Self { by_path }
    }
}

impl GitmindServer {
    pub fn new(store: Store, root: PathBuf) -> Self {
        let cache = Arc::new(MapCache::build(&store));
        tracing::info!(
            files = cache.by_path.len(),
            "preloaded code map into RAM for MCP server"
        );
        Self {
            state: Arc::new(ServerState {
                store: RwLock::new(store),
                root,
                cache,
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
            if let Some(hash) = hashing::from_hex(&entry.hash_hex) {
                match store.read_l2(&hash) {
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
                        response.l2_status = Some(
                            "missing — run `gitmind query outline <path> --l2` to materialize",
                        );
                    }
                    Err(e) => {
                        response.l2_status = Some("error");
                        return Err(McpError::internal_error(format!("read_l2: {e}"), None));
                    }
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
        let needle = params.needle.as_str();
        let max_total = limit.saturating_mul(64).max(2_000);
        let mut results: Vec<SearchHitView> = Vec::with_capacity(limit);
        let mut total: usize = 0;
        let mut total_is_partial = false;
        'outer: for (path, l1) in &self.state.cache.by_path {
            for sym in &l1.symbols {
                if !sym.name.contains(needle) {
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
            .min(LIST_LIMIT_MAX);
        let store = self.state.store.read().await;

        let needle = params.path_contains.as_deref();
        let lang_filter = params.language.as_deref();

        let mut all: Vec<ListFilesEntry> = store
            .index
            .files
            .iter()
            .filter(|(p, e)| {
                needle.is_none_or(|n| p.contains(n)) && lang_filter.is_none_or(|l| e.language == l)
            })
            .map(|(p, e)| ListFilesEntry {
                path: p.clone(),
                language: e.language.clone(),
                size_bytes: e.size_bytes,
            })
            .collect();
        let total = all.len();
        all.sort_by(|a, b| a.path.cmp(&b.path));
        let truncated = total > limit as usize;
        all.truncate(limit as usize);

        json_result(&ListFilesResponse {
            total,
            returned: all.len(),
            truncated,
            files: all,
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
        // Pure-RAM scan; reuses the L3 substring heuristic but feeds it from the preloaded
        // cache rather than re-reading every L1 blob from disk.
        let by_path: std::collections::HashMap<PathBuf, Vec<Import>> = self
            .state
            .cache
            .by_path
            .iter()
            .map(|(p, l1)| (PathBuf::from(p), l1.imports.clone()))
            .collect();
        let paths: Vec<String> = crate::extract::l3::dependents_of(&params.module, &by_path)
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
}

#[tool_handler]
impl ServerHandler for GitmindServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "gitmind exposes a tree-sitter-backed code map. Prefer `outline` for navigating a file, \
             `search_symbols` for cross-repo name lookup, and `list_files` to enumerate what's indexed. \
             All paths are repository-relative with forward-slash separators.",
        )
    }
}
