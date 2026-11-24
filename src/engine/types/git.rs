//! Types of the git operations: the wire arguments of `git_log` / `git_blame` / `git_diff`, the
//! per-operation shapes the bodies in `engine/git` take (`RecentChangesParams`, `BlameFileParams`,
//! …), and their answers. `mcp::types` and `mcp::types_tools` re-export them, so the tool schemas
//! and the public paths stay where they were.

use serde::{Deserialize, Serialize};

use crate::engine::cursor::Cursor;
use crate::path::RelPath;

/// Arguments of `git_log`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitLogParams {
    /// Only commits touching this file or directory (trailing `/`); narrows a `query`/`author` search.
    #[serde(default)]
    pub path: Option<RelPath>,
    /// Words the commit message must all contain; typos, word parts and a last-word prefix match too, after exact ones.
    #[serde(default)]
    pub query: Option<String>,
    /// Words the author name or email must all contain, matched like `query`.
    #[serde(default)]
    pub author: Option<String>,
    /// With `query`/`author`: only commits since `YYYY-MM-DD` or `<N>d|w|m|y` ago.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    /// With `query`/`author`: only commits until then (same forms, day included).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
    /// Commits that changed this symbol's body, ignoring comments and whitespace (`git log -L` counts them); needs `path`, combines only with `line`.
    #[serde(default)]
    pub symbol: Option<String>,
    /// With `symbol`: a 1-based line inside the definition meant.
    #[serde(default)]
    pub line: Option<u32>,
    /// Show this one commit in full; combines with nothing.
    #[serde(default)]
    pub rev: Option<String>,
    /// Default 20, max 100 (500 for a directory `path`).
    #[serde(default)]
    pub limit: Option<u32>,
    /// A prior `next_cursor`.
    #[serde(default)]
    pub cursor: Option<Cursor>,
    /// Default 6000.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Also list each commit's changed files in a recent or directory list (capped; see `files_more`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub files: bool,
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
    /// Merged hunks; default 100, max 1000.
    #[serde(default)]
    pub limit: Option<u32>,
    /// A prior `next_cursor`.
    #[serde(default)]
    pub cursor: Option<Cursor>,
    /// Default 6000.
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
    /// Symbol-level diff of `from` against the indexed working tree, not text hunks.
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
    /// Only commits that touched this file, or a file under this directory (trailing `/`).
    #[serde(default)]
    pub path: Option<RelPath>,
    /// Only commits authored at or after this time: `YYYY-MM-DD`, or `<N>d`, `<N>w`, `<N>m`, `<N>y`
    /// ago.
    #[serde(default)]
    pub since: Option<String>,
    /// Only commits authored at or before the end of this day, or of `<N>` days, weeks, months or
    /// years ago.
    #[serde(default)]
    pub until: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct RecentChangesParams {
    /// Number of commits to walk back from HEAD. Default 20, max 100.
    #[serde(default)]
    pub limit: Option<u32>,
    /// When true, include the per-file change list for each commit. Default false.
    #[serde(default)]
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
    /// Cap on hunks returned per page, counted after adjacent hunks of one commit merge. Default
    /// 100, max 1000.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Resume token returned by the previous call's `next_cursor`. Encodes the last-returned
    /// hunk's `end_line`; on resume the helper skips hunks whose `start_line <= offset`.
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
    /// When true, list what each commit changed. Default false.
    #[serde(default)]
    pub include_files: bool,
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
    /// Cap on hunks returned per page, counted after adjacent hunks of one commit merge. Default
    /// 100, max 1000.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Resume token returned by the previous call's `next_cursor`. Encodes the last-returned
    /// hunk's `end_line`; on resume the helper skips hunks whose `start_line <= offset`.
    #[serde(default)]
    pub cursor: Option<Cursor>,
    /// Token budget for the returned hunks; overflow is dropped and flagged `budgeted`.
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

/// One commit of a `git_log` list: its short sha, author, date and subject, which is what finding
/// a commit takes. The whole message, the author's email and the changed files are
/// `git_log {rev}`'s answer; a recent or directory list adds the files on request.
#[derive(Debug, schemars::JsonSchema)]
pub(crate) struct CommitView {
    pub sha: String,
    pub summary: String,
    pub author: String,
    /// Written in the row as its UTC day, `YYYY-MM-DD`.
    pub author_time_unix: i64,
    /// The files the commit changed: each path with how (`added`, `modified`, `deleted`,
    /// `renamed`). A merge lists only what it changed against every parent. The recent list and a
    /// directory's history write them in the response's `files`, keyed by sha; the row never does.
    pub files: Option<Vec<CommitFileView>>,
}

impl Serialize for CommitView {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;

        let mut row = serializer.serialize_struct("CommitView", 4)?;
        row.serialize_field("sha", &self.sha)?;
        row.serialize_field("author", &self.author)?;
        row.serialize_field("date", &utc_day(self.author_time_unix))?;
        row.serialize_field("subject", &self.summary)?;
        row.end()
    }
}

