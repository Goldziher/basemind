//! The `by_path` operation behind `git_log` with a directory `path` and `basemind git by-path`: a
//! cheap pickaxe that pages through the history a window of commits at a time. Split from
//! `log.rs` for the 1000-line per-file cap.

use super::common::{commit_to_view, git_history_if_fresh, head_sha, head_snapshot_id, require_git_repo};
use super::show::GIT_BUDGET_DEFAULT_TOKENS;
use crate::engine::budget::keep_prefix;
use crate::engine::cursor::reject_zero_limit;
use crate::engine::error::EngineError;
use crate::engine::tokens::bytes_to_tokens;
use crate::engine::types::git::{CommitFileView, CommitView, FindCommitsByPathParams, FindCommitsByPathResponse};
use crate::engine::workspace::Workspace;

/// Set in a paged window's cursor offset when a commit on an earlier page matched.
pub(super) const MATCHED_EARLIER: u64 = 1 << 63;

/// Set in a paged window's cursor snapshot when its offset counts in the live walk's list. The
/// git-history index appends the commits a sync finds after the ones it holds, where the live walk
/// interleaves them by commit time, so an offset into one list is not an offset into the other, and
/// the index can come up between two pages.
pub(super) const LIVE_WALK: u32 = 1;

/// One page of [`by_path`].
pub(super) struct ByPathPage {
    pub(super) response: FindCommitsByPathResponse,
    /// A commit on this page or an earlier one matched.
    pub(super) matched: bool,
}

impl Workspace {
    /// `basemind git by-path`: a cheap pickaxe — regex over the changed **paths** of a bounded
    /// window of commits, never over patch text.
    pub(crate) fn git_by_path(
        &self,
        params: FindCommitsByPathParams,
    ) -> Result<FindCommitsByPathResponse, EngineError> {
        Ok(by_path(self, "git_by_path", params)?.response)
    }
}

