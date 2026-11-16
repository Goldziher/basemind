//! Bodies of the git verb tools (`git_log`, `git_blame`, `git_diff`) and of the commit-log
//! operations behind them.
//!
//! This file owns the commit-log family (`status`, `recent`, `touching`, `churn`, `search`);
//! `by_path` lives in `helpers_git_by_path.rs`, and the operations scoped to a single path (`diff`,
//! `diff_outline`, `blame`, `blame_symbol`, `symbol_history`) in `helpers_git_file.rs`. The split
//! is only about the 1000-line per-file cap.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::ServerState;
use super::helpers::{
    LOG_LIMIT_DEFAULT, LOG_LIMIT_MAX, LOG_WALK_MAX, commit_to_view, elapsed_us, git_history_if_fresh, head_sha,
    head_snapshot_id, json_result, require_git_repo,
};
use super::helpers_git_by_path::{LIVE_WALK, by_path};
use super::helpers_git_show::{budget_commits, commit_to_hit, resolve_body_chars, run_show};
use super::index_route::blocking;
use super::tool_contract::budget;
use super::types::{
    BlameFileParams, BlameSymbolParams, CommitView, CommitsTouchingParams, CommitsTouchingResponse, DiffFileParams,
    DiffOutlineParams, FindCommitsByPathParams, HotFileEntry, HotFilesParams, HotFilesResponse, RecentChangesParams,
    RecentChangesResponse, SearchGitHistoryParams, SearchGitHistoryResponse, SymbolHistoryParams,
    WorkingTreeStatusView,
};
use super::types_tools::{GitBlameParams, GitDiffParams, GitLogParams};
use crate::git::CommitInfo;
use crate::git_history::fts::{self, FtsScope};
use crate::path::RelPath;

/// The argument shapes `git_log` accepts, quoted in every shape error.
const LOG_SHAPES: &str = "accepted shapes: {} (recent commits), {path} (commits touching a file, or a directory when \
     it ends with `/`), {query} and/or {author} (search messages / authors), {path, symbol} (when a symbol's body \
     changed; add `line` when several there share the name), {rev} (one commit in full); `limit`, `cursor` and \
     `max_tokens` page every list";

