//! Types of `find_files`: the wire arguments, the listing's and the fuzzy search's own arguments,
//! and the answer both give.

use serde::{Deserialize, Serialize};

use super::LifecycleNotice;
use crate::engine::cursor::Cursor;
use crate::path::RelPath;

/// Arguments of `find_files`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FindFilesToolParams {
    /// Fuzzy fragment of a file name or path; omit to list.
    #[serde(default)]
    pub query: Option<String>,
    /// Only paths starting with this, e.g. `src/mcp/`.
    #[serde(default)]
    pub path_prefix: Option<String>,
    /// Language filter, e.g. `rust`, `markdown`.
    #[serde(default)]
    pub language: Option<String>,
    /// Max files (default 200).
    #[serde(default)]
    pub limit: Option<u32>,
    /// `next_cursor` of the previous page.
    #[serde(default)]
    pub cursor: Option<Cursor>,
    /// Token budget for the list (default 6000).
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

/// `find_files` without a `query`: the listing's own arguments.
#[derive(Debug)]
pub(crate) struct ListFilesParams {
    /// Optional substring matched against the path. Cheaper than reading a glob crate.
    pub path_contains: Option<String>,
    /// Optional prefix the path must start with.
    pub path_prefix: Option<String>,
    /// Filter by language (e.g. "rust", "python").
    pub language: Option<String>,
    /// Cap. Default 200, max 5000.
    pub limit: Option<u32>,
    /// Optional token budget bounding the returned file list (not the whole envelope).
    /// Entries are kept in order until the budget is hit; the rest are dropped and the
    /// response carries `budgeted: true` plus a `next_cursor` to page them.
    pub max_tokens: Option<u32>,
    /// Resume token returned by the previous call's `next_cursor`. Cursors are scoped to
    /// the in-RAM index snapshot and invalidate on rescan.
    pub cursor: Option<Cursor>,
}

/// `find_files` with a `query`: the fuzzy search's own arguments.
#[derive(Debug)]
pub(crate) struct FindFilesParams {
    /// Fuzzy query matched as a subsequence against each indexed path (fzf/fd-style — letters
    /// of `query` must appear in order in the path, not necessarily contiguous). Case-insensitive.
    pub query: String,
    /// Optional prefix filter applied before scoring (e.g. "src/mcp/").
    pub path_prefix: Option<String>,
    /// Filter by language (e.g. "rust", "python"), applied before scoring.
    pub language: Option<String>,
    /// Cap. Default 200, max 5000 (same convention as `list_files`).
    pub limit: Option<u32>,
    /// Optional token budget bounding the returned file list (not the whole envelope).
    /// Entries are kept in score order until the budget is hit; the rest are dropped and the
    /// response carries `budgeted: true` plus a `next_cursor` to page them.
    pub max_tokens: Option<u32>,
    /// Resume token returned by the previous call's `next_cursor`. Cursors are scoped to
    /// the in-RAM index snapshot (and the score ordering computed for `query`); a rescan
    /// invalidates them same as `list_files`.
    pub cursor: Option<Cursor>,
}

/// Response of `find_files`: a fuzzy search ranked best first, or a listing in path order.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct FilesResponse {
    /// Files that match, from this page on: `next_cursor` continues past this one.
    pub total: usize,
    /// True when the caller's requested `limit` exceeded the hard cap (`LIST_LIMIT_MAX`) and
    /// was clamped down to it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub limit_clamped: bool,
    /// True when a `max_tokens` budget dropped trailing files. Page the rest with `next_cursor`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    /// Repo-relative paths: by descending match quality for a query (ties by path), else by path.
    pub files: Vec<RelPath>,
    /// Opaque cursor to pass back on the next call when more results are available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    /// True when the caller passed a `cursor` minted against a different in-RAM snapshot
    /// (a rescan happened between calls). The caller must restart pagination from the top.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub cursor_invalidated: bool,
    /// Lifecycle notice when the server isn't fully ready (warming/building/rescanning); absent when
    /// ready. Lets a caller tell "index still loading — retry" from a genuine empty result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<LifecycleNotice>,
}
