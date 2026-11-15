//! The five git operations scoped to a single path — `diff`, `diff_outline`, `blame`, `blame_symbol`,
//! `symbol_history`.
//!
//! Split out of `helpers_git.rs`, which owns the `git_log` / `git_blame` / `git_diff` bodies and the
//! commit-log family, purely to keep both files under the 1000-line cap. Nothing here is reachable
//! except through those three bodies.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::ServerState;
use super::helpers::{
    HashMode, LOG_WALK_MAX, blame_resume_after, blame_symbol_too_large_response, blame_too_large_response, elapsed_us,
    git_history_if_fresh, head_sha, head_snapshot_id, json_result, kind_to_str, outline_entry_for_blob,
    paginate_blame_hunks, parse_hash_mode, parse_kind, require_git_repo, symbol_fingerprint, symbol_line_range,
};
use super::helpers_git::reject_external_path;
use super::helpers_pick::pick_symbol;
use super::types::{
    BlameFileParams, BlameResponse, BlameSymbolParams, BlameSymbolResponse, DiffFileParams, DiffFileResponse,
    DiffOutlineParams, DiffOutlineResponse, DiffSymbolView, HunkView, SymbolHistoryEntry, SymbolHistoryParams,
    SymbolHistoryResponse,
};
use crate::git::CommitInfo;

/// What the old side of a text diff is compared against.
const WORKING_TREE: &str = "working tree";

/// Hunks one `git_diff` returns. A rewrite of a generated file is thousands of them.
const DIFF_HUNKS_MAX: usize = 200;
/// Bytes of hunk text one `git_diff` returns.
const DIFF_TEXT_MAX: usize = 128 * 1024;

/// The `diff` operation: content hunks for one path between `rev_old` and either another revision
/// or, when `rev_new` is `None`, the file as it is in the working tree.
pub(super) fn run_diff(state: &ServerState, params: DiffFileParams) -> Result<CallToolResult, McpError> {
    let started = std::time::Instant::now();
    let repo = require_git_repo(state)?;
    reject_external_path(&params.path)?;
    // Checked for both forms: only the working-tree side reads the disk, but a path that leaves the
    // repository names nothing in a revision either.
    crate::path::contained_file(repo.workdir(), &params.path)
        .map_err(|_| super::tool_contract::outside_repository("git_diff", &params.path))?;
    let resolve = |rev: &str| {
        repo.resolve_rev(rev)
            .map_err(|e| McpError::invalid_params(format!("`git_diff`: cannot resolve revision `{rev}`: {e}"), None))
    };
    let old_sha = resolve(&params.rev_old)?;
    let (result, to) = match params.rev_new.as_deref() {
        Some(rev_new) => {
            let new_sha = resolve(rev_new)?;
            (repo.diff_file(&old_sha, &new_sha, &params.path), new_sha)
        }
        None => (
            repo.diff_file_worktree(&old_sha, &params.path),
            WORKING_TREE.to_string(),
        ),
    };
    let result = result.map_err(|e| match e {
        crate::git::GitError::OutsideRepo(path) => super::tool_contract::outside_repository("git_diff", &path),
        other => McpError::internal_error(format!("`git_diff`: {other}"), None),
    })?;
    let diff = result.unwrap_or(crate::git::FileDiff {
        hunks: Vec::new(),
        old_exists: false,
        new_exists: false,
        binary: false,
    });
    let total_hunks = diff.hunks.len();
    let mut text_bytes = 0usize;
    let hunks: Vec<HunkView> = diff
        .hunks
        .into_iter()
        .take(DIFF_HUNKS_MAX)
        .take_while(|hunk| {
            // The first hunk is always returned, so an oversized one is not an empty answer.
            let fits = text_bytes == 0 || text_bytes + hunk.text.len() <= DIFF_TEXT_MAX;
            text_bytes += hunk.text.len();
            fits
        })
        .map(|h| HunkView {
            kind: h.kind.as_str(),
            old_line_start: h.old_line_start,
            old_line_count: h.old_line_count,
            new_line_start: h.new_line_start,
            new_line_count: h.new_line_count,
            text: h.text,
        })
        .collect();
    let truncated = hunks.len() < total_hunks;
    json_result(&DiffFileResponse {
        path: params.path,
        from: old_sha,
        to,
        present_at_from: diff.old_exists,
        present_at_to: diff.new_exists,
        binary: diff.binary,
        truncated,
        total_hunks: truncated.then_some(total_hunks),
        hunks,
        elapsed_us: elapsed_us(started),
    })
}

