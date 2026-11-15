//! Body of `code_read`: one symbol's source body, or an explicit line span of an indexed file.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::ServerState;
use super::helpers::{json_result, kind_to_str, parse_kind};
use super::helpers_pick::pick_symbol;
use super::tool_contract::{indexed_file, not_indexed, not_indexed_in};
use super::types_code::{ExpandParams, ExpandResponse, ReadSpanResponse};
use super::types_tools::CodeReadParams;
use crate::query;

/// Maximum body size returned in a single read (128 KiB).
///
/// Symbols and spans larger than this are truncated at the boundary and `truncated: true` is set in
/// the response. Chosen to comfortably cover any hand-written function while bounding the response
/// against generated code or minified JS.
const EXPAND_BODY_CAP: usize = 128 * 1024;

/// The argument shapes `code_read` accepts, quoted in every shape error.
const READ_SHAPES: &str = "pass {path, symbol} (a symbol's body; add `line`, any line inside it, or `kind` when \
     several share the name) or {path, line, end_line} (a 1-based inclusive line span)";

/// `code_read`: a symbol's source body, or a 1-based inclusive line span of an indexed file.
pub(super) async fn code_read(state: &ServerState, p: CodeReadParams) -> Result<CallToolResult, McpError> {
    let shape_error = |problem: &str| McpError::invalid_params(format!("`code_read`: {problem}; {READ_SHAPES}"), None);
    match (p.symbol, p.line) {
        (None, None) => Err(shape_error("neither `symbol` nor `line` was given")),
        (Some(name), line) => {
            if p.end_line.is_some() {
                return Err(shape_error("`end_line` applies only to a line span"));
            }
            run_expand(
                state,
                ExpandParams {
                    path: p.path,
                    name,
                    kind: p.kind,
                    line,
                },
            )
            .await
        }
        (None, Some(line)) => {
            if p.kind.is_some() {
                return Err(shape_error("`kind` applies only to a symbol read"));
            }
            read_span(state, &p.path, line, p.end_line.unwrap_or(line)).await
        }
    }
}

/// Read lines `line..=end_line` of an indexed file (code or document) from the working tree.
async fn read_span(
    state: &ServerState,
    path: &crate::path::RelPath,
    line: u32,
    end_line: u32,
) -> Result<CallToolResult, McpError> {
    if line == 0 || end_line < line {
        return Err(McpError::invalid_params(
            format!("`code_read`: lines are 1-based and `end_line` must be >= `line` (got {line}..{end_line})"),
            None,
        ));
    }
    state.await_cache_ready().await;
    let file = indexed_file(state, &*state.shared.store.read().await, "code_read", path)?;
    let bytes = std::fs::read(file).map_err(|_| not_indexed(state, "code_read", path))?;
    let newlines: Vec<usize> = memchr::memchr_iter(b'\n', &bytes).collect();
    let total_lines = newlines.len() as u32 + u32::from(bytes.last().is_some_and(|byte| *byte != b'\n'));
    if line > total_lines.max(1) {
        return Err(McpError::invalid_params(
            format!("`code_read`: `{path}` has {total_lines} lines; line {line} is out of range"),
            None,
        ));
    }
    let start = if line == 1 { 0 } else { newlines[line as usize - 2] + 1 };
    let last = end_line.min(total_lines.max(1));
    let end = newlines.get(last as usize - 1).copied().unwrap_or(bytes.len());
    let raw = bytes.get(start..end).unwrap_or(&[]);
    let (body, truncated) = cap_body(raw);
    json_result(&ReadSpanResponse {
        path: path.to_string(),
        line,
        end_line: last,
        total_lines,
        body,
        bytes: raw.len(),
        truncated,
    })
}

/// `raw` as text, cut at [`EXPAND_BODY_CAP`].
fn cap_body(raw: &[u8]) -> (String, bool) {
    let truncated = raw.len() > EXPAND_BODY_CAP;
    let kept = if truncated { &raw[..EXPAND_BODY_CAP] } else { raw };
    (String::from_utf8_lossy(kept).into_owned(), truncated)
}

/// Resolve one symbol by `(path, name[, kind][, line])` from the L1 outline, then return
/// `file_bytes[start_byte..end_byte]`, the raw source body.
///
/// A name that several definitions share is an error listing each with its line span (see
/// [`pick_symbol`]), never a silent first match.
async fn run_expand(state: &ServerState, params: ExpandParams) -> Result<CallToolResult, McpError> {
    let kind_filter = params
        .kind
        .as_deref()
        .map(parse_kind)
        .transpose()
        .map_err(|e| McpError::invalid_params(format!("`code_read`: invalid kind {:?}: {e}", params.kind), None))?;

    let (l1, file) = {
        let store = state.shared.store.read().await;
        let l1 = query::file_outline(&store, &params.path)
            .map_err(|_| not_indexed_in(&store, &state.shared.root, "code_read", &params.path))?;
        (l1, indexed_file(state, &store, "code_read", &params.path)?)
    };
    let file_bytes = std::fs::read(file)
        .map_err(|e| McpError::invalid_params(format!("`code_read`: read {}: {e}", params.path), None))?;
    // Byte spans from the outline only line up with the bytes that were indexed.
    let source = (file_bytes.len() as u64 == l1.size_bytes).then_some(file_bytes.as_slice());
    let symbol = pick_symbol(
        "code_read",
        &params.path,
        &l1.symbols,
        source,
        &params.name,
        kind_filter,
        params.line,
    )?
    .symbol;

    let start = symbol.start_byte as usize;
    let end = (symbol.end_byte as usize).min(file_bytes.len());
    let raw = file_bytes.get(start..end).unwrap_or(&[]);

    let end_row = {
        let slice_for_count = file_bytes.get(..end).unwrap_or(&[]);
        slice_for_count.iter().filter(|&&b| b == b'\n').count() as u32
    };

    let full_bytes = raw.len();
    let (body, truncated) = cap_body(raw);

    let response = ExpandResponse {
        path: params.path.to_string(),
        name: symbol.name.clone(),
        kind: kind_to_str(symbol.kind).to_string(),
        line: symbol.start_row + 1,
        end_line: (end_row + 1).max(symbol.start_row + 1),
        body,
        bytes: full_bytes,
        truncated,
    };

    json_result(&response)
}
