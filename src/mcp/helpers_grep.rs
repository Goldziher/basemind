//! `run_workspace_grep` helper — kept in its own file so `helpers.rs` stays under the
//! 1000-line cap as the MCP surface grows.

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::ServerState;
use super::helpers::{SEARCH_LIMIT_DEFAULT, SEARCH_LIMIT_MAX};
use super::types::{GrepHit, WorkspaceGrepParams, WorkspaceGrepResponse};

/// Body of the `workspace_grep` MCP tool.
///
/// Iterates over indexed files (bounded by `scan_cap = limit * 8`), reads each as UTF-8, and
/// applies the compiled regex. Non-UTF-8 files are silently skipped. Returns up to `limit` hits
/// with optional 1-line context. Supports in-memory pagination via `cursor` / `next_cursor`
/// using the same `encode_in_memory(offset, generation)` scheme as `list_files`.
pub(super) fn run_workspace_grep(state: &ServerState, params: WorkspaceGrepParams) -> Result<CallToolResult, McpError> {
    let format = super::toon::ResponseFormat::parse(params.format.as_deref());
    let limit = params.limit.unwrap_or(SEARCH_LIMIT_DEFAULT).min(SEARCH_LIMIT_MAX) as usize;
    let scan_cap = limit.saturating_mul(8).max(2_000);
    let generation = state.cache_generation.load(Ordering::Relaxed);

    // Decode cursor and check snapshot id. Stale cursor → bail with empty page +
    // cursor_invalidated=true so the caller can restart.
    let skip: usize = match params.cursor.as_ref() {
        Some(c) => {
            let (offset, snapshot_id) = c.decode_in_memory()?;
            if snapshot_id != generation {
                return super::toon::format_result(
                    &WorkspaceGrepResponse {
                        pattern: params.pattern,
                        total_files_matched: 0,
                        total_matches: 0,
                        truncated: false,
                        budgeted: false,
                        hits: Vec::new(),
                        next_cursor: None,
                        cursor_invalidated: true,
                    },
                    format,
                );
            }
            offset as usize
        }
        None => 0,
    };

    let re = regex::Regex::new(&params.pattern)
        .map_err(|e| McpError::invalid_params(format!("invalid regex: {e}"), None))?;

    let path_finder = params
        .path_contains
        .as_deref()
        .map(|n| memchr::memmem::Finder::new(n.as_bytes()));
    let lang_filter = params.language.as_deref();

    let cache = state.cache.load_full();

    let mut hits: Vec<GrepHit> = Vec::with_capacity(limit.min(64));
    // Parallel to `hits`: the 1-based `files_seen` index of the file that produced each hit.
    // Lets a token budget re-anchor the cursor to the file of the last KEPT hit so no hit is
    // permanently lost when the budget cuts mid-file (that boundary file is re-scanned).
    let mut hit_file_idx: Vec<usize> = Vec::with_capacity(limit.min(64));
    let mut total_matches: u32 = 0;
    let mut total_files_matched: usize = 0;
    let mut truncated = false;
    let mut files_visited: usize = 0;
    // Track how many files passed filters so far (for cursor offset).
    let mut files_seen: usize = 0;

    'files: for (path, entry) in &cache.by_path {
        // Honour scan_cap: stop iterating files once we've visited enough.
        if files_visited >= scan_cap {
            truncated = true;
            break;
        }

        // Path filter (memchr).
        let path_ok = path_finder.as_ref().is_none_or(|f| f.find(path.as_bytes()).is_some());
        if !path_ok {
            continue;
        }

        // Language filter.
        let lang_ok = lang_filter.is_none_or(|l| entry.language == l);
        if !lang_ok {
            continue;
        }

        // Skip files that were already returned on prior pages.
        if files_seen < skip {
            files_seen += 1;
            continue;
        }
        files_seen += 1;
        files_visited += 1;

        // Read the file from the working tree; skip non-UTF-8 silently.
        let abs = state.root.join(path.to_path_buf());
        let source = match std::fs::read_to_string(&abs) {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(path = %abs.display(), error = %e, "workspace_grep: skipping unreadable file");
                continue;
            }
        };

        // Pre-build line offset table so match→line/col is O(hits), not O(file).
        // `line_starts[i]` = byte offset of the start of line `i` (0-based).
        let line_starts: Vec<usize> = std::iter::once(0)
            .chain(memchr::memchr_iter(b'\n', source.as_bytes()).map(|pos| pos + 1))
            .collect();

        let mut file_had_match = false;

        for mat in re.find_iter(&source) {
            total_matches = total_matches.saturating_add(1);
            file_had_match = true;

            if hits.len() >= limit {
                // Keep counting total_matches, but stop materialising hits.
                // Once limit is saturated, set truncated and stop processing files.
                truncated = true;
                break 'files;
            }

            // Binary search for the line that contains the match start.
            let match_start = mat.start();
            let line_idx = line_starts.partition_point(|&ls| ls <= match_start).saturating_sub(1);
            let line_start_byte = line_starts[line_idx];
            let line_num = (line_idx as u32) + 1; // 1-based
            let column = (match_start - line_start_byte) as u32; // 0-based byte col

            // Context: extract line-string helper (strip trailing '\n'/'\r\n').
            let context_before = if params.include_context && line_idx > 0 {
                Some(extract_line(&source, &line_starts, line_idx - 1))
            } else {
                None
            };
            let context_after = if params.include_context && line_idx + 1 < line_starts.len() {
                Some(extract_line(&source, &line_starts, line_idx + 1))
            } else {
                None
            };

            hits.push(GrepHit {
                path: path.clone(),
                line_num,
                column,
                matched_text: mat.as_str().to_owned(),
                context_before,
                context_after,
            });
            hit_file_idx.push(files_seen);
        }

        if file_had_match {
            total_files_matched += 1;
        }
    }

    // Emit a cursor when hits are saturated and there may be more files to visit.
    // `files_seen` is the position past the last file we processed; the next page
    // skips all files before that index.
    let next_cursor = if truncated {
        Some(super::cursor::Cursor::encode_in_memory(files_seen as u64, generation))
    } else {
        None
    };

    // Apply the token budget over the materialised hits. When it drops trailing hits,
    // re-anchor the cursor to RE-SCAN the file of the last kept hit (offset = that file's
    // index minus one) so no hit is permanently lost — the boundary file may re-emit a few
    // already-returned hits, which is the safe trade-off for a file-granular cursor.
    let budget = super::budget::apply_budget(hits, params.max_tokens);
    let (hits, budgeted, next_cursor) = if budget.budgeted {
        let kept = budget.items.len();
        let resume_offset = hit_file_idx[kept - 1].saturating_sub(1);
        (
            budget.items,
            true,
            Some(super::cursor::Cursor::encode_in_memory(
                resume_offset as u64,
                generation,
            )),
        )
    } else {
        (budget.items, false, next_cursor)
    };

    super::toon::format_result(
        &WorkspaceGrepResponse {
            pattern: params.pattern,
            total_files_matched,
            total_matches,
            truncated,
            budgeted,
            hits,
            next_cursor,
            cursor_invalidated: false,
        },
        format,
    )
}

/// Extract the content of line `line_idx` from `source`, stripping the trailing
/// `\n` / `\r\n`. Returns an empty string when the line is empty or the index is
/// out of range.
fn extract_line(source: &str, line_starts: &[usize], line_idx: usize) -> String {
    let start = line_starts[line_idx];
    let end = line_starts.get(line_idx + 1).copied().unwrap_or(source.len());
    // `end` points past the '\n'; trim trailing CR+LF.
    let raw = &source[start..end];
    raw.trim_end_matches('\n').trim_end_matches('\r').to_owned()
}
