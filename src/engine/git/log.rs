//! Bodies of the git verb tools (`git_log`, `git_blame`, `git_diff`) and of the commit-log
//! operations behind them.
//!
//! This file owns the commit-log family (`status`, `recent`, `touching`, `churn`, `search`);
//! `by_path` lives in `by_path.rs`, and the operations scoped to a single path (`diff`,
//! `diff_outline`, `blame`, `blame_symbol`, `symbol_history`) in `file.rs`. The split
//! is only about the 1000-line per-file cap.

use super::by_path::{LIVE_WALK, MATCHED_EARLIER, by_path};
use super::common::{
    LOG_LIMIT_DEFAULT, LOG_LIMIT_MAX, commit_to_view, git_history_if_fresh, head_sha, head_snapshot_id, live_walk_span,
    require_git_repo,
};
use super::search::{no_such_path, run_search};
use super::show::{budget_commits, budget_commits_with_files, run_show};
use crate::engine::budget::budget;
use crate::engine::error::EngineError;
use crate::engine::timing::elapsed_us;
use crate::engine::types::SPAN_LIMIT;
use crate::engine::types::git::{
    BlameFileParams, BlameSymbolParams, CommitView, CommitsTouchingParams, CommitsTouchingResponse, DiffFileParams,
    DiffOutlineParams, FindCommitsByPathParams, GitBlameParams, GitBlameResult, GitDiffParams, GitDiffResult,
    GitLogParams, GitLogResult, HotFileEntry, HotFilesParams, HotFilesResponse, RecentChangesParams,
    RecentChangesResponse, SearchGitHistoryParams, SymbolHistoryParams, WorkingTreeStatusView,
};
use crate::engine::workspace::Workspace;
use crate::git::{CommitInfo, TouchingSpan};
use crate::path::RelPath;

/// The argument shapes `git_log` accepts, quoted in every shape error.
const LOG_SHAPES: &str = "accepted shapes: {} (recent commits), {path} (commits touching a file, or a directory when \
     it ends with `/`), {query} and/or {author} (search messages / authors), {path, symbol} (when a symbol's body \
     changed; add `line` when several there share the name), {rev} (one commit in full); `limit`, `cursor` and \
     `max_tokens` page every list";

impl Workspace {
    /// `git_log`: recent commits, a path's history, a commit search, a symbol's history, or one
    /// commit.
    pub(crate) fn git_log(&self, p: GitLogParams) -> Result<GitLogResult, EngineError> {
        git_log(self, p)
    }

    /// `git_blame`: who last changed each line of a file, a line span, or one symbol.
    pub(crate) fn git_blame(&self, p: GitBlameParams) -> Result<GitBlameResult, EngineError> {
        git_blame(self, p)
    }

    /// `git_diff`: the working-tree status, a file's text diff, or its symbol-level diff.
    pub(crate) fn git_diff(&self, p: GitDiffParams) -> Result<GitDiffResult, EngineError> {
        git_diff(self, p)
    }

    /// `basemind git churn`: the top-K most-frequently-modified files in a recent window.
    pub(crate) fn git_churn(&self, p: HotFilesParams) -> Result<HotFilesResponse, EngineError> {
        run_churn(self, p)
    }
}

