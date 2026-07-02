//! Body of the `search_git_history` tool, extracted from `tools_git.rs` to keep it under the
//! 1000-line cap. The other git shims are still inline there; this one is a helper because the
//! FTS query + live fallback is large enough to tip the file over.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::ServerState;
use super::helpers::{
    LOG_LIMIT_DEFAULT, LOG_LIMIT_MAX, LOG_WALK_MAX, git_history_if_fresh, head_sha,
    head_snapshot_id, json_result, require_git_repo,
};
use super::types::{GitCommitHit, SearchGitHistoryParams, SearchGitHistoryResponse};
use crate::git::CommitInfo;
use crate::git_history::fts::{self, FtsScope};

/// Full-text search over git history. Uses the git-history inverted index when it is fresh
/// (`last_indexed_head == HEAD`), searching author name + email + summary + full body; otherwise
/// degrades to a bounded live walk over the recent window, flagged `partial` (author + summary
/// only, no body). Pagination and the HEAD-scoped cursor mirror `recent_changes`.
pub(super) fn run_search_git_history(
    state: &ServerState,
    params: SearchGitHistoryParams,
) -> Result<CallToolResult, McpError> {
    let repo = require_git_repo(state)?;
    let limit = params.limit.unwrap_or(LOG_LIMIT_DEFAULT).min(LOG_LIMIT_MAX) as usize;
    let scope = FtsScope::parse(params.field.as_deref());
    let head = head_sha(repo)?;
    let snapshot = head_snapshot_id(&head);

    // Decode the cursor (offset + HEAD-snapshot guard). A stale snapshot → empty page +
    // cursor_invalidated so the caller restarts, exactly like the other git-log tools.
    let skip = match params.cursor.as_ref() {
        Some(cursor) => {
            let (offset, snapshot_id) = cursor.decode_in_memory()?;
            if snapshot_id != snapshot {
                return json_result(&SearchGitHistoryResponse {
                    commits: Vec::new(),
                    partial: false,
                    next_cursor: None,
                    cursor_invalidated: true,
                });
            }
            offset as usize
        }
        None => 0,
    };

    // One extra past the page tells us whether more remain.
    let want = limit.saturating_add(1);
    let (mut hits, partial) = match git_history_if_fresh(state, &head) {
        Some(index) => (
            index.search_commits(&params.pattern, scope, skip, want),
            false,
        ),
        None => {
            // No fresh index (read-only session or still building): bounded live fallback. Walk the
            // recent window and reuse the same tokenized-AND matcher for consistent semantics.
            // Tokenize the (loop-invariant) query ONCE, not per commit across the whole window.
            let mut query_terms = ahash::AHashSet::new();
            fts::tokenize(&params.pattern, &mut query_terms);
            let window = LOG_WALK_MAX as u32;
            let live = state
                .git_cache
                .log(repo, &head, None, window, false)
                .map_err(|e| McpError::internal_error(format!("log: {e}"), None))?;
            let matched: Vec<CommitInfo> = live
                .iter()
                .filter(|c| fts::commit_matches_terms(c, &query_terms, scope))
                .skip(skip)
                .take(want)
                .cloned()
                .collect();
            (matched, true)
        }
    };

    let has_more = hits.len() > limit;
    hits.truncate(limit);
    let next_cursor = has_more
        .then(|| super::cursor::Cursor::encode_in_memory((skip + hits.len()) as u64, snapshot));

    let commits: Vec<GitCommitHit> = hits.into_iter().map(commit_to_hit).collect();
    json_result(&SearchGitHistoryResponse {
        commits,
        partial,
        next_cursor,
        cursor_invalidated: false,
    })
}

fn commit_to_hit(c: CommitInfo) -> GitCommitHit {
    GitCommitHit {
        sha: c.sha,
        short_sha: c.short_sha,
        summary: c.summary,
        author: c.author,
        author_email: c.author_email,
        author_time_unix: c.author_time_unix,
        body: c.body,
    }
}
