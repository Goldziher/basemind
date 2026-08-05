//! Helper bodies for the consolidated `git` domain tool — one `run_<mode>` per [`GitMode`], plus
//! the [`run_git`] dispatcher the `#[tool]` shim and the CLI both call.
//!
//! This file owns the commit-log family (`status`, `recent`, `touching`, `by_path`, `churn`,
//! `search`); the five modes scoped to a single path (`diff`, `diff_outline`, `blame`,
//! `blame_symbol`, `symbol_history`) live in `helpers_git_file.rs`. The split is only about the
//! 1000-line per-file cap — both halves are reached through the dispatcher here.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::ServerState;
use super::helpers::{
    LOG_LIMIT_DEFAULT, LOG_LIMIT_MAX, LOG_WALK_MAX, commit_to_view, elapsed_us, git_history_if_fresh, head_sha,
    head_snapshot_id, json_result, require_git_repo,
};
use super::mode::{GitMode, reject_unsupported};
use super::types::{
    CommitView, CommitsTouchingParams, CommitsTouchingResponse, FindCommitsByPathParams, FindCommitsByPathResponse,
    GitCommitHit, HotFileEntry, HotFilesParams, HotFilesResponse, RecentChangesParams, RecentChangesResponse,
    SearchGitHistoryParams, SearchGitHistoryResponse, WorkingTreeStatusView,
};
use super::types_git::GitParams;
use crate::git::CommitInfo;
use crate::git_history::fts::{self, FtsScope};
use crate::path::RelPath;

/// Fail a mode that was given a field belonging to some other mode.
///
/// Inverted against `allowed` rather than listing every rejected field per mode: with eleven modes
/// and sixteen sibling fields, an explicit per-mode reject list is where a newly added field
/// silently becomes accept-everywhere.
fn reject_foreign_fields(mode: GitMode, present: &[(&str, bool)], allowed: &[&str]) -> Result<(), McpError> {
    let foreign: Vec<(&str, bool)> = present
        .iter()
        .filter(|(field, _)| !allowed.contains(field))
        .copied()
        .collect();
    reject_unsupported(GitMode::DOMAIN, mode.as_str(), &foreign)
}

/// Unwrap a field this mode cannot run without, naming the exact `mode`/field pair.
pub(super) fn require_field<T>(mode: GitMode, field: &str, value: Option<T>) -> Result<T, McpError> {
    value.ok_or_else(|| McpError::invalid_params(format!("`git` mode=\"{}\" requires `{field}`", mode.as_str()), None))
}