/// `git_log`: recent commits, a path's history, a commit search, a symbol's history, or one commit.
fn git_log(ws: &Workspace, p: GitLogParams) -> Result<GitLogResult, EngineError> {
    let shape_error = |problem: &str| EngineError::invalid_argument(format!("`git_log`: {problem}; {LOG_SHAPES}"));
    let max_tokens = budget(p.max_tokens);

    if let Some(rev) = p.rev {
        if p.path.is_some()
            || p.query.is_some()
            || p.author.is_some()
            || p.symbol.is_some()
            || p.line.is_some()
            || p.since.is_some()
            || p.until.is_some()
        {
            return Err(shape_error("`rev` shows one commit and combines with no other filter"));
        }
        if p.limit.is_some() || p.cursor.is_some() {
            return Err(shape_error("`rev` shows one commit and takes no `limit` or `cursor`"));
        }
        return run_show(ws, &rev, max_tokens).map(GitLogResult::Show);
    }
    if let Some(symbol) = p.symbol {
        let Some(path) = p.path else {
            return Err(shape_error("`symbol` needs the `path` of the file defining it"));
        };
        if p.query.is_some() || p.author.is_some() || p.since.is_some() || p.until.is_some() {
            return Err(shape_error(
                "`symbol` history does not combine with `query` or `author`",
            ));
        }
        return super::file::run_symbol_history(
            ws,
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
        )
        .map(GitLogResult::Symbol);
    }
    if p.line.is_some() {
        return Err(shape_error("`line` picks one definition of `symbol` and needs it"));
    }
    if p.query.is_some() || p.author.is_some() {
        // `query` is matched against the message and `author` against the author, each on its own
        // field: joined into one all-field search, `author: "alice"` matched a commit by bob that
        // mentioned alice.
        let (pattern, field, author) = match (p.query, p.author) {
            (Some(query), author) => (query, "message", author),
            (None, Some(author)) => (author, "author", None),
            (None, None) => unreachable!("guarded by the enclosing `if`"),
        };
        return run_search(
            ws,
            SearchGitHistoryParams {
                pattern,
                field: Some(field.to_string()),
                limit: p.limit,
                cursor: p.cursor,
                max_tokens,
                author,
                path: p.path.map(|path| as_directory(ws, path)),
                since: p.since,
                until: p.until,
            },
        )
        .map(GitLogResult::Search);
    }
    if p.since.is_some() || p.until.is_some() {
        return Err(shape_error("`since` and `until` bound a `query` / `author` search"));
    }
    match p.path.map(|path| as_directory(ws, path)) {
        Some(path) if path.as_bytes().ends_with(b"/") => {
            let mut page = by_path(
                ws,
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
                let repo = require_git_repo(ws)?;
                if !repo.is_shallow() && !on_disk(repo, &path) {
                    if page.response.next_cursor.is_none() {
                        return Err(no_such_path(ws, &path));
                    }
                    page.response.unmatched = Some(format!(
                        "no commit inspected so far touches `{path}` and no such directory exists; `next_cursor` \
                         looks further back. If the path is mistyped, locate it with `find_files`."
                    ));
                }
            }
            Ok(GitLogResult::Directory(page.response))
        }
        Some(path) => run_touching(
            ws,
            CommitsTouchingParams {
                path,
                limit: p.limit,
                cursor: p.cursor,
                max_tokens,
            },
        )
        .map(GitLogResult::Touching),
        None => run_recent(
            ws,
            RecentChangesParams {
                limit: p.limit,
                include_files: true,
                cursor: p.cursor,
                max_tokens,
            },
        )
        .map(GitLogResult::Recent),
    }
}

/// `path` with a trailing slash when it names a directory on disk or at HEAD. A commit's file list
/// holds files, never directories, so a directory's history is the commits that change a file under
/// it whether or not the caller wrote the slash.
fn as_directory(ws: &Workspace, path: RelPath) -> RelPath {
    if path.as_bytes().ends_with(b"/") || path.as_bytes().is_empty() {
        return path;
    }
    let Ok(repo) = require_git_repo(ws) else {
        return path;
    };
    let on_disk = crate::path::contained_file(repo.workdir(), &path).is_ok_and(|file| file.is_dir());
    if !on_disk && !repo.is_directory_at("HEAD", path.as_ref()) {
        return path;
    }
    let mut slashed = path.as_bytes().to_vec();
    slashed.push(b'/');
    RelPath::from(slashed.as_slice())
}

