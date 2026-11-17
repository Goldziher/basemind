//! The five git operations scoped to a single path — `diff`, `diff_outline`, `blame`, `blame_symbol`,
//! `symbol_history`.
//!
//! Split out of `helpers_git.rs`, which owns the `git_log` / `git_blame` / `git_diff` bodies and the
//! commit-log family, purely to keep both files under the 1000-line cap. Nothing here is reachable
//! except through those three bodies.

use super::by_path::MATCHED_EARLIER;
use super::common::{
    LOG_WALK_MAX, blame_resume_after, blame_symbol_too_large_response, blame_too_large_response, git_history_if_fresh,
    head_sha, head_snapshot_id, paginate_blame_hunks, require_git_repo,
};
use super::fingerprint::{HashMode, outline_entry_for_blob, parse_hash_mode, symbol_fingerprint};
use super::log::{history_page, reject_external_path, walk_cursor};
use crate::engine::error::EngineError;
use crate::engine::pick::pick_symbol;
use crate::engine::timing::elapsed_us;
use crate::engine::types::code::{kind_to_str, parse_kind};
use crate::engine::types::git::{
    BlameFileParams, BlameResponse, BlameSymbolParams, BlameSymbolResponse, DiffFileParams, DiffFileResponse,
    DiffOutlineParams, DiffOutlineResponse, DiffSymbolView, HunkView, SymbolHistoryEntry, SymbolHistoryParams,
    SymbolHistoryResponse,
};
use crate::engine::workspace::Workspace;
use crate::git::CommitInfo;

/// What the old side of a text diff is compared against.
const WORKING_TREE: &str = "working tree";

/// Hunks one `git_diff` returns. A rewrite of a generated file is thousands of them.
const DIFF_HUNKS_MAX: usize = 200;
/// Bytes of hunk text one `git_diff` returns.
const DIFF_TEXT_MAX: usize = 128 * 1024;

/// The `diff` operation: content hunks for one path between `rev_old` and either another revision
/// or, when `rev_new` is `None`, the file as it is in the working tree.
pub(super) fn run_diff(ws: &Workspace, params: DiffFileParams) -> Result<DiffFileResponse, EngineError> {
    let repo = require_git_repo(ws)?;
    reject_external_path(&params.path)?;
    // Between two revisions only blobs are read, so the lexical check (`..`, absolute) is the whole
    // of it: a tracked symlink is a blob holding its target path, wherever that points. The
    // working-tree side reads the disk, and `diff_file_worktree` resolves the path before it does.
    crate::path::lexical_file(repo.workdir(), &params.path)
        .map_err(|_| crate::engine::files::outside_repository("git_diff", &params.path))?;
    let resolve = |rev: &str| {
        repo.resolve_rev(rev)
            .map_err(|e| EngineError::invalid_argument(format!("`git_diff`: cannot resolve revision `{rev}`: {e}")))
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
        crate::git::GitError::OutsideRepo(path) => crate::engine::files::outside_repository("git_diff", &path),
        other => EngineError::internal(format!("`git_diff`: {other}")),
    })?;
    let Some(diff) = result else {
        let to_side = params
            .rev_new
            .as_deref()
            .map_or_else(|| "in the working tree".to_string(), |rev| format!("at `{rev}`"));
        return Err(EngineError::invalid_argument(format!(
            "`git_diff`: `{}` exists neither at `{}` nor {to_side}. `path` is a repo-relative file \
                 path; locate it with `find_files`.",
            params.path, params.rev_old
        )));
    };
    let total_hunks = diff.hunks.len();
    let (hunks, clipped) = capped_hunks(diff.hunks);
    let truncated_reason = if hunks.len() == DIFF_HUNKS_MAX && total_hunks > DIFF_HUNKS_MAX {
        Some("hunk_cap")
    } else if clipped || hunks.len() < total_hunks {
        Some("byte_cap")
    } else {
        None
    };
    Ok(DiffFileResponse {
        path: params.path,
        from: old_sha,
        to,
        present_at_from: diff.old_exists,
        present_at_to: diff.new_exists,
        binary: diff.binary,
        too_large: diff.too_large,
        truncated: truncated_reason.is_some(),
        truncated_reason,
        omitted: total_hunks - hunks.len(),
        hunks,
    })
}