/// `git_log`: recent commits, a path's history, a commit search, a symbol's history, or one commit.
pub(super) fn git_log(state: &ServerState, p: GitLogParams) -> Result<CallToolResult, McpError> {
    let shape_error = |problem: &str| McpError::invalid_params(format!("`git_log`: {problem}; {LOG_SHAPES}"), None);
    let max_tokens = budget(p.max_tokens);

    if let Some(rev) = p.rev {
        if p.path.is_some() || p.query.is_some() || p.author.is_some() || p.symbol.is_some() || p.line.is_some() {
            return Err(shape_error("`rev` shows one commit and combines with no other filter"));
        }
        if p.limit.is_some() || p.cursor.is_some() {
            return Err(shape_error("`rev` shows one commit and takes no `limit` or `cursor`"));
        }
        return run_show(state, &rev, max_tokens);
    }
    if let Some(symbol) = p.symbol {
        let Some(path) = p.path else {
            return Err(shape_error("`symbol` needs the `path` of the file defining it"));
        };
        if p.query.is_some() || p.author.is_some() {
            return Err(shape_error(
                "`symbol` history does not combine with `query` or `author`",
            ));
        }
        return super::helpers_git_file::run_symbol_history(
            state,
            SymbolHistoryParams {
                path,
                name: symbol,
                kind: None,
                line: p.line,
                limit: p.limit,
                hash_mode: None,
                cursor: p.cursor,
                max_tokens,
            },
        );
    }
    if p.line.is_some() {
        return Err(shape_error("`line` picks one definition of `symbol` and needs it"));
    }
    if p.query.is_some() || p.author.is_some() {
        if p.path.is_some() {
            return Err(shape_error(
                "`query` / `author` search the whole history and do not combine with `path`",
            ));
        }
        // `query` is matched against the message and `author` against the author, each on its own
        // field: joined into one all-field search, `author: "alice"` matched a commit by bob that
        // mentioned alice.
        let (pattern, field, author) = match (p.query, p.author) {
            (Some(query), author) => (query, "message", author),
            (None, Some(author)) => (author, "author", None),
            (None, None) => unreachable!("guarded by the enclosing `if`"),
        };
        return run_search(
            state,
            SearchGitHistoryParams {
                pattern,
                field: Some(field.to_string()),
                limit: p.limit,
                cursor: p.cursor,
                max_tokens,
                body_chars: None,
                author,
            },
        );
    }
    match p.path {
        Some(path) if path.as_bytes().ends_with(b"/") => {
            let mut page = by_path(
                state,
                "git_log",
                FindCommitsByPathParams {
                    pattern: format!("^{}", regex::escape(&path.to_str_lossy())),
                    window: None,
                    limit: Some(p.limit.unwrap_or(LOG_LIMIT_DEFAULT)),
                    cursor: p.cursor,
                    max_tokens,
                },
            )?;
            // A directory is held to what a file is: no commit in the whole history and nothing on
            // disk means the path is wrong. The last page says so as an error; a page before it
            // has not seen the whole history, and says why it is empty.
            if !page.matched {
                let repo = require_git_repo(state)?;
                if !repo.is_shallow() && !on_disk(repo, &path) {
                    if page.response.next_cursor.is_none() {
                        return Err(no_such_path(&path));
                    }
                    page.response.unmatched = Some(format!(
                        "no commit inspected so far touches `{path}` and no such directory exists; `next_cursor` \
                         looks further back. If the path is mistyped, locate it with `find_files`."
                    ));
                }
            }
            json_result(&page.response)
        }
        Some(path) => run_touching(
            state,
            CommitsTouchingParams {
                path,
                limit: p.limit,
                cursor: p.cursor,
                max_tokens,
            },
        ),
        None => run_recent(
            state,
            RecentChangesParams {
                limit: p.limit,
                include_files: true,
                cursor: p.cursor,
                max_tokens,
            },
        ),
    }
}

/// `git_blame`: who last changed each line of a file, a line span, or one symbol.
pub(super) fn git_blame(state: &ServerState, p: GitBlameParams) -> Result<CallToolResult, McpError> {
    let shape_error = |problem: &str| {
        McpError::invalid_params(
            format!(
                "`git_blame`: {problem}; accepted shapes: {{path}} (whole file), {{path, line, end_line}} (a 1-based \
                 inclusive span; `end_line` defaults to `line`), {{path, symbol}} (one symbol's lines; add `line` \
                 when several share the name); `rev` picks the revision"
            ),
            None,
        )
    };
    if p.line == Some(0) || p.end_line == Some(0) {
        return Err(shape_error("lines are 1-based"));
    }
    let max_tokens = budget(p.max_tokens);
    match p.symbol {
        Some(symbol) => {
            if p.end_line.is_some() {
                return Err(shape_error(
                    "`end_line` applies only to a line span; with `symbol`, `line` picks the definition",
                ));
            }
            super::helpers_git_file::run_blame_symbol(
                state,
                BlameSymbolParams {
                    path: p.path,
                    name: symbol,
                    kind: None,
                    line: p.line,
                    rev: p.rev,
                    limit: p.limit,
                    cursor: p.cursor,
                    max_tokens,
                },
            )
        }
        None => {
            let (line_start, line_end) = match (p.line, p.end_line) {
                (Some(line), Some(end)) if end < line => {
                    return Err(shape_error(&format!(
                        "`end_line` must be >= `line` (got {line}..{end})"
                    )));
                }
                (Some(line), end) => (Some(line), Some(end.unwrap_or(line))),
                (None, Some(_)) => return Err(shape_error("`end_line` needs `line`")),
                (None, None) => (None, None),
            };
            super::helpers_git_file::run_blame(
                state,
                BlameFileParams {
                    path: p.path,
                    line_start,
                    line_end,
                    rev: p.rev,
                    limit: p.limit,
                    cursor: p.cursor,
                    max_tokens,
                },
            )
        }
    }
}

