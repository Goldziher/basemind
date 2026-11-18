//! Types of the git operations: the wire arguments of `git_log` / `git_blame` / `git_diff`, the
//! per-operation shapes the bodies in `engine/git` take (`RecentChangesParams`, `BlameFileParams`,
//! …), and their answers. `mcp::types` and `mcp::types_tools` re-export them, so the tool schemas
//! and the public paths stay where they were.

use serde::{Deserialize, Serialize};

use super::default_true;
use crate::engine::cursor::Cursor;
use crate::path::RelPath;

/// Arguments of `git_log`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitLogParams {
    /// Only commits touching this file; a trailing `/` means a directory (200 commits inspected per
    /// page; follow `next_cursor`). Not with `query` or `author`.
    #[serde(default)]
    pub path: Option<RelPath>,
    /// Words the commit message must all contain. Combines with `author` only.
    #[serde(default)]
    pub query: Option<String>,
    /// Words the author name or email must all contain. Combines with `query` only.
    #[serde(default)]
    pub author: Option<String>,
    /// History of this symbol's body, one window of commits per page; needs `path`, combines
    /// with nothing else but `line`.
    #[serde(default)]
    pub symbol: Option<String>,
    /// With `symbol`: 1-based line inside the definition meant, when several share the name.
    #[serde(default)]
    pub line: Option<u32>,
    /// Show this one commit in full; `max_tokens` bounds its file list. Combines with nothing.
    #[serde(default)]
    pub rev: Option<String>,
    /// Max commits (default 20).
    #[serde(default)]
    pub limit: Option<u32>,
    /// `next_cursor` of the previous page.
    #[serde(default)]
    pub cursor: Option<Cursor>,
    /// Token budget (default 6000).
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

/// Arguments of `git_blame`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitBlameParams {
    /// Repo-relative file path.
    pub path: RelPath,
    /// First line of a span, 1-based, as numbered at `rev`. With `symbol`: a line inside the
    /// definition meant.
    #[serde(default)]
    pub line: Option<u32>,
    /// Last line of a span, inclusive (default: `line`). Needs `line`; not with `symbol`.
    #[serde(default)]
    pub end_line: Option<u32>,
    /// Blame this symbol's lines, as they are at `rev`, instead of a span.
    #[serde(default)]
    pub symbol: Option<String>,
    /// Revision to blame at (default HEAD).
    #[serde(default)]
    pub rev: Option<String>,
    /// Max hunks (default 100).
    #[serde(default)]
    pub limit: Option<u32>,
    /// `next_cursor` of the previous page.
    #[serde(default)]
    pub cursor: Option<Cursor>,
    /// Token budget (default 6000).
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

/// Arguments of `git_diff`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitDiffParams {
    /// File to diff, or a directory ending in `/` to narrow the status; omit every argument for
    /// the whole working-tree status.
    #[serde(default)]
    pub path: Option<RelPath>,
    /// Old revision (default HEAD). Needs `path`.
    #[serde(default)]
    pub from: Option<String>,
    /// New revision (default: the working tree). Needs `path`; not with `structural`.
    #[serde(default)]
    pub to: Option<String>,
    /// Symbol-level diff of `from` against the indexed working tree, instead of text hunks.
    #[serde(default)]
    pub structural: Option<bool>,
}

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
    /// Token budget for the returned history entries.
    #[serde(default)]
    pub max_tokens: Option<u32>,
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

/// One commit of a `git_log` list.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct CommitView {
    pub sha: String,
    pub summary: String,
    pub author: String,
    pub author_time_unix: i64,
    /// The files the commit changed: each path with how (`added`, `modified`, `deleted`,
    /// `renamed`). A merge lists only what it changed against every parent.
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "files_by_path_opt")]
    pub files: Option<Vec<CommitFileView>>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct CommitFileView {
    pub path: RelPath,
    pub change: &'static str,
}

/// A file list written as `{"<path>": "<change>", ...}`: a path is a file's key, not a field
/// repeated beside its change. A path that is not UTF-8 is written lossily.
fn files_by_path<S: serde::Serializer>(files: &[CommitFileView], serializer: S) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeMap;

    let mut map = serializer.serialize_map(Some(files.len()))?;
    for file in files {
        map.serialize_entry(&file.path.to_str_lossy(), file.change)?;
    }
    map.end()
}