/// The leading hunks that fit [`DIFF_HUNKS_MAX`] and [`DIFF_TEXT_MAX`], and whether the first one
/// had its text clipped. The first hunk is always returned, so an oversized one is not an empty
/// answer; it is cut to the byte cap instead, because a new or rewritten file is ONE hunk holding
/// the whole file.
fn capped_hunks(hunks: Vec<crate::git::Hunk>) -> (Vec<HunkView>, bool) {
    let mut views: Vec<HunkView> = Vec::with_capacity(hunks.len().min(DIFF_HUNKS_MAX));
    let mut text_bytes = 0usize;
    let mut clipped = false;
    for mut hunk in hunks.into_iter().take(DIFF_HUNKS_MAX) {
        if text_bytes + hunk.text.len() > DIFF_TEXT_MAX {
            if !views.is_empty() {
                break;
            }
            clip_to_whole_lines(&mut hunk.text, DIFF_TEXT_MAX);
            clipped = true;
        }
        text_bytes += hunk.text.len();
        views.push(HunkView {
            old_line_start: hunk.old_line_start,
            old_line_count: hunk.old_line_count,
            new_line_start: hunk.new_line_start,
            new_line_count: hunk.new_line_count,
            text: hunk.text,
        });
        if clipped {
            break;
        }
    }
    (views, clipped)
}

/// Cut `text` to at most `cap` bytes, ending after its last whole line so a clipped hunk never
/// ends in a line fragment. Text with no newline within the cap is cut at a character boundary.
fn clip_to_whole_lines(text: &mut String, cap: usize) {
    if text.len() <= cap {
        return;
    }
    let cut = memchr::memrchr(b'\n', &text.as_bytes()[..cap]).map_or_else(
        || (0..=cap).rev().find(|&at| text.is_char_boundary(at)).unwrap_or(0),
        |newline| newline + 1,
    );
    text.truncate(cut);
}

/// Clamp a 1-based inclusive blame span to the file as it is at `rev`, or explain why it does not
/// fit. gix reports an out-of-range span as an opaque internal error.
fn span_in_file(
    repo: &crate::git::Repo,
    rev: &str,
    path: &crate::path::RelPath,
    line: u32,
    end_line: u32,
) -> Result<(u32, u32), EngineError> {
    let Some(text) = path.as_str() else {
        return Ok((line, end_line));
    };
    let blob = repo
        .read_blob_at_rev(rev, text)
        .map_err(|e| EngineError::internal(format!("`git_blame`: read {rev}:{path}: {e}")))?
        .ok_or_else(|| EngineError::invalid_argument(format!("`git_blame`: no file `{path}` at {rev}")))?;
    let newlines = memchr::memchr_iter(b'\n', &blob).count() as u32;
    let total = newlines + u32::from(blob.last().is_some_and(|byte| *byte != b'\n'));
    if line > total.max(1) {
        return Err(EngineError::invalid_argument(format!(
            "`git_blame`: `{path}` has {total} lines at {rev}; line {line} is out of range"
        )));
    }
    Ok((line, end_line.min(total.max(1))))
}

/// Grammar for `path`, honouring the workspace's `[languages]` overrides so git-side tools agree
/// with what the scanner indexed.
pub(super) fn detect_lang(ws: &Workspace, path: impl AsRef<std::ffi::OsStr>) -> Option<crate::lang::LangId> {
    ws.detect_lang(path)
}