/// The argument shapes `git_diff` accepts, quoted in every shape error.
const DIFF_SHAPES: &str = "accepted shapes: {} (working-tree status), {path} ending in `/` (the status under that \
     directory), {path} (that file's working-tree diff against HEAD), {path, from?, to?} (text diff; `from` \
     defaults to HEAD, `to` to the working tree), {path, structural:true} with optional `from` (symbols added / \
     removed since `from`, default HEAD)";

/// `git_diff`: the working-tree status, a file's text diff, or its symbol-level diff.
pub(super) async fn git_diff(state: &ServerState, p: GitDiffParams) -> Result<CallToolResult, McpError> {
    let shape_error = |problem: &str| McpError::invalid_params(format!("`git_diff`: {problem}; {DIFF_SHAPES}"), None);
    let structural = p.structural.unwrap_or(false);
    let Some(path) = p.path else {
        if p.from.is_some() || p.to.is_some() || structural {
            return Err(shape_error("a diff needs the `path` of one file"));
        }
        return blocking(|| run_status(state, None));
    };
    if path.as_bytes().ends_with(b"/") {
        if p.from.is_some() || p.to.is_some() || structural {
            return Err(shape_error(
                "a directory `path` narrows the working-tree status and takes no `from`, `to` or `structural`",
            ));
        }
        return blocking(|| run_status(state, Some(path)));
    }
    if let Some(refusal) = blocking(|| directory_without_slash(state, &path, p.from.as_deref(), p.to.as_deref())) {
        return Err(refusal);
    }
    if structural {
        if p.to.is_some() {
            return Err(shape_error(
                "a structural diff compares `from` against the indexed working tree and takes no `to`",
            ));
        }
        state.await_cache_ready().await;
        return blocking(|| super::helpers_git_file::run_diff_outline(state, DiffOutlineParams { path, rev: p.from }));
    }
    blocking(|| {
        super::helpers_git_file::run_diff(
            state,
            DiffFileParams {
                rev_old: p.from.unwrap_or_else(|| "HEAD".to_string()),
                rev_new: p.to,
                path,
            },
        )
    })
}