/// Dispatch the single `git` tool onto the per-operation helper its `mode` selects.
///
/// Fields belonging to another mode are rejected rather than dropped: a silently ignored `rev` on a
/// `diff` call reads to an agent as a diff against the revision it named.
pub(super) async fn run_git(state: &ServerState, params: GitParams) -> Result<CallToolResult, McpError> {
    let GitParams {
        mode,
        path,
        pattern,
        field,
        name,
        kind,
        limit,
        cursor,
        include_files,
        top_k,
        window,
        rev,
        rev_old,
        rev_new,
        line_start,
        line_end,
        hash_mode,
    } = params;
    let present = [
        ("path", path.is_some()),
        ("pattern", pattern.is_some()),
        ("field", field.is_some()),
        ("name", name.is_some()),
        ("kind", kind.is_some()),
        ("limit", limit.is_some()),
        ("cursor", cursor.is_some()),
        ("include_files", include_files.is_some()),
        ("top_k", top_k.is_some()),
        ("window", window.is_some()),
        ("rev", rev.is_some()),
        ("rev_old", rev_old.is_some()),
        ("rev_new", rev_new.is_some()),
        ("line_start", line_start.is_some()),
        ("line_end", line_end.is_some()),
        ("hash_mode", hash_mode.is_some()),
    ];
    let reject = |allowed: &[&str]| reject_foreign_fields(mode, &present, allowed);

    match mode {
        GitMode::Status => {
            reject(&[])?;
            run_status(state)
        }
        GitMode::Recent => {
            reject(&["limit", "cursor", "include_files"])?;
            run_recent(
                state,
                RecentChangesParams {
                    limit,
                    include_files: include_files.unwrap_or(true),
                    cursor,
                },
            )
        }
        GitMode::Touching => {
            reject(&["path", "limit", "cursor"])?;
            run_touching(
                state,
                CommitsTouchingParams {
                    path: require_field(mode, "path", path)?,
                    limit,
                    cursor,
                },
            )
        }
        GitMode::ByPath => {
            reject(&["pattern", "window", "limit", "cursor"])?;
            run_by_path(
                state,
                FindCommitsByPathParams {
                    pattern: require_field(mode, "pattern", pattern)?,
                    window,
                    limit,
                    cursor,
                },
            )
        }
        GitMode::Churn => {
            reject(&["window", "top_k"])?;
            run_churn(state, HotFilesParams { window, top_k })
        }
        GitMode::Diff => {
            reject(&["path", "rev_old", "rev_new"])?;
            super::helpers_git_file::run_diff(
                state,
                super::types::DiffFileParams {
                    rev_old: require_field(mode, "rev_old", rev_old)?,
                    rev_new: require_field(mode, "rev_new", rev_new)?,
                    path: require_field(mode, "path", path)?,
                },
            )
        }
        GitMode::DiffOutline => {
            reject(&["path", "rev"])?;
            super::helpers_git_file::run_diff_outline(
                state,
                super::types::DiffOutlineParams {
                    path: require_field(mode, "path", path)?,
                    rev,
                },
            )
            .await
        }
        GitMode::Blame => {
            reject(&["path", "rev", "line_start", "line_end", "limit", "cursor"])?;
            super::helpers_git_file::run_blame(
                state,
                super::types::BlameFileParams {
                    path: require_field(mode, "path", path)?,
                    line_start,
                    line_end,
                    rev,
                    limit,
                    cursor,
                },
            )
        }
        GitMode::BlameSymbol => {
            reject(&["path", "name", "kind", "rev", "limit", "cursor"])?;
            super::helpers_git_file::run_blame_symbol(
                state,
                super::types::BlameSymbolParams {
                    path: require_field(mode, "path", path)?,
                    name: require_field(mode, "name", name)?,
                    kind,
                    rev,
                    limit,
                    cursor,
                },
            )
            .await
        }
        GitMode::SymbolHistory => {
            reject(&["path", "name", "kind", "limit", "hash_mode", "cursor"])?;
            super::helpers_git_file::run_symbol_history(
                state,
                super::types::SymbolHistoryParams {
                    path: require_field(mode, "path", path)?,
                    name: require_field(mode, "name", name)?,
                    kind,
                    limit,
                    hash_mode,
                    cursor,
                },
            )
        }
        GitMode::Search => {
            reject(&["pattern", "field", "limit", "cursor"])?;
            run_search(
                state,
                SearchGitHistoryParams {
                    pattern: require_field(mode, "pattern", pattern)?,
                    field,
                    limit,
                    cursor,
                },
            )
        }
    }
}

/// Reject a path that lives outside the repository (a `scan.extra_roots` file, keyed by its
/// absolute path). Such files are indexed for the code map but are not tracked by git, so blame
/// has nothing to resolve against — return a clear error instead of an opaque gix failure.
pub(super) fn reject_external_path(path: &RelPath) -> Result<(), McpError> {
    if path.is_external() {
        return Err(McpError::invalid_params(
            format!(
                "path is outside the git repository (indexed via scan.extra_roots); \
                 blame is unavailable for external files: {path}"
            ),
            None,
        ));
    }
    Ok(())
}

