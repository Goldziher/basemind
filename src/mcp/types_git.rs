//! Internal request shapes and response payloads of the git bodies.
//!
//! The wire arguments of `git_log` / `git_blame` / `git_diff` live in `types_tools.rs`; the
//! per-operation structs below (`RecentChangesParams`, `BlameFileParams`, …) are what the bodies in
//! `helpers_git.rs` / `helpers_git_file.rs` take.
//!
//! Split out of `types.rs` to keep both files within the per-file size budget; the public paths stay
//! stable via re-exports in `types.rs`.

use rmcp::schemars;
use serde::{Deserialize, Serialize};

use super::cursor::Cursor;
use super::types::default_true;
use crate::path::RelPath;

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SearchGitHistoryParams {
    /// Full-text query over commit history. Tokenized (lowercased, split on non-alphanumeric) and
    /// matched as an AND — a commit is returned only when EVERY query token is present in the
    /// scoped field. `"null deref"` requires both `null` and `deref`; `"jane@example.com"` matches
    /// commits whose author tokenizes to `jane`, `example`, and `com`.
    pub pattern: String,
    /// Which field to search: `author` (name + email), `message` (summary + body), or `all`
    /// (default). `summary` / `body` are accepted as aliases for `message`.
    #[serde(default)]
    pub field: Option<String>,
    /// Max commits to return. Default 20, max 100.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Resume token returned by the previous call's `next_cursor`. Cursors are scoped to the repo's
    /// HEAD sha at mint time; on HEAD movement the response carries `cursor_invalidated: true` and
    /// the caller must restart.
    #[serde(default)]
    pub cursor: Option<Cursor>,
    /// Token budget for the returned hits. Default 20000; overflow is dropped, `budgeted: true`
    /// is set and `next_cursor` resumes at the first dropped hit.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Cap on each hit's commit body, in characters. Default 1000; 0 omits the body.
    #[serde(default)]
    pub body_chars: Option<u32>,
    /// Terms every hit's author name or email must also contain. `pattern` is matched against its
    /// own `field`; these are matched against the author only, never the message.
    #[serde(default)]
    pub author: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct RecentChangesParams {
    /// Number of commits to walk back from HEAD. Default 20, max 100.
    #[serde(default)]
    pub limit: Option<u32>,
    /// When true, include the per-file change list for each commit. Default true.
    #[serde(default = "default_true")]
    pub include_files: bool,
    /// Resume token returned by the previous call's `next_cursor`. Cursors are scoped to
    /// the repo's HEAD sha at mint time; on HEAD movement the response carries
    /// `cursor_invalidated: true` and the caller must restart.
    #[serde(default)]
    pub cursor: Option<Cursor>,
    /// Token budget for the returned commits. Default 20000; overflow is dropped, `budgeted: true`
    /// is set and `next_cursor` resumes at the first dropped commit.
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct CommitsTouchingParams {
    /// Repository-relative path (forward-slash) of the file to follow.
    pub path: RelPath,
    /// Number of commits returned, newest first. Default 20, max 100.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Resume token returned by the previous call's `next_cursor`. Cursors are scoped to
    /// the repo's HEAD sha at mint time; on HEAD movement the response carries
    /// `cursor_invalidated: true` and the caller must restart.
    #[serde(default)]
    pub cursor: Option<Cursor>,
    /// Token budget for the returned commits. Default 20000; overflow is dropped, `budgeted: true`
    /// is set and `next_cursor` resumes at the first dropped commit.
    #[serde(default)]
    pub max_tokens: Option<u32>,
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
pub struct BlameFileParams {
    pub path: RelPath,
    #[serde(default)]
    pub line_start: Option<u32>,
    #[serde(default)]
    pub line_end: Option<u32>,
    #[serde(default)]
    pub rev: Option<String>,
    /// Cap on hunks returned per page. Default 100, max 1000.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Resume token returned by the previous call's `next_cursor`. Encodes the last-returned
    /// hunk's `start_line`; on resume the helper skips hunks whose `start_line <= offset`.
    #[serde(default)]
    pub cursor: Option<Cursor>,
    /// Token budget for the returned hunks; overflow is dropped and flagged `budgeted`.
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct FindCommitsByPathParams {
    /// Regular expression matched against each commit's changed **file paths** (not commit
    /// messages): a commit is returned when any path it touched matches. Invalid regex is a
    /// param error.
    pub pattern: String,
    #[serde(default)]
    pub window: Option<u32>,
    #[serde(default)]
    pub limit: Option<u32>,
    /// Resume token returned by the previous call's `next_cursor`. Cursors are scoped to
    /// the repo's HEAD sha at mint time; on HEAD movement the response carries
    /// `cursor_invalidated: true` and the caller must restart.
    #[serde(default)]
    pub cursor: Option<Cursor>,
    /// Token budget for the returned commits. Default 20000; overflow is dropped, `budgeted: true`
    /// is set and `next_cursor` resumes at the first dropped commit.
    #[serde(default)]
    pub max_tokens: Option<u32>,
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
    /// Old revision.
    pub rev_old: String,
    /// New revision; `None` compares against the working tree.
    pub rev_new: Option<String>,
    /// Repository-relative path.
    pub path: RelPath,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SymbolHistoryParams {
    pub path: RelPath,
    pub name: String,
    #[serde(default)]
    pub kind: Option<String>,
    /// 1-based line inside the definition meant, when several in the file share the name.
    #[serde(default)]
    pub line: Option<u32>,
    #[serde(default)]
    pub limit: Option<u32>,
    /// Fingerprint strategy for detecting body changes between commits. One of
    /// `"normalized"` (default — byte compare after comment+whitespace strip),
    /// `"structural"` (AST shape + identifiers + literal text, formatter-stable), or
    /// `"structural_loose"` (AST shape + identifiers only, ignores literal contents —
    /// useful when i18n string churn dominates).
    #[serde(default)]
    pub hash_mode: Option<String>,
    /// Resume token returned by the previous call's `next_cursor`. Cursors are scoped to
    /// the repo's HEAD sha at mint time; on HEAD movement the response carries
    /// `cursor_invalidated: true` and the caller must restart.
    #[serde(default)]
    pub cursor: Option<Cursor>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct BlameSymbolParams {
    pub path: RelPath,
    pub name: String,
    #[serde(default)]
    pub kind: Option<String>,
    /// 1-based line inside the definition meant, when several in the file share the name.
    #[serde(default)]
    pub line: Option<u32>,
    #[serde(default)]
    pub rev: Option<String>,
    /// Cap on hunks returned per page. Default 100, max 1000.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Resume token returned by the previous call's `next_cursor`. Encodes the last-returned
    /// hunk's `start_line`; on resume the helper skips hunks whose `start_line <= offset`.
    #[serde(default)]
    pub cursor: Option<Cursor>,
    /// Token budget for the returned hunks; overflow is dropped and flagged `budgeted`.
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(in crate::mcp) struct CommitView {
    pub sha: String,
    pub short_sha: String,
    pub summary: String,
    pub author: String,
    pub author_time_unix: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<CommitFileView>>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(in crate::mcp) struct CommitFileView {
    pub path: RelPath,
    pub change: &'static str,
}

/// A `search_git_history` hit — carries the author email and full commit body (the fields the FTS
/// index adds over the other git tools' [`CommitView`]).
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(in crate::mcp) struct GitCommitHit {
    pub sha: String,
    pub short_sha: String,
    pub summary: String,
    pub author: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub author_email: String,
    pub author_time_unix: i64,
    /// Full message body, present only for indexed hits with a non-empty body.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub body: String,
    /// True when `body` was cut at `body_chars`; it then ends in a marker naming `git_log` with `rev`.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub body_truncated: bool,
    /// Full body length in bytes, present only when `body_truncated`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_bytes: Option<usize>,
}

/// Response of `git_log` with `rev`: one commit with its whole message and changed-file list.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(in crate::mcp) struct GitShowResponse {
    pub commit: GitCommitHit,
    pub files: Vec<CommitFileView>,
    /// Server-side handler latency in microseconds. See [`crate::mcp::helpers::timing`].
    #[serde(default)]
    pub elapsed_us: u64,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(in crate::mcp) struct SearchGitHistoryResponse {
    pub commits: Vec<GitCommitHit>,
    /// `true` when served from the bounded live-walk fallback (the git-history index wasn't fresh),
    /// so results cover only the recent window and may omit body/email matches. Rescan to get the
    /// full indexed search.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub partial: bool,
    /// True when `max_tokens` (or the default budget) dropped trailing commits; `next_cursor`
    /// resumes at the first dropped one.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub cursor_invalidated: bool,
    /// Server-side handler latency in microseconds — the tool body's own execution (git walk /
    /// index lookup + response construction), excluding MCP transport, argument deserialization,
    /// and response serialization. A first call against a cold server also includes lazily
    /// building or loading the git-history index. See [`crate::mcp::helpers::timing`] for the
    /// full contract.
    #[serde(default)]
    pub elapsed_us: u64,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(in crate::mcp) struct WorkingTreeStatusView {
    pub staged_added: Vec<RelPath>,
    pub staged_modified: Vec<RelPath>,
    pub staged_deleted: Vec<RelPath>,
    pub modified: Vec<RelPath>,
    pub untracked: Vec<RelPath>,
    pub is_clean: bool,
    /// Server-side handler latency in microseconds — the tool body's own execution (git walk /
    /// index lookup + response construction), excluding MCP transport, argument deserialization,
    /// and response serialization. A first call against a cold server also includes lazily
    /// building or loading the git-history index. See [`crate::mcp::helpers::timing`] for the
    /// full contract.
    #[serde(default)]
    pub elapsed_us: u64,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(in crate::mcp) struct RecentChangesResponse {
    pub commits: Vec<CommitView>,
    /// `true` when the walk may have stopped early (today: shallow clone). Agents should
    /// treat the absence of an expected commit as inconclusive when this is set.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<&'static str>,
    /// Opaque cursor to pass back on the next call when more results are available.
    /// True when `max_tokens` (or the default budget) dropped trailing commits; `next_cursor`
    /// resumes at the first dropped one.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    /// True when the caller passed a `cursor` minted against a different HEAD sha (HEAD
    /// moved between calls). The caller must restart pagination from the top.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub cursor_invalidated: bool,
    /// Server-side handler latency in microseconds — the tool body's own execution (git walk /
    /// index lookup + response construction), excluding MCP transport, argument deserialization,
    /// and response serialization. A first call against a cold server also includes lazily
    /// building or loading the git-history index. See [`crate::mcp::helpers::timing`] for the
    /// full contract.
    #[serde(default)]
    pub elapsed_us: u64,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(in crate::mcp) struct CommitsTouchingResponse {
    pub path: RelPath,
    pub commits: Vec<CommitView>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<&'static str>,
    /// Opaque cursor to pass back on the next call when more results are available.
    /// True when `max_tokens` (or the default budget) dropped trailing commits; `next_cursor`
    /// resumes at the first dropped one.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    /// True when the caller passed a `cursor` minted against a different HEAD sha (HEAD
    /// moved between calls). The caller must restart pagination from the top.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub cursor_invalidated: bool,
    /// Server-side handler latency in microseconds — the tool body's own execution (git walk /
    /// index lookup + response construction), excluding MCP transport, argument deserialization,
    /// and response serialization. A first call against a cold server also includes lazily
    /// building or loading the git-history index. See [`crate::mcp::helpers::timing`] for the
    /// full contract.
    #[serde(default)]
    pub elapsed_us: u64,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(in crate::mcp) struct DiffSymbolView {
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(in crate::mcp) struct DiffOutlineResponse {
    pub path: RelPath,
    pub rev: String,
    pub added: Vec<DiffSymbolView>,
    pub removed: Vec<DiffSymbolView>,
    pub common: Vec<DiffSymbolView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Server-side handler latency in microseconds — the tool body's own execution (git walk /
    /// index lookup + response construction), excluding MCP transport, argument deserialization,
    /// and response serialization. A first call against a cold server also includes lazily
    /// building or loading the git-history index. See [`crate::mcp::helpers::timing`] for the
    /// full contract.
    #[serde(default)]
    pub elapsed_us: u64,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(in crate::mcp) struct BlameHunkView {
    pub commit_sha: String,
    pub short_sha: String,
    /// 1-based first line of the hunk in the blamed file.
    #[serde(rename = "line")]
    pub start_line: u32,
    /// 1-based last line of the hunk, inclusive.
    pub end_line: u32,
    /// 1-based line the hunk starts at in the commit that introduced it.
    #[serde(rename = "source_line")]
    pub source_start_line: u32,
    pub author: String,
    pub author_time_unix: i64,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_path: Option<RelPath>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(in crate::mcp) struct BlameResponse {
    pub path: RelPath,
    pub suspect_sha: String,
    pub hunks: Vec<BlameHunkView>,
    /// True when the token budget dropped trailing hunks; `next_cursor` resumes at the first one.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<&'static str>,
    /// Opaque cursor to pass back on the next call when more hunks are available. Encodes
    /// the last-returned hunk's `start_line` so the next page resumes immediately after.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    /// Server-side handler latency in microseconds — the tool body's own execution (git walk /
    /// index lookup + response construction), excluding MCP transport, argument deserialization,
    /// and response serialization. A first call against a cold server also includes lazily
    /// building or loading the git-history index. See [`crate::mcp::helpers::timing`] for the
    /// full contract.
    #[serde(default)]
    pub elapsed_us: u64,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(in crate::mcp) struct BlameSymbolResponse {
    pub path: RelPath,
    pub suspect_sha: String,
    pub name: String,
    pub kind: String,
    /// 1-based first line of the symbol, as `code_outline` reports it.
    #[serde(rename = "line")]
    pub line_start: u32,
    /// 1-based last line of the symbol, inclusive.
    #[serde(rename = "end_line")]
    pub line_end: u32,
    pub hunks: Vec<BlameHunkView>,
    /// True when the token budget dropped trailing hunks; `next_cursor` resumes at the first one.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<&'static str>,
    /// Opaque cursor to pass back on the next call when more hunks are available. Encodes
    /// the last-returned hunk's `start_line` so the next page resumes immediately after.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    /// Server-side handler latency in microseconds — the tool body's own execution (git walk /
    /// index lookup + response construction), excluding MCP transport, argument deserialization,
    /// and response serialization. A first call against a cold server also includes lazily
    /// building or loading the git-history index. See [`crate::mcp::helpers::timing`] for the
    /// full contract.
    #[serde(default)]
    pub elapsed_us: u64,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(in crate::mcp) struct FindCommitsByPathResponse {
    pub pattern: String,
    pub window_inspected: u32,
    /// True when the whole window was used up, so older commits were not inspected.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<&'static str>,
    pub commits: Vec<CommitView>,
    /// Opaque cursor to pass back on the next call when more matches are available.
    /// True when `max_tokens` (or the default budget) dropped trailing commits; `next_cursor`
    /// resumes at the first dropped one.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    /// True when the caller passed a `cursor` minted against a different HEAD sha (HEAD
    /// moved between calls). The caller must restart pagination from the top.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub cursor_invalidated: bool,
    /// Server-side handler latency in microseconds — the tool body's own execution (git walk /
    /// index lookup + response construction), excluding MCP transport, argument deserialization,
    /// and response serialization. A first call against a cold server also includes lazily
    /// building or loading the git-history index. See [`crate::mcp::helpers::timing`] for the
    /// full contract.
    #[serde(default)]
    pub elapsed_us: u64,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(in crate::mcp) struct HotFileEntry {
    pub path: RelPath,
    pub commits_touching: u32,
    pub added: u32,
    pub modified: u32,
    pub deleted: u32,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(in crate::mcp) struct HotFilesResponse {
    pub window_inspected: u32,
    pub total_files_changed: u32,
    pub files: Vec<HotFileEntry>,
    /// Server-side handler latency in microseconds — the tool body's own execution (git walk /
    /// index lookup + response construction), excluding MCP transport, argument deserialization,
    /// and response serialization. A first call against a cold server also includes lazily
    /// building or loading the git-history index. See [`crate::mcp::helpers::timing`] for the
    /// full contract.
    #[serde(default)]
    pub elapsed_us: u64,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(in crate::mcp) struct HunkView {
    pub kind: &'static str,
    pub old_line_start: u32,
    pub old_line_count: u32,
    pub new_line_start: u32,
    pub new_line_count: u32,
    pub text: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(in crate::mcp) struct DiffFileResponse {
    pub path: RelPath,
    /// Full sha of the old side.
    pub from: String,
    /// Full sha of the new side, or `working tree`.
    pub to: String,
    pub present_at_from: bool,
    pub present_at_to: bool,
    /// True for a binary file, which has no text hunks.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub binary: bool,
    /// True when a side is over the diff size ceiling: the file was not compared and has no hunks.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub too_large: bool,
    /// True when `hunks` was cut at the hunk or byte cap, or the one hunk returned had its text
    /// clipped to the byte cap; `total_hunks` is the full count.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_hunks: Option<usize>,
    pub hunks: Vec<HunkView>,
    /// Server-side handler latency in microseconds — the tool body's own execution (git walk /
    /// index lookup + response construction), excluding MCP transport, argument deserialization,
    /// and response serialization. A first call against a cold server also includes lazily
    /// building or loading the git-history index. See [`crate::mcp::helpers::timing`] for the
    /// full contract.
    #[serde(default)]
    pub elapsed_us: u64,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(in crate::mcp) struct SymbolHistoryEntry {
    pub sha: String,
    pub short_sha: String,
    pub summary: String,
    pub author: String,
    pub author_time_unix: i64,
    pub change: &'static str,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(in crate::mcp) struct SymbolHistoryResponse {
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
    /// Opaque cursor to pass back on the next call when more history entries are available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    /// True when the caller passed a `cursor` minted against a different HEAD sha (HEAD
    /// moved between calls). The caller must restart pagination from the top.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub cursor_invalidated: bool,
    /// Server-side handler latency in microseconds — the tool body's own execution (git walk /
    /// index lookup + response construction), excluding MCP transport, argument deserialization,
    /// and response serialization. A first call against a cold server also includes lazily
    /// building or loading the git-history index. See [`crate::mcp::helpers::timing`] for the
    /// full contract.
    #[serde(default)]
    pub elapsed_us: u64,
}