/// `git_blame`: who last changed each line of a file, a line span, or one symbol.
fn git_blame(ws: &Workspace, p: GitBlameParams) -> Result<GitBlameResult, EngineError> {
    let shape_error = |problem: &str| {
        EngineError::invalid_argument(format!(
            "`git_blame`: {problem}; accepted shapes: {{path}} (whole file), {{path, line, end_line}} (a 1-based \
                 inclusive span; `end_line` defaults to `line`), {{path, symbol}} (one symbol's lines; add `line` \
                 when several share the name); `rev` picks the revision"
        ))
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
            super::file::run_blame_symbol(
                ws,
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
            .map(GitBlameResult::Symbol)
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
            super::file::run_blame(
                ws,
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
            .map(GitBlameResult::File)
        }
    }
}

/// The argument shapes `git_diff` accepts, quoted in every shape error.
const DIFF_SHAPES: &str = "accepted shapes: {} (working-tree status), {path} ending in `/` (the status under that \
     directory), {path} (that file's working-tree diff against HEAD), {path, from?, to?} (text diff; `from` \
     defaults to HEAD, `to` to the working tree), {path, structural:true} with optional `from` (symbols added / \
     removed since `from`, default HEAD)";

/// `git_diff`: the working-tree status, a file's text diff, or its symbol-level diff.
fn git_diff(ws: &Workspace, p: GitDiffParams) -> Result<GitDiffResult, EngineError> {
    let shape_error = |problem: &str| EngineError::invalid_argument(format!("`git_diff`: {problem}; {DIFF_SHAPES}"));
    let structural = p.structural.unwrap_or(false);
    let Some(path) = p.path else {
        if p.from.is_some() || p.to.is_some() || structural {
            return Err(shape_error("a diff needs the `path` of one file"));
        }
        return run_status(ws, None).map(GitDiffResult::Status);
    };
    if path.as_bytes().ends_with(b"/") {
        if p.from.is_some() || p.to.is_some() || structural {
            return Err(shape_error(
                "a directory `path` narrows the working-tree status and takes no `from`, `to` or `structural`",
            ));
        }
        return run_status(ws, Some(path)).map(GitDiffResult::Status);
    }
    if let Some(refusal) = directory_without_slash(ws, &path, p.from.as_deref(), p.to.as_deref()) {
        return Err(refusal);
    }
    if structural {
        if p.to.is_some() {
            return Err(shape_error(
                "a structural diff compares `from` against the indexed working tree and takes no `to`",
            ));
        }
        ws.wait_map_ready();
        return super::file::run_diff_outline(ws, DiffOutlineParams { path, rev: p.from })
            .map(GitDiffResult::Structural);
    }
    super::file::run_diff(
        ws,
        DiffFileParams {
            rev_old: p.from.unwrap_or_else(|| "HEAD".to_string()),
            rev_new: p.to,
            path,
        },
    )
    .map(GitDiffResult::Text)
}

/// The refusal for a `git_diff` `path` that names a directory and does not end with `/`: on disk,
/// or in either revision compared. A diff is of one file, and a directory's status is its own
/// shape, so the message names it.
fn directory_without_slash(
    ws: &Workspace,
    path: &RelPath,
    from: Option<&str>,
    to: Option<&str>,
) -> Option<EngineError> {
    let repo = require_git_repo(ws).ok()?;
    let revisions: Vec<&str> = [from.unwrap_or("HEAD")].into_iter().chain(to).collect();
    // A submodule's checkout is a directory on disk and a gitlink in the tree: its diff is its commit ids.
    let submodule = revisions.iter().any(|rev| repo.is_gitlink_at(rev, path.as_ref()));
    let on_disk = !submodule && crate::path::contained_file(repo.workdir(), path).is_ok_and(|file| file.is_dir());
    let in_a_revision = revisions.iter().any(|rev| repo.is_directory_at(rev, path.as_ref()));
    (on_disk || in_a_revision).then(|| {
        EngineError::invalid_argument(format!(
            "`git_diff`: `{path}` is a directory and a diff is of one file. Pass `{path}/` (with the \
                 trailing slash, and no `from`, `to` or `structural`) for the working-tree status under it, \
                 the path of a file in it, or use `git_log` with `path: \"{path}/\"` for its history."
        ))
    })
}

/// Reject a path that lives outside the repository (a `scan.extra_roots` file, keyed by its
/// absolute path). Such files are indexed for the code map but are not tracked by git, so blame
/// has nothing to resolve against — return a clear error instead of an opaque gix failure.
pub(super) fn reject_external_path(path: &RelPath) -> Result<(), EngineError> {
    if path.is_external() {
        return Err(EngineError::invalid_argument(format!(
            "path is outside the git repository (indexed via scan.extra_roots); \
                 blame is unavailable for external files: {path}"
        )));
    }
    Ok(())
}

/// Paths listed per status bucket. A checkout with an unignored build directory has tens of
/// thousands of untracked files, and the status is asked for to see what changed, not to list them.
const STATUS_PATHS_MAX: usize = 200;

/// The `status` operation: the working tree's staged / unstaged / untracked buckets, for the whole
/// tree or for the paths under the directory `within`.
fn run_status(ws: &Workspace, within: Option<RelPath>) -> Result<WorkingTreeStatusView, EngineError> {
    let repo = require_git_repo(ws)?;
    // A narrowed status lists every untracked file: a wholly untracked directory collapsed into one
    // entry is an ancestor of the paths asked about, or hides them behind its own name.
    let s = match within {
        Some(_) => repo.status_per_file(),
        None => repo.status_porcelain(),
    }
    .map_err(|e| EngineError::internal(format!("git status: {e}")))?;
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
        return Err(EngineError::invalid_argument(format!(
            "`git_diff`: no directory `{directory}` exists and no changed path is under it. `path` is \
                 repo-relative; a directory ends with `/`. Locate it with `find_files`."
        )));
    }
    Ok(WorkingTreeStatusView {
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
    })
}