/// One page inspects a window of commits, newest first, starting at the commit offset its cursor
/// carries. A window that ends before the history does hands back a cursor to the commit after it,
/// so every match is reachable however far back it lies; a page can then be short, or empty, and
/// still not be the last.
pub(super) fn by_path(ws: &Workspace, tool: &str, params: FindCommitsByPathParams) -> Result<ByPathPage, EngineError> {
    // A window or page of 0 makes no progress, and its cursor would hand back the same offset forever.
    reject_zero_limit(tool, params.limit)?;
    if params.window == Some(0) {
        return Err(EngineError::invalid_argument(format!(
            "`{tool}`: `window` must be at least 1; omit it for the default of 200 commits."
        )));
    }
    let repo = require_git_repo(ws)?;
    let re =
        regex::Regex::new(&params.pattern).map_err(|e| EngineError::invalid_argument(format!("invalid regex: {e}")))?;
    let window = params.window.unwrap_or(200).min(1000) as usize;
    let limit = params.limit.unwrap_or(50).min(500) as usize;

    let head = head_sha(repo)?;
    let snapshot_of = |live: bool| (head_snapshot_id(&head) & !LIVE_WALK) | u32::from(live);

    let (start, matched_earlier, cursor_snapshot) = match params.cursor.as_ref() {
        Some(c) => {
            let (packed, snapshot_id) = c.decode_in_memory()?;
            if snapshot_id & !LIVE_WALK != snapshot_of(false) {
                return Ok(ByPathPage {
                    response: FindCommitsByPathResponse {
                        window_inspected: 0,
                        unmatched: None,
                        truncated: false,
                        truncated_reason: None,
                        commits: Vec::new(),
                        files: Vec::new(),
                        next_cursor: None,
                        budgeted: false,
                        cursor_invalidated: true,
                    },
                    matched: true,
                });
            }
            (
                usize::try_from(packed & !MATCHED_EARLIER).unwrap_or(usize::MAX),
                packed & MATCHED_EARLIER != 0,
                Some(snapshot_id),
            )
        }
        None => (0, false, None),
    };

    // One commit past the window is asked for, only to learn whether history continues beyond it.
    let fetch = window + 1;
    // ~keep A cursor cut from the live walk keeps paging it, so an index that catches up between two
    // ~keep pages does not cut the walk off; one cut from the index needs the index.
    let live_cursor = cursor_snapshot.is_some_and(|issued| issued & LIVE_WALK != 0);
    let indexed = git_history_if_fresh(ws, &head)
        .filter(|_| !live_cursor)
        .map(|index| index.recent_commits(start, fetch, true))
        .filter(|commits| !commits.is_empty());
    let snapshot = snapshot_of(indexed.is_none());
    if cursor_snapshot.is_some_and(|issued| issued != snapshot) {
        return Err(crate::engine::cursor::walk_switched(tool));
    }
    let mut commits = match indexed {
        Some(commits) => commits,
        // Also the answer to an empty indexed lookup, which a failed forward returns as well. Only
        // this page's window has its files listed, and nothing is kept per page: a log cached under
        // its depth would be a new entry, walked again from HEAD, for every page.
        None => repo
            .log_window(&head, start, fetch, true)
            .map_err(|e| EngineError::internal(format!("log: {e}")))?,
    };
    // A history that ends exactly at the window is whole.
    let window_exhausted = commits.len() > window;
    commits.truncate(window);

    // Each hit with its commit's offset in the newest-first list, and the first hit `limit` left out.
    let mut offsets: Vec<usize> = Vec::new();
    let mut hits: Vec<CommitView> = Vec::new();
    let mut left_out: Option<usize> = None;
    for (index, c) in commits.iter().enumerate() {
        if !c.files.iter().any(|(p, _)| re.is_match(&p.to_str_lossy())) {
            continue;
        }
        if hits.len() >= limit {
            left_out = Some(start + index);
            break;
        }
        offsets.push(start + index);
        hits.push(commit_to_view(c.clone(), true));
    }
    let matched = matched_earlier || !offsets.is_empty();
    // A row is written without its files, which go to `files`: cost both.
    let budget = keep_prefix(
        hits,
        Some(params.max_tokens.unwrap_or(GIT_BUDGET_DEFAULT_TOKENS)),
        |commit| {
            let files = commit.files.as_deref().map_or(0, |files| {
                files
                    .iter()
                    .map(|file| file.path.as_bytes().len() as u64 + 16)
                    .sum::<u64>()
            });
            bytes_to_tokens(commit.sha.len() as u64 + commit.summary.len() as u64 + 8 + files).max(1)
        },
    );
    let (mut hits, budgeted) = (budget.items, budget.budgeted);
    let files: Vec<(String, Vec<CommitFileView>)> = hits
        .iter_mut()
        .map(|commit| (commit.sha.clone(), commit.files.take().unwrap_or_default()))
        .collect();
    // The next page starts at the first hit this one left out, or past the window when the window
    // held no more hits and the history goes on.
    let next_offset = offsets
        .get(hits.len())
        .copied()
        .or(left_out)
        .or(window_exhausted.then_some(start + window));
    let window_edge = window_exhausted && left_out.is_none() && hits.len() == offsets.len();
    let carried = if matched { MATCHED_EARLIER } else { 0 };
    Ok(ByPathPage {
        response: FindCommitsByPathResponse {
            window_inspected: u32::try_from(commits.len()).unwrap_or(u32::MAX),
            unmatched: None,
            truncated: window_edge,
            truncated_reason: window_edge.then_some("window"),
            commits: hits,
            files,
            budgeted,
            next_cursor: next_offset
                .map(|offset| crate::engine::cursor::Cursor::encode_in_memory(offset as u64 | carried, snapshot)),
            cursor_invalidated: false,
        },
        matched,
    })
}

#[cfg(test)]
#[path = "by_path_tests.rs"]
mod tests;