/// The `diff_outline` operation: which symbols the served view has that `rev` does not, and
/// vice versa — "what did this branch add" without reading source. Synchronous: the caller waits for
/// the code map first, then runs this off the reactor.
pub(super) fn run_diff_outline(ws: &Workspace, params: DiffOutlineParams) -> Result<DiffOutlineResponse, EngineError> {
    let repo = require_git_repo(ws)?;
    let rev_spec = params.rev.as_deref().unwrap_or("HEAD");
    let rev_sha = repo
        .resolve_rev(rev_spec)
        .map_err(|e| EngineError::invalid_argument(format!("resolve_rev({rev_spec}): {e}")))?;

    let cache = ws.cache.load_full();
    let here = cache.get(&params.path).map(|l1| {
        l1.symbols
            .iter()
            .map(|s| (s.name.clone(), kind_to_str(s.kind)))
            .collect::<Vec<(String, &'static str)>>()
    });

    let rev_blob = repo
        .read_blob_at_rev(&rev_sha, &params.path)
        .map_err(|e| EngineError::internal(format!("read blob {rev_sha}:{}: {e}", params.path)))?;

    let there: Option<Vec<(String, &'static str)>> = match rev_blob {
        Some(bytes) => {
            let lang = detect_lang(ws, &params.path)
                .ok_or_else(|| EngineError::invalid_argument(format!("unsupported language for {}", params.path)))?;
            let l1 = crate::extract::l1::extract_l1(lang, &bytes)
                .map_err(|e| EngineError::internal(format!("extract {rev_sha}:{}: {e}", params.path)))?;
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
            let common = hs.intersection(&ts).count();
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
            0,
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
            0,
            Some("path not indexed in the current view; entire file treated as removed".to_string()),
        ),
        (None, None) => {
            return Err(EngineError::invalid_argument(format!(
                "path not present in current view or at {rev_spec}: {}",
                params.path
            )));
        }
    };

    Ok(DiffOutlineResponse {
        path: params.path,
        rev: rev_sha,
        added,
        removed,
        common,
        note,
    })
}

/// The `blame` operation: per-line blame at `rev`, optionally clamped to a 1-based inclusive
/// line range.
pub(super) fn run_blame(ws: &Workspace, params: BlameFileParams) -> Result<BlameResponse, EngineError> {
    let started = std::time::Instant::now();
    let repo = require_git_repo(ws)?;
    reject_external_path(&params.path)?;
    let suspect_sha = match params.rev.as_deref() {
        Some(r) => repo
            .resolve_rev(r)
            .map_err(|e| EngineError::invalid_argument(format!("resolve_rev({r}): {e}")))?,
        None => head_sha(repo)?,
    };
    let range = match (params.line_start, params.line_end) {
        (Some(lo), Some(hi)) => Some(span_in_file(repo, &suspect_sha, &params.path, lo, hi)?),
        (None, None) => None,
        _ => {
            return Err(EngineError::invalid_argument(
                "`git_blame`: `line` and `end_line` must be provided together",
            ));
        }
    };
    let snapshot = head_snapshot_id(&suspect_sha);
    let resume_after = blame_resume_after(params.cursor.as_ref(), snapshot)?;
    let result = match ws.git_cache.blame(repo, &suspect_sha, &params.path, range) {
        Ok(r) => r,
        Err(e) => {
            if let Some(too_large) = blame_too_large_response(&params.path, &suspect_sha, &e, started) {
                return Ok(too_large);
            }
            return Err(EngineError::internal(format!("blame: {e}")));
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
    Ok(BlameResponse {
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

/// The `blame_symbol` operation: blame clamped to one symbol's line span.
///
/// The span is the symbol's in the blamed revision's own blob. The working-tree outline describes
/// other lines as soon as the file differs from that revision above the symbol.
pub(super) fn run_blame_symbol(ws: &Workspace, params: BlameSymbolParams) -> Result<BlameSymbolResponse, EngineError> {
    let started = std::time::Instant::now();
    let repo = require_git_repo(ws)?;
    reject_external_path(&params.path)?;
    let kind = params.kind.as_deref().map(parse_kind).transpose()?;
    let suspect_sha = match params.rev.as_deref() {
        Some(r) => repo
            .resolve_rev(r)
            .map_err(|e| EngineError::invalid_argument(format!("resolve_rev({r}): {e}")))?,
        None => head_sha(repo)?,
    };
    let at = params.rev.as_deref().unwrap_or("HEAD");
    let lang = detect_lang(ws, &params.path).ok_or_else(|| {
        EngineError::invalid_argument(format!(
            "`git_blame`: `{}` is not in a language with symbols; blame a line span with `line` and \
                 `end_line` instead.",
            params.path
        ))
    })?;
    let (bytes, oid) = repo
        .read_blob_at_rev_with_oid(&suspect_sha, &params.path)
        .map_err(|e| EngineError::internal(format!("blob: {e}")))?
        .ok_or_else(|| {
            EngineError::invalid_argument(format!("`git_blame`: `{}` does not exist at {at}.", params.path))
        })?;
    // The scan parses no file over `scan.max_file_bytes`; a revision's blob is held to the same.
    let max_bytes = ws.config.scan.max_file_bytes;
    if bytes.len() as u64 > max_bytes {
        return Err(EngineError::invalid_argument(format!(
            "`git_blame`: `{}` is {} bytes at {at}, over `scan.max_file_bytes` ({max_bytes}), and is not \
                 parsed for symbols. Blame a line span with `line` and `end_line` instead.",
            params.path,
            bytes.len()
        )));
    }
    let outline = outline_entry_for_blob(&ws.outline_cache, oid, lang, bytes)
        .ok_or_else(|| EngineError::internal(format!("`git_blame`: could not parse `{}` at {at}", params.path)))?;
    let sym = pick_symbol(
        "git_blame",
        &params.path,
        Some(at),
        &outline.map.symbols,
        Some(outline.source.as_slice()),
        &params.name,
        kind,
        params.line,
    )?
    .symbol;
    let (line_start, line_end) = crate::engine::pick::line_span(sym, Some(outline.source.as_slice()));
    let snapshot = head_snapshot_id(&suspect_sha);
    let resume_after = blame_resume_after(params.cursor.as_ref(), snapshot)?;
    let result = match ws
        .git_cache
        .blame(repo, &suspect_sha, &params.path, Some((line_start, line_end)))
    {
        Ok(r) => r,
        Err(e) => {
            if let Some(too_large) =
                blame_symbol_too_large_response(&params.path, &suspect_sha, sym, line_start, line_end, &e, started)
            {
                return Ok(too_large);
            }
            return Err(EngineError::internal(format!("blame: {e}")));
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
    Ok(BlameSymbolResponse {
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
///
/// One call inspects a window of the commits touching the file, newest first, starting at the
/// commit offset its cursor carries. A window that ends before the history does hands back a
/// cursor to the commit after it, so every change back to the introduction is reachable however
/// many commits lie in between; a page can then be short, or empty, and still not be the last.
pub(super) fn run_symbol_history(
    ws: &Workspace,
    params: SymbolHistoryParams,
) -> Result<SymbolHistoryResponse, EngineError> {
    let repo = require_git_repo(ws)?;
    let kind = params.kind.as_deref().map(parse_kind).transpose()?;
    let limit = params.limit.unwrap_or(20).min(100) as usize;
    let lang = detect_lang(ws, &params.path)
        .ok_or_else(|| EngineError::invalid_argument(format!("unsupported language: {}", params.path)))?;
    let hash_mode = match params.hash_mode.as_deref() {
        Some(s) => parse_hash_mode(s)?,
        None => HashMode::Normalized,
    };

    // Which of the file's same-named definitions is meant. Line numbers differ at every revision,
    // so the choice is carried through history as "the n-th definition of that name in the file".
    // A file that is no longer indexed (deleted, renamed) has no outline to choose from and keeps
    // the first. So does a symbol the working tree no longer defines: its history is still there
    // to read, and only a name no inspected commit defines either is a mistake in the call.
    // The outline and the bytes its spans are laid over follow one index entry; with the store
    // busy, the in-RAM outline is used without spans.
    let cache = ws.cache.load_full();
    let outline = match ws.try_store_read() {
        Ok(store) => crate::engine::freshness::indexed_outline(&store, &cache, &params.path).map(|(entry, l1)| {
            let file = crate::engine::files::IndexedFiles::of(ws)
                .file(&store, &params.path)
                .ok();
            (l1, file.map(|file| (file, entry)))
        }),
        Err(_) => cache.get(&params.path).map(|l1| (l1, None)),
    };
    let mut absent_now: Option<EngineError> = None;
    let occurrence = match outline {
        Some((l1, indexed)) => {
            let source = indexed.and_then(|(file, entry)| {
                crate::engine::freshness::load(&file, (&entry).into(), ws.config.scan.max_file_bytes)
                    .ok()
                    .filter(|live| live.fresh)
                    .map(|live| live.bytes)
            });
            let defined_now = l1
                .symbols
                .iter()
                .any(|symbol| symbol.name == params.name && kind.is_none_or(|wanted| symbol.kind == wanted));
            match pick_symbol(
                "git_log",
                &params.path,
                None,
                &l1.symbols,
                source.as_deref(),
                &params.name,
                kind,
                params.line,
            ) {
                Ok(picked) => picked.occurrence,
                Err(error) if !defined_now && params.line.is_none() => {
                    absent_now = Some(error);
                    0
                }
                Err(error) => return Err(error),
            }
        }
        None => 0,
    };

    let head = head_sha(repo)?;
    let invalidated = || {
        Ok(SymbolHistoryResponse {
            kind: kind.map(|k| kind_to_str(k).to_string()),
            commits_inspected: 0,
            history: Vec::new(),
            hash_mode: hash_mode.as_str(),
            unmatched: None,
            budgeted: false,
            truncated: false,
            truncated_reason: None,
            next_cursor: None,
            cursor_invalidated: true,
        })
    };

    // The cursor is an offset into the commits touching the file, newest first, and whether a
    // commit on an earlier page defined the name. Its snapshot says which list the offset counts
    // in: the live walk simplifies history where the index keeps every commit touching the file.
    let Some((packed, issued)) = walk_cursor(params.cursor.as_ref(), &head)? else {
        return invalidated();
    };
    let start = usize::try_from(packed & !MATCHED_EARLIER).unwrap_or(usize::MAX);
    let defined_earlier = packed & MATCHED_EARLIER != 0;

    let window = limit.saturating_add(1).saturating_mul(4).min(LOG_WALK_MAX);
    // One commit past the window: it tells a used-up window from a whole history, and it is the
    // state the window's oldest commit is compared against. Only the window is walked into: a log
    // cached under its depth would be a new entry, walked again from HEAD, for every page.
    let fetch = window + 1;
    let (mut commits, snapshot) = history_page(
        "git_log",
        &head,
        issued,
        git_history_if_fresh(ws, &head).map(|index| index.commits_touching(&params.path, start, fetch)),
        || {
            repo.log_window_touching(&head, &params.path, start, fetch)
                .map_err(|e| EngineError::internal(format!("log: {e}")))
        },
    )?;
    let fingerprint_at = |sha: &str| -> Result<Option<Vec<u8>>, EngineError> {
        let blob = repo
            .read_blob_at_rev_with_oid(sha, &params.path)
            .map_err(|e| EngineError::internal(format!("blob: {e}")))?;
        Ok(blob.and_then(|(bytes, oid)| {
            outline_entry_for_blob(&ws.outline_cache, oid, lang, bytes)
                .and_then(|entry| symbol_fingerprint(&entry, &params.name, kind, occurrence, lang, hash_mode))
        }))
    };
    // Without the seed the oldest commit inspected would read as the symbol's introduction
    // whenever the symbol exists there.
    let beyond_window = (commits.len() > window).then(|| commits.split_off(window));
    let window_exhausted = beyond_window.is_some();
    let seed = match beyond_window.as_deref().and_then(<[CommitInfo]>::first) {
        Some(older) => fingerprint_at(&older.sha)?,
        None => None,
    };

    // Oldest first, each change tagged with its commit's offset in the newest-first list.
    let mut history: Vec<(usize, SymbolHistoryEntry)> = Vec::new();
    let mut prev_existed = seed.is_some();
    let mut defined = defined_earlier || seed.is_some();
    let mut prev_fp: Option<Vec<u8>> = seed;
    let inspected = commits.len();
    for (index, c) in commits.iter().enumerate().rev() {
        let fingerprint = fingerprint_at(&c.sha)?;
        defined |= fingerprint.is_some();
        let change = match (prev_existed, fingerprint.as_ref()) {
            (false, Some(_)) => Some("introduced"),
            (true, None) => Some("removed"),
            (true, Some(curr)) => (prev_fp.as_deref() != Some(curr.as_slice())).then_some("modified"),
            (false, None) => None,
        };
        if let Some(change) = change {
            history.push((
                start + index,
                SymbolHistoryEntry {
                    sha: super::common::abbreviated_sha(c.sha.clone()),
                    summary: c.summary.clone(),
                    author: c.author.clone(),
                    author_time_unix: c.author_time_unix,
                    change,
                },
            ));
        }
        prev_existed = fingerprint.is_some();
        prev_fp = fingerprint;
    }
    history.reverse();

    let offsets: Vec<usize> = history.iter().map(|(offset, _)| *offset).collect();
    let page: Vec<SymbolHistoryEntry> = history.into_iter().map(|(_, entry)| entry).take(limit).collect();
    let budget = crate::engine::budget::apply_budget(page, params.max_tokens);
    let (page, budgeted) = (budget.items, budget.budgeted);
    // The next page starts at the first change this one left out, or past the window when the
    // window held no more changes and the history goes on.
    let next_offset = match offsets.get(page.len()) {
        Some(left_out) => Some(*left_out),
        None => window_exhausted.then_some(start + window),
    };
    // A name the working tree does not define and no commit inspected so far defines either: the
    // last page says it is a mistake in the call, and every page before it says why it is empty.
    let unmatched = match absent_now.filter(|_| !defined) {
        Some(error) if next_offset.is_none() => return Err(error),
        Some(error) => Some(format!(
            "{}. No commit inspected so far defines it either; `next_cursor` looks further back.",
            error
        )),
        None => None,
    };
    let window_edge = window_exhausted && offsets.len() <= page.len();
    let truncated_reason = if window_edge {
        Some("window")
    } else if repo.is_shallow() {
        Some("shallow_clone")
    } else {
        None
    };
    Ok(SymbolHistoryResponse {
        kind: kind.map(|k| kind_to_str(k).to_string()),
        commits_inspected: u32::try_from(inspected).unwrap_or(u32::MAX),
        history: page,
        hash_mode: hash_mode.as_str(),
        unmatched,
        budgeted,
        truncated: truncated_reason.is_some(),
        truncated_reason,
        next_cursor: next_offset.map(|offset| {
            let carried = if defined { MATCHED_EARLIER } else { 0 };
            crate::engine::cursor::Cursor::encode_in_memory(offset as u64 | carried, snapshot)
        }),
        cursor_invalidated: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hunk(text: String) -> crate::git::Hunk {
        crate::git::Hunk {
            kind: crate::git::HunkKind::Added,
            old_line_start: 0,
            old_line_count: 0,
            new_line_start: 1,
            new_line_count: 1,
            text,
        }
    }

    #[test]
    fn an_oversized_hunk_is_clipped_after_its_last_whole_line() {
        let line = "+short line\n";
        assert_ne!(DIFF_TEXT_MAX % line.len(), 0, "the cap must fall inside a line");
        let (views, clipped) = capped_hunks(vec![hunk(line.repeat(DIFF_TEXT_MAX / line.len() + 10))]);
        assert!(clipped);
        let text = &views[0].text;
        assert!(
            text.ends_with('\n'),
            "ends in a fragment: {:?}",
            &text[text.len() - 20..]
        );
        assert_eq!(text.len(), DIFF_TEXT_MAX - DIFF_TEXT_MAX % line.len());
    }

    #[test]
    fn an_oversized_hunk_with_no_newline_is_clipped_at_a_character_boundary() {
        let (views, clipped) = capped_hunks(vec![hunk(format!("+{}", "é".repeat(DIFF_TEXT_MAX)))]);
        assert!(clipped);
        assert_eq!(views[0].text.len(), DIFF_TEXT_MAX - 1, "the cap is mid-character");
        assert!(views[0].text.ends_with('é'));

        let (whole, clipped) = capped_hunks(vec![hunk("+a\n".to_string())]);
        assert!(!clipped);
        assert_eq!(whole[0].text, "+a\n");
    }
}
