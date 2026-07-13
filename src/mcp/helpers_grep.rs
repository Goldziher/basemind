//! `run_workspace_grep` helper — kept in its own file so `helpers.rs` stays under the
//! 1000-line cap as the MCP surface grows.

use std::sync::atomic::Ordering;

use memchr::memmem::Finder;
use rayon::prelude::*;
use regex::Regex;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::ServerState;
use super::cursor::Cursor;
use super::helpers::{SEARCH_LIMIT_DEFAULT, SEARCH_LIMIT_MAX};
use super::types::{GrepHit, GrepTruncation, WorkspaceGrepParams, WorkspaceGrepResponse};
use crate::path::RelPath;

/// Upper bound on the file content one `workspace_grep` call may read, summed from the sizes the
/// index already recorded at scan time.
///
/// The bound is on BYTES, not on files visited: grep is a full-corpus linear scan by definition, so
/// a files-visited cap would silently hide every match past the cut — and a rare identifier, which
/// is precisely what one greps for, is exactly what does not live in the first N files. The budget
/// exists only to keep a single call from reading an unbounded workspace (a repo of vendored
/// minified bundles), and it is derived from indexed sizes rather than from a wall-clock deadline so
/// that the cut point is deterministic — a non-deterministic cut would corrupt cursor pagination.
const GREP_BYTE_BUDGET: u64 = 2 * 1024 * 1024 * 1024;

/// Body of the `workspace_grep` MCP tool.
///
/// Scans every indexed file that passes the `path_contains` / `language` filters — the full corpus,
/// in parallel — reads each as UTF-8 (non-UTF-8 and unreadable files are skipped), and applies the
/// compiled regex. `limit` caps returned HITS, never files scanned, so `total_matches` and
/// `total_files_matched` are exact for the scanned window. Supports in-memory pagination via
/// `cursor` / `next_cursor` on the same `encode_in_memory(offset, generation)` scheme as
/// `list_files`.
pub(super) fn run_workspace_grep(
    state: &ServerState,
    params: WorkspaceGrepParams,
    started: std::time::Instant,
) -> Result<CallToolResult, McpError> {
    let format = super::toon::ResponseFormat::parse(params.format.as_deref());
    let limit = params.limit.unwrap_or(SEARCH_LIMIT_DEFAULT).min(SEARCH_LIMIT_MAX) as usize;
    let generation = state.cache_generation.load(Ordering::Relaxed);

    let (skip_files, skip_hits) = match params.cursor.as_ref() {
        Some(c) => {
            let (offset, snapshot_id) = c.decode_in_memory()?;
            if snapshot_id != generation {
                return super::toon::format_result(
                    &WorkspaceGrepResponse {
                        pattern: params.pattern,
                        total_files_matched: 0,
                        total_matches: 0,
                        truncated: false,
                        truncation_reason: None,
                        budgeted: false,
                        hits: Vec::new(),
                        next_cursor: None,
                        cursor_invalidated: true,
                        notice: state.lifecycle_notice(),
                        elapsed_us: super::helpers::elapsed_us(started),
                    },
                    format,
                );
            }
            unpack_cursor(offset)
        }
        None => (0, 0),
    };

    let re = Regex::new(&params.pattern).map_err(|e| McpError::invalid_params(format!("invalid regex: {e}"), None))?;

    // `escape` escapes every meta character, so an escape that changes nothing proves the pattern
    // has none — the regex then matches exactly that byte string and memmem can reject a whole file
    // with one SIMD pass instead of driving the regex engine across it. Anything else (alternation,
    // optional atoms, inline flags) has no soundly extractable required literal here, so it runs
    // unprefiltered rather than risking a false negative — a missed match is the bug this tool had.
    let literal =
        (regex::escape(&params.pattern) == params.pattern).then(|| Finder::new(params.pattern.as_bytes()).into_owned());

    let path_finder = params.path_contains.as_deref().map(|n| Finder::new(n.as_bytes()));
    let lang_filter = params.language.as_deref();

    let cache = state.cache.load_full();

    // `by_path` is a BTreeMap, so the candidate list is in sorted path order on every call. Both the
    // parallel scan below and the cursor offsets depend on that determinism.
    let candidates: Vec<(&RelPath, &crate::extract::FileMapL1)> = cache
        .by_path
        .iter()
        .filter(|(path, entry)| {
            path_finder.as_ref().is_none_or(|f| f.find(path.as_bytes()).is_some())
                && lang_filter.is_none_or(|l| entry.language == l)
        })
        .collect();

    let window = candidates.get(skip_files.min(candidates.len())..).unwrap_or(&[]);
    let (scanned, byte_budget_hit) = apply_byte_budget(window);

    let counts = count_all(state, scanned, &re, literal.as_ref(), skip_hits);

    let total_matches = counts.iter().fold(0u32, |acc, &c| acc.saturating_add(c));
    let total_files_matched = counts.iter().filter(|&&c| c > 0).count();

    let (selected, limit_resume) = select_hits(&counts, limit, skip_files, skip_hits);
    let (hits, hit_cursors) = materialize(state, scanned, &selected, &re, params.include_context, skip_files);

    let (truncated, truncation_reason, next_cursor) = match (limit_resume, byte_budget_hit) {
        (Some(offset), _) => (
            true,
            Some(GrepTruncation::Limit),
            Some(Cursor::encode_in_memory(offset, generation)),
        ),
        (None, true) => (
            true,
            Some(GrepTruncation::ByteBudget),
            Some(Cursor::encode_in_memory(
                pack_cursor(skip_files + scanned.len(), 0),
                generation,
            )),
        ),
        (None, false) => (false, None, None),
    };

    let budget = super::budget::apply_budget(hits, params.max_tokens);
    let (hits, budgeted, next_cursor) = if budget.budgeted {
        // `budgeted` is only ever set when at least one hit was dropped, so `hit_cursors[kept]` is
        // the first dropped hit — resuming exactly there re-emits nothing and loses nothing.
        let kept = budget.items.len();
        let resume = hit_cursors
            .get(kept)
            .map(|&offset| Cursor::encode_in_memory(offset, generation));
        (budget.items, true, resume.or(next_cursor))
    } else {
        (budget.items, false, next_cursor)
    };

    super::toon::format_result(
        &WorkspaceGrepResponse {
            pattern: params.pattern,
            total_files_matched,
            total_matches,
            truncated,
            truncation_reason,
            budgeted,
            hits,
            next_cursor,
            cursor_invalidated: false,
            notice: state.lifecycle_notice(),
            elapsed_us: super::helpers::elapsed_us(started),
        },
        format,
    )
}