/// `unix` seconds as a UTC `YYYY-MM-DD` day (proleptic Gregorian, after Hinnant's `civil_from_days`).
pub(crate) fn utc_day(unix: i64) -> String {
    let days = unix.div_euclid(86_400) + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// The changed files of a directory's history: `{"<sha>": {"<path>": "<change>"}}`.
fn files_by_commit<S: serde::Serializer>(
    commits: &[(String, Vec<CommitFileView>)],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeMap;

    struct Files<'a>(&'a [CommitFileView]);
    impl Serialize for Files<'_> {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            files_by_path(self.0, serializer)
        }
    }
    let mut map = serializer.serialize_map(Some(commits.len()))?;
    for (sha, files) in commits {
        map.serialize_entry(sha, &Files(files))?;
    }
    map.end()
}

/// Each listed commit's changed files, keyed by its sha: a list response's top-level `files`.
pub(crate) type FilesByCommit = Vec<(String, Vec<CommitFileView>)>;

/// How many changed files each listed commit's entry in `files` leaves out: `{"<sha>": <count>}`.
pub(crate) type FilesMore = Vec<(String, usize)>;

/// A [`FilesMore`] written as the map it stands for.
fn files_more<S: serde::Serializer>(more: &FilesMore, serializer: S) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeMap;

    let mut map = serializer.serialize_map(Some(more.len()))?;
    for (sha, count) in more {
        map.serialize_entry(sha, count)?;
    }
    map.end()
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct CommitFileView {
    pub path: RelPath,
    /// How the commit changed it; a collapsed directory says how many files it stands for.
    pub change: std::borrow::Cow<'static, str>,
}

/// A file list written as `{"<path>": "<change>", ...}`: a path is a file's key, not a field
/// repeated beside its change. A path that is not UTF-8 is written lossily.
fn files_by_path<S: serde::Serializer>(files: &[CommitFileView], serializer: S) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeMap;

    let mut map = serializer.serialize_map(Some(files.len()))?;
    for file in files {
        map.serialize_entry(&file.path.to_str_lossy(), &file.change)?;
    }
    map.end()
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
    /// The shas of the commits that matched only through a tolerance (a typo, a word part, the
    /// start of a word); the rest matched the words as typed and come first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fuzzy: Vec<String>,
    /// True when the search did not cover the whole history.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// `unindexed_scan`: the git-history index was not ready, so only the subjects and authors of the
    /// newest 10,000 commits were searched as typed, no message bodies; retry shortly.
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
    /// One `{sha, author, date, subject}` row per commit, newest first.
    pub commits: Vec<CommitView>,
    /// What each listed commit changed, on request (`files`): `{"<sha>": {"<path>": "<change>"}}`.
    /// A commit with many files lists a few, a directory standing for several (`"<n> files"`).
    #[serde(serialize_with = "files_by_commit", skip_serializing_if = "Vec::is_empty")]
    #[schemars(with = "serde_json::Value")]
    pub files: FilesByCommit,
    /// The changed files a commit's entry in `files` leaves out: `{"<sha>": <count>}`; absent when
    /// none.
    #[serde(serialize_with = "files_more", skip_serializing_if = "Vec::is_empty")]
    #[schemars(with = "serde_json::Value")]
    pub files_more: FilesMore,
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

/// One blame hunk: a run of lines one commit last changed. Written as a row of
/// [`BlameHunks`], its commit's author, time and summary once per page under `commits`.
#[derive(Debug, Clone)]
pub(crate) struct BlameHunkView {
    pub commit_sha: String,
    /// 1-based first line of the hunk in the blamed file.
    pub start_line: u32,
    /// 1-based last line of the hunk, inclusive.
    pub end_line: u32,
    pub author: String,
    pub author_time_unix: i64,
    pub summary: String,
    /// The file the lines came from in that commit, when it is not the blamed path.
    pub source_path: Option<RelPath>,
}

impl BlameHunkView {
    /// The commit's sha as a row names it: [`BLAME_SHA_ABBREV`] hex digits, as in a `git_log` list.
    pub(crate) fn sha(&self) -> &str {
        &self.commit_sha[..self.commit_sha.len().min(BLAME_SHA_ABBREV)]
    }

