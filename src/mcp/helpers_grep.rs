//! `run_workspace_grep` helper — kept in its own file so `helpers.rs` stays under the
//! 1000-line cap as the MCP surface grows.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::ServerState;
use super::helpers::{SEARCH_LIMIT_DEFAULT, SEARCH_LIMIT_MAX, json_result};
use super::types::{GrepHit, WorkspaceGrepParams, WorkspaceGrepResponse};

/// Body of the `workspace_grep` MCP tool.
///
/// Iterates over indexed files (bounded by `scan_cap = limit * 8`), reads each as UTF-8, and
/// applies the compiled regex. Non-UTF-8 files are silently skipped. Returns up to `limit` hits
/// with optional 1-line context.
pub(super) fn run_workspace_grep(
    state: &ServerState,
    params: WorkspaceGrepParams,
) -> Result<CallToolResult, McpError> {
    let limit = params
        .limit
        .unwrap_or(SEARCH_LIMIT_DEFAULT)
        .min(SEARCH_LIMIT_MAX) as usize;
    let scan_cap = limit.saturating_mul(8).max(2_000);

    let re = regex::Regex::new(&params.pattern)
        .map_err(|e| McpError::invalid_params(format!("invalid regex: {e}"), None))?;

    let path_finder = params
        .path_contains
        .as_deref()
        .map(|n| memchr::memmem::Finder::new(n.as_bytes()));
    let lang_filter = params.language.as_deref();

    let cache = state.cache.load_full();

    let mut hits: Vec<GrepHit> = Vec::with_capacity(limit.min(64));
    let mut total_matches: u32 = 0;
    let mut total_files_matched: usize = 0;
    let mut truncated = false;
    let mut files_visited: usize = 0;

    for (path, entry) in &cache.by_path {
        // Honour scan_cap: stop iterating files once we've visited enough.
        if files_visited >= scan_cap {
            truncated = true;
            break;
        }

        // Path filter (memchr).
        let path_ok = path_finder
            .as_ref()
            .is_none_or(|f| f.find(path.as_bytes()).is_some());
        if !path_ok {
            continue;
        }

        // Language filter.
        let lang_ok = lang_filter.is_none_or(|l| entry.language == l);
        if !lang_ok {
            continue;
        }

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
                continue;
            }

            // Binary search for the line that contains the match start.
            let match_start = mat.start();
            let line_idx = line_starts
                .partition_point(|&ls| ls <= match_start)
                .saturating_sub(1);
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
        }

        if file_had_match {
            total_files_matched += 1;
        }

        // If we've already hit `limit` hits and have also saturated scan_cap visits,
        // the truncated flag was set at the top of the loop; here we detect the
        // "hits saturated" truncation path.
        if hits.len() >= limit && total_matches > limit as u32 {
            truncated = true;
        }
    }

    json_result(&WorkspaceGrepResponse {
        pattern: params.pattern,
        total_files_matched,
        total_matches,
        truncated,
        hits,
    })
}

/// Extract the content of line `line_idx` from `source`, stripping the trailing
/// `\n` / `\r\n`. Returns an empty string when the line is empty or the index is
/// out of range.
fn extract_line(source: &str, line_starts: &[usize], line_idx: usize) -> String {
    let start = line_starts[line_idx];
    let end = line_starts
        .get(line_idx + 1)
        .copied()
        .unwrap_or(source.len());
    // `end` points past the '\n'; trim trailing CR+LF.
    let raw = &source[start..end];
    raw.trim_end_matches('\n').trim_end_matches('\r').to_owned()
}
