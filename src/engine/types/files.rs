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
    /// Default 200.
    #[serde(default)]
    pub limit: Option<u32>,
    /// A prior `next_cursor`.
    #[serde(default)]
    pub cursor: Option<Cursor>,
    /// Default 6000.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Only code files of at least this many lines, largest first; `files` maps path to lines.
    #[serde(default)]
    pub min_lines: Option<u32>,
    /// Files changed since rev's merge base (empty: HEAD), in A..B or A...B (e.g. main...HEAD);
    /// +importers adds importers.
    #[serde(default)]
    pub changed: Option<String>,
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
    /// Files that match, from this page on. Written only beside a `next_cursor`, which continues
    /// past this page: a page without one lists every match.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<usize>,
    /// True when the caller's requested `limit` exceeded the hard cap (`LIST_LIMIT_MAX`) and
    /// was clamped down to it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub limit_clamped: bool,
    /// True when a `max_tokens` budget dropped trailing files. Page the rest with `next_cursor`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    /// Repo-relative paths: by descending match quality for a query (ties by path), else by path.
    /// With `min_lines`, `{"<path>": <lines>}`, largest first (ties by path).
    #[schemars(with = "serde_json::Value")]
    pub files: FileList,
    /// Opaque cursor to pass back on the next call when more results are available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    /// True when the caller passed a `cursor` minted against a different in-RAM snapshot
    /// (a rescan happened between calls). The caller must restart pagination from the top.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub cursor_invalidated: bool,
    /// Lifecycle notice when the server isn't fully ready (warming/building/rescanning); absent when
    /// ready. Lets a caller tell "index still loading — retry" from a genuine empty result.
    /// Files the `changed` scope holds, when one was passed: `0` is a change that touched nothing, told
    /// apart from a change with no hit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_files: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<LifecycleNotice>,
}

/// A `find_files` page: a list of paths, or with `min_lines` (`sized`) a map from each path to its
/// line count, the way `wc -l` pairs them, empty or not.
#[derive(Debug)]
pub(crate) struct FileList {
    pub rows: Vec<FileHit>,
    pub sized: bool,
}

impl Serialize for FileList {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{SerializeMap, SerializeSeq};

        if self.sized {
            let mut map = serializer.serialize_map(Some(self.rows.len()))?;
            for file in &self.rows {
                match file {
                    FileHit::Sized(sized) => map.serialize_entry(&sized.path.to_str_lossy(), &sized.lines)?,
                    FileHit::Path(path) => map.serialize_entry(&path.to_str_lossy(), &serde_json::Value::Null)?,
                }
            }
            map.end()
        } else {
            let mut seq = serializer.serialize_seq(Some(self.rows.len()))?;
            for file in &self.rows {
                seq.serialize_element(file)?;
            }
            seq.end()
        }
    }
}

/// One `find_files` row: a path, or with `min_lines` the path and its size.
#[derive(Debug, Serialize, schemars::JsonSchema)]
#[serde(untagged)]
pub(crate) enum FileHit {
    Path(RelPath),
    Sized(SizedFile),
}

/// A code file and its size as indexed: a `find_files` row under `min_lines`, and a row of
/// `index_status`'s `large_files`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub(crate) struct SizedFile {
    pub path: RelPath,
    /// Lines as scanned: every `\n` ends one, and a final unterminated line counts.
    pub lines: u32,
    pub bytes: u64,
}