/// The in-memory cursor carries one `u64` offset, but grep must resume at a HIT, not at a file: a
/// single file can hold more matches than `limit`. A file-granular cursor would either replay that
/// file's leading hits forever (no forward progress) or drop its tail (silent loss). Packing
/// `(candidate index, hit ordinal within that file)` into the halves of the existing offset makes
/// both impossible without changing the cursor wire shape.
fn pack_cursor(file_idx: usize, hit_ordinal: u32) -> u64 {
    ((file_idx as u64) << 32) | u64::from(hit_ordinal)
}

fn unpack_cursor(offset: u64) -> (usize, u32) {
    ((offset >> 32) as usize, offset as u32)
}

/// Trim the candidate window to the leading run whose indexed sizes fit [`GREP_BYTE_BUDGET`].
/// Returns the slice to scan and whether anything was cut. At least one file always survives, so a
/// paging caller can never stall on a cursor that refuses to advance.
fn apply_byte_budget<'a, 'b>(
    window: &'a [(&'b RelPath, &'b crate::extract::FileMapL1)],
) -> (&'a [(&'b RelPath, &'b crate::extract::FileMapL1)], bool) {
    let mut used: u64 = 0;
    let mut end = window.len();
    for (i, (_, entry)) in window.iter().enumerate() {
        used = used.saturating_add(entry.size_bytes);
        if used > GREP_BYTE_BUDGET {
            end = i.max(1);
            break;
        }
    }
    (&window[..end], end < window.len())
}

