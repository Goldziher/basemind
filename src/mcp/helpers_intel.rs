//! Helper body for the `goto_definition` MCP tool — the read surface over the scanner's
//! scope/import-resolved edges (the code-intelligence tier).
//!
//! Resolution facts live in the content-addressed `<hash>.rref.msgpack` blob
//! ([`crate::intel::model::FileResolvedRefs`]), written by the scanner's post-scan resolve pass.
//! `goto_definition` reads the blob for the *in-file* edge (the blob carries the full use
//! `use_start..use_end` span, so a caller can point at any byte inside the identifier), then follows
//! a **cross-file** hop through the Fjall `refs_by_path` partition when the in-file definition is
//! itself an import binding that resolves across modules. Blobs are concurrently readable, so the
//! in-file path answers even in a read-only session that lost the single-holder Fjall lock; the
//! cross-file hop reads the index — locally when open, else forwarded to the machine daemon (the
//! sole fjall writer) on a `daemon_writer` serve, so precise cross-file resolution holds there too.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::ServerState;
use super::helpers::json_result;
use super::types::{DefinitionLocation, GotoDefinitionParams, GotoDefinitionResponse};
use crate::intel::model::ResolvedEdge;
use crate::path::RelPath;

/// Body of the `goto_definition` MCP tool. Resolves the reference at `path`:`line`:`column` to its
/// definition, following a cross-file hop when the in-file binding is an import. Returns
/// `definition: None` (not an error) when the position holds no resolved binding.
pub(super) async fn run_goto_definition(
    state: &ServerState,
    params: GotoDefinitionParams,
) -> Result<CallToolResult, McpError> {
    // The `goto_definition` shim delegates immediately, so this IS the first statement of the body.
    let started = std::time::Instant::now();
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

    let resolved: Option<(RelPath, u32)> = {
        let store = state.store.read().await;
        let Some(entry) = store.lookup(&params.path) else {
            return Err(McpError::invalid_params(
                format!("goto_definition: {} is not indexed", params.path),
                None,
            ));
        };
        let refs = store.read_resolved_by_hex(&entry.hash_hex).unwrap_or_else(|error| {
            tracing::debug!(path = %params.path, %error, "goto_definition: resolution blob unreadable — treating as no in-file edge");
            None
        });
        let intra_def = refs
            .as_ref()
            .and_then(|r| containing_edge(&r.intra, pos))
            .map(|e| e.def_start);
        match intra_def {
            Some(def_start) => Some(
                resolve_definition(state, &store, &params.path, def_start)
                    .await
                    .unwrap_or((params.path.clone(), def_start)),
            ),
            None => resolve_definition(state, &store, &params.path, pos).await,
        }
    };

    let definition = resolved
        .and_then(|(def_path, def_start)| definition_location(state, &params.path, &source, def_path, def_start));

    let (line, column) = offset_to_line_col(&source, pos);
    json_result(&GotoDefinitionResponse {
        path: params.path,
        line,
        column,
        definition,
        elapsed_us: super::helpers::elapsed_us(started),
    })
}

/// The definition the use at `(use_path, use_start)` binds to. A `daemon_writer` serve has no open
/// index, so the cross-file hop (`refs_by_path`) forwards to the machine daemon — the sole fjall
/// writer, which holds it; every other serve reads its open index or the intra-file `.rref` blob
/// locally. A daemon-forward failure degrades to `None`: goto then reports no binding for the
/// position, exactly as a genuine unresolved use does, rather than erroring the tool.
async fn resolve_definition(
    state: &ServerState,
    store: &crate::store::Store,
    use_path: &RelPath,
    use_start: u32,
) -> Option<(RelPath, u32)> {
    // `state` drives only the daemon-forward branch below; on a non-comms build there is no daemon.
    #[cfg(not(all(feature = "comms", any(unix, windows))))]
    let _ = state;
    #[cfg(all(feature = "comms", any(unix, windows)))]
    if state.daemon_writer {
        use crate::comms::resolved_proto::{ResolvedRefQuery, ResolvedRefResult};
        let client = match super::helpers_comms::resolve_comms_client(state, None).await {
            Ok(client) => client,
            Err(error) => {
                tracing::debug!(%error, "goto_definition: daemon client unavailable; no cross-file binding");
                return None;
            }
        };
        let query = ResolvedRefQuery::DefinitionOf {
            use_path: use_path.clone(),
            use_start,
        };
        return match client.lock().await.resolved_refs(state.root.clone(), query).await {
            Ok(ResolvedRefResult::Definition(definition)) => definition,
            Ok(_) => None,
            Err(error) => {
                tracing::debug!(%error, "goto_definition: daemon resolved-refs forward failed; no cross-file binding");
                None
            }
        };
    }
    crate::query::definition_of(store, use_path, use_start)
}

