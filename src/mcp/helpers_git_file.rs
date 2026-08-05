//! The five `git` modes scoped to a single path — `diff`, `diff_outline`, `blame`, `blame_symbol`,
//! `symbol_history`.
//!
//! Split out of `helpers_git.rs`, which owns the dispatcher and the commit-log family, purely to
//! keep both files under the 1000-line cap. Nothing here is reachable except through
//! [`super::helpers_git::run_git`].

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::ServerState;
use super::helpers::{
    HashMode, LOG_WALK_MAX, blame_symbol_too_large_response, blame_too_large_response, elapsed_us,
    git_history_if_fresh, head_sha, head_snapshot_id, json_result, kind_to_str, outline_entry_for_blob,
    paginate_blame_hunks, parse_hash_mode, parse_kind, require_git_repo, symbol_fingerprint, symbol_line_range,
};
use super::helpers_git::reject_external_path;
use super::types::{
    BlameFileParams, BlameResponse, BlameSymbolParams, BlameSymbolResponse, DiffFileParams, DiffFileResponse,
    DiffOutlineParams, DiffOutlineResponse, DiffSymbolView, HunkView, SymbolHistoryEntry, SymbolHistoryParams,
    SymbolHistoryResponse,
};
use crate::git::CommitInfo;

/// Body for `git` mode `diff`: content hunks for one path between two revisions.
pub(super) fn run_diff(state: &ServerState, params: DiffFileParams) -> Result<CallToolResult, McpError> {
    let started = std::time::Instant::now();
    let repo = require_git_repo(state)?;
    let old_sha = repo
        .resolve_rev(&params.rev_old)
        .map_err(|e| McpError::invalid_params(format!("resolve_rev({}): {e}", params.rev_old), None))?;
    let new_sha = repo
        .resolve_rev(&params.rev_new)
        .map_err(|e| McpError::invalid_params(format!("resolve_rev({}): {e}", params.rev_new), None))?;
    let result = repo
        .diff_file(&old_sha, &new_sha, &params.path)
        .map_err(|e| McpError::internal_error(format!("diff: {e}"), None))?;
    let (hunks, present_old, present_new) = result.unwrap_or((Vec::new(), false, false));
    let hunks = hunks
        .into_iter()
        .map(|h| HunkView {
            kind: h.kind.as_str(),
            old_line_start: h.old_line_start,
            old_line_count: h.old_line_count,
            new_line_start: h.new_line_start,
            new_line_count: h.new_line_count,
            text: h.text,
        })
        .collect();
    json_result(&DiffFileResponse {
        path: params.path,
        rev_old: old_sha,
        rev_new: new_sha,
        present_at_old: present_old,
        present_at_new: present_new,
        hunks,
        elapsed_us: elapsed_us(started),
    })
}

/// Body for `git` mode `diff_outline`: which symbols the served view has that `rev` does not, and
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
    let here = cache.by_path.get(&params.path).map(|l1| {
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
            let lang = crate::lang::detect(std::path::Path::new(&params.path))
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

/// Body for `git` mode `blame`: per-line blame at `rev`, optionally clamped to a 1-based inclusive
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
        (Some(lo), Some(hi)) => Some((lo, hi)),
        (None, None) => None,
        _ => {
            return Err(McpError::invalid_params(
                "line_start and line_end must be provided together",
                None,
            ));
        }
    };
    let resume_after: u32 = match params.cursor.as_ref() {
        Some(c) => c.decode_in_memory()?.0.min(u32::MAX as u64) as u32,
        None => 0,
    };
    let result = match state.shared.git_cache.blame(repo, &suspect_sha, &params.path, range) {
        Ok(r) => r,
        Err(e) => {
            if let Some(too_large) = blame_too_large_response(&params.path, &suspect_sha, &e, started) {
                return json_result(&too_large);
            }
            return Err(McpError::internal_error(format!("blame: {e}"), None));
        }
    };
    let (hunks, next_cursor) = paginate_blame_hunks(result.hunks.iter(), resume_after, params.limit);
    let truncated_reason: Option<&'static str> = match result.truncated_reason.as_deref() {
        Some("shallow_clone") => Some("shallow_clone"),
        Some(_) => Some("truncated"),
        None if repo.is_shallow() => Some("shallow_clone"),
        None => None,
    };
    json_result(&BlameResponse {
        path: result.path.clone(),
        suspect_sha: result.suspect_sha.clone(),
        hunks,
        truncated: truncated_reason.is_some(),
        truncated_reason,
        next_cursor,
        elapsed_us: elapsed_us(started),
    })
}

/// Body for `git` mode `blame_symbol`: blame clamped to one symbol's line span, resolved through
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
        .by_path
        .get(&params.path)
        .ok_or_else(|| McpError::invalid_params(format!("file not indexed in current view: {}", params.path), None))?;
    let sym = l1
        .symbols
        .iter()
        .find(|s| s.name == params.name && kind.is_none_or(|k| s.kind == k))
        .ok_or_else(|| {
            McpError::invalid_params(
                format!(
                    "symbol `{}`{} not found in {}",
                    params.name,
                    kind.map(|k| format!(" (kind={})", kind_to_str(k))).unwrap_or_default(),
                    params.path
                ),
                None,
            )
        })?;
    let (line_start, line_end) = symbol_line_range(repo, &params.path, sym);
    let suspect_sha = match params.rev.as_deref() {
        Some(r) => repo
            .resolve_rev(r)
            .map_err(|e| McpError::invalid_params(format!("resolve_rev({r}): {e}"), None))?,
        None => head_sha(repo)?,
    };
    let resume_after: u32 = match params.cursor.as_ref() {
        Some(c) => c.decode_in_memory()?.0.min(u32::MAX as u64) as u32,
        None => 0,
    };
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
    let (hunks, next_cursor) = paginate_blame_hunks(result.hunks.iter(), resume_after, params.limit);
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
        hunks,
        truncated: truncated_reason.is_some(),
        truncated_reason,
        next_cursor,
        elapsed_us: elapsed_us(started),
    })
}

/// Body for `git` mode `symbol_history`: tree-sitter × git — the commits where a named symbol's
/// body fingerprint actually changed (or where it was introduced / removed).
pub(super) fn run_symbol_history(state: &ServerState, params: SymbolHistoryParams) -> Result<CallToolResult, McpError> {
    let started = std::time::Instant::now();
    let repo = require_git_repo(state)?;
    let kind = params.kind.as_deref().map(parse_kind).transpose()?;
    let limit = params.limit.unwrap_or(20).min(100) as usize;
    let lang = crate::lang::detect(std::path::Path::new(&params.path))
        .ok_or_else(|| McpError::invalid_params(format!("unsupported language: {}", params.path), None))?;
    let hash_mode = match params.hash_mode.as_deref() {
        Some(s) => parse_hash_mode(s)?,
        None => HashMode::Normalized,
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
                .and_then(|entry| symbol_fingerprint(&entry, &params.name, kind, lang, hash_mode)),
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