/// The refusal for a `git_diff` `path` that names a directory and does not end with `/`: on disk,
/// or in either revision compared. A diff is of one file, and a directory's status is its own
/// shape, so the message names it.
fn directory_without_slash(
    state: &ServerState,
    path: &RelPath,
    from: Option<&str>,
    to: Option<&str>,
) -> Option<McpError> {
    let repo = require_git_repo(state).ok()?;
    let on_disk = crate::path::contained_file(repo.workdir(), path).is_ok_and(|file| file.is_dir());
    let in_a_revision = [from.unwrap_or("HEAD")]
        .into_iter()
        .chain(to)
        .any(|rev| repo.is_directory_at(rev, path.as_ref()));
    (on_disk || in_a_revision).then(|| {
        McpError::invalid_params(
            format!(
                "`git_diff`: `{path}` is a directory and a diff is of one file. Pass `{path}/` (with the \
                 trailing slash, and no `from`, `to` or `structural`) for the working-tree status under it, \
                 or the path of a file in it."
            ),
            None,
        )
    })
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

/// Paths listed per status bucket. A checkout with an unignored build directory has tens of
/// thousands of untracked files, and the status is asked for to see what changed, not to list them.
const STATUS_PATHS_MAX: usize = 200;

/// The `status` operation: the working tree's staged / unstaged / untracked buckets, for the whole
/// tree or for the paths under the directory `within`.
fn run_status(state: &ServerState, within: Option<RelPath>) -> Result<CallToolResult, McpError> {
    let started = std::time::Instant::now();
    let repo = require_git_repo(state)?;
    // A narrowed status lists every untracked file: a wholly untracked directory collapsed into one
    // entry is an ancestor of the paths asked about, or hides them behind its own name.
    let s = match within {
        Some(_) => repo.status_per_file(),
        None => repo.status_porcelain(),
    }
    .map_err(|e| McpError::internal_error(format!("git status: {e}"), None))?;
    let mut omitted = 0;
    let mut any = false;
    let mut bucket = |mut paths: Vec<RelPath>| {
        if let Some(directory) = within.as_ref() {
            paths.retain(|path| path.as_bytes().starts_with(directory.as_bytes()));
        }
        any |= !paths.is_empty();
        // In path order, so the paths that survive the cap are the same on every call.
        paths.sort_unstable();
        omitted += paths.len().saturating_sub(STATUS_PATHS_MAX);
        paths.truncate(STATUS_PATHS_MAX);
        paths
    };
    let (staged_added, staged_modified, staged_deleted, modified, untracked) = (
        bucket(s.staged_added),
        bucket(s.staged_modified),
        bucket(s.staged_deleted),
        bucket(s.modified),
        bucket(s.untracked),
    );
    if let Some(directory) = within.as_ref()
        && !any
        && !crate::path::contained_file(repo.workdir(), directory).is_ok_and(|on_disk| on_disk.is_dir())
    {
        return Err(McpError::invalid_params(
            format!(
                "`git_diff`: no directory `{directory}` exists and no changed path is under it. `path` is \
                 repo-relative; a directory ends with `/`. Locate it with `find_files`."
            ),
            None,
        ));
    }
    json_result(&WorkingTreeStatusView {
        staged_added,
        staged_modified,
        staged_deleted,
        modified,
        untracked,
        path: within,
        truncated: omitted > 0,
        truncated_reason: (omitted > 0).then_some("path_cap"),
        omitted,
        is_clean: !any,
        elapsed_us: elapsed_us(started),
    })
}

/// The offset a paged git walk's cursor carries and the snapshot it was issued on (`None` without a
/// cursor), or `None` when HEAD moved since it was issued.
pub(super) fn walk_cursor(
    cursor: Option<&super::cursor::Cursor>,
    head: &str,
) -> Result<Option<(u64, Option<u32>)>, McpError> {
    let Some(cursor) = cursor else {
        return Ok(Some((0, None)));
    };
    let (packed, snapshot) = cursor.decode_in_memory()?;
    Ok((snapshot & !LIVE_WALK == head_snapshot_id(head) & !LIVE_WALK).then_some((packed, Some(snapshot))))
}

/// One page of a newest-first history, and the cursor snapshot that says which list it was cut from:
/// the git-history index's page when it returned one, the live walk's otherwise.
///
/// The index answers with an empty list both when no commit matches and when the lookup failed (a
/// forward to the daemon that did not come back), so an empty answer is checked against the
/// repository itself. A cursor `issued` on the other list is refused: the index appends the commits
/// a sync finds after the ones it holds, where the live walk interleaves them by commit time, so an
/// offset into one is not an offset into the other.
pub(super) fn history_page(
    tool: &str,
    head: &str,
    issued: Option<u32>,
    indexed: Option<Vec<CommitInfo>>,
    live: impl FnOnce() -> Result<Vec<CommitInfo>, McpError>,
) -> Result<(Vec<CommitInfo>, u32), McpError> {
    let indexed = indexed.filter(|commits| !commits.is_empty());
    let snapshot = (head_snapshot_id(head) & !LIVE_WALK) | u32::from(indexed.is_none());
    if issued.is_some_and(|issued| issued != snapshot) {
        return Err(super::cursor_bind::walk_switched(tool));
    }
    match indexed {
        Some(commits) => Ok((commits, snapshot)),
        None => Ok((live()?, snapshot)),
    }
}

/// The refusal for a `path` that no commit reachable from HEAD touches and that is not on disk
/// either. An empty history would not say the path is wrong.
fn no_such_path(path: &RelPath) -> McpError {
    let kind = if path.as_bytes().ends_with(b"/") {
        "directory"
    } else {
        "file"
    };
    McpError::invalid_params(
        format!(
            "`git_log`: no commit reachable from HEAD touches `{path}` and no such {kind} exists. `path` is a \
             repo-relative file path, or a directory when it ends with `/`; locate it with `find_files`."
        ),
        None,
    )
}

/// Whether `path` names something in the working tree.
fn on_disk(repo: &crate::git::Repo, path: &RelPath) -> bool {
    crate::path::contained_file(repo.workdir(), path).is_ok_and(|file| file.exists())
}

/// The `recent` operation: HEAD's ancestry, newest first, one page at a time.
fn run_recent(state: &ServerState, params: RecentChangesParams) -> Result<CallToolResult, McpError> {
    let started = std::time::Instant::now();
    let repo = require_git_repo(state)?;
    let limit = params.limit.unwrap_or(LOG_LIMIT_DEFAULT).min(LOG_LIMIT_MAX) as usize;
    let head = head_sha(repo)?;
    let Some((skip, issued)) = walk_cursor(params.cursor.as_ref(), &head)? else {
        return json_result(&RecentChangesResponse {
            commits: Vec::new(),
            truncated: false,
            truncated_reason: None,
            next_cursor: None,
            budgeted: false,
            cursor_invalidated: true,
            elapsed_us: elapsed_us(started),
        });
    };
    let skip = usize::try_from(skip).unwrap_or(usize::MAX);
    // One commit past the page, only to learn whether the history goes on. Only this page's commits
    // are listed, and nothing is kept per page: a log cached under its depth would be a new entry,
    // walked again from HEAD, for every page.
    let fetch = limit + 1;
    let (commits, snapshot) = history_page(
        "git_log",
        &head,
        issued,
        git_history_if_fresh(state, &head).map(|index| index.recent_commits(skip, fetch, params.include_files)),
        || {
            repo.log_window(&head, skip, fetch, params.include_files)
                .map_err(|e| McpError::internal_error(format!("log: {e}"), None))
        },
    )?;
    let more = commits.len() > limit;
    let page: Vec<CommitView> = commits
        .into_iter()
        .take(limit)
        .map(|c| commit_to_view(c, params.include_files))
        .collect();
    let (page, budgeted) = budget_commits(page, params.max_tokens);
    let next_cursor =
        (more || budgeted).then(|| super::cursor::Cursor::encode_in_memory((skip + page.len()) as u64, snapshot));
    let truncated = repo.is_shallow();
    json_result(&RecentChangesResponse {
        commits: page,
        budgeted,
        truncated,
        truncated_reason: truncated.then_some("shallow_clone"),
        next_cursor,
        cursor_invalidated: false,
        elapsed_us: elapsed_us(started),
    })
}

/// The `touching` operation: commits whose tree differs from the parent's at `path`, a page at a time.
fn run_touching(state: &ServerState, params: CommitsTouchingParams) -> Result<CallToolResult, McpError> {
    let started = std::time::Instant::now();
    let repo = require_git_repo(state)?;
    let limit = params.limit.unwrap_or(LOG_LIMIT_DEFAULT).min(LOG_LIMIT_MAX) as usize;
    let head = head_sha(repo)?;
    let Some((skip, issued)) = walk_cursor(params.cursor.as_ref(), &head)? else {
        return json_result(&CommitsTouchingResponse {
            path: params.path,
            commits: Vec::new(),
            truncated: false,
            truncated_reason: None,
            next_cursor: None,
            budgeted: false,
            cursor_invalidated: true,
            elapsed_us: elapsed_us(started),
        });
    };
    let skip = usize::try_from(skip).unwrap_or(usize::MAX);
    let fetch = limit + 1;
    let (commits, snapshot) = history_page(
        "git_log",
        &head,
        issued,
        git_history_if_fresh(state, &head).map(|index| index.commits_touching(&params.path, skip, fetch)),
        || {
            repo.log_window_touching(&head, &params.path, skip, fetch)
                .map_err(|e| McpError::internal_error(format!("log: {e}"), None))
        },
    )?;
    if commits.is_empty() && skip == 0 && !repo.is_shallow() && !on_disk(repo, &params.path) {
        return Err(no_such_path(&params.path));
    }
    let more = commits.len() > limit;
    let page: Vec<CommitView> = commits
        .into_iter()
        .take(limit)
        .map(|c| commit_to_view(c, false))
        .collect();
    let (page, budgeted) = budget_commits(page, params.max_tokens);
    let next_cursor =
        (more || budgeted).then(|| super::cursor::Cursor::encode_in_memory((skip + page.len()) as u64, snapshot));
    let truncated = repo.is_shallow();
    json_result(&CommitsTouchingResponse {
        path: params.path,
        commits: page,
        budgeted,
        truncated,
        truncated_reason: truncated.then_some("shallow_clone"),
        next_cursor,
        cursor_invalidated: false,
        elapsed_us: elapsed_us(started),
    })
}

/// The `churn` operation: the top-K most-frequently-modified files in a recent window.
pub(super) fn run_churn(state: &ServerState, params: HotFilesParams) -> Result<CallToolResult, McpError> {
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

/// Commits one indexed search round fetches while filtering by author.
const AUTHOR_FILTER_CHUNK: usize = 500;
/// Commits an author-filtered search inspects before it reports itself partial.
const AUTHOR_FILTER_SCAN_MAX: usize = 20_000;

/// Page `[skip, skip + take)` of the commits `fetch(offset, count)` yields whose AUTHOR also holds
/// every one of `author_terms`. `fetch` is the message search, newest first; filtering its pages
/// here keeps the two fields apart without a second index query shape. Returns the page and
/// whether the scan stopped at [`AUTHOR_FILTER_SCAN_MAX`] before the search was exhausted.
fn by_author(
    mut fetch: impl FnMut(usize, usize) -> Vec<CommitInfo>,
    author_terms: &ahash::AHashSet<String>,
    skip: usize,
    take: usize,
) -> (Vec<CommitInfo>, bool) {
    let mut page = Vec::new();
    let mut matched = 0usize;
    let mut offset = 0usize;
    while offset < AUTHOR_FILTER_SCAN_MAX {
        let chunk = fetch(offset, AUTHOR_FILTER_CHUNK);
        let fetched = chunk.len();
        for commit in chunk {
            if !fts::commit_matches_terms(&commit, author_terms, FtsScope::Author) {
                continue;
            }
            matched += 1;
            if matched > skip {
                page.push(commit);
                if page.len() == take {
                    return (page, false);
                }
            }
        }
        if fetched < AUTHOR_FILTER_CHUNK {
            return (page, false);
        }
        offset += fetched;
    }
    (page, true)
}

/// The `search` operation: full-text search over git history. Uses the git-history inverted
/// index when it is fresh (`last_indexed_head == HEAD`), searching author name + email + summary +
/// full body; otherwise degrades to a bounded live walk over the recent window, flagged
/// `truncated` with reason `live_walk` (author + summary only, no body). Pagination and the HEAD-scoped cursor mirror `recent`.
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
                    truncated: false,
                    truncated_reason: None,
                    next_cursor: None,
                    budgeted: false,
                    cursor_invalidated: true,
                    elapsed_us: elapsed_us(started),
                });
            }
            offset as usize
        }
        None => 0,
    };

    let want = limit.saturating_add(1);
    let author_terms = params.author.as_deref().map(|author| {
        let mut terms = ahash::AHashSet::new();
        fts::tokenize(author, &mut terms);
        terms
    });
    let (mut hits, truncated_reason) = match git_history_if_fresh(state, &head) {
        Some(index) => match author_terms.as_ref() {
            None => (index.search_commits(&params.pattern, scope, skip, want), None),
            Some(author_terms) => {
                let (hits, capped) = by_author(
                    |offset, chunk| index.search_commits(&params.pattern, scope, offset, chunk),
                    author_terms,
                    skip,
                    want,
                );
                (hits, capped.then_some(super::types::SCAN_CAP))
            }
        },
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
                .filter(|c| {
                    author_terms
                        .as_ref()
                        .is_none_or(|terms| fts::commit_matches_terms(c, terms, FtsScope::Author))
                })
                .skip(skip)
                .take(want)
                .cloned()
                .collect();
            (matched, Some("live_walk"))
        }
    };

    let has_more = hits.len() > limit;
    hits.truncate(limit);

    let body_chars = resolve_body_chars(params.body_chars);
    let commits = hits.into_iter().map(|c| commit_to_hit(c, body_chars)).collect();
    let (commits, budgeted) = budget_commits(commits, params.max_tokens);
    let has_more = has_more || budgeted;
    let kept = commits.len();
    let next_cursor = has_more.then(|| super::cursor::Cursor::encode_in_memory((skip + kept) as u64, snapshot));
    json_result(&SearchGitHistoryResponse {
        commits,
        budgeted,
        truncated: truncated_reason.is_some(),
        truncated_reason,
        next_cursor,
        cursor_invalidated: false,
        elapsed_us: elapsed_us(started),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(ordinal: usize, author: &str) -> CommitInfo {
        CommitInfo {
            sha: format!("{ordinal:040x}"),
            short_sha: format!("{ordinal:07x}"),
            summary: format!("commit {ordinal} mentions alice"),
            author: author.to_string(),
            author_email: format!("{author}@e.x"),
            author_time_unix: 0,
            body: String::new(),
            files: Vec::new(),
        }
    }

    fn terms(text: &str) -> ahash::AHashSet<String> {
        let mut terms = ahash::AHashSet::new();
        fts::tokenize(text, &mut terms);
        terms
    }

    /// Every third commit is by alice; all of them mention her in the message.
    fn history(len: usize) -> Vec<CommitInfo> {
        (0..len)
            .map(|ordinal| commit(ordinal, if ordinal % 3 == 0 { "alice" } else { "bob" }))
            .collect()
    }

    fn fetch_from(history: &[CommitInfo]) -> impl FnMut(usize, usize) -> Vec<CommitInfo> + '_ {
        |offset, count| history.iter().skip(offset).take(count).cloned().collect()
    }

    #[test]
    fn an_empty_indexed_answer_is_checked_against_the_live_walk() {
        let head = "0123456789abcdef0123456789abcdef01234567";
        let walked = || Ok(vec![commit(1, "alice")]);
        let page = |indexed| history_page("git_log", head, None, indexed, walked).expect("page");
        let (indexed, snapshot) = page(Some(vec![commit(7, "bob")]));
        assert_eq!(indexed[0].author, "bob", "a non-empty indexed answer is used as is");
        assert_eq!(snapshot & LIVE_WALK, 0, "and its cursor counts in the index's list");
        let (failed_forward, snapshot) = page(Some(Vec::new()));
        assert_eq!(
            failed_forward[0].author, "alice",
            "an empty one may be a lookup that failed"
        );
        assert_eq!(snapshot & LIVE_WALK, LIVE_WALK, "the live walk's list");
        let (no_index, _) = page(None);
        assert_eq!(no_index[0].author, "alice");
        let (truly_empty, _) = history_page("git_log", head, None, Some(Vec::new()), || Ok(Vec::new())).expect("live");
        assert!(truly_empty.is_empty());
    }

    #[test]
    fn author_terms_are_matched_against_the_author_not_the_message() {
        let history = history(30);
        let (page, capped) = by_author(fetch_from(&history), &terms("alice"), 0, 100);
        assert!(!capped);
        assert_eq!(page.len(), 10, "ten of thirty commits are by alice");
        assert!(page.iter().all(|commit| commit.author == "alice"));
        let (none, _) = by_author(fetch_from(&history), &terms("carol"), 0, 100);
        assert!(none.is_empty());
    }

    #[test]
    fn an_author_filtered_page_skips_and_takes_matches_across_fetch_chunks() {
        let history = history(AUTHOR_FILTER_CHUNK * 3);
        let all = by_author(fetch_from(&history), &terms("alice"), 0, usize::MAX).0;
        let (page, capped) = by_author(fetch_from(&history), &terms("alice"), 200, 5);
        assert!(!capped);
        let shas: Vec<&str> = page.iter().map(|commit| commit.sha.as_str()).collect();
        let expected: Vec<&str> = all[200..205].iter().map(|commit| commit.sha.as_str()).collect();
        assert_eq!(shas, expected, "the page spans the first and second fetch");
    }

    #[test]
    fn a_search_that_outruns_the_scan_cap_says_so() {
        let history = history(AUTHOR_FILTER_SCAN_MAX + AUTHOR_FILTER_CHUNK);
        let (page, capped) = by_author(fetch_from(&history), &terms("carol"), 0, 1);
        assert!(page.is_empty());
        assert!(
            capped,
            "the walk stopped at the cap, so an empty page is not a complete answer"
        );
    }
}