/// The working-tree bytes of an indexed file, when they are still the bytes that were indexed (the
/// outline's byte spans only line up with those).
fn indexed_source(state: &ServerState, path: &crate::path::RelPath, indexed_size: u64) -> Option<Vec<u8>> {
    let file = super::tool_contract::indexed_file(state, "git", path).ok()?;
    std::fs::read(file)
        .ok()
        .filter(|bytes| bytes.len() as u64 == indexed_size)
}

/// Clamp a 1-based inclusive blame span to the file as it is at `rev`, or explain why it does not
/// fit. gix reports an out-of-range span as an opaque internal error.
fn span_in_file(
    repo: &crate::git::Repo,
    rev: &str,
    path: &crate::path::RelPath,
    line: u32,
    end_line: u32,
) -> Result<(u32, u32), McpError> {
    let Some(text) = path.as_str() else {
        return Ok((line, end_line));
    };
    let blob = repo
        .read_blob_at_rev(rev, text)
        .map_err(|e| McpError::internal_error(format!("`git_blame`: read {rev}:{path}: {e}"), None))?
        .ok_or_else(|| McpError::invalid_params(format!("`git_blame`: no file `{path}` at {rev}"), None))?;
    let newlines = memchr::memchr_iter(b'\n', &blob).count() as u32;
    let total = newlines + u32::from(blob.last().is_some_and(|byte| *byte != b'\n'));
    if line > total.max(1) {
        return Err(McpError::invalid_params(
            format!("`git_blame`: `{path}` has {total} lines at {rev}; line {line} is out of range"),
            None,
        ));
    }
    Ok((line, end_line.min(total.max(1))))
}

/// Grammar for `path`, honouring the workspace's `[languages]` overrides so git-side tools agree
/// with what the scanner indexed.
fn detect_lang(state: &ServerState, path: impl AsRef<std::ffi::OsStr>) -> Option<crate::lang::LangId> {
    crate::lang_rules::LangRules::from_config(&state.shared.config.languages)
        .unwrap_or_default()
        .detect(std::path::Path::new(path.as_ref()))
        .lang()
}