/// Build a [`DefinitionLocation`] for a resolved `(def_path, def_start)`. Reuses the already-read
/// `query_source` when the definition is in the queried file; otherwise reads the target file (a
/// cross-file definition). The identifier text is recovered from `def_start` since only the start
/// byte is indexed for cross-file / `locals` edges. Returns `None` if a cross-file target can't be
/// read (e.g. removed between scan and query).
fn definition_location(
    state: &ServerState,
    query_path: &RelPath,
    query_source: &[u8],
    def_path: RelPath,
    def_start: u32,
) -> Option<DefinitionLocation> {
    if &def_path == query_path {
        let (line, column) = offset_to_line_col(query_source, def_start);
        let name = identifier_at(query_source, def_start);
        return Some(DefinitionLocation {
            path: def_path,
            line,
            column,
            name,
        });
    }
    let abs = state.root.join(def_path.to_path_buf());
    let def_source = std::fs::read(&abs).ok()?;
    let (line, column) = offset_to_line_col(&def_source, def_start);
    let name = identifier_at(&def_source, def_start);
    Some(DefinitionLocation {
        path: def_path,
        line,
        column,
        name,
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
/// is past end-of-file or the column runs past the end of that line (so a too-large column can't
/// silently land inside the next line).
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
    let line_end = match memchr::memchr(b'\n', &source[line_start..]) {
        Some(rel) => line_start + rel,
        None => source.len(),
    };
    let offset = line_start + column as usize;
    (offset <= line_end).then_some(offset as u32)
}

/// Convert an absolute byte offset into a 1-based line + 0-based byte column. Clamps to the source
/// bounds so an out-of-range offset never panics.
pub(super) fn offset_to_line_col(source: &[u8], offset: u32) -> (u32, u32) {
    let offset = (offset as usize).min(source.len());
    let before = &source[..offset];
    let line = 1 + memchr::memchr_iter(b'\n', before).count() as u32;
    let line_start = memchr::memrchr(b'\n', before).map_or(0, |p| p + 1);
    (line, (offset - line_start) as u32)
}

/// The identifier token starting at `start` (ASCII/JS identifier bytes, incl. `_` and `$`). Labels a
/// definition whose end span isn't indexed — cross-file `refs_by_path` edges and zero-width `locals`
/// spans both store only the start byte. Empty when `start` is out of range or not an identifier
/// start (non-ASCII identifiers are best-effort: the leading ASCII run is returned).
pub(super) fn identifier_at(source: &[u8], start: u32) -> String {
    let start = start as usize;
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'$';
    if start >= source.len() || !is_ident(source[start]) {
        return String::new();
    }
    let mut end = start;
    while end < source.len() && is_ident(source[end]) {
        end += 1;
    }
    String::from_utf8_lossy(&source[start..end]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_col_to_offset_maps_positions() {
        let src = b"const count = 1;\nreturn count;\n";
        assert_eq!(line_col_to_offset(src, 1, 6), Some(6));
        let second = (src.iter().position(|&b| b == b'\n').unwrap() + 1 + "return ".len()) as u32;
        assert_eq!(line_col_to_offset(src, 2, 7), Some(second));
        assert_eq!(line_col_to_offset(src, 9, 0), None);
        assert_eq!(
            line_col_to_offset(src, 1, 17),
            None,
            "col past EOL must be None, not next line"
        );
        assert_eq!(line_col_to_offset(src, 1, 16), Some(16));
    }

    #[test]
    fn identifier_at_reads_the_token() {
        let src = b"const count = 1;";
        assert_eq!(identifier_at(src, 6), "count");
        assert_eq!(identifier_at(b"let a_1$ = 2;", 4), "a_1$");
        assert_eq!(identifier_at(src, 5), "");
        assert_eq!(identifier_at(src, 999), "");
    }

    #[test]
    fn offset_to_line_col_roundtrips() {
        let src = b"a\nbb\nccc";
        assert_eq!(offset_to_line_col(src, 0), (1, 0));
        assert_eq!(offset_to_line_col(src, 2), (2, 0));
        assert_eq!(offset_to_line_col(src, 5), (3, 0));
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
        assert!(containing_edge(std::slice::from_ref(&zero_width), 10).is_some());
        assert!(containing_edge(std::slice::from_ref(&zero_width), 11).is_none());

        let spanned = ResolvedEdge {
            use_start: 20,
            use_end: 25,
            def_start: 0,
            def_end: 5,
        };
        assert!(containing_edge(std::slice::from_ref(&spanned), 20).is_some());
        assert!(containing_edge(std::slice::from_ref(&spanned), 24).is_some());
        assert!(containing_edge(std::slice::from_ref(&spanned), 25).is_none());
    }
}