/// Body for `git` mode `status`: the working tree's staged / unstaged / untracked buckets.
fn run_status(state: &ServerState) -> Result<CallToolResult, McpError> {
    let started = std::time::Instant::now();
    let repo = require_git_repo(state)?;
    let s = repo
        .status_porcelain()
        .map_err(|e| McpError::internal_error(format!("git status: {e}"), None))?;
    let is_clean = s.staged_added.is_empty()
        && s.staged_modified.is_empty()
        && s.staged_deleted.is_empty()
        && s.modified.is_empty()
        && s.untracked.is_empty();
    json_result(&WorkingTreeStatusView {
        staged_added: s.staged_added,
        staged_modified: s.staged_modified,
        staged_deleted: s.staged_deleted,
        modified: s.modified,
        untracked: s.untracked,
        is_clean,
        elapsed_us: elapsed_us(started),
    })
}

/// Body for `git` mode `recent`: a bounded recency window over HEAD's ancestry, newest first.
fn run_recent(state: &ServerState, params: RecentChangesParams) -> Result<CallToolResult, McpError> {
    let started = std::time::Instant::now();
    let repo = require_git_repo(state)?;
    let limit = params.limit.unwrap_or(LOG_LIMIT_DEFAULT).min(LOG_LIMIT_MAX) as usize;
    let head = head_sha(repo)?;
    let snapshot = head_snapshot_id(&head);

    let skip = match params.cursor.as_ref() {
        Some(c) => {
            let (offset, snapshot_id) = c.decode_in_memory()?;
            if snapshot_id != snapshot {
                return json_result(&RecentChangesResponse {
                    commits: Vec::new(),
                    truncated: false,
                    truncated_reason: None,
                    next_cursor: None,
                    cursor_invalidated: true,
                    elapsed_us: elapsed_us(started),
                });
            }
            offset as usize
        }
        None => 0,
    };

    let walk_depth = (skip.saturating_add(limit).saturating_add(1)).min(LOG_WALK_MAX) as u32;
    let commits: Vec<CommitInfo> = match git_history_if_fresh(state, &head) {
        Some(index) => index.recent_commits(0, walk_depth as usize, params.include_files),
        None => state
            .shared
            .git_cache
            .log(repo, &head, None, walk_depth, params.include_files)
            .map_err(|e| McpError::internal_error(format!("log: {e}"), None))?
            .as_ref()
            .clone(),
    };
    let page: Vec<CommitView> = commits
        .iter()
        .skip(skip)
        .take(limit)
        .cloned()
        .map(|c| commit_to_view(c, params.include_files))
        .collect();
    let has_more = commits.len() > skip + page.len();
    let next_cursor = has_more.then(|| super::cursor::Cursor::encode_in_memory((skip + page.len()) as u64, snapshot));
    let truncated = repo.is_shallow();
    json_result(&RecentChangesResponse {
        commits: page,
        truncated,
        truncated_reason: truncated.then_some("shallow_clone"),
        next_cursor,
        cursor_invalidated: false,
        elapsed_us: elapsed_us(started),
    })
}