/// The offset a paged git walk's cursor carries and the snapshot it was issued on (`None` without a
/// cursor), or `None` when HEAD moved since it was issued.
pub(super) fn walk_cursor(
    cursor: Option<&crate::engine::cursor::Cursor>,
    head: &str,
) -> Result<Option<(u64, Option<u32>)>, EngineError> {
    let Some(cursor) = cursor else {
        return Ok(Some((0, None)));
    };
    let (packed, snapshot) = cursor.decode_in_memory()?;
    Ok((snapshot & !LIVE_WALK == head_snapshot_id(head) & !LIVE_WALK).then_some((packed, Some(snapshot))))
}

/// One page of a newest-first history, and the cursor snapshot that says which list it was cut from:
/// the git-history index's page when it returned one, the live walk's otherwise.
///
/// `indexed` is the index's answer, `None` when there is no fresh index or its lookup failed. An
/// answer is trusted as given, an empty one too: it is the answer for a path no commit touches. A
/// cursor `issued` on the index is refused once the index stops answering: the index appends the
/// commits a sync finds after the ones it holds, where the live walk interleaves them by commit
/// time, so an offset into one is not an offset into the other. A cursor issued on the live walk
/// keeps paging the live walk, which still lists the same HEAD: an index that catches up between
/// two pages does not cut the walk off.
pub(super) fn history_page<T>(
    tool: &str,
    head: &str,
    issued: Option<u32>,
    indexed: Option<T>,
    live: impl FnOnce() -> Result<T, EngineError>,
) -> Result<(T, u32), EngineError> {
    let live_cursor = issued.is_some_and(|issued| issued & LIVE_WALK != 0);
    let indexed = indexed.filter(|_| !live_cursor);
    let snapshot = (head_snapshot_id(head) & !LIVE_WALK) | u32::from(indexed.is_none());
    if issued.is_some_and(|issued| issued != snapshot) {
        return Err(crate::engine::cursor::walk_switched(tool));
    }
    match indexed {
        Some(page) => Ok((page, snapshot)),
        None => Ok((live()?, snapshot)),
    }
}

/// The commits touching `path` from offset `from` on, at most `take` of them, and the cursor
/// snapshot of the list they come from. The index answers with offsets into its list of the
/// commits touching the path. Without it the live walk tests at most [`super::common::LOG_WALK_MAX`] commits from
/// position `from` of the whole history, so a path that rarely or never changed costs a bounded
/// walk, and a span that runs out says where the next page resumes.
pub(super) fn touching_page(
    ws: &Workspace,
    head: &str,
    issued: Option<u32>,
    path: &RelPath,
    from: usize,
    take: usize,
) -> Result<(TouchingSpan, u32), EngineError> {
    let repo = require_git_repo(ws)?;
    // The index keys a path by its exact bytes and the live walk ignores empty components, so both
    // read the path through its components, or the index would answer `src//lib.rs` with nothing.
    let path = &without_empty_components(path);
    // The lookup's reads take no lock, so a sync that clears or rebuilds the index between the
    // freshness check and them can make a path with history read as untouched: the answer counts
    // only when the index still stamps `head` after it.
    let indexed = git_history_if_fresh(ws, head).and_then(|index| {
        index
            .commits_touching(path, from, take)
            .inspect_err(|error| tracing::warn!(%error, "git-history lookup failed; walking the history live"))
            .ok()
            .filter(|_| index.last_indexed_head_hex().as_deref() == Some(head))
    });
    history_page(
        "git_log",
        head,
        issued,
        indexed.map(|commits| TouchingSpan {
            commits: commits
                .into_iter()
                .enumerate()
                .map(|(index, commit)| (from + index, commit))
                .collect(),
            resume: None,
        }),
        || {
            repo.touching_span(head, path, from, take, live_walk_span())
                .map_err(|e| EngineError::internal(format!("log: {e}")))
        },
    )
}

