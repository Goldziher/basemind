//! Parameter shapes (deserialized from MCP tool-call arguments) and JSON response shapes
//! (serialized into tool-call results). Kept separate from `tools.rs` so the impl block
//! itself stays readable and within the per-file size budget.

use std::collections::BTreeMap;

use rmcp::schemars;
use serde::{Deserialize, Serialize};

use super::cursor::Cursor;
use crate::path::RelPath;

// ─── Parameter shapes ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct OutlineParams {
    /// Repository-relative path (forward-slash). Must be a file basemind has scanned.
    pub path: RelPath,
    /// When true, also include calls + doc comments (L2). Falls back to empty
    /// arrays if no L2 blob exists for the file's current content.
    #[serde(default)]
    pub l2: bool,
    /// Optional token budget bounding the returned `symbols` list (not the whole envelope,
    /// and not `imports` / `calls` / `docs`). Symbols are kept in file order until the
    /// budget is hit; the rest are dropped and the response carries `budgeted: true`.
    #[serde(default, alias = "token_budget", alias = "budget")]
    pub max_tokens: Option<u32>,
    /// Wire format for the response: `"json"` (default) or `"toon"`. TOON is a compact
    /// tabular encoding of the `symbols` list — far fewer tokens than JSON for large outlines.
    #[serde(default, alias = "encoding")]
    pub format: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SearchSymbolsParams {
    /// Substring matched against symbol name (case-sensitive).
    #[serde(
        alias = "query",
        alias = "pattern",
        alias = "name",
        alias = "q",
        alias = "term",
        alias = "symbol"
    )]
    pub needle: String,
    /// Optional kind filter: function, method, struct, enum, class, interface,
    /// trait, type, const, module, macro.
    #[serde(default)]
    pub kind: Option<String>,
    /// Cap the number of results returned. Default 100, max 1000.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Optional token budget bounding the returned result list (not the whole envelope).
    /// Items are kept best-first until the budget is hit; the rest are dropped and the
    /// response carries `budgeted: true` plus a `next_cursor` to page them.
    #[serde(default, alias = "token_budget", alias = "budget")]
    pub max_tokens: Option<u32>,
    /// Wire format for the response: `"json"` (default) or `"toon"`. TOON is a compact
    /// tabular encoding of the `results` list — far fewer tokens than JSON for large hit sets.
    #[serde(default, alias = "encoding")]
    pub format: Option<String>,
    /// Resume token returned by the previous call's `next_cursor`. Cursors are scoped to
    /// the in-RAM index snapshot and invalidate on rescan.
    #[serde(default)]
    pub cursor: Option<Cursor>,
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
    /// Optional token budget bounding the returned file list (not the whole envelope).
    /// Entries are kept in order until the budget is hit; the rest are dropped and the
    /// response carries `budgeted: true` plus a `next_cursor` to page them.
    #[serde(default, alias = "token_budget", alias = "budget")]
    pub max_tokens: Option<u32>,
    /// Wire format for the response: `"json"` (default) or `"toon"`. TOON is a compact
    /// tabular encoding of the `files` list — far fewer tokens than JSON for large listings.
    #[serde(default, alias = "encoding")]
    pub format: Option<String>,
    /// Resume token returned by the previous call's `next_cursor`. Cursors are scoped to
    /// the in-RAM index snapshot and invalidate on rescan.
    #[serde(default)]
    pub cursor: Option<Cursor>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct DependentsParams {
    /// Module / import target (e.g. "tokio::sync" or "react").
    #[serde(alias = "name", alias = "query", alias = "import")]
    pub module: String,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct StatusParams {}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct RepoInfoParams {}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct FindReferencesParams {
    /// The callee identifier to look up. Substring match — case-sensitive, no scope
    /// resolution; both `Foo::bar()` and `bar()` register as callee `"bar"`. Use with
    /// caution on common names like `new` or `get`.
    #[serde(alias = "needle", alias = "pattern", alias = "query", alias = "symbol", alias = "q")]
    pub name: String,
    /// Cap on results returned. Default 100, max 1000.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Optional token budget bounding the returned `hits` list (not the whole envelope).
    /// Hits are kept in scan order until the budget is hit; the rest are dropped and the
    /// response carries `budgeted: true` plus a `next_cursor` to page them.
    #[serde(default, alias = "token_budget", alias = "budget")]
    pub max_tokens: Option<u32>,
    /// Wire format for the response: `"json"` (default) or `"toon"`. TOON is a compact
    /// tabular encoding of the `hits` list — far fewer tokens than JSON for large hit sets.
    #[serde(default, alias = "encoding")]
    pub format: Option<String>,
    /// Resume token returned by the previous call's `next_cursor`. Stable across rescans
    /// because the underlying Fjall keys are content-addressed.
    #[serde(default)]
    pub cursor: Option<Cursor>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct FindCallersParams {
    /// Repository-relative path of the definition file.
    pub path: RelPath,
    /// Name of the definition.
    #[serde(alias = "needle", alias = "query", alias = "symbol")]
    pub name: String,
    /// Optional kind filter for resolving the definition (function/method/class/...).
    #[serde(default)]
    pub kind: Option<String>,
    /// Cap on results returned. Default 100, max 1000.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Optional token budget bounding the returned `hits` list (not the whole envelope).
    /// Hits are kept in scan order until the budget is hit; the rest are dropped and the
    /// response carries `budgeted: true` plus a `next_cursor` to page them.
    #[serde(default, alias = "token_budget", alias = "budget")]
    pub max_tokens: Option<u32>,
    /// Resume token returned by the previous call's `next_cursor`. Stable across rescans
    /// because the underlying Fjall keys are content-addressed.
    #[serde(default)]
    pub cursor: Option<Cursor>,
}

pub(super) fn default_true() -> bool {
    true
}

// ─── Response shapes ─────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub(super) struct OutlineResponse {
    pub path: RelPath,
    pub language: String,
    pub size_bytes: u64,
    pub had_errors: bool,
    pub error_count: u32,
    /// True when a `max_tokens` budget dropped trailing `symbols`. Outline has no cursor;
    /// raise `max_tokens` (or omit it) to retrieve the full symbol list.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    pub symbols: Vec<SymbolView>,
    pub imports: Vec<ImportView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub calls: Option<Vec<CallView>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub docs: Option<Vec<DocView>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub l2_status: Option<&'static str>,
}

#[derive(Debug, Serialize)]
pub(super) struct SymbolView {
    pub name: String,
    pub kind: &'static str,
    pub start_row: u32,
    pub start_col: u32,
    pub start_byte: u32,
    pub end_byte: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

#[derive(Debug, Serialize)]
pub(super) struct ImportView {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
    pub raw: String,
    pub start_byte: u32,
}

#[derive(Debug, Serialize)]
pub(super) struct CallView {
    pub callee: String,
    pub start_byte: u32,
}

#[derive(Debug, Serialize)]
pub(super) struct DocView {
    pub text: String,
    pub start_byte: u32,
}

#[derive(Debug, Serialize)]
pub(super) struct SearchHitView {
    pub path: RelPath,
    pub name: String,
    pub kind: &'static str,
    pub start_row: u32,
    pub start_col: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

#[derive(Debug, Serialize)]
pub(super) struct SearchResponse {
    /// Matches scanned up to the per-call cap (`limit * 64`, min 2000) — NOT the global
    /// corpus total. When the cap is hit this is a lower bound; see `total_is_partial`.
    pub total: usize,
    /// True when the scan stopped at the cap, so `total` is a lower bound rather than the
    /// exact number of matching symbols in the corpus (bug #16).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub total_is_partial: bool,
    pub truncated: bool,
    /// True when a `max_tokens` budget dropped trailing results. The kept prefix is
    /// best-first; page the rest with `next_cursor`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    pub results: Vec<SearchHitView>,
    /// Opaque cursor to pass back on the next call when more results are available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    /// True when the caller passed a `cursor` minted against a different in-RAM snapshot
    /// (a rescan happened between calls). The caller must restart pagination from the top.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub cursor_invalidated: bool,
}

#[derive(Debug, Serialize)]
pub(super) struct ListFilesEntry {
    pub path: RelPath,
    pub language: String,
    pub size_bytes: u64,
}

#[derive(Debug, Serialize)]
pub(super) struct ListFilesResponse {
    pub total: usize,
    pub returned: usize,
    pub truncated: bool,
    /// True when the caller's requested `limit` exceeded the hard cap (`LIST_LIMIT_MAX`) and
    /// was clamped down to it. Surfaced so callers know the page size was reduced (bug #17).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub limit_clamped: bool,
    /// True when a `max_tokens` budget dropped trailing files. Page the rest with `next_cursor`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    pub files: Vec<ListFilesEntry>,
    /// Opaque cursor to pass back on the next call when more results are available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    /// True when the caller passed a `cursor` minted against a different in-RAM snapshot
    /// (a rescan happened between calls). The caller must restart pagination from the top.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub cursor_invalidated: bool,
}

#[derive(Debug, Serialize)]
pub(super) struct DependentsResponse {
    pub module: String,
    pub paths: Vec<RelPath>,
}

#[derive(Debug, Serialize)]
pub(super) struct StatusResponse {
    pub file_count: usize,
    /// Count of content-addressed blob files in `.basemind/blobs/` (one `.fm.msgpack` per
    /// indexed content hash). Reported alongside `file_count` so a lost/empty view index over
    /// live blobs is visible rather than silently reading `file_count: 0` (bug #10).
    pub blob_count: usize,
    /// One-line advisory, present only when the view index is empty but blobs exist on disk
    /// (index lost/wiped) — suggests a rescan. Absent for a populated or legitimately
    /// unscanned (no-blobs) view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// `true` when a writer (a running `scan`/`rescan`/`watch`) currently holds the store
    /// lock, so this report was served *without* blocking on the in-progress rebuild. The
    /// index counts (`file_count`, `languages`) reflect the pre-rebuild state or are omitted;
    /// `blob_count` is still read fresh from disk. Absent (false) on the common uncontended
    /// path — status then reflects the fully-committed index.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rebuild_in_progress: bool,
    pub total_size_bytes: u64,
    pub languages: BTreeMap<String, usize>,
    pub cache_dir: String,
    pub schema_version: u16,
    pub root: String,
    /// Forward-slash worktree roots of every submodule declared in `.gitmodules`. Always
    /// reported regardless of `scan.skip_submodules` — lets clients see the boundary the
    /// scanner respects (or didn't, when the knob is disabled). Empty for repos with no
    /// submodules and for non-repo serves.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub submodules: Vec<RelPath>,
}

#[derive(Debug, Serialize)]
pub(super) struct ReferenceHit {
    pub path: RelPath,
    /// 1-based.
    pub line: u32,
    /// 0-based byte column from the start of the line.
    pub column: u32,
    /// The exact callee identifier the index captured.
    pub callee: String,
}

#[derive(Debug, Serialize)]
pub(super) struct FindReferencesResponse {
    pub name: String,
    pub total: u32,
    /// True when `total` was capped at `limit` and more matches exist on disk.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub total_is_partial: bool,
    /// True when a `max_tokens` budget dropped trailing `hits`. Page the rest with `next_cursor`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    pub hits: Vec<ReferenceHit>,
    /// Opaque cursor to pass back on the next call when more results are available.
    /// Stable across rescans.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
}

#[derive(Debug, Serialize)]
pub(super) struct FindCallersResponse {
    /// Echo of the definition we resolved before scanning for callers.
    pub definition: Option<DefinitionView>,
    pub total: u32,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub total_is_partial: bool,
    /// True when a `max_tokens` budget dropped trailing `hits`. Page the rest with `next_cursor`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    pub hits: Vec<ReferenceHit>,
    /// Opaque cursor to pass back on the next call when more results are available.
    /// Stable across rescans.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
}

#[derive(Debug, Serialize)]
pub(super) struct DefinitionView {
    pub path: RelPath,
    pub name: String,
    pub kind: &'static str,
    pub start_row: u32,
    pub start_col: u32,
}

#[derive(Debug, Serialize)]
pub(super) struct RepoInfoResponse {
    pub workdir: String,
    pub head_sha: Option<String>,
    pub head_short_sha: Option<String>,
    pub branch: Option<String>,
}

// ─── Document-search shapes ──────────────────────────────────────────────────
//
// Memory tool shapes (`Memory*Params/Response`, `Visibility`) live in `types_memory.rs`.

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct WorkspaceGrepParams {
    /// Rust regex syntax (`regex` crate). Required.
    #[serde(alias = "query", alias = "needle", alias = "regex", alias = "q", alias = "search")]
    pub pattern: String,
    /// Optional language filter (e.g. `"rust"`, `"typescript"`). Same ID convention as
    /// `list_files`.
    #[serde(default)]
    pub language: Option<String>,
    /// Optional substring filter on path. Same convention as `list_files`.
    #[serde(default)]
    pub path_contains: Option<String>,
    /// Max number of hits returned. Default 100, max 1000. Files visited are bounded
    /// by `scan_cap = limit * 8`.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Optional token budget bounding the returned `hits` list (not the whole envelope).
    /// Hits are kept in scan order until the budget is hit; the rest are dropped and the
    /// response carries `budgeted: true` plus a `next_cursor` to page them.
    #[serde(default, alias = "token_budget", alias = "budget")]
    pub max_tokens: Option<u32>,
    /// Wire format for the response: `"json"` (default) or `"toon"`. TOON is a compact
    /// tabular encoding of the `hits` list — far fewer tokens than JSON for large hit sets.
    #[serde(default, alias = "encoding")]
    pub format: Option<String>,
    /// Include 1 line of context before + after each hit. Default true.
    #[serde(default = "default_true")]
    pub include_context: bool,
    /// Resume token returned by the previous call's `next_cursor`. Cursors are scoped to
    /// the in-RAM index snapshot and invalidate on rescan.
    #[serde(default)]
    pub cursor: Option<Cursor>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct GrepHit {
    pub path: RelPath,
    /// 1-based line number of the match.
    pub line_num: u32,
    /// 0-based byte column within the line.
    pub column: u32,
    /// The exact matched substring from the source.
    pub matched_text: String,
    /// The line immediately before the match, when `include_context` is true and line > 1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_before: Option<String>,
    /// The line immediately after the match, when `include_context` is true and line < EOF.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_after: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct WorkspaceGrepResponse {
    /// Echoed pattern from the request.
    pub pattern: String,
    /// Number of files that had at least one match.
    pub total_files_matched: usize,
    /// Total hit count across all visited files (may exceed `hits.len()` when truncated).
    pub total_matches: u32,
    /// True when the result was cut short by `limit` or `scan_cap`.
    pub truncated: bool,
    /// True when a `max_tokens` budget dropped trailing `hits`. Page the rest with `next_cursor`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    pub hits: Vec<GrepHit>,
    /// Opaque cursor to pass back on the next call when more results are available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    /// True when the caller passed a `cursor` minted against a different in-RAM snapshot
    /// (a rescan happened between calls). The caller must restart pagination from the top.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub cursor_invalidated: bool,
}

// ─── rescan ──────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct RescanParams {
    /// Optional list of repo-relative paths to scope the rescan. When omitted
    /// the full repo is walked. Paths are forward-slash with no leading `/`.
    #[serde(default)]
    pub paths: Option<Vec<String>>,
    /// Force a complete working-tree re-index even when `paths` is supplied (full wins).
    /// Use when the index is stale or reports "no indexed files" and a scoped rescan won't
    /// rebuild it.
    #[serde(default)]
    pub full: bool,
}

#[derive(Debug, Serialize)]
pub(super) struct RescanResponse {
    pub scanned: usize,
    pub updated: usize,
    pub removed: usize,
    pub skipped_unchanged: usize,
    pub skipped_no_lang: usize,
    pub extract_failed: usize,
    pub elapsed_ms: u128,
    pub root: String,
}

// ─── telemetry_summary ───────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TelemetrySummaryParams {
    /// Time window for aggregation. `"today"` (default — since 00:00 local),
    /// `"1h"` (last hour), `"all"` (no window).
    #[serde(default)]
    pub window: Option<String>,
    /// Optional exact tool-name filter (e.g. `"outline"`).
    #[serde(default)]
    pub tool: Option<String>,
}

#[derive(Debug, Serialize)]
pub(super) struct TelemetrySummaryResponse {
    pub window: String,
    pub total_calls: usize,
    pub total_resp_bytes: u64,
    pub total_est_tokens_saved: u64,
    pub per_tool: Vec<ToolCallCount>,
    pub per_baseline: Vec<BaselineCount>,
    pub recent: Vec<RecentCallView>,
    /// True when the JSONL grew past the in-memory read cap and the dashboard
    /// only inspected the tail. Aggregates are still over the inspected slice.
    pub truncated: bool,
    /// Disclosure of the estimator model — read by `/basemind-stats --explain`
    /// to remind the user that savings numbers are heuristic.
    pub savings_note: &'static str,
}

#[derive(Debug, Serialize)]
pub(super) struct ToolCallCount {
    pub tool: String,
    pub calls: usize,
    pub est_tokens_saved: u64,
}

#[derive(Debug, Serialize)]
pub(super) struct BaselineCount {
    pub baseline: String,
    pub calls: usize,
    pub est_tokens_saved: u64,
}

#[derive(Debug, Serialize)]
pub(super) struct RecentCallView {
    pub ts_micros: i64,
    pub tool: String,
    pub resp_bytes: u64,
    pub elapsed_ms: u64,
    pub est_tokens_saved: u64,
}

// ─── web_scrape / web_crawl / web_map ────────────────────────────────────────

#[cfg(feature = "crawl")]
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct WebScrapeParams {
    /// Absolute http or https URL to fetch.
    pub url: crate::url::Url,
    /// When true (default), chunk + embed + write to LanceDB so the page is
    /// reachable via `search_documents`. When false, fetch and return metadata
    /// only — useful for previewing a URL before paying the embedding cost.
    #[serde(default = "WebScrapeParams::default_index")]
    pub index: bool,
    /// LanceDB `scope` tag. Default `"web:<host>"`. Override to share a scope
    /// across many hosts or to namespace per project.
    #[serde(default)]
    pub scope: Option<String>,
}

#[cfg(feature = "crawl")]
impl WebScrapeParams {
    fn default_index() -> bool {
        true
    }
}

#[cfg(feature = "crawl")]
#[derive(Debug, Serialize)]
pub(super) struct WebScrapeResponse {
    pub url: String,
    pub final_url: String,
    pub status_code: u16,
    pub content_type: String,
    pub bytes: usize,
    pub chunks_indexed: usize,
    pub indexed: bool,
    pub scope: String,
}

#[cfg(feature = "crawl")]
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct WebCrawlParams {
    /// Seed URL. The crawler follows links breadth-first from this page.
    pub url: crate::url::Url,
    /// Overrides the global `[crawl].max_pages` cap for this call only.
    #[serde(default)]
    pub max_pages: Option<u32>,
    /// Overrides the global `[crawl].max_depth` cap for this call only.
    #[serde(default)]
    pub max_depth: Option<u32>,
    /// LanceDB `scope` tag. Default `"web:<host>"` derived from the seed URL's
    /// host. Every page indexed by this crawl uses the same scope so
    /// `search_documents { scope: ... }` retrieves them together.
    #[serde(default)]
    pub scope: Option<String>,
}

#[cfg(feature = "crawl")]
#[derive(Debug, Serialize)]
pub(super) struct WebCrawlResponse {
    pub seed_url: String,
    pub pages_visited: usize,
    pub pages_indexed: usize,
    pub total_chunks: usize,
    pub scope: String,
    /// Per-page indexing outcomes — surfaced so an agent can tell which URLs
    /// landed in LanceDB vs which were skipped (binary content, empty body).
    pub pages: Vec<WebCrawlPageOutcome>,
    /// Crawl-level error, if any (e.g. seed URL unreachable). Per-page errors
    /// land in `pages[*].error` instead.
    pub error: Option<String>,
}

#[cfg(feature = "crawl")]
#[derive(Debug, Serialize)]
pub(super) struct WebCrawlPageOutcome {
    pub url: String,
    pub status_code: u16,
    pub chunks_indexed: usize,
    pub indexed: bool,
    pub error: Option<String>,
}

#[cfg(feature = "crawl")]
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct WebMapParams {
    /// Site to discover. Returns sitemap entries + linked URLs without
    /// fetching their bodies.
    pub url: crate::url::Url,
}

#[cfg(feature = "crawl")]
#[derive(Debug, Serialize)]
pub(super) struct WebMapResponse {
    pub url: String,
    pub total_urls: usize,
    pub urls: Vec<WebMapEntry>,
}

#[cfg(feature = "crawl")]
#[derive(Debug, Serialize)]
pub(super) struct WebMapEntry {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lastmod: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changefreq: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
}

pub use super::types_admin::{CacheClearParams, CacheGcParams, CacheStatsParams};
pub(super) use super::types_admin::{CacheClearResponse, CacheGcResponse, CacheStatsResponse};
// Git-tool types live in `types_git.rs`; re-exported here so existing `super::types::<T>`
// references (the `params` re-export in `mod.rs`, the `use super::types::*` glob in
// `tools_git.rs`, and the explicit imports in `helpers.rs`) keep resolving unchanged.
pub use super::types_documents::SearchDocumentsParams;
#[cfg(feature = "documents")]
pub(super) use super::types_documents::{DocumentSearchHit, SearchDocumentsResponse};
pub use super::types_git::{
    BlameFileParams, BlameSymbolParams, CommitsTouchingParams, DiffFileParams, DiffOutlineParams,
    FindCommitsByPathParams, HotFilesParams, RecentChangesParams, SearchGitHistoryParams, SymbolHistoryParams,
    WorkingTreeStatusParams,
};
pub(super) use super::types_git::{
    BlameHunkView, BlameResponse, BlameSymbolResponse, CommitFileView, CommitView, CommitsTouchingResponse,
    DiffFileResponse, DiffOutlineResponse, DiffSymbolView, FindCommitsByPathResponse, GitCommitHit, HotFileEntry,
    HotFilesResponse, HunkView, RecentChangesResponse, SearchGitHistoryResponse, SymbolHistoryEntry,
    SymbolHistoryResponse, WorkingTreeStatusView,
};
pub use super::types_graph::CallGraphParams;
pub use super::types_impls::FindImplementationsParams;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_grep_accepts_query_alias_for_pattern() {
        let params: WorkspaceGrepParams = serde_json::from_value(serde_json::json!({ "query": "foo" })).unwrap();
        assert_eq!(params.pattern, "foo");
    }

    #[test]
    fn search_symbols_accepts_pattern_alias_for_needle() {
        let params: SearchSymbolsParams = serde_json::from_value(serde_json::json!({ "pattern": "x" })).unwrap();
        assert_eq!(params.needle, "x");
    }

    #[test]
    fn search_symbols_accepts_symbol_alias_for_needle() {
        let params: SearchSymbolsParams = serde_json::from_value(serde_json::json!({ "symbol": "Foo" })).unwrap();
        assert_eq!(params.needle, "Foo");
    }

    #[test]
    fn workspace_grep_accepts_regex_and_needle_aliases() {
        let by_regex: WorkspaceGrepParams = serde_json::from_value(serde_json::json!({ "regex": "a.*b" })).unwrap();
        assert_eq!(by_regex.pattern, "a.*b");
        let by_needle: WorkspaceGrepParams = serde_json::from_value(serde_json::json!({ "needle": "lit" })).unwrap();
        assert_eq!(by_needle.pattern, "lit");
    }

    #[test]
    fn find_references_accepts_symbol_alias_for_name() {
        let params: FindReferencesParams = serde_json::from_value(serde_json::json!({ "symbol": "spawn" })).unwrap();
        assert_eq!(params.name, "spawn");
    }

    #[test]
    fn call_graph_accepts_query_alias_for_name() {
        let params: super::CallGraphParams = serde_json::from_value(serde_json::json!({ "query": "main" })).unwrap();
        assert_eq!(params.name, "main");
    }

    #[test]
    fn find_implementations_accepts_trait_alias_for_trait_name() {
        let params: super::FindImplementationsParams =
            serde_json::from_value(serde_json::json!({ "trait": "Iterator" })).unwrap();
        assert_eq!(params.trait_name, "Iterator");
    }
}