/// `serialize_with` for an optional file list; `skip_serializing_if` leaves out the `None`.
fn files_by_path_opt<S: serde::Serializer>(
    files: &Option<Vec<CommitFileView>>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    files_by_path(files.as_deref().unwrap_or_default(), serializer)
}

/// `skip_serializing_if` for a flag that is written only when false.
fn is_true(flag: &bool) -> bool {
    *flag
}

/// The commit `git_log {rev}` shows: its whole message and its author's email, beside what a list
/// row carries.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct GitCommitHit {
    pub sha: String,
    pub summary: String,
    pub author: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub author_email: String,
    pub author_time_unix: i64,
    /// The message after its summary line; absent when there is none.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub body: String,
}

/// Response of `git_log` with `rev`: one commit with its whole message and changed-file list.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct GitShowResponse {
    pub commit: GitCommitHit,
    /// The files the commit changed, each path with how.
    #[serde(serialize_with = "files_by_path")]
    pub files: Vec<CommitFileView>,
    /// True when `max_tokens` dropped trailing `files`; raise it to list them.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    /// Changed files left out of `files` when `budgeted`.
    #[serde(skip_serializing_if = "crate::engine::types::is_zero")]
    pub omitted: usize,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct SearchGitHistoryResponse {
    /// Matching commits, newest first. A message's body is not listed: `git_log {rev}` shows it.
    pub commits: Vec<CommitView>,
    /// True when the search did not cover the whole history.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// `live_walk`: the git-history index was not ready, so only the subjects and authors of the
    /// newest 10,000 commits were searched, no message bodies; retry shortly. `scan_cap`: an
    /// `author` filter stopped after 20,000 commits matching `query`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<&'static str>,
    /// True when `max_tokens` (or the default budget) dropped trailing commits; `next_cursor`
    /// resumes at the first dropped one.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub cursor_invalidated: bool,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct WorkingTreeStatusView {
    /// Each list is left out when empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub staged_added: Vec<RelPath>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub staged_modified: Vec<RelPath>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub staged_deleted: Vec<RelPath>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub modified: Vec<RelPath>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub untracked: Vec<RelPath>,
    /// The directory the status was narrowed to, when `path` named one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<RelPath>,
    /// True when a list above was cut at its cap of 200 paths. Narrow with a directory `path`
    /// (one ending in `/`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// `path_cap` whenever `truncated` is true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<&'static str>,
    /// Paths left out, over all five lists.
    #[serde(skip_serializing_if = "crate::engine::types::is_zero")]
    pub omitted: usize,
    /// Nothing is staged, modified or untracked: in the whole working tree, or under `path` when
    /// the status was narrowed to a directory.
    pub is_clean: bool,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct RecentChangesResponse {
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
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct CommitsTouchingResponse {
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
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct DiffSymbolView {
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct DiffOutlineResponse {
    pub path: RelPath,
    pub rev: String,
    pub added: Vec<DiffSymbolView>,
    pub removed: Vec<DiffSymbolView>,
    /// How many symbols both sides have. A count: the list is the file's outline, which
    /// `code_outline` returns.
    pub common: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct BlameHunkView {
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
pub(crate) struct BlameResponse {
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
    /// building or loading the git-history index. See [`crate::engine::timing`] for the
    /// full contract.
    #[serde(default)]
    pub elapsed_us: u64,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct BlameSymbolResponse {
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
    /// building or loading the git-history index. See [`crate::engine::timing`] for the
    /// full contract.
    #[serde(default)]
    pub elapsed_us: u64,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct FindCommitsByPathResponse {
    /// Commits this page inspected, newest first from where its cursor starts.
    pub window_inspected: u32,
    /// Why `commits` is empty so far: no commit inspected up to this page touches the directory
    /// and it is not on disk either. The last page is then an error instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unmatched: Option<String>,
    /// True when this page used up its window before filling: older commits were not inspected,
    /// and `next_cursor` continues with them, even from an empty page.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// `window` whenever `truncated` is true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<&'static str>,
    pub commits: Vec<CommitView>,
    /// True when `max_tokens` (or the default budget) dropped trailing commits; `next_cursor`
    /// resumes at the first dropped one.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    /// Continues with older commits: the matches this page left out, or the next window.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    /// True when the caller passed a `cursor` minted against a different HEAD sha (HEAD
    /// moved between calls). The caller must restart pagination from the top.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub cursor_invalidated: bool,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct HotFileEntry {
    pub path: RelPath,
    pub commits_touching: u32,
    pub added: u32,
    pub modified: u32,
    pub deleted: u32,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct HotFilesResponse {
    pub window_inspected: u32,
    pub total_files_changed: u32,
    pub files: Vec<HotFileEntry>,
    /// Server-side handler latency in microseconds — the tool body's own execution (git walk /
    /// index lookup + response construction), excluding MCP transport, argument deserialization,
    /// and response serialization. A first call against a cold server also includes lazily
    /// building or loading the git-history index. See [`crate::engine::timing`] for the
    /// full contract.
    #[serde(default)]
    pub elapsed_us: u64,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct HunkView {
    pub old_line_start: u32,
    pub old_line_count: u32,
    pub new_line_start: u32,
    pub new_line_count: u32,
    /// The added lines when nothing was removed, the removed lines when nothing was added, else the
    /// removed lines prefixed `-` followed by the added lines prefixed `+`.
    pub text: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct DiffFileResponse {
    pub path: RelPath,
    /// Full sha of the old side.
    pub from: String,
    /// Full sha of the new side, or `working tree`.
    pub to: String,
    /// False when the file does not exist at `from` (it was added); absent when it does.
    #[serde(skip_serializing_if = "is_true")]
    pub present_at_from: bool,
    /// False when the file does not exist at `to` (it was deleted); absent when it does.
    #[serde(skip_serializing_if = "is_true")]
    pub present_at_to: bool,
    /// True for a binary file, which has no text hunks.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub binary: bool,
    /// True when a side is over the diff size ceiling: the file was not compared and has no hunks.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub too_large: bool,
    /// True when `hunks` was cut at the hunk or byte cap, or the one hunk returned had its text
    /// clipped to the byte cap. A diff has no cursor: narrow it with a closer `from` / `to`.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// `hunk_cap` (200 hunks) or `byte_cap` (128 KiB of hunk text), whenever `truncated` is true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<&'static str>,
    /// Hunks left out of `hunks`.
    #[serde(skip_serializing_if = "crate::engine::types::is_zero")]
    pub omitted: usize,
    pub hunks: Vec<HunkView>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct SymbolHistoryEntry {
    pub sha: String,
    pub summary: String,
    pub author: String,
    pub author_time_unix: i64,
    pub change: &'static str,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct SymbolHistoryResponse {
    pub kind: Option<String>,
    pub commits_inspected: u32,
    pub history: Vec<SymbolHistoryEntry>,
    /// Echoes the fingerprint strategy that produced this response — `"normalized"`,
    /// `"structural"`, or `"structural_loose"`. Clients can use this to confirm the mode
    /// they got matches the mode they asked for.
    pub hash_mode: &'static str,
    /// Why `history` is empty so far: the working tree does not define `name` (the text lists what
    /// the file does define) and no commit inspected up to this page does either. The last page is
    /// then an error instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unmatched: Option<String>,
    /// Entries were dropped to fit `max_tokens`; `next_cursor` continues with them.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    /// The history may reach further back than what is listed.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// `window`: this page inspected `commits_inspected` commits touching the file and older ones
    /// remain, so the symbol's earlier changes (and its introduction) are on later pages:
    /// `next_cursor` continues with the next window, even from an empty page. `shallow_clone`: the
    /// clone holds no older history.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<&'static str>,
    /// Continues with older commits: the changes this page left out, or the next window.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    /// True when the caller passed a `cursor` minted against a different HEAD sha (HEAD
    /// moved between calls). The caller must restart pagination from the top.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub cursor_invalidated: bool,
}

/// Answer of `git_log`: the shape its arguments select, serialized as that shape alone.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum GitLogResult {
    Recent(RecentChangesResponse),
    Touching(CommitsTouchingResponse),
    Directory(FindCommitsByPathResponse),
    Search(SearchGitHistoryResponse),
    Symbol(SymbolHistoryResponse),
    Show(GitShowResponse),
}

/// Answer of `git_blame`: a file or span, or one symbol.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum GitBlameResult {
    File(BlameResponse),
    Symbol(BlameSymbolResponse),
}

/// Answer of `git_diff`: the working-tree status, a text diff or a symbol-level diff.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum GitDiffResult {
    Status(WorkingTreeStatusView),
    Text(DiffFileResponse),
    Structural(DiffOutlineResponse),
}