/// `path` with its empty components (a doubled, leading or trailing `/`) dropped, bytes untouched.
fn without_empty_components(path: &RelPath) -> RelPath {
    let kept: Vec<&[u8]> = path
        .as_bytes()
        .split(|&b| b == b'/')
        .filter(|c| !c.is_empty())
        .collect();
    RelPath::from(kept.join(&b'/').as_slice())
}

/// Whether `path` names something in the working tree.
fn on_disk(repo: &crate::git::Repo, path: &RelPath) -> bool {
    crate::path::contained_file(repo.workdir(), path).is_ok_and(|file| file.exists())
}

/// The `recent` operation: HEAD's ancestry, newest first, one page at a time.
fn run_recent(ws: &Workspace, params: RecentChangesParams) -> Result<RecentChangesResponse, EngineError> {
    let repo = require_git_repo(ws)?;
    let limit = params.limit.unwrap_or(LOG_LIMIT_DEFAULT).min(LOG_LIMIT_MAX) as usize;
    let head = head_sha(repo)?;
    let Some((skip, issued)) = walk_cursor(params.cursor.as_ref(), &head)? else {
        return Ok(RecentChangesResponse {
            commits: Vec::new(),
            files: Vec::new(),
            truncated: false,
            truncated_reason: None,
            next_cursor: None,
            budgeted: false,
            cursor_invalidated: true,
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
        // The global log reads a row that fails as absent, so an empty page is checked against
        // the live walk, which costs this page's commits only.
        git_history_if_fresh(ws, &head)
            .map(|index| index.recent_commits(skip, fetch, params.include_files))
            .filter(|commits| !commits.is_empty()),
        || {
            repo.log_window(&head, skip, fetch, params.include_files)
                .map_err(|e| EngineError::internal(format!("log: {e}")))
        },
    )?;
    let more = commits.len() > limit;
    let page: Vec<CommitView> = commits
        .into_iter()
        .take(limit)
        .map(|c| commit_to_view(c, params.include_files))
        .collect();
    let (page, files, budgeted) = budget_commits_with_files(page, params.max_tokens);
    let next_cursor = (more || budgeted)
        .then(|| crate::engine::cursor::Cursor::encode_in_memory((skip + page.len()) as u64, snapshot));
    let truncated = repo.is_shallow();
    Ok(RecentChangesResponse {
        commits: page,
        files,
        budgeted,
        truncated,
        truncated_reason: truncated.then_some("shallow_clone"),
        next_cursor,
        cursor_invalidated: false,
    })
}

/// The `touching` operation: commits whose tree differs from the parent's at `path`, a page at a time.
fn run_touching(ws: &Workspace, params: CommitsTouchingParams) -> Result<CommitsTouchingResponse, EngineError> {
    let repo = require_git_repo(ws)?;
    let limit = params.limit.unwrap_or(LOG_LIMIT_DEFAULT).min(LOG_LIMIT_MAX) as usize;
    let head = head_sha(repo)?;
    let Some((packed, issued)) = walk_cursor(params.cursor.as_ref(), &head)? else {
        return Ok(CommitsTouchingResponse {
            commits: Vec::new(),
            truncated: false,
            truncated_reason: None,
            next_cursor: None,
            budgeted: false,
            cursor_invalidated: true,
        });
    };
    let skip = usize::try_from(packed & !MATCHED_EARLIER).unwrap_or(usize::MAX);
    let matched_earlier = packed & MATCHED_EARLIER != 0;
    let fetch = limit + 1;
    let (found, snapshot) = touching_page(ws, &head, issued, &params.path, skip, fetch)?;
    // The history ran out with no page matching: the path is a mistake in the call unless it is on
    // disk. One an earlier page matched has a history, which may end in its deletion.
    if found.commits.is_empty()
        && found.resume.is_none()
        && !matched_earlier
        && !repo.is_shallow()
        && !on_disk(repo, &params.path)
    {
        return Err(no_such_path(ws, &params.path));
    }
    Ok(touching_response(
        found,
        limit,
        params.max_tokens,
        snapshot,
        repo.is_shallow(),
        matched_earlier,
    ))
}

/// The `touching` page for the commits `found`, `limit` of them at most, its cursor cut from the
/// list `snapshot` names and carrying whether this page or one before it (`matched_earlier`) listed
/// a commit.
fn touching_response(
    found: TouchingSpan,
    limit: usize,
    max_tokens: Option<u32>,
    snapshot: u32,
    shallow: bool,
    matched_earlier: bool,
) -> CommitsTouchingResponse {
    let offsets: Vec<usize> = found.commits.iter().map(|(offset, _)| *offset).collect();
    let page: Vec<CommitView> = found
        .commits
        .into_iter()
        .take(limit)
        .map(|(_, c)| commit_to_view(c, false))
        .collect();
    let (page, budgeted) = budget_commits(page, max_tokens);
    let matched = matched_earlier || !page.is_empty();
    // The next page starts at the first commit this one left out, or where the live walk's span ran
    // out when it held no more.
    let resume = found.resume.map(|(position, _)| position);
    let next_offset = offsets.get(page.len()).copied().or(resume);
    let span_edge = resume.is_some() && offsets.len() <= page.len();
    let truncated_reason = if span_edge {
        Some(SPAN_LIMIT)
    } else {
        shallow.then_some("shallow_clone")
    };
    CommitsTouchingResponse {
        commits: page,
        budgeted,
        truncated: truncated_reason.is_some(),
        truncated_reason,
        next_cursor: next_offset.map(|offset| {
            let carried = if matched { MATCHED_EARLIER } else { 0 };
            crate::engine::cursor::Cursor::encode_in_memory(offset as u64 | carried, snapshot)
        }),
        cursor_invalidated: false,
    }
}

/// The `churn` operation: the top-K most-frequently-modified files in a recent window.
fn run_churn(ws: &Workspace, params: HotFilesParams) -> Result<HotFilesResponse, EngineError> {
    let started = std::time::Instant::now();
    let repo = require_git_repo(ws)?;
    let window = params.window.unwrap_or(200).min(2000);
    let top_k = params.top_k.unwrap_or(20).min(200) as usize;
    let head = head_sha(repo)?;
    let commits: Vec<CommitInfo> = match git_history_if_fresh(ws, &head) {
        Some(index) => index.window_commits(window as usize),
        None => ws
            .git_cache
            .log(repo, &head, None, window, true)
            .map_err(|e| EngineError::internal(format!("log: {e}")))?
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

    Ok(HotFilesResponse {
        window_inspected: window,
        total_files_changed,
        files: ranked,
        elapsed_us: elapsed_us(started),
    })
}

#[cfg(test)]
#[path = "touching_tests.rs"]
mod touching_tests;

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

    #[test]
    fn an_indexed_answer_is_used_as_is_even_when_empty() {
        let head = "0123456789abcdef0123456789abcdef01234567";
        let walked = || Ok(vec![commit(1, "alice")]);
        let page = |indexed| history_page("git_log", head, None, indexed, walked).expect("page");
        let (indexed, snapshot) = page(Some(vec![commit(7, "bob")]));
        assert_eq!(indexed[0].author, "bob", "a non-empty indexed answer is used as is");
        assert_eq!(snapshot & LIVE_WALK, 0, "and its cursor counts in the index's list");
        let (untouched, snapshot) = page(Some(Vec::new()));
        assert_eq!(
            untouched,
            Vec::<CommitInfo>::new(),
            "an empty one is the answer for a path no commit touches"
        );
        assert_eq!(snapshot & LIVE_WALK, 0, "the index's list");
        let (no_index, snapshot) = page(None);
        assert_eq!(
            no_index[0].author, "alice",
            "no answer from the index is the live walk's"
        );
        assert_eq!(snapshot & LIVE_WALK, LIVE_WALK, "the live walk's list");
    }

    #[test]
    fn a_live_walk_cursor_keeps_the_live_walk_once_the_index_answers() {
        let head = "0123456789abcdef0123456789abcdef01234567";
        let live = (head_snapshot_id(head) & !LIVE_WALK) | LIVE_WALK;
        let walked = || Ok(vec![commit(1, "alice")]);
        let (page, snapshot) =
            history_page("git_log", head, Some(live), Some(vec![commit(7, "bob")]), walked).expect("page");
        assert_eq!(page[0].author, "alice", "the walk the cursor was cut from");
        assert_eq!(snapshot, live);
        let indexed = live & !LIVE_WALK;
        let refused = history_page("git_log", head, Some(indexed), None, walked).expect_err("index gone");
        assert!(refused.to_string().contains("git-history index"), "{refused}");
    }
}