/// Count the matches in every scanned file, in parallel. Order-preserving: rayon's `map`/`collect`
/// keeps the input order, which the cursor arithmetic downstream relies on.
///
/// `skip_hits` discounts the matches the caller already consumed inside the file it resumed in, so
/// the totals mean "matches remaining from the cursor position", not "matches in the repo".
fn count_all(
    state: &ServerState,
    scanned: &[(&RelPath, &crate::extract::FileMapL1)],
    re: &Regex,
    literal: Option<&Finder<'static>>,
    skip_hits: u32,
) -> Vec<u32> {
    let mut counts: Vec<u32> = scanned
        .par_iter()
        .map(|(path, _)| match read_indexed(state, path) {
            Some(source) => count_matches(&source, re, literal),
            None => 0,
        })
        .collect();
    if let Some(first) = counts.first_mut() {
        *first = first.saturating_sub(skip_hits);
    }
    counts
}

/// Read an indexed file as UTF-8. Non-UTF-8 and unreadable files (deleted since the scan, permission
/// denied) are skipped rather than failing the whole grep.
fn read_indexed(state: &ServerState, path: &RelPath) -> Option<String> {
    let abs = state.root.join(path.to_path_buf());
    match std::fs::read_to_string(&abs) {
        Ok(source) => Some(source),
        Err(e) => {
            tracing::debug!(path = %abs.display(), error = %e, "workspace_grep: skipping unreadable file");
            None
        }
    }
}

fn count_matches(source: &str, re: &Regex, literal: Option<&Finder<'static>>) -> u32 {
    if let Some(finder) = literal
        && finder.find(source.as_bytes()).is_none()
    {
        return 0;
    }
    re.find_iter(source).count().min(u32::MAX as usize) as u32
}

/// One file's contribution to the page: its index in the scanned window, how many of its matches the
/// cursor already consumed, and how many to emit.
struct Selection {
    file_idx: usize,
    hit_skip: u32,
    take: usize,
}

/// Walk the per-file counts in order and pick the files that fill the page, plus the packed cursor to
/// resume from when `limit` cut the result short.
fn select_hits(counts: &[u32], limit: usize, skip_files: usize, skip_hits: u32) -> (Vec<Selection>, Option<u64>) {
    let mut selected: Vec<Selection> = Vec::new();
    let mut remaining = limit;

    for (file_idx, &count) in counts.iter().enumerate() {
        if count == 0 {
            continue;
        }
        let hit_skip = if file_idx == 0 { skip_hits } else { 0 };
        if remaining == 0 {
            return (selected, Some(pack_cursor(skip_files + file_idx, hit_skip)));
        }
        let take = (count as usize).min(remaining);
        selected.push(Selection {
            file_idx,
            hit_skip,
            take,
        });
        remaining -= take;
        if take < count as usize {
            let consumed = hit_skip.saturating_add(take as u32);
            return (selected, Some(pack_cursor(skip_files + file_idx, consumed)));
        }
    }
    (selected, None)
}

/// Re-read only the files that actually contribute to this page (at most `limit` of them, already
/// warm in the page cache from the counting pass) and build their hits. Flattened in window order,
/// so the page is deterministic and the cursors are monotonic.
///
/// Returns the hits alongside, for each hit, the packed cursor that resumes AT that hit — the token
/// budget uses it to page a dropped tail exactly.
fn materialize(
    state: &ServerState,
    scanned: &[(&RelPath, &crate::extract::FileMapL1)],
    selected: &[Selection],
    re: &Regex,
    include_context: bool,
    skip_files: usize,
) -> (Vec<GrepHit>, Vec<u64>) {
    let per_file: Vec<Vec<(u64, GrepHit)>> = selected
        .par_iter()
        .map(|sel| {
            let (path, _) = scanned[sel.file_idx];
            let Some(source) = read_indexed(state, path) else {
                return Vec::new();
            };
            collect_hits(path, &source, re, sel, include_context, skip_files + sel.file_idx)
        })
        .collect();

    let total: usize = per_file.iter().map(Vec::len).sum();
    let mut hits = Vec::with_capacity(total);
    let mut cursors = Vec::with_capacity(total);
    for file_hits in per_file {
        for (cursor, hit) in file_hits {
            hits.push(hit);
            cursors.push(cursor);
        }
    }
    (hits, cursors)
}

