//! Parameter shapes (deserialized from MCP tool-call arguments) and JSON response shapes
//! (serialized into tool-call results). Kept separate from `tools.rs` so the impl block
//! itself stays readable and within the per-file size budget.

use std::collections::BTreeMap;

use rmcp::schemars;
use serde::{Deserialize, Serialize};

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
    pub path: RelPath,
    /// Number of commits returned, newest first. Default 20, max 100.
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct DiffOutlineParams {
    /// Repository-relative path of the file to diff.
    pub path: RelPath,
    /// Revision to compare against the *current view*. Defaults to "HEAD".
    #[serde(default)]
    pub rev: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct RepoInfoParams {}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct BlameFileParams {
    pub path: RelPath,
    #[serde(default)]
    pub line_start: Option<u32>,
    #[serde(default)]
    pub line_end: Option<u32>,
    #[serde(default)]
    pub rev: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct FindCommitsByPathParams {
    pub pattern: String,
    #[serde(default)]
    pub window: Option<u32>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct HotFilesParams {
    #[serde(default)]
    pub window: Option<u32>,
    #[serde(default)]
    pub top_k: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct DiffFileParams {
    pub rev_old: String,
    pub rev_new: String,
    pub path: RelPath,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SymbolHistoryParams {
    pub path: RelPath,
    pub name: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
    /// Fingerprint strategy for detecting body changes between commits. One of
    /// `"normalized"` (default — byte compare after comment+whitespace strip),
    /// `"structural"` (AST shape + identifiers + literal text, formatter-stable), or
    /// `"structural_loose"` (AST shape + identifiers only, ignores literal contents —
    /// useful when i18n string churn dominates).
    #[serde(default)]
    pub hash_mode: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct FindReferencesParams {
    /// The callee identifier to look up. Substring match — case-sensitive, no scope
    /// resolution; both `Foo::bar()` and `bar()` register as callee `"bar"`. Use with
    /// caution on common names like `new` or `get`.
    pub name: String,
    /// Cap on results returned. Default 100, max 1000.
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct FindCallersParams {
    /// Repository-relative path of the definition file.
    pub path: RelPath,
    /// Name of the definition.
    pub name: String,
    /// Optional kind filter for resolving the definition (function/method/class/...).
    #[serde(default)]
    pub kind: Option<String>,
    /// Cap on results returned. Default 100, max 1000.
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct BlameSymbolParams {
    pub path: RelPath,
    pub name: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub rev: Option<String>,
}

fn default_true() -> bool {
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
    pub kind: String,
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
    pub kind: String,
    pub start_row: u32,
    pub start_col: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

#[derive(Debug, Serialize)]
pub(super) struct SearchResponse {
    pub total: usize,
    pub truncated: bool,
    pub results: Vec<SearchHitView>,
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
    pub files: Vec<ListFilesEntry>,
}

#[derive(Debug, Serialize)]
pub(super) struct DependentsResponse {
    pub module: String,
    pub paths: Vec<RelPath>,
}

#[derive(Debug, Serialize)]
pub(super) struct StatusResponse {
    pub file_count: usize,
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
pub(super) struct CommitView {
    pub sha: String,
    pub short_sha: String,
    pub summary: String,
    pub author: String,
    pub author_time_unix: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<CommitFileView>>,
}

#[derive(Debug, Serialize)]
pub(super) struct CommitFileView {
    pub path: RelPath,
    pub change: &'static str,
}

#[derive(Debug, Serialize)]
pub(super) struct WorkingTreeStatusView {
    pub staged_added: Vec<RelPath>,
    pub staged_modified: Vec<RelPath>,
    pub staged_deleted: Vec<RelPath>,
    pub modified: Vec<RelPath>,
    pub untracked: Vec<RelPath>,
    pub is_clean: bool,
}

#[derive(Debug, Serialize)]
pub(super) struct RecentChangesResponse {
    pub commits: Vec<CommitView>,
    /// `true` when the walk may have stopped early (today: shallow clone). Agents should
    /// treat the absence of an expected commit as inconclusive when this is set.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<&'static str>,
}

#[derive(Debug, Serialize)]
pub(super) struct CommitsTouchingResponse {
    pub path: RelPath,
    pub commits: Vec<CommitView>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<&'static str>,
}

#[derive(Debug, Serialize)]
pub(super) struct DiffSymbolView {
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Serialize)]
pub(super) struct DiffOutlineResponse {
    pub path: RelPath,
    pub rev: String,
    pub added: Vec<DiffSymbolView>,
    pub removed: Vec<DiffSymbolView>,
    pub common: Vec<DiffSymbolView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Serialize)]
pub(super) struct BlameHunkView {
    pub commit_sha: String,
    pub short_sha: String,
    pub start_line: u32,
    pub len: u32,
    pub source_start_line: u32,
    pub author: String,
    pub author_time_unix: i64,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_path: Option<RelPath>,
}

#[derive(Debug, Serialize)]
pub(super) struct BlameResponse {
    pub path: RelPath,
    pub suspect_sha: String,
    pub hunks: Vec<BlameHunkView>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<&'static str>,
}

#[derive(Debug, Serialize)]
pub(super) struct BlameSymbolResponse {
    pub path: RelPath,
    pub suspect_sha: String,
    pub name: String,
    pub kind: String,
    pub line_start: u32,
    pub line_end: u32,
    pub hunks: Vec<BlameHunkView>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<&'static str>,
}

#[derive(Debug, Serialize)]
pub(super) struct FindCommitsByPathResponse {
    pub pattern: String,
    pub window_inspected: u32,
    pub commits: Vec<CommitView>,
}

#[derive(Debug, Serialize)]
pub(super) struct HotFileEntry {
    pub path: RelPath,
    pub commits_touching: u32,
    pub added: u32,
    pub modified: u32,
    pub deleted: u32,
}

#[derive(Debug, Serialize)]
pub(super) struct HotFilesResponse {
    pub window_inspected: u32,
    pub total_files_changed: u32,
    pub files: Vec<HotFileEntry>,
}

#[derive(Debug, Serialize)]
pub(super) struct HunkView {
    pub kind: &'static str,
    pub old_line_start: u32,
    pub old_line_count: u32,
    pub new_line_start: u32,
    pub new_line_count: u32,
    pub text: String,
}

#[derive(Debug, Serialize)]
pub(super) struct DiffFileResponse {
    pub path: RelPath,
    pub rev_old: String,
    pub rev_new: String,
    pub present_at_old: bool,
    pub present_at_new: bool,
    pub hunks: Vec<HunkView>,
}

#[derive(Debug, Serialize)]
pub(super) struct SymbolHistoryEntry {
    pub sha: String,
    pub short_sha: String,
    pub summary: String,
    pub author: String,
    pub author_time_unix: i64,
    pub change: &'static str,
}

#[derive(Debug, Serialize)]
pub(super) struct SymbolHistoryResponse {
    pub path: RelPath,
    pub name: String,
    pub kind: Option<String>,
    pub commits_inspected: u32,
    pub history: Vec<SymbolHistoryEntry>,
    /// Echoes the fingerprint strategy that produced this response — `"normalized"`,
    /// `"structural"`, or `"structural_loose"`. Clients can use this to confirm the mode
    /// they got matches the mode they asked for.
    pub hash_mode: &'static str,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<&'static str>,
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
    pub hits: Vec<ReferenceHit>,
}

#[derive(Debug, Serialize)]
pub(super) struct FindCallersResponse {
    /// Echo of the definition we resolved before scanning for callers.
    pub definition: Option<DefinitionView>,
    pub total: u32,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub total_is_partial: bool,
    pub hits: Vec<ReferenceHit>,
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

// ─── Memory + document-search shapes ─────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MemoryPutParams {
    pub key: String,
    pub value: String,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default = "default_true")]
    pub embed: bool,
}

#[cfg(feature = "memory")]
#[derive(Debug, Serialize)]
pub(super) struct MemoryPutResponse {
    pub key: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MemoryGetParams {
    pub key: String,
}

#[cfg(feature = "memory")]
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct MemoryEntry {
    pub key: String,
    pub value: String,
    pub tags: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MemoryListParams {
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[cfg(feature = "memory")]
#[derive(Debug, Serialize)]
pub(super) struct MemoryListResponse {
    pub total: usize,
    pub truncated: bool,
    pub entries: Vec<MemoryEntry>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MemorySearchParams {
    pub query: String,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub tag: Option<String>,
}

#[cfg(feature = "memory")]
#[derive(Debug, Serialize)]
pub(super) struct MemorySearchHit {
    pub key: String,
    pub value: String,
    pub tags: Vec<String>,
    pub distance: f32,
}

#[cfg(feature = "memory")]
#[derive(Debug, Serialize)]
pub(super) struct MemorySearchResponse {
    pub query: String,
    pub hits: Vec<MemorySearchHit>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MemoryDeleteParams {
    pub key: String,
}

#[cfg(feature = "memory")]
#[derive(Debug, Serialize)]
pub(super) struct MemoryDeleteResponse {
    pub deleted: bool,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SearchDocumentsParams {
    pub query: String,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub mime_type: Option<String>,
}

#[cfg(feature = "documents")]
#[derive(Debug, Serialize)]
pub(super) struct DocumentSearchHit {
    pub path: String,
    pub chunk_idx: u32,
    pub text: String,
    pub mime_type: String,
    pub byte_start: u32,
    pub byte_end: u32,
    pub distance: f32,
}

#[cfg(feature = "documents")]
#[derive(Debug, Serialize)]
pub(super) struct SearchDocumentsResponse {
    pub query: String,
    pub hits: Vec<DocumentSearchHit>,
}

#[cfg(feature = "memory")]
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct MemoryRecord {
    pub value: String,
    pub tags: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

// ─── rescan ──────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct RescanParams {
    /// Optional list of repo-relative paths to scope the rescan. When omitted
    /// the full repo is walked. Paths are forward-slash with no leading `/`.
    #[serde(default)]
    pub paths: Option<Vec<String>>,
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