/// Body for `git` mode `touching`: commits whose tree differs from the parent's at `path`.
fn run_touching(state: &ServerState, params: CommitsTouchingParams) -> Result<CallToolResult, McpError> {
    let started = std::time::Instant::now();
    let repo = require_git_repo(state)?;
    let limit = params.limit.unwrap_or(LOG_LIMIT_DEFAULT).min(LOG_LIMIT_MAX) as usize;
    let head = head_sha(repo)?;
    let snapshot = head_snapshot_id(&head);

    let skip = match params.cursor.as_ref() {
        Some(c) => {
            let (offset, snapshot_id) = c.decode_in_memory()?;
            if snapshot_id != snapshot {
                return json_result(&CommitsTouchingResponse {
                    path: params.path,
                    commits: Vec::new(),
                    truncated: false,
                    truncated_reason: None,
                    next_cursor: None,
                    cursor_invalidated: true,
                    elapsed_us: elapsed_us(started),
                });
            }
            offset as usize
        }
        None => 0,
    };

    let walk_depth = (skip.saturating_add(limit).saturating_add(1)).min(LOG_WALK_MAX) as u32;
    let commits: Vec<CommitInfo> = match git_history_if_fresh(state, &head) {
        Some(index) => index.commits_touching(&params.path, 0, walk_depth as usize),
        None => state
            .shared
            .git_cache
            .log(repo, &head, Some(&params.path), walk_depth, false)
            .map_err(|e| McpError::internal_error(format!("log: {e}"), None))?
            .as_ref()
            .clone(),
    };
    let page: Vec<CommitView> = commits
        .iter()
        .skip(skip)
        .take(limit)
        .cloned()
        .map(|c| commit_to_view(c, false))
        .collect();
    let has_more = commits.len() > skip + page.len();
    let next_cursor = has_more.then(|| super::cursor::Cursor::encode_in_memory((skip + page.len()) as u64, snapshot));
    let truncated = repo.is_shallow();
    json_result(&CommitsTouchingResponse {
        path: params.path,
        commits: page,
        truncated,
        truncated_reason: truncated.then_some("shallow_clone"),
        next_cursor,
        cursor_invalidated: false,
        elapsed_us: elapsed_us(started),
    })
}

/// Body for `git` mode `by_path`: a cheap pickaxe — regex over the changed **paths** of a bounded
/// recent window, never over patch text.
fn run_by_path(state: &ServerState, params: FindCommitsByPathParams) -> Result<CallToolResult, McpError> {
    let started = std::time::Instant::now();
    let repo = require_git_repo(state)?;
    let re = regex::Regex::new(&params.pattern)
        .map_err(|e| McpError::invalid_params(format!("invalid regex: {e}"), None))?;
    let window = params.window.unwrap_or(200).min(1000);
    let limit = params.limit.unwrap_or(50).min(500) as usize;

    let head = head_sha(repo)?;
    let snapshot = head_snapshot_id(&head);

    let skip = match params.cursor.as_ref() {
        Some(c) => {
            let (offset, snapshot_id) = c.decode_in_memory()?;
            if snapshot_id != snapshot {
                return json_result(&FindCommitsByPathResponse {
                    pattern: params.pattern,
                    window_inspected: window,
                    commits: Vec::new(),
                    next_cursor: None,
                    cursor_invalidated: true,
                    elapsed_us: elapsed_us(started),
                });
            }
            offset as usize
        }
        None => 0,
    };

    let commits: Vec<CommitInfo> = match git_history_if_fresh(state, &head) {
        Some(index) => index.window_commits(window as usize),
        None => state
            .shared
            .git_cache
            .log(repo, &head, None, window, true)
            .map_err(|e| McpError::internal_error(format!("log: {e}"), None))?
            .as_ref()
            .clone(),
    };

    let mut hits: Vec<CommitView> = Vec::new();
    let mut seen: usize = 0;
    let mut has_more = false;
    for c in commits.iter() {
        if !c.files.iter().any(|(p, _)| re.is_match(&p.to_str_lossy())) {
            continue;
        }
        if seen < skip {
            seen += 1;
            continue;
        }
        if hits.len() >= limit {
            has_more = true;
            break;
        }
        seen += 1;
        hits.push(commit_to_view(c.clone(), true));
    }
    let next_cursor = has_more.then(|| super::cursor::Cursor::encode_in_memory((skip + hits.len()) as u64, snapshot));
    json_result(&FindCommitsByPathResponse {
        pattern: params.pattern,
        window_inspected: window,
        commits: hits,
        next_cursor,
        cursor_invalidated: false,
        elapsed_us: elapsed_us(started),
    })
}

