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
    /// Default 100.
    #[serde(default)]
    pub limit: Option<u32>,
    /// A prior `next_cursor`.
    #[serde(default)]
    pub cursor: Option<Cursor>,
    /// Default 6000.
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

/// One `code_grep` hit. Written under its file and the symbol enclosing it ([`serialize_rows`]), as
/// the line `git grep -n` prints: `"<line>: <text>"`.
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
    pub symbol: Option<String>,
    /// The line immediately before the match, when context was requested and line > 1.
    pub context_before: Option<String>,
    /// The line immediately after the match, when context was requested and line < EOF.
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

    /// The row as the token budget costs it: its match row and context rows, without the symbol key
    /// a run of rows shares.
    fn serialize_row<S: serde::Serializer>(&self, serializer: S, column: bool) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;

        let columns = if column {
            format!(":{}", self.column)
        } else {
            String::new()
        };
        let mut seq = serializer.serialize_seq(None)?;
        if let Some(before) = &self.context_before {
            seq.serialize_element(&context_row(self.line - 1, before))?;
        }
        seq.serialize_element(&format!("{}{columns}: {}", self.line, self.text))?;
        if let Some(after) = &self.context_after {
            seq.serialize_element(&context_row(self.line + 1, after))?;
        }
        seq.end()
    }
}

/// A line around a match, as `grep -C` prints it: `"<line>- <text>"`.
fn context_row(line: u32, text: &str) -> String {
    format!("{line}- {text}")
}

/// A hit on its own, as the token budget costs it.
impl Serialize for GrepHit {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        crate::engine::by_path::Located::serialize_row(self, serializer, false)
    }
}

/// Write grep hits as `{"<path>": {"<symbol>": ["<line>: <text>", ...]}}`: files in the order of
/// their first hit, a file's symbols in the order of their first hit (`""` for a hit outside every
/// symbol), rows in the order found. Matches on one line are one row naming each 1-based column,
/// `"<line>:<column>,<column>: <text>"` (unless the line is clipped to a window around each); a lone
/// match writes its column when its text is clipped. A
/// context line (`"<line>- <text>"`) is written once per file, and never for a line that matches.
fn serialize_rows<S: serde::Serializer>(hits: &[GrepHit], serializer: S) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeMap;

    type Groups<'a> = Vec<(&'a str, Vec<String>)>;
    let mut files: Vec<(&RelPath, Groups<'_>)> = Vec::new();
    let mut file_slot: ahash::AHashMap<&RelPath, usize> = ahash::AHashMap::new();
    let match_lines: ahash::AHashSet<(&RelPath, u32)> = hits.iter().map(|hit| (&hit.path, hit.line)).collect();
    let mut context_written: ahash::AHashSet<(&RelPath, u32)> = ahash::AHashSet::new();
    let mut i = 0;
    while i < hits.len() {
        let hit = &hits[i];
        let mut end = i + 1;
        // A clipped line is a window around its own match, so only matches showing one text merge.
        while end < hits.len() && hits[end].path == hit.path && hits[end].line == hit.line && hits[end].text == hit.text
        {
            end += 1;
        }
        let same_line = &hits[i..end];
        let columns = if same_line.len() > 1 || hit.write_column {
            let listed: Vec<String> = same_line.iter().map(|h| h.column.to_string()).collect();
            format!(":{}", listed.join(","))
        } else {
            String::new()
        };
        let slot = *file_slot.entry(&hit.path).or_insert_with(|| {
            files.push((&hit.path, Vec::new()));
            files.len() - 1
        });
        let groups = &mut files[slot].1;
        let symbol = hit.symbol.as_deref().unwrap_or("");
        let group = match groups.iter().position(|(name, _)| *name == symbol) {
            Some(group) => group,
            None => {
                groups.push((symbol, Vec::new()));
                groups.len() - 1
            }
        };
        let rows = &mut groups[group].1;
        let mut context = |line: u32, text: &Option<String>, rows: &mut Vec<String>| {
            if let Some(text) = text
                && !match_lines.contains(&(&hit.path, line))
                && context_written.insert((&hit.path, line))
            {
                rows.push(context_row(line, text));
            }
        };
        context(hit.line.saturating_sub(1), &hit.context_before, rows);
        rows.push(format!("{}{columns}: {}", hit.line, hit.text));
        let last = &hits[end - 1];
        context(hit.line + 1, &last.context_after, rows);
        i = end;
    }
    let mut map = serializer.serialize_map(Some(files.len()))?;
    for (path, groups) in &files {
        map.serialize_entry(&path.to_str_lossy(), &GroupMap(groups))?;
    }
    map.end()
}

/// One file's rows by enclosing symbol.
struct GroupMap<'a>(&'a [(&'a str, Vec<String>)]);

impl Serialize for GroupMap<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;

        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (symbol, rows) in self.0 {
            map.serialize_entry(symbol, rows)?;
        }
        map.end()
    }
}

/// The hits of a `code_grep` page: one row per matching line, or with `ranges` each file's hits
/// merged into read ranges (`{"<path>": {"ranges": [[start, end], ...], "hits": n}}`, lines 1-based
/// inclusive).
#[derive(Debug)]
pub(crate) enum GrepHits {
    Rows(Vec<GrepHit>),
    Ranges(Vec<FileRanges>),
}

impl GrepHits {
    /// The files edited since the scan whose rows were read from disk, in the order listed. Ranges
    /// mark their own (`"stale": true` on the file).
    pub(crate) fn stale_files(&self) -> Vec<RelPath> {
        let mut stale: Vec<RelPath> = Vec::new();
        if let Self::Rows(rows) = self {
            for hit in rows.iter().filter(|hit| hit.stale) {
                if !stale.contains(&hit.path) {
                    stale.push(hit.path.clone());
                }
            }
        }
        stale
    }
}

impl Serialize for GrepHits {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Rows(rows) => serialize_rows(rows, serializer),
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
    /// just the returned page. Written only beside a `next_cursor`: a page without one lists every
    /// hit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u32>,
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
    /// The hits under the file and the symbol each is in, files in the order searched.
    #[schemars(with = "serde_json::Value")]
    pub hits: GrepHits,
    /// Files edited since the scan whose hits were read from disk as they are now; their rows are
    /// listed under `""`, since the indexed outline no longer places them. Absent when none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stale: Vec<RelPath>,
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
