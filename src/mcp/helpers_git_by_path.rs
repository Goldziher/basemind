//! The `by_path` operation behind `git_log` with a directory `path` and `basemind git by-path`: a
//! cheap pickaxe that pages through the history a window of commits at a time. Split from
//! `helpers_git.rs` for the 1000-line per-file cap.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::ServerState;
use super::helpers::{
    commit_to_view, elapsed_us, git_history_if_fresh, head_sha, head_snapshot_id, json_result, require_git_repo,
};
use super::helpers_filters::reject_zero_limit;
use super::helpers_git_show::budget_commits;
use super::types::{CommitView, FindCommitsByPathParams, FindCommitsByPathResponse};

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

/// The `by_path` operation: a cheap pickaxe — regex over the changed **paths** of a bounded
/// window of commits, never over patch text.
pub(super) fn run_by_path(state: &ServerState, params: FindCommitsByPathParams) -> Result<CallToolResult, McpError> {
    json_result(&by_path(state, "git_by_path", params)?.response)
}

/// One page inspects a window of commits, newest first, starting at the commit offset its cursor
/// carries. A window that ends before the history does hands back a cursor to the commit after it,
/// so every match is reachable however far back it lies; a page can then be short, or empty, and
/// still not be the last.
pub(super) fn by_path(
    state: &ServerState,
    tool: &str,
    params: FindCommitsByPathParams,
) -> Result<ByPathPage, McpError> {
    let started = std::time::Instant::now();
    // A window or page of 0 makes no progress, and its cursor would hand back the same offset forever.
    reject_zero_limit(tool, params.limit)?;
    if params.window == Some(0) {
        return Err(McpError::invalid_params(
            format!("`{tool}`: `window` must be at least 1; omit it for the default of 200 commits."),
            None,
        ));
    }
    let repo = require_git_repo(state)?;
    let re = regex::Regex::new(&params.pattern)
        .map_err(|e| McpError::invalid_params(format!("invalid regex: {e}"), None))?;
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
                        pattern: params.pattern,
                        window_inspected: 0,
                        unmatched: None,
                        truncated: false,
                        truncated_reason: None,
                        commits: Vec::new(),
                        next_cursor: None,
                        budgeted: false,
                        cursor_invalidated: true,
                        elapsed_us: elapsed_us(started),
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
    let indexed = git_history_if_fresh(state, &head)
        .map(|index| index.recent_commits(start, fetch, true))
        .filter(|commits| !commits.is_empty());
    let snapshot = snapshot_of(indexed.is_none());
    if cursor_snapshot.is_some_and(|issued| issued != snapshot) {
        return Err(super::cursor_bind::walk_switched(tool));
    }
    let mut commits = match indexed {
        Some(commits) => commits,
        // Also the answer to an empty indexed lookup, which a failed forward returns as well. Only
        // this page's window has its files listed, and nothing is kept per page: a log cached under
        // its depth would be a new entry, walked again from HEAD, for every page.
        None => repo
            .log_window(&head, start, fetch, true)
            .map_err(|e| McpError::internal_error(format!("log: {e}"), None))?,
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
    let (hits, budgeted) = budget_commits(hits, params.max_tokens);
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
            pattern: params.pattern,
            window_inspected: u32::try_from(commits.len()).unwrap_or(u32::MAX),
            unmatched: None,
            truncated: window_edge,
            truncated_reason: window_edge.then_some("window"),
            commits: hits,
            budgeted,
            next_cursor: next_offset
                .map(|offset| super::cursor::Cursor::encode_in_memory(offset as u64 | carried, snapshot)),
            cursor_invalidated: false,
            elapsed_us: elapsed_us(started),
        },
        matched,
    })
}

#[cfg(test)]
#[path = "helpers_git_by_path_tests.rs"]
mod tests;