/// Body for `git` mode `churn`: the top-K most-frequently-modified files in a recent window.
fn run_churn(state: &ServerState, params: HotFilesParams) -> Result<CallToolResult, McpError> {
    let started = std::time::Instant::now();
    let repo = require_git_repo(state)?;
    let window = params.window.unwrap_or(200).min(2000);
    let top_k = params.top_k.unwrap_or(20).min(200) as usize;
    let head = head_sha(repo)?;
    let commits: Vec<CommitInfo> = match git_history_if_fresh(state, &head) {
        Some(index) => index.window_commits(window as usize),
        None => state
            .shared
            .git_cache
            .log(repo, &head, None, window, true)
            .map_err(|e| McpError::internal_error(format!("log: {e}"), None))?
            .as_ref()
            .clone(),
    };

    let mut counts: ahash::AHashMap<RelPath, (u32, u32, u32, u32)> = ahash::AHashMap::new();
    for c in commits.iter() {
        for (path, kind) in &c.files {
            let entry = counts.entry(path.clone()).or_insert((0, 0, 0, 0));
            entry.0 += 1;
            match kind {
                crate::git::ChangeKind::Added => entry.1 += 1,
                crate::git::ChangeKind::Modified | crate::git::ChangeKind::Renamed => entry.2 += 1,
                crate::git::ChangeKind::Deleted => entry.3 += 1,
            }
        }
    }
    let total_files_changed = counts.len() as u32;
    let mut ranked: Vec<HotFileEntry> = counts
        .into_iter()
        .map(|(path, (n, added, modified, deleted))| HotFileEntry {
            path,
            commits_touching: n,
            added,
            modified,
            deleted,
        })
        .collect();
    ranked.sort_by(|a, b| b.commits_touching.cmp(&a.commits_touching).then(a.path.cmp(&b.path)));
    ranked.truncate(top_k);

    json_result(&HotFilesResponse {
        window_inspected: window,
        total_files_changed,
        files: ranked,
        elapsed_us: elapsed_us(started),
    })
}

/// Body for `git` mode `search`: full-text search over git history. Uses the git-history inverted
/// index when it is fresh (`last_indexed_head == HEAD`), searching author name + email + summary +
/// full body; otherwise degrades to a bounded live walk over the recent window, flagged `partial`
/// (author + summary only, no body). Pagination and the HEAD-scoped cursor mirror `recent`.
fn run_search(state: &ServerState, params: SearchGitHistoryParams) -> Result<CallToolResult, McpError> {
    let started = std::time::Instant::now();
    let repo = require_git_repo(state)?;
    let limit = params.limit.unwrap_or(LOG_LIMIT_DEFAULT).min(LOG_LIMIT_MAX) as usize;
    let scope = FtsScope::parse(params.field.as_deref());
    let head = head_sha(repo)?;
    let snapshot = head_snapshot_id(&head);

    let skip = match params.cursor.as_ref() {
        Some(cursor) => {
            let (offset, snapshot_id) = cursor.decode_in_memory()?;
            if snapshot_id != snapshot {
                return json_result(&SearchGitHistoryResponse {
                    commits: Vec::new(),
                    partial: false,
                    next_cursor: None,
                    cursor_invalidated: true,
                    elapsed_us: elapsed_us(started),
                });
            }
            offset as usize
        }
        None => 0,
    };

    let want = limit.saturating_add(1);
    let (mut hits, partial) = match git_history_if_fresh(state, &head) {
        Some(index) => (index.search_commits(&params.pattern, scope, skip, want), false),
        None => {
            let mut query_terms = ahash::AHashSet::new();
            fts::tokenize(&params.pattern, &mut query_terms);
            let window = LOG_WALK_MAX as u32;
            let live = state
                .shared
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
    let next_cursor = has_more.then(|| super::cursor::Cursor::encode_in_memory((skip + hits.len()) as u64, snapshot));

    let commits: Vec<GitCommitHit> = hits.into_iter().map(commit_to_hit).collect();
    json_result(&SearchGitHistoryResponse {
        commits,
        partial,
        next_cursor,
        cursor_invalidated: false,
        elapsed_us: elapsed_us(started),
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