fn collect_hits(
    path: &RelPath,
    source: &str,
    re: &Regex,
    sel: &Selection,
    include_context: bool,
    global_file_idx: usize,
) -> Vec<(u64, GrepHit)> {
    let line_starts: Vec<usize> = std::iter::once(0)
        .chain(memchr::memchr_iter(b'\n', source.as_bytes()).map(|pos| pos + 1))
        .collect();

    let mut out = Vec::with_capacity(sel.take);
    for (ordinal, mat) in re.find_iter(source).enumerate().skip(sel.hit_skip as usize) {
        if out.len() >= sel.take {
            break;
        }
        let match_start = mat.start();
        let line_idx = line_starts.partition_point(|&ls| ls <= match_start).saturating_sub(1);
        let line_start_byte = line_starts[line_idx];

        let context_before = if include_context && line_idx > 0 {
            Some(extract_line(source, &line_starts, line_idx - 1))
        } else {
            None
        };
        let context_after = if include_context && line_idx + 1 < line_starts.len() {
            Some(extract_line(source, &line_starts, line_idx + 1))
        } else {
            None
        };

        out.push((
            pack_cursor(global_file_idx, ordinal as u32),
            GrepHit {
                path: path.clone(),
                line_num: (line_idx as u32) + 1,
                column: (match_start - line_start_byte) as u32,
                matched_text: mat.as_str().to_owned(),
                context_before,
                context_after,
            },
        ));
    }
    out
}

/// Extract the content of line `line_idx` from `source`, stripping the trailing
/// `\n` / `\r\n`. Returns an empty string when the line is empty or the index is
/// out of range.
fn extract_line(source: &str, line_starts: &[usize], line_idx: usize) -> String {
    let start = line_starts[line_idx];
    let end = line_starts.get(line_idx + 1).copied().unwrap_or(source.len());
    let raw = &source[start..end];
    raw.trim_end_matches('\n').trim_end_matches('\r').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packs_and_unpacks_a_file_and_hit_ordinal() {
        assert_eq!(unpack_cursor(pack_cursor(0, 0)), (0, 0));
        assert_eq!(unpack_cursor(pack_cursor(68_291, 7)), (68_291, 7));
        assert_eq!(unpack_cursor(pack_cursor(1, u32::MAX)), (1, u32::MAX));
    }

    #[test]
    fn selects_whole_files_until_the_limit_is_filled() {
        let (selected, resume) = select_hits(&[2, 0, 3], 5, 0, 0);
        assert_eq!(selected.len(), 2, "both matching files fit the limit");
        assert_eq!(selected[0].take, 2);
        assert_eq!(selected[1].file_idx, 2);
        assert_eq!(selected[1].take, 3);
        assert!(resume.is_none(), "nothing left over, so no resume cursor");
    }

    #[test]
    fn resumes_inside_a_file_whose_matches_exceed_the_limit() {
        let (selected, resume) = select_hits(&[10], 4, 0, 0);
        assert_eq!(selected[0].take, 4);
        assert_eq!(
            unpack_cursor(resume.expect("limit cut the file short")),
            (0, 4),
            "resume at the 5th match of file 0 — no replay, no loss"
        );
    }

    #[test]
    fn resume_cursor_skips_the_hits_already_returned_from_the_first_file() {
        let (selected, resume) = select_hits(&[6], 4, 3, 4);
        assert_eq!(selected[0].hit_skip, 4, "the first file resumes past 4 consumed hits");
        assert_eq!(selected[0].take, 4);
        assert_eq!(
            unpack_cursor(resume.expect("still more matches in the file")),
            (3, 8),
            "next page starts at the 9th match of candidate file 3"
        );
    }

    #[test]
    fn a_literal_pattern_is_prefiltered_and_a_metacharacter_pattern_is_not() {
        assert_eq!(regex::escape("OptimizationStatus"), "OptimizationStatus");
        assert_ne!(regex::escape("fn (spawn|block)"), "fn (spawn|block)");
    }

    #[test]
    fn counts_every_match_and_the_prefilter_never_changes_the_count() {
        let re = Regex::new("needle").expect("regex");
        let finder = Finder::new("needle".as_bytes()).into_owned();
        let source = "needle\nhay\nneedle needle\n";
        assert_eq!(count_matches(source, &re, None), 3);
        assert_eq!(count_matches(source, &re, Some(&finder)), 3);
        assert_eq!(count_matches("hay only", &re, Some(&finder)), 0);
    }
}
