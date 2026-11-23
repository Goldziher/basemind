//! The commit search behind `git_log` with `query` and/or `author`: tolerant matching of words,
//! combined with a `path` and a `since` / `until` time range. Split from `log.rs` for the 1000-line
//! per-file cap.

use super::common::{
    LOG_LIMIT_DEFAULT, LOG_LIMIT_MAX, LOG_WALK_MAX, commit_to_view, git_history_if_fresh, head_sha, head_snapshot_id,
    require_git_repo,
};
use super::show::budget_commits;
use crate::engine::error::EngineError;
use crate::engine::types::UNINDEXED_SCAN;
use crate::engine::types::git::{CommitView, SearchGitHistoryParams, SearchGitHistoryResponse};
use crate::engine::workspace::Workspace;
use crate::git::CommitInfo;
use crate::git_history::fts::{self, FtsScope};
use crate::git_history::fuzzy::{CommitQuery, SearchHit};
use crate::path::RelPath;

const SECS_PER_DAY: i64 = 86_400;

/// Paths named in the refusal of a `path` the history does not hold.
const SUGGESTED_PATHS: usize = 5;

/// What a `since` / `until` bound may look like, quoted in its error.
const BOUND_FORMS: &str =
    "`YYYY-MM-DD`, or a count of days, weeks, months or years ago such as `90d`, `12w`, `3m` or `1y`";

/// The `search` operation: words in the commit message and/or the author, tolerant of typos, word
/// parts and the start of the last word, optionally narrowed to a `path` and a time range. Exact
/// matches come first, and the shas of the ones that matched only through a tolerance are listed in
/// `fuzzy`. Uses the git-history index when it is fresh (it holds HEAD as a tip); otherwise
/// degrades to a bounded live walk over the recent window that matches the words as typed, flagged
/// `truncated` with reason `unindexed_scan` (author + summary only, no body). Pagination and the
/// HEAD-scoped cursor mirror `recent`.
pub(super) fn run_search(
    ws: &Workspace,
    mut params: SearchGitHistoryParams,
) -> Result<SearchGitHistoryResponse, EngineError> {
    params.path = params.path.take().map(|path| without_empty_components(&path));
    let repo = require_git_repo(ws)?;
    let limit = params.limit.unwrap_or(LOG_LIMIT_DEFAULT).min(LOG_LIMIT_MAX) as usize;
    let scope = FtsScope::parse(params.field.as_deref());
    let range = TimeRange::parse(params.since.as_deref(), params.until.as_deref(), now_unix())?;
    let head = head_sha(repo)?;
    let snapshot = head_snapshot_id(&head);

    let skip = match params.cursor.as_ref() {
        Some(cursor) => {
            let (offset, snapshot_id) = cursor.decode_in_memory()?;
            if snapshot_id != snapshot {
                return Ok(SearchGitHistoryResponse {
                    commits: Vec::new(),
                    fuzzy: Vec::new(),
                    truncated: false,
                    truncated_reason: None,
                    next_cursor: None,
                    budgeted: false,
                    cursor_invalidated: true,
                });
            }
            offset as usize
        }
        None => 0,
    };

    let want = limit.saturating_add(1);
    // The index reads take no lock, so a sync that rebuilds the index between the freshness check
    // and the search can make it answer with too little: the answer counts only when the index still
    // holds `head` as it was read after it, else the live walk answers.
    let indexed = git_history_if_fresh(ws, &head).and_then(|index| {
        let query = CommitQuery {
            pattern: &params.pattern,
            scope,
            author: params.author.as_deref(),
            path: params.path.as_ref(),
            since: range.since,
            until: range.until,
        };
        let hits = index.search_commits_fuzzy(&query, skip, want);
        index.is_current().then_some(hits)
    });
    let (mut hits, truncated_reason) = match indexed {
        Some(hits) => {
            if hits.is_empty()
                && skip == 0
                && let Some(path) = &params.path
                && !path_in_history(ws, path)
            {
                return Err(no_such_path(ws, path));
            }
            (hits, None)
        }
        None => {
            let live = ws
                .git_cache
                .log(repo, &head, None, LOG_WALK_MAX as u32, params.path.is_some())
                .map_err(|e| EngineError::internal(format!("log: {e}")))?;
            let live_query = LiveQuery::new(&params, scope, range);
            let matched = live
                .iter()
                .filter(|commit| live_query.matches(commit))
                .skip(skip)
                .take(want)
                .map(|commit| SearchHit {
                    commit: commit.clone(),
                    fuzzy: false,
                })
                .collect();
            (matched, Some(UNINDEXED_SCAN))
        }
    };

    let has_more = hits.len() > limit;
    hits.truncate(limit);

    // `author` matches part of the name or the email, so it can select several people: each row
    // then says whose commit it is.
    let with_author = params.author.is_some() || scope == FtsScope::Author;
    let rows: Vec<(CommitView, bool)> = hits
        .into_iter()
        .map(|hit| {
            let view = CommitView {
                with_author,
                ..commit_to_view(hit.commit, false)
            };
            (view, hit.fuzzy)
        })
        .collect();
    let flags: Vec<bool> = rows.iter().map(|(_, fuzzy)| *fuzzy).collect();
    let (commits, budgeted) = budget_commits(rows.into_iter().map(|(view, _)| view).collect(), params.max_tokens);
    let fuzzy = commits
        .iter()
        .zip(flags)
        .filter(|(_, fuzzy)| *fuzzy)
        .map(|(view, _)| view.sha.clone())
        .collect();
    let has_more = has_more || budgeted;
    let kept = commits.len();
    let next_cursor = has_more.then(|| crate::engine::cursor::Cursor::encode_in_memory((skip + kept) as u64, snapshot));
    Ok(SearchGitHistoryResponse {
        commits,
        fuzzy,
        budgeted,
        truncated: truncated_reason.is_some(),
        truncated_reason,
        next_cursor,
        cursor_invalidated: false,
    })
}

