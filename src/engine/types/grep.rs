//! Types of `code_grep`.

use serde::{Deserialize, Serialize};

use super::{LifecycleNotice, is_zero};
use crate::engine::cursor::Cursor;
use crate::path::RelPath;

/// Most lines of context `code_grep` writes on each side of a hit.
pub(crate) const GREP_CONTEXT_MAX: u32 = 10;

/// Arguments of `code_grep`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CodeGrepParams {
    /// Text to find, not empty; a literal unless `regex` is true.
    pub pattern: String,
    /// Treat `pattern` as a Rust regex; `^` and `$` match at every line.
    #[serde(default)]
    pub regex: Option<bool>,
    /// Prefix, or a glob when it has `*`, e.g. `src/`.
    #[serde(default)]
    pub path: Option<String>,
    /// Language filter, e.g. `rust`.
    #[serde(default)]
    pub language: Option<String>,
    /// Files by role: `include`, `exclude`, `only`, role names (`infra,test`) or `-role` names.
    #[serde(default)]
    pub roles: Option<String>,
    /// Lines before and after each hit, 0-10.
    #[serde(default)]
    pub context: Option<u32>,
    /// Default 100, max 1000.
    #[serde(default)]
    pub limit: Option<u32>,
    /// A prior `next_cursor`.
    #[serde(default)]
    pub cursor: Option<Cursor>,
    /// Default 6000.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Changed since a rev (empty: HEAD), A..B or A...B; main...HEAD+importers adds importers.
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
    /// Optional `path` filter: a repo-relative prefix, or a glob when it holds `*`
    /// ([`PathFilter`](crate::path_filter::PathFilter)). Same convention as `find_files`.
    #[serde(default)]
    pub path: Option<String>,
    /// Optional `roles` filter ([`RoleFilter`](crate::config::RoleFilter)).
    #[serde(default)]
    pub roles: Option<String>,
    /// Max number of HITS returned. Default 100, max 1000. It bounds the files read only once more
    /// than this many matches are found: until then grep sweeps the indexed corpus (after the
    /// `language` / `path` filters), so a rare token is found wherever it lives.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Optional token budget bounding the returned `hits` list (not the whole envelope).
    /// Hits are kept in scan order until the budget is hit; the rest are dropped and the
    /// response carries `budgeted: true` plus a `next_cursor` to page them.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Lines of context before and after each hit, at most [`GREP_CONTEXT_MAX`]; 0 for none.
    #[serde(default)]
    pub context: u32,
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
    /// The context lines before the match that no earlier hit of the page shows, ending at
    /// `line - 1`: up to the requested `context` of them.
    pub context_before: Vec<String>,
    /// The context lines after the match, from `line + 1`, up to the requested `context` of them,
    /// stopping before the file's end and before the next hit of the page.
    pub context_after: Vec<String>,
    /// Byte offset of the match in the file; used to find `symbol`, not part of the response.
    pub start_byte: u32,
    /// The file changed since it was indexed. The hit is from the file as it is now, and `symbol`
    /// is left out: the indexed outline describes other bytes until `index_rescan`.
    pub stale: bool,
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

    fn group(&self) -> Option<&str> {
        Some(self.symbol.as_deref().unwrap_or(""))
    }

    /// The row as the token budget costs it: its match row and context rows. The symbol key the
    /// rows of a group share is costed once, by [`crate::engine::budget::apply_budget_by_path`].
    fn serialize_row<S: serde::Serializer>(&self, serializer: S, column: bool) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;

        let columns = if column {
            format!(":{}", self.column)
        } else {
            String::new()
        };
        let mut seq = serializer.serialize_seq(None)?;
        for (line, text) in self.before() {
            seq.serialize_element(&context_row(line, text))?;
        }
        seq.serialize_element(&format!("{}{columns}: {}", self.line, self.text))?;
        for (line, text) in self.after() {
            seq.serialize_element(&context_row(line, text))?;
        }
        seq.end()
    }
}

impl GrepHit {
    /// The context lines before the match, each with its 1-based line.
    fn before(&self) -> impl Iterator<Item = (u32, &String)> {
        let first = self.line - self.context_before.len() as u32;
        (first..).zip(&self.context_before)
    }

    /// The context lines after the match, each with its 1-based line.
    fn after(&self) -> impl Iterator<Item = (u32, &String)> {
        (self.line + 1..).zip(&self.context_after)
    }
}

