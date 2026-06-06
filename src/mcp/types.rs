//! Parameter shapes (deserialized from MCP tool-call arguments) and JSON response shapes
//! (serialized into tool-call results). Kept separate from `tools.rs` so the impl block
//! itself stays readable and within the per-file size budget.

use std::collections::BTreeMap;

use rmcp::schemars;
use serde::{Deserialize, Serialize};

// ─── Parameter shapes ────────────────────────────────────────────────────────

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

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct BlameFileParams {
    pub path: String,
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
    pub path: String,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SymbolHistoryParams {
    pub path: String,
    pub name: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct BlameSymbolParams {
    pub path: String,
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
    pub path: String,
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
    pub path: String,
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
    pub path: String,
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
    pub paths: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(super) struct StatusResponse {
    pub file_count: usize,
    pub total_size_bytes: u64,
    pub languages: BTreeMap<String, usize>,
    pub cache_dir: String,
    pub schema_version: u16,
    pub root: String,
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
    pub path: String,
    pub change: &'static str,
}

#[derive(Debug, Serialize)]
pub(super) struct WorkingTreeStatusView {
    pub staged_added: Vec<String>,
    pub staged_modified: Vec<String>,
    pub staged_deleted: Vec<String>,
    pub modified: Vec<String>,
    pub untracked: Vec<String>,
    pub is_clean: bool,
}

#[derive(Debug, Serialize)]
pub(super) struct RecentChangesResponse {
    pub commits: Vec<CommitView>,
}

#[derive(Debug, Serialize)]
pub(super) struct CommitsTouchingResponse {
    pub path: String,
    pub commits: Vec<CommitView>,
}

#[derive(Debug, Serialize)]
pub(super) struct DiffSymbolView {
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Serialize)]
pub(super) struct DiffOutlineResponse {
    pub path: String,
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
    pub source_path: Option<String>,
}

#[derive(Debug, Serialize)]
pub(super) struct BlameResponse {
    pub path: String,
    pub suspect_sha: String,
    pub hunks: Vec<BlameHunkView>,
}

#[derive(Debug, Serialize)]
pub(super) struct BlameSymbolResponse {
    pub path: String,
    pub suspect_sha: String,
    pub name: String,
    pub kind: String,
    pub line_start: u32,
    pub line_end: u32,
    pub hunks: Vec<BlameHunkView>,
}

#[derive(Debug, Serialize)]
pub(super) struct FindCommitsByPathResponse {
    pub pattern: String,
    pub window_inspected: u32,
    pub commits: Vec<CommitView>,
}

#[derive(Debug, Serialize)]
pub(super) struct HotFileEntry {
    pub path: String,
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
    pub path: String,
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
    pub path: String,
    pub name: String,
    pub kind: Option<String>,
    pub commits_inspected: u32,
    pub history: Vec<SymbolHistoryEntry>,
}

#[derive(Debug, Serialize)]
pub(super) struct RepoInfoResponse {
    pub workdir: String,
    pub head_sha: Option<String>,
    pub head_short_sha: Option<String>,
    pub branch: Option<String>,
}