    /// `"<line>[-<end_line>] <sha>[ <source_path>]"`.
    pub(crate) fn row(&self) -> String {
        let mut row = self.start_line.to_string();
        if self.end_line != self.start_line {
            row.push_str(&format!("-{}", self.end_line));
        }
        row.push(' ');
        row.push_str(self.sha());
        if let Some(source) = &self.source_path {
            row.push(' ');
            row.push_str(&source.to_str_lossy());
        }
        row
    }
}

/// Hex digits of a commit sha in a blame row; the same twelve as a `git_log` list.
const BLAME_SHA_ABBREV: usize = 12;

/// A page of blame hunks, written as two keys of the response: `hunks`, one
/// `"<line>[-<end_line>] <sha>[ <source_path>]"` row per hunk, and `commits`, each sha's `author`,
/// `time` (unix seconds) and `summary` once, left out with no hunk.
#[derive(Debug, Default)]
pub(crate) struct BlameHunks(pub Vec<BlameHunkView>);

impl Serialize for BlameHunks {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;

        #[derive(Serialize)]
        struct Commit<'a> {
            author: &'a str,
            time: i64,
            summary: &'a str,
        }
        struct Commits<'a>(&'a [BlameHunkView]);
        impl Serialize for Commits<'_> {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                let mut seen: Vec<&str> = Vec::new();
                let mut map = serializer.serialize_map(None)?;
                for hunk in self.0 {
                    if seen.contains(&hunk.sha()) {
                        continue;
                    }
                    seen.push(hunk.sha());
                    let commit = Commit {
                        author: &hunk.author,
                        time: hunk.author_time_unix,
                        summary: &hunk.summary,
                    };
                    map.serialize_entry(hunk.sha(), &commit)?;
                }
                map.end()
            }
        }
        let rows: Vec<String> = self.0.iter().map(BlameHunkView::row).collect();
        let mut map = serializer.serialize_map(Some(if rows.is_empty() { 1 } else { 2 }))?;
        map.serialize_entry("hunks", &rows)?;
        if !rows.is_empty() {
            map.serialize_entry("commits", &Commits(&self.0))?;
        }
        map.end()
    }
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct BlameResponse {
    /// `hunks` and `commits` ([`BlameHunks`]).
    #[serde(flatten)]
    #[schemars(skip)]
    pub hunks: BlameHunks,
    /// True when the token budget dropped trailing hunks; `next_cursor` resumes at the first one.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<&'static str>,
    /// Opaque cursor to pass back on the next call when more hunks are available. Encodes
    /// the last-returned hunk's `end_line` so the next page resumes immediately after.
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
    /// 1-based first line of the symbol, as `code_outline` reports it.
    #[serde(rename = "line")]
    pub line_start: u32,
    /// 1-based last line of the symbol, inclusive.
    #[serde(rename = "end_line")]
    pub line_end: u32,
    /// `hunks` and `commits` ([`BlameHunks`]).
    #[serde(flatten)]
    #[schemars(skip)]
    pub hunks: BlameHunks,
    /// True when the token budget dropped trailing hunks; `next_cursor` resumes at the first one.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<&'static str>,
    /// Opaque cursor to pass back on the next call when more hunks are available. Encodes
    /// the last-returned hunk's `end_line` so the next page resumes immediately after.
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
    /// Where the next page starts: how many commits, newest first, this page and the ones before it
    /// have gone through. A page that leaves matches out stops there, short of its window.
    pub commits_reached: u64,
    /// The commits in the whole history, so `commits_reached` reads as progress; absent when the
    /// page was walked live, which has no count without walking the whole history.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_commits: Option<u64>,
    /// Why `commits` is empty so far: no commit inspected up to this page touches the directory
    /// and it is not on disk either. The last page is then an error instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unmatched: Option<String>,
    /// True when this page used up its window before filling: older commits were not inspected,
    /// and `next_cursor` continues with them, even from an empty page.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// `span_limit` whenever `truncated` is true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<&'static str>,
    /// One `{sha, author, date, subject}` row per commit, newest first.
    pub commits: Vec<CommitView>,
    /// What each listed commit changed, on request (`files`): `{"<sha>": {"<path>": "<change>"}}`.
    /// A commit with many files lists a few, a directory standing for several (`"<n> files"`).
    #[serde(serialize_with = "files_by_commit", skip_serializing_if = "Vec::is_empty")]
    #[schemars(with = "serde_json::Value")]
    pub files: FilesByCommit,
    /// The changed files a commit's entry in `files` leaves out: `{"<sha>": <count>}`; absent when
    /// none.
    #[serde(serialize_with = "files_more", skip_serializing_if = "Vec::is_empty")]
    #[schemars(with = "serde_json::Value")]
    pub files_more: FilesMore,
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
    /// True when a hunk line is not valid UTF-8 (a file in a legacy encoding): the line is in its
    /// hunk's `text` with each invalid byte sequence read as U+FFFD.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub lossy_text: bool,
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

/// One change of a symbol's history, written `"<sha> <summary>"`, with `[introduced]` or
/// `[removed]` after the sha when the commit added or deleted the symbol rather than changed it. A
/// change whose summary itself starts with `[` is marked `[modified]`, so a marker is never read out
/// of a summary. When it was written is `git_log {rev}`'s answer: a date on every row costs the
/// list the value gate's locating floor (ADR-0052).
#[derive(Debug, schemars::JsonSchema)]
pub(crate) struct SymbolHistoryEntry {
    pub sha: String,
    pub summary: String,
    /// Not written in the row; `code explain` reads it.
    pub author: String,
    /// Not written in the row; `code explain` reads it.
    pub author_time_unix: i64,
    pub change: &'static str,
}

impl Serialize for SymbolHistoryEntry {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let row = match self.change {
            "modified" if !self.summary.starts_with('[') => format!("{} {}", self.sha, self.summary),
            change => format!("{} [{change}] {}", self.sha, self.summary),
        };
        serializer.serialize_str(&row)
    }
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct SymbolHistoryResponse {
    /// The kind the symbol was pinned to, when `kind` was resolved; absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Commits touching the file this page inspected; written beside a `next_cursor`, which looks
    /// further back.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commits_inspected: Option<u32>,
    /// `"<sha> [introduced] <summary>"`, newest first; `[removed]` likewise, and a plain
    /// `"<sha> <summary>"` for a commit that changed the symbol (`[modified]` when its summary
    /// starts with `[`).
    pub history: Vec<SymbolHistoryEntry>,
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
    /// `span_limit`: this page inspected `commits_inspected` commits touching the file and older ones
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

/// Arguments of `basemind git hotspots` (CLI-only, ADR-0047).
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HotspotsParams {
    /// Window ending at HEAD's commit time: `<N>d`, `<N>w` or `<N>y`; default `90d`.
    #[serde(default)]
    pub since: Option<String>,
    /// Only files at this path or under this directory.
    #[serde(default)]
    pub path: Option<RelPath>,
    /// Rows returned; default 20, at most 200.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// The smallest value in each signal's top slice; absent when no file has the signal.
#[derive(Debug, Default, Serialize)]
pub(crate) struct HotspotThresholds {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commits: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authors: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbols: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub longest_function: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub importers: Option<u32>,
}

/// One file where two or more signals are in their top slice. Every number is raw; there is no score.
#[derive(Debug, Serialize)]
pub(crate) struct HotspotRow {
    pub path: RelPath,
    /// The signals whose top slice holds this file, in [`HotspotThresholds`] order.
    pub signals: Vec<&'static str>,
    /// Commits in the window that touched the file.
    pub commits: u32,
    /// Distinct author emails (the name when a commit has none) among those commits.
    pub authors: u32,
    pub lines: u32,
    pub symbols: u32,
    /// Lines of the longest function or method (0 when it has none); `null` when the file changed
    /// since the scan.
    pub longest_function: Option<u32>,
    /// `name:line` of that function.
    pub longest_function_at: Option<String>,
    /// Files importing this one (runtime imports); `null` when no resolver reads its language.
    pub importers: Option<u32>,
}

/// The answer of `basemind git hotspots`.
#[derive(Debug, Serialize)]
pub(crate) struct HotspotsResponse {
    /// The window as asked (`90d`).
    pub since: String,
    /// Unix seconds: the window is `[window_start, window_end]`, ending at HEAD's commit time.
    pub window_start: i64,
    pub window_end: i64,
    /// Commits in the window.
    pub commits: u32,
    /// Indexed files in scope that a commit in the window touched: the population ranked.
    pub files_considered: u32,
    /// The top slice of each signal, as a percentage of `files_considered`.
    pub slice_percent: u32,
    pub thresholds: HotspotThresholds,
    /// Files with two or more signals in their top slice, before `limit`.
    pub total: u32,
    pub files: Vec<HotspotRow>,
    /// Set when the commit walk stopped before the window's start.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<&'static str>,
    /// Signals that were not measured everywhere, and why.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    pub elapsed_us: u64,
}