/// The refusal for a `path` that no commit reachable from HEAD touches and that is not on disk
/// either: an empty history would not say the path is wrong. It names the paths of the history
/// nearest to the one given, when the index can say.
pub(super) fn no_such_path(ws: &Workspace, path: &RelPath) -> EngineError {
    let kind = if path.as_bytes().ends_with(b"/") {
        "directory"
    } else {
        "file"
    };
    let nearest = nearest_paths(ws, path);
    let hint = if nearest.is_empty() {
        String::new()
    } else {
        format!(" Nearest paths in the history: {}.", nearest.join(", "))
    };
    EngineError::invalid_argument(format!(
        "`git_log`: no commit reachable from HEAD touches `{path}` and no such {kind} exists. `path` is a \
             repo-relative file path, or a directory when it ends with `/`; locate it with `find_files`.{hint}"
    ))
}

/// Up to [`SUGGESTED_PATHS`] paths the history holds, nearest to `path` first, ranked as
/// `find_files` ranks a file name. Empty while the index is not fresh.
fn nearest_paths(ws: &Workspace, path: &RelPath) -> Vec<String> {
    let Ok(repo) = require_git_repo(ws) else {
        return Vec::new();
    };
    let Some(index) = head_sha(repo).ok().and_then(|head| git_history_if_fresh(ws, &head)) else {
        return Vec::new();
    };
    let all = index.history_paths();
    let candidates: Vec<&RelPath> = all.iter().collect();
    let wanted = path.to_str_lossy();
    let mut scored = crate::engine::ops::find_files::score(wanted.trim_end_matches('/'), &candidates);
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    scored
        .into_iter()
        .take(SUGGESTED_PATHS)
        .map(|(_, nearest)| nearest.to_str_lossy().into_owned())
        .collect()
}

/// Whether `path` is in the history, or on disk, or the clone is shallow, so the history before
/// its boundary says nothing: any of them makes an empty search an answer.
fn path_in_history(ws: &Workspace, path: &RelPath) -> bool {
    let Ok(repo) = require_git_repo(ws) else {
        return true;
    };
    if repo.is_shallow() || crate::path::contained_file(repo.workdir(), path).is_ok_and(|file| file.exists()) {
        return true;
    }
    let Some(index) = head_sha(repo).ok().and_then(|head| git_history_if_fresh(ws, &head)) else {
        return true;
    };
    let wanted = path.as_bytes();
    index.history_paths().iter().any(|known| {
        if wanted.ends_with(b"/") {
            known.as_bytes().starts_with(wanted)
        } else {
            known.as_bytes() == wanted
        }
    })
}

/// The bounds of a `since` / `until` pair, as Unix seconds of author time, both inclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct TimeRange {
    since: Option<i64>,
    until: Option<i64>,
}

impl TimeRange {
    fn parse(since: Option<&str>, until: Option<&str>, now: i64) -> Result<Self, EngineError> {
        let range = Self {
            since: since.map(|value| parse_bound("since", value, now, false)).transpose()?,
            until: until.map(|value| parse_bound("until", value, now, true)).transpose()?,
        };
        if let (Some(since), Some(until)) = (range.since, range.until)
            && since > until
        {
            return Err(EngineError::invalid_argument(format!(
                "`git_log`: `since` ({}) is after `until` ({}); no commit can be in that range",
                date_text(since),
                date_text(until)
            )));
        }
        Ok(range)
    }

