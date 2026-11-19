//! Types of `code_grep`.

use serde::{Deserialize, Serialize};

use super::{LifecycleNotice, default_true, is_zero};
use crate::engine::cursor::Cursor;
use crate::engine::ranges::FileRanges;
use crate::path::RelPath;

/// Arguments of `code_grep`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CodeGrepParams {
    /// Text to find, not empty; a literal unless `regex` is true.
    pub pattern: String,
    /// Treat `pattern` as a Rust regex; `^` and `$` match at every line.
    #[serde(default)]
    pub regex: Option<bool>,
    /// Literal substring of the repo-relative path, e.g. `src/mcp/`; not a glob.
    #[serde(default)]
    pub path: Option<String>,
    /// Language filter, e.g. `rust`.
    #[serde(default)]
    pub language: Option<String>,
    /// Add one line before and after each hit.
    #[serde(default)]
    pub context: Option<bool>,
    /// Merged ±5-line read spans per file, for `code_read`.
    #[serde(default)]
    pub ranges: Option<bool>,
    /// Max hits (default 100).
    #[serde(default)]
    pub limit: Option<u32>,
    /// `next_cursor` of the previous page.
    #[serde(default)]
    pub cursor: Option<Cursor>,
    /// Token budget (default 6000).
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Only files changed since rev (empty: HEAD) or in A..B; +importers adds importers.
    #[serde(default)]
    pub changed: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct WorkspaceGrepParams {
    /// Rust regex syntax (`regex` crate). Required.
    pub pattern: String,
    /// Optional language filter (e.g. `"rust"`, `"typescript"`). Same ID convention as
    /// `list_files`.
    #[serde(default)]
    pub language: Option<String>,
    /// Optional substring filter on path. Same convention as `list_files`.
    #[serde(default)]
    pub path_contains: Option<String>,
    /// Max number of HITS returned. Default 100, max 1000. It does not bound the files scanned:
    /// grep always sweeps the whole indexed corpus (after the `language` / `path_contains`
    /// filters), so a rare token is found wherever it lives.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Optional token budget bounding the returned `hits` list (not the whole envelope).
    /// Hits are kept in scan order until the budget is hit; the rest are dropped and the
    /// response carries `budgeted: true` plus a `next_cursor` to page them.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Include 1 line of context before + after each hit. Default true.
    #[serde(default = "default_true")]
    pub include_context: bool,
    /// Answer each file's hits as merged read ranges instead of lines.
    #[serde(default)]
    pub ranges: bool,
    /// Match `pattern` as literal text instead of compiling it as a regex.
    #[serde(default)]
    pub literal: bool,
    /// Resume token returned by the previous call's `next_cursor`. Cursors are scoped to
    /// the in-RAM index snapshot and invalidate on rescan.
    #[serde(default)]
    pub cursor: Option<Cursor>,
}

/// One `code_grep` hit, listed under its file ([`crate::engine::by_path`] writes it).
#[derive(Debug, schemars::JsonSchema)]
pub(crate) struct GrepHit {
    /// The file the hit is in: the key the hit is listed under, not a field of its own.
    pub path: RelPath,
    /// 1-based line number of the match.
    pub line: u32,
    /// 1-based byte column of the match, written only when [`write_column`](Self::write_column)
    /// is set.
    pub column: u32,
    /// The file holds another match on this line, or `text` is a clipped window of the line: the
    /// line alone does not place the match, so `column` is written. Decided over every match in
    /// the file, so a page boundary does not change it.
    #[schemars(skip)]
    pub write_column: bool,
    /// The matching line, so the hit can be read without opening the file. A line over 400 bytes is
    /// cut to a window around the match, `…` marking each cut side.
    pub text: String,
    /// Name of the innermost symbol whose span holds the match; absent outside every symbol.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    /// The line immediately before the match, when context was requested and line > 1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_before: Option<String>,
    /// The line immediately after the match, when context was requested and line < EOF.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_after: Option<String>,
    /// Byte offset of the match in the file; used to find `symbol`, not part of the response.
    pub start_byte: u32,
    /// The file changed since it was indexed. The hit is from the file as it is now, and `symbol`
    /// is left out: the indexed outline describes other bytes until `index_rescan`.
    pub stale: bool,
    /// Lines in the file the hit was read from; bounds its read range, not part of the response.
    #[schemars(skip)]
    pub file_lines: u32,
}

impl crate::engine::by_path::Located for GrepHit {
    fn located(&self) -> &RelPath {
        &self.path
    }

    fn writes_column(&self) -> Option<bool> {
        Some(self.write_column)
    }

    fn line(&self) -> u32 {
        self.line
    }

    fn serialize_row<S: serde::Serializer>(&self, serializer: S, column: bool) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;

        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("line", &self.line)?;
        if column {
            map.serialize_entry("column", &self.column)?;
        }
        map.serialize_entry("text", &self.text)?;
        if let Some(symbol) = &self.symbol {
            map.serialize_entry("symbol", symbol)?;
        }
        if let Some(before) = &self.context_before {
            map.serialize_entry("context_before", before)?;
        }
        if let Some(after) = &self.context_after {
            map.serialize_entry("context_after", after)?;
        }
        if self.stale {
            map.serialize_entry("stale", &true)?;
        }
        map.end()
    }
}

/// A hit on its own, as the token budget costs it.
impl Serialize for GrepHit {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        crate::engine::by_path::Located::serialize_row(self, serializer, false)
    }
}

/// The hits of a `code_grep` page: one row per hit, or with `ranges` each file's hits merged into
/// read ranges (`{"<path>": {"ranges": [[start, end], ...], "hits": n}}`, lines 1-based inclusive).
#[derive(Debug)]
pub(crate) enum GrepHits {
    Rows(Vec<GrepHit>),
    Ranges(Vec<FileRanges>),
}

impl Serialize for GrepHits {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Rows(rows) => crate::engine::by_path::serialize(rows, serializer),
            Self::Ranges(files) => crate::engine::ranges::serialize_files(files, serializer),
        }
    }
}

/// Why a `workspace_grep` result was cut short. Present only when `truncated` is true — a grep that
/// returned every match carries no reason, so an agent can tell a complete zero from a bounded one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GrepTruncation {
    /// The corpus was larger than one call may read. Narrow with `path_contains` / `language`, or
    /// page with `next_cursor`. Only reachable on workspaces of multiple gigabytes of source.
    ByteBudget,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct WorkspaceGrepResponse {
    /// Exact hit count from the cursor position on (the whole scanned corpus on a first page), not
    /// just the returned page. Exceeds the hits listed when `limit` or the token budget cut the
    /// page; `next_cursor` continues.
    pub total: u32,
    /// True when files were left unread, so `total` does not cover them. Never true because
    /// `limit` cut the page or because the corpus is large: grep scans every candidate file it may
    /// read. Absent when false.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// `byte_budget` whenever `truncated` is true. There is no `omitted`: the matches in the
    /// unread files were never counted. `next_cursor` continues with those files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<GrepTruncation>,
    /// True when a `max_tokens` budget dropped trailing hits. Page the rest with `next_cursor`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    /// The hits under the file each is in, files in the order searched.
    #[schemars(with = "serde_json::Value")]
    pub hits: GrepHits,
    /// Opaque cursor to pass back on the next call when more results are available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    /// Files this call opened and could not search: unreadable, or not UTF-8 text.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub skipped_files: usize,
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
