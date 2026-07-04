//! Helper body for the `goto_definition` MCP tool — the read surface over the scanner's
//! scope/import-resolved edges (the code-intelligence tier).
//!
//! Resolution facts live in the content-addressed `<hash>.rref.msgpack` blob
//! ([`crate::intel::model::FileResolvedRefs`]), written by the scanner's post-scan resolve pass.
//! `goto_definition` reads the blob (not the Fjall index) because the blob carries the full use
//! *span* (`use_start..use_end`), which lets a caller point at any byte inside the identifier —
//! and because blobs are concurrently readable, the tool answers even in a read-only session that
//! lost the single-holder Fjall lock.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::ServerState;
use super::helpers::json_result;
use super::types::{DefinitionLocation, GotoDefinitionParams, GotoDefinitionResponse};
use crate::intel::model::ResolvedEdge;

/// Body of the `goto_definition` MCP tool. Resolves the reference at `path`:`line`:`column` to its
/// definition via the file's persisted resolution edges. Returns `definition: None` (not an error)
/// when the position holds no resolved binding.
pub(super) async fn run_goto_definition(
    state: &ServerState,
    params: GotoDefinitionParams,
) -> Result<CallToolResult, McpError> {
    // Pull the resolution facts out under a single store guard, then release it before touching
    // the filesystem (mirrors `run_expand`). `read_resolved_by_hex` returns owned data, so nothing
    // borrows the store past this block.
    let refs = {
        let store = state.store.read().await;
        let entry = store.lookup(&params.path).ok_or_else(|| {
            McpError::invalid_params(format!("goto_definition: {} is not indexed", params.path), None)
        })?;
        store
            .read_resolved_by_hex(&entry.hash_hex)
            .map_err(|e| McpError::internal_error(format!("goto_definition: read resolution blob: {e}"), None))?
    };

    // The source is needed both to turn the input line/column into a byte offset and to turn the
    // resolved definition's byte offset back into a line/column for the response.
    let abs = state.root.join(params.path.to_path_buf());
    let source = std::fs::read(&abs)
        .map_err(|e| McpError::invalid_params(format!("goto_definition: read {}: {e}", params.path), None))?;

    let pos = line_col_to_offset(&source, params.line, params.column).ok_or_else(|| {
        McpError::invalid_params(
            format!(
                "goto_definition: position {}:{} is out of range in {}",
                params.line, params.column, params.path
            ),
            None,
        )
    })?;

    let definition = refs
        .as_ref()
        .and_then(|refs| containing_edge(&refs.intra, pos))
        .map(|edge| {
            let (line, column) = offset_to_line_col(&source, edge.def_start);
            DefinitionLocation {
                // Intra-file edges only today: the definition lives in the queried file.
                path: params.path.clone(),
                line,
                column,
                name: slice_span(&source, edge.def_start, edge.def_end),
            }
        });

    let (line, column) = offset_to_line_col(&source, pos);
    json_result(&GotoDefinitionResponse {
        path: params.path,
        line,
        column,
        definition,
    })
}

/// The intra-file edge whose use span contains `pos`. A zero-width span (`use_end == use_start`,
/// as the tree-sitter `locals` engine records today) matches only the exact start byte; a real
/// span (oxc) matches any byte inside the identifier.
fn containing_edge(edges: &[ResolvedEdge], pos: u32) -> Option<&ResolvedEdge> {
    edges
        .iter()
        .find(|e| pos >= e.use_start && pos < e.use_end.max(e.use_start + 1))
}

/// Convert a 1-based line + 0-based byte column into an absolute byte offset. `None` when the line
/// is past end-of-file or the resulting offset would exceed the source length.
fn line_col_to_offset(source: &[u8], line: u32, column: u32) -> Option<u32> {
    if line == 0 {
        return None;
    }
    let mut line_start = 0usize;
    let mut cursor = 0usize;
    let mut current = 1u32;
    while current < line {
        let rel = memchr::memchr(b'\n', &source[cursor..])?;
        cursor += rel + 1;
        line_start = cursor;
        current += 1;
    }
    let offset = line_start + column as usize;
    (offset <= source.len()).then_some(offset as u32)
}

/// Convert an absolute byte offset into a 1-based line + 0-based byte column. Clamps to the source
/// bounds so an out-of-range offset never panics.
fn offset_to_line_col(source: &[u8], offset: u32) -> (u32, u32) {
    let offset = (offset as usize).min(source.len());
    let before = &source[..offset];
    let line = 1 + memchr::memchr_iter(b'\n', before).count() as u32;
    let line_start = memchr::memrchr(b'\n', before).map_or(0, |p| p + 1);
    (line, (offset - line_start) as u32)
}

/// Lossy UTF-8 slice of `source[start..end]`, or empty when the span is zero-width / inverted.
fn slice_span(source: &[u8], start: u32, end: u32) -> String {
    let (start, end) = (start as usize, (end as usize).min(source.len()));
    if end > start {
        String::from_utf8_lossy(&source[start..end]).into_owned()
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_col_to_offset_maps_positions() {
        let src = b"const count = 1;\nreturn count;\n";
        // line 1, col 6 → the `count` in the declaration.
        assert_eq!(line_col_to_offset(src, 1, 6), Some(6));
        // line 2, col 7 → the `count` use (offset of the second `count`).
        let second = (src.iter().position(|&b| b == b'\n').unwrap() + 1 + "return ".len()) as u32;
        assert_eq!(line_col_to_offset(src, 2, 7), Some(second));
        // line past EOF → None.
        assert_eq!(line_col_to_offset(src, 9, 0), None);
    }

    #[test]
    fn offset_to_line_col_roundtrips() {
        let src = b"a\nbb\nccc";
        assert_eq!(offset_to_line_col(src, 0), (1, 0));
        assert_eq!(offset_to_line_col(src, 2), (2, 0));
        assert_eq!(offset_to_line_col(src, 5), (3, 0));
        // Clamps past EOF rather than panicking.
        assert_eq!(offset_to_line_col(src, 999), (3, 3));
    }

    #[test]
    fn containing_edge_respects_spans() {
        let zero_width = ResolvedEdge {
            use_start: 10,
            use_end: 10,
            def_start: 0,
            def_end: 0,
        };
        // Zero-width (locals) span matches only the exact start byte.
        assert!(containing_edge(std::slice::from_ref(&zero_width), 10).is_some());
        assert!(containing_edge(std::slice::from_ref(&zero_width), 11).is_none());

        let spanned = ResolvedEdge {
            use_start: 20,
            use_end: 25,
            def_start: 0,
            def_end: 5,
        };
        // Real (oxc) span matches any byte inside the identifier, not its end.
        assert!(containing_edge(std::slice::from_ref(&spanned), 20).is_some());
        assert!(containing_edge(std::slice::from_ref(&spanned), 24).is_some());
        assert!(containing_edge(std::slice::from_ref(&spanned), 25).is_none());
    }
}