    fn contains(&self, time: i64) -> bool {
        self.since.is_none_or(|since| time >= since) && self.until.is_none_or(|until| time <= until)
    }
}

fn date_text(seconds: i64) -> String {
    let (year, month, day) = civil_from_days(seconds.div_euclid(SECS_PER_DAY));
    format!("{year:04}-{month:02}-{day:02}")
}

/// One bound of a time range, at `now`. A date is that day in UTC: its first second for a `since`,
/// its last for an `until`. A count of days (`d`), weeks (`w`), months of 30 days (`m`) or years of
/// 365 days (`y`) is that long before `now`.
fn parse_bound(name: &str, value: &str, now: i64, end_of_day: bool) -> Result<i64, EngineError> {
    let refuse = || EngineError::invalid_argument(format!("`git_log`: `{name}` must be {BOUND_FORMS}, not `{value}`"));
    let value = value.trim();
    if let Some(day) = parse_date(value) {
        return Ok(if end_of_day { day + SECS_PER_DAY - 1 } else { day });
    }
    let unit = value.chars().last().ok_or_else(refuse)?;
    let days_per_unit = match unit {
        'd' => 1,
        'w' => 7,
        'm' => 30,
        'y' => 365,
        _ => return Err(refuse()),
    };
    let count: i64 = value[..value.len() - 1].parse().map_err(|_| refuse())?;
    if count < 0 {
        return Err(refuse());
    }
    count
        .checked_mul(days_per_unit * SECS_PER_DAY)
        .and_then(|span| now.checked_sub(span))
        .ok_or_else(refuse)
}

/// `YYYY-MM-DD` as the Unix seconds of that day's first second in UTC, or `None` for anything else,
/// including a day the month does not have.
fn parse_date(text: &str) -> Option<i64> {
    let mut parts = text.split('-');
    let (year, month, day) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || year.len() != 4 || month.len() != 2 || day.len() != 2 {
        return None;
    }
    let (year, month, day): (i64, i64, i64) = (year.parse().ok()?, month.parse().ok()?, day.parse().ok()?);
    let days = days_from_civil(year, month, day);
    // A date that rolled over into another month was not a date.
    (civil_from_days(days) == (year, month, day)).then(|| days * SECS_PER_DAY)
}

/// Days from 1970-01-01 to the proleptic Gregorian date (Hinnant's `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let shifted_month = (month + 9) % 12;
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The proleptic Gregorian date of `days` since 1970-01-01 (Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
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
    (year, month, day)
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
}

/// The search as the live walk, which has no index, can answer it: the words as typed.
struct LiveQuery<'a> {
    pattern: ahash::AHashSet<String>,
    scope: FtsScope,
    author: Option<ahash::AHashSet<String>>,
    path: Option<&'a RelPath>,
    range: TimeRange,
}

impl<'a> LiveQuery<'a> {
    fn new(params: &'a SearchGitHistoryParams, scope: FtsScope, range: TimeRange) -> Self {
        Self {
            pattern: fts::query_terms(&params.pattern).into_iter().collect(),
            scope,
            author: params
                .author
                .as_deref()
                .map(|author| fts::query_terms(author).into_iter().collect()),
            path: params.path.as_ref(),
            range,
        }
    }

    fn matches(&self, commit: &CommitInfo) -> bool {
        fts::commit_matches_terms(commit, &self.pattern, self.scope)
            && self
                .author
                .as_ref()
                .is_none_or(|terms| fts::commit_matches_terms(commit, terms, FtsScope::Author))
            && self.range.contains(commit.author_time_unix)
            && self.path.is_none_or(|path| touches(commit, path))
    }
}

/// `path` with its empty components (a doubled or leading `/`) dropped, bytes untouched. The
/// trailing `/` that marks a directory stays, so `src//config/` still matches the files under it.
fn without_empty_components(path: &RelPath) -> RelPath {
    let bytes = path.as_bytes();
    let kept: Vec<&[u8]> = bytes.split(|&b| b == b'/').filter(|c| !c.is_empty()).collect();
    let mut normal = kept.join(&b'/');
    if !normal.is_empty() && bytes.ends_with(b"/") {
        normal.push(b'/');
    }
    RelPath::from(normal.as_slice())
}

/// Whether `commit` changed `path`, or a file under it when it ends with `/`.
fn touches(commit: &CommitInfo, path: &RelPath) -> bool {
    let wanted = path.as_bytes();
    commit.files.iter().any(|(file, _)| {
        if wanted.ends_with(b"/") {
            file.as_bytes().starts_with(wanted)
        } else {
            file.as_bytes() == wanted
        }
    })
}

#[cfg(test)]
#[path = "search_tests.rs"]
mod tests;