impl GrepHit {
    /// Bytes this hit's rows take in the text answer's row block: its context rows and its match row
    /// (with its columns when it writes them), each with its line break.
    pub(crate) fn plain_bytes(&self) -> u64 {
        let digits = |line: u32| u64::from(line.checked_ilog10().unwrap_or(0) + 1);
        let context: u64 = self
            .before()
            .chain(self.after())
            .map(|(line, text)| digits(line) + 3 + text.len() as u64)
            .sum();
        let column = if self.write_column { digits(self.column) + 1 } else { 0 };
        context + digits(self.line) + column + 3 + self.text.len() as u64
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
/// context line (`"<line>- <text>"`) is written once per file, and never for a line that matches:
/// overlapping windows merge, as `rg -C` merges them.
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
        let mut context = |lines: &mut dyn Iterator<Item = (u32, &String)>, rows: &mut Vec<String>| {
            for (line, text) in lines {
                if !match_lines.contains(&(&hit.path, line)) && context_written.insert((&hit.path, line)) {
                    rows.push(context_row(line, text));
                }
            }
        };
        context(&mut hit.before(), rows);
        rows.push(format!("{}{columns}: {}", hit.line, hit.text));
        context(&mut hits[end - 1].after(), rows);
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

/// The hits of a `code_grep` page, one row per matching line, written by [`serialize_rows`].
#[derive(Debug)]
pub(crate) struct GrepHits(pub(crate) Vec<GrepHit>);

impl GrepHits {
    /// The files edited since the scan whose rows were read from disk, in the order listed.
    pub(crate) fn stale_files(&self) -> Vec<RelPath> {
        let mut stale: Vec<RelPath> = Vec::new();
        for hit in self.0.iter().filter(|hit| hit.stale) {
            if !stale.contains(&hit.path) {
                stale.push(hit.path.clone());
            }
        }
        stale
    }
}

impl Serialize for GrepHits {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serialize_rows(&self.0, serializer)
    }
}

/// Why a `workspace_grep` result was cut short. Present only when `truncated` is true — a grep that
/// returned every match carries no reason, so an agent can tell a complete zero from a bounded one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GrepTruncation {
    /// The corpus was larger than one call may read. Narrow with `path` / `language`, or
    /// page with `next_cursor`. Only reachable on workspaces of multiple gigabytes of source.
    ByteBudget,
    /// The sweep ran out of wall-clock time before reading every file. `notice` says so and
    /// `next_cursor` continues with the files it did not reach.
    TimeBudget,
}

/// Why `code_grep` could not search the files it counts in `skipped_files`. A reason with no file
/// is absent.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub(crate) struct SkippedBreakdown {
    /// Gone since the scan, or now leading out of the repository.
    #[serde(skip_serializing_if = "is_zero")]
    pub unavailable: usize,
    /// Grown past `scan.max_file_bytes`.
    #[serde(skip_serializing_if = "is_zero")]
    pub too_large: usize,
    /// Present, but opening or reading it failed (permission denied, an I/O error).
    #[serde(skip_serializing_if = "is_zero")]
    pub unreadable: usize,
}

impl SkippedBreakdown {
    /// Files skipped for any reason.
    pub(crate) fn total(&self) -> usize {
        self.unavailable + self.too_large + self.unreadable
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.total() == 0
    }
}

impl std::ops::AddAssign for SkippedBreakdown {
    fn add_assign(&mut self, other: Self) {
        self.unavailable += other.unavailable;
        self.too_large += other.too_large;
        self.unreadable += other.unreadable;
    }
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct WorkspaceGrepResponse {
    /// Hit count from the cursor position on, not just the returned page: exact over the scanned
    /// corpus, or the count over the files read so far when `total_is_partial`. Written only beside
    /// a `next_cursor`: a page without one lists every hit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u32>,
    /// True when the sweep stopped reading once the files read held more than `limit` hits, or at its
    /// time budget, so
    /// `total` is a lower bound. `next_cursor` continues, and each later page counts on from where
    /// this one stopped. Absent when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub total_is_partial: bool,
    /// True when the byte or time budget left files unread, so `total` does not cover them. Never true
    /// because `limit` cut the page or because the sweep stopped once the page was full. Absent when
    /// false.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// `byte_budget` or `time_budget` whenever `truncated` is true. There is no `omitted`: the matches
    /// in the unread files were never counted. `next_cursor` continues with those files.
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
    /// Files this call could not search: gone, unreadable, or leading out of the repository.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub skipped_files: usize,
    /// `skipped_files` by why, each reason beside its count; absent when nothing was skipped.
    #[serde(default, skip_serializing_if = "SkippedBreakdown::is_empty")]
    pub skipped_reasons: SkippedBreakdown,
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