/// The `diff_outline` operation: which symbols the served view has that `rev` does not, and
/// vice versa — "what did this branch add" without reading source.
pub(super) async fn run_diff_outline(
    state: &ServerState,
    params: DiffOutlineParams,
) -> Result<CallToolResult, McpError> {
    let started = std::time::Instant::now();
    let repo = require_git_repo(state)?;
    let rev_spec = params.rev.as_deref().unwrap_or("HEAD");
    let rev_sha = repo
        .resolve_rev(rev_spec)
        .map_err(|e| McpError::invalid_params(format!("resolve_rev({rev_spec}): {e}"), None))?;

    state.await_cache_ready().await;
    let cache = state.shared.cache.load_full();
    let here = cache.get(&params.path).map(|l1| {
        l1.symbols
            .iter()
            .map(|s| (s.name.clone(), kind_to_str(s.kind)))
            .collect::<Vec<(String, &'static str)>>()
    });

    let rev_blob = repo
        .read_blob_at_rev(&rev_sha, &params.path)
        .map_err(|e| McpError::internal_error(format!("read blob {rev_sha}:{}: {e}", params.path), None))?;

    let there: Option<Vec<(String, &'static str)>> = match rev_blob {
        Some(bytes) => {
            let lang = detect_lang(state, &params.path)
                .ok_or_else(|| McpError::invalid_params(format!("unsupported language for {}", params.path), None))?;
            let l1 = crate::extract::l1::extract_l1(lang, &bytes)
                .map_err(|e| McpError::internal_error(format!("extract {rev_sha}:{}: {e}", params.path), None))?;
            Some(l1.symbols.into_iter().map(|s| (s.name, kind_to_str(s.kind))).collect())
        }
        None => None,
    };

    let (added, removed, common, note) = match (here, there) {
        (Some(h), Some(t)) => {
            let hs: ahash::AHashSet<(String, &'static str)> = h.iter().cloned().collect();
            let ts: ahash::AHashSet<(String, &'static str)> = t.iter().cloned().collect();
            let added = h
                .iter()
                .filter(|p| !ts.contains(*p))
                .cloned()
                .map(|(n, k)| DiffSymbolView {
                    name: n,
                    kind: k.to_string(),
                })
                .collect();
            let removed = t
                .iter()
                .filter(|p| !hs.contains(*p))
                .cloned()
                .map(|(n, k)| DiffSymbolView {
                    name: n,
                    kind: k.to_string(),
                })
                .collect();
            let common = h
                .iter()
                .filter(|p| ts.contains(*p))
                .cloned()
                .map(|(n, k)| DiffSymbolView {
                    name: n,
                    kind: k.to_string(),
                })
                .collect();
            (added, removed, common, None)
        }
        (Some(h), None) => (
            h.into_iter()
                .map(|(n, k)| DiffSymbolView {
                    name: n,
                    kind: k.to_string(),
                })
                .collect(),
            Vec::new(),
            Vec::new(),
            Some(format!("path absent at {rev_spec}; entire file treated as added")),
        ),
        (None, Some(t)) => (
            Vec::new(),
            t.into_iter()
                .map(|(n, k)| DiffSymbolView {
                    name: n,
                    kind: k.to_string(),
                })
                .collect(),
            Vec::new(),
            Some("path not indexed in the current view; entire file treated as removed".to_string()),
        ),
        (None, None) => {
            return Err(McpError::invalid_params(
                format!("path not present in current view or at {rev_spec}: {}", params.path),
                None,
            ));
        }
    };

    json_result(&DiffOutlineResponse {
        path: params.path,
        rev: rev_sha,
        added,
        removed,
        common,
        note,
        elapsed_us: elapsed_us(started),
    })
}

/// The `blame` operation: per-line blame at `rev`, optionally clamped to a 1-based inclusive
/// line range.
pub(super) fn run_blame(state: &ServerState, params: BlameFileParams) -> Result<CallToolResult, McpError> {
    let started = std::time::Instant::now();
    let repo = require_git_repo(state)?;
    reject_external_path(&params.path)?;
    let suspect_sha = match params.rev.as_deref() {
        Some(r) => repo
            .resolve_rev(r)
            .map_err(|e| McpError::invalid_params(format!("resolve_rev({r}): {e}"), None))?,
        None => head_sha(repo)?,
    };
    let range = match (params.line_start, params.line_end) {
        (Some(lo), Some(hi)) => Some(span_in_file(repo, &suspect_sha, &params.path, lo, hi)?),
        (None, None) => None,
        _ => {
            return Err(McpError::invalid_params(
                "`git_blame`: `line` and `end_line` must be provided together",
                None,
            ));
        }
    };
    let snapshot = head_snapshot_id(&suspect_sha);
    let resume_after = blame_resume_after(params.cursor.as_ref(), snapshot)?;
    let result = match state.shared.git_cache.blame(repo, &suspect_sha, &params.path, range) {
        Ok(r) => r,
        Err(e) => {
            if let Some(too_large) = blame_too_large_response(&params.path, &suspect_sha, &e, started) {
                return json_result(&too_large);
            }
            return Err(McpError::internal_error(format!("blame: {e}"), None));
        }
    };
    let page = paginate_blame_hunks(
        result.hunks.iter(),
        resume_after,
        params.limit,
        params.max_tokens,
        snapshot,
    );
    let truncated_reason: Option<&'static str> = match result.truncated_reason.as_deref() {
        Some("shallow_clone") => Some("shallow_clone"),
        Some(_) => Some("truncated"),
        None if repo.is_shallow() => Some("shallow_clone"),
        None => None,
    };
    json_result(&BlameResponse {
        path: result.path.clone(),
        suspect_sha: result.suspect_sha.clone(),
        hunks: page.hunks,
        budgeted: page.budgeted,
        truncated: truncated_reason.is_some(),
        truncated_reason,
        next_cursor: page.next_cursor,
        elapsed_us: elapsed_us(started),
    })
}

/// The `blame_symbol` operation: blame clamped to one symbol's line span, resolved through
/// the cached L1 outline of the served view.
pub(super) async fn run_blame_symbol(
    state: &ServerState,
    params: BlameSymbolParams,
) -> Result<CallToolResult, McpError> {
    let started = std::time::Instant::now();
    let repo = require_git_repo(state)?;
    reject_external_path(&params.path)?;
    let kind = params.kind.as_deref().map(parse_kind).transpose()?;
    state.await_cache_ready().await;
    let cache = state.shared.cache.load_full();
    let l1 = cache
        .get(&params.path)
        .ok_or_else(|| super::tool_contract::not_indexed(state, "git_blame", &params.path))?;
    let source = indexed_source(state, &params.path, l1.size_bytes);
    let sym = pick_symbol(
        "git_blame",
        &params.path,
        &l1.symbols,
        source.as_deref(),
        &params.name,
        kind,
        params.line,
    )?
    .symbol;
    let (line_start, line_end) = symbol_line_range(repo, &params.path, sym);
    let suspect_sha = match params.rev.as_deref() {
        Some(r) => repo
            .resolve_rev(r)
            .map_err(|e| McpError::invalid_params(format!("resolve_rev({r}): {e}"), None))?,
        None => head_sha(repo)?,
    };
    let snapshot = head_snapshot_id(&suspect_sha);
    let resume_after = blame_resume_after(params.cursor.as_ref(), snapshot)?;
    let result = match state
        .shared
        .git_cache
        .blame(repo, &suspect_sha, &params.path, Some((line_start, line_end)))
    {
        Ok(r) => r,
        Err(e) => {
            if let Some(too_large) =
                blame_symbol_too_large_response(&params.path, &suspect_sha, sym, line_start, line_end, &e, started)
            {
                return json_result(&too_large);
            }
            return Err(McpError::internal_error(format!("blame: {e}"), None));
        }
    };
    let page = paginate_blame_hunks(
        result.hunks.iter(),
        resume_after,
        params.limit,
        params.max_tokens,
        snapshot,
    );
    let truncated_reason: Option<&'static str> = match result.truncated_reason.as_deref() {
        Some("shallow_clone") => Some("shallow_clone"),
        Some(_) => Some("truncated"),
        None if repo.is_shallow() => Some("shallow_clone"),
        None => None,
    };
    json_result(&BlameSymbolResponse {
        path: result.path.clone(),
        suspect_sha: result.suspect_sha.clone(),
        name: sym.name.clone(),
        kind: kind_to_str(sym.kind).to_string(),
        line_start,
        line_end,
        hunks: page.hunks,
        budgeted: page.budgeted,
        truncated: truncated_reason.is_some(),
        truncated_reason,
        next_cursor: page.next_cursor,
        elapsed_us: elapsed_us(started),
    })
}

/// The `symbol_history` operation: tree-sitter × git — the commits where a named symbol's
/// body fingerprint actually changed (or where it was introduced / removed).
pub(super) fn run_symbol_history(state: &ServerState, params: SymbolHistoryParams) -> Result<CallToolResult, McpError> {
    let started = std::time::Instant::now();
    let repo = require_git_repo(state)?;
    let kind = params.kind.as_deref().map(parse_kind).transpose()?;
    let limit = params.limit.unwrap_or(20).min(100) as usize;
    let lang = detect_lang(state, &params.path)
        .ok_or_else(|| McpError::invalid_params(format!("unsupported language: {}", params.path), None))?;
    let hash_mode = match params.hash_mode.as_deref() {
        Some(s) => parse_hash_mode(s)?,
        None => HashMode::Normalized,
    };

    // Which of the file's same-named definitions is meant. Line numbers differ at every revision,
    // so the choice is carried through history as "the n-th definition of that name in the file".
    // A file that is no longer indexed (deleted, renamed) has no outline to choose from and keeps
    // the first.
    let outline = state.shared.cache.load_full().get(&params.path).or_else(|| {
        let store = state.shared.store.try_read().ok()?;
        crate::query::file_outline(&store, &params.path)
            .ok()
            .map(std::sync::Arc::new)
    });
    let occurrence = match outline {
        Some(l1) => {
            let source = indexed_source(state, &params.path, l1.size_bytes);
            pick_symbol(
                "git_log",
                &params.path,
                &l1.symbols,
                source.as_deref(),
                &params.name,
                kind,
                params.line,
            )?
            .occurrence
        }
        None => 0,
    };

    let head = head_sha(repo)?;
    let snapshot = head_snapshot_id(&head);

    let skip = match params.cursor.as_ref() {
        Some(c) => {
            let (offset, snapshot_id) = c.decode_in_memory()?;
            if snapshot_id != snapshot {
                return json_result(&SymbolHistoryResponse {
                    path: params.path,
                    name: params.name,
                    kind: kind.map(|k| kind_to_str(k).to_string()),
                    commits_inspected: 0,
                    history: Vec::new(),
                    hash_mode: hash_mode.as_str(),
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

    let walk_depth = (skip.saturating_add(limit).saturating_add(1).saturating_mul(4)).min(LOG_WALK_MAX) as u32;
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

    let chronological: Vec<CommitInfo> = commits.iter().cloned().rev().collect();

    let mut history = Vec::new();
    let mut prev_fp: Option<Vec<u8>> = None;
    let mut prev_existed = false;
    let mut inspected: u32 = 0;
    for c in chronological {
        inspected += 1;
        let blob = repo
            .read_blob_at_rev_with_oid(&c.sha, &params.path)
            .map_err(|e| McpError::internal_error(format!("blob: {e}"), None))?;
        let fingerprint = match blob {
            Some((bytes, oid)) => outline_entry_for_blob(&state.shared.outline_cache, oid, lang, bytes)
                .and_then(|entry| symbol_fingerprint(&entry, &params.name, kind, occurrence, lang, hash_mode)),
            None => None,
        };
        let change = match (prev_existed, fingerprint.as_ref()) {
            (false, Some(_)) => Some("introduced"),
            (true, None) => Some("removed"),
            (true, Some(curr)) => {
                if prev_fp.as_deref() != Some(curr.as_slice()) {
                    Some("modified")
                } else {
                    None
                }
            }
            (false, None) => None,
        };
        if let Some(kind_str) = change {
            history.push(SymbolHistoryEntry {
                sha: c.sha.clone(),
                short_sha: c.short_sha.clone(),
                summary: c.summary.clone(),
                author: c.author.clone(),
                author_time_unix: c.author_time_unix,
                change: kind_str,
            });
        }
        prev_existed = fingerprint.is_some();
        prev_fp = fingerprint;
    }
    history.reverse();

    let total_history = history.len();
    let page: Vec<SymbolHistoryEntry> = history.into_iter().skip(skip).take(limit).collect();
    let has_more = total_history > skip + page.len();
    let next_cursor = has_more.then(|| super::cursor::Cursor::encode_in_memory((skip + page.len()) as u64, snapshot));

    let truncated = repo.is_shallow();
    json_result(&SymbolHistoryResponse {
        path: params.path,
        name: params.name,
        kind: kind.map(|k| kind_to_str(k).to_string()),
        commits_inspected: inspected,
        history: page,
        hash_mode: hash_mode.as_str(),
        truncated,
        truncated_reason: truncated.then_some("shallow_clone"),
        next_cursor,
        cursor_invalidated: false,
        elapsed_us: elapsed_us(started),
    })
}
