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
//! cross-file hop reads the index when the session holds one (a writer, or a session the daemon
//! hosts, which borrows the daemon's handle and checks each hop against its own file map).

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::ServerState;
use super::helpers::json_result;
use super::types::{DefinitionLocation, GotoDefinitionParams, GotoDefinitionResponse};
use crate::intel::model::ResolvedEdge;
use crate::path::RelPath;

/// Body of `code_refs` relation `definition`. Resolves the reference at `path`:`line`:`column` to
/// its definition, following a cross-file hop when the in-file binding is an import. Returns
/// `definition: None` (not an error) when the position holds no resolved binding.
///
/// Resolution edges are byte offsets into the content that was indexed, so the file is read once,
/// bounded, and must still be that content; the same holds for the file a cross-file edge lands in.
///
/// The store is read-locked for the whole call, file reads included: the stamp each file is judged
/// against, the resolution blob and a locally read cross-file edge then come from one scan, never
/// from either side of a rescan that landed in between. A rescan waits for the reads, which are
/// bounded by `scan.max_file_bytes`. A hosted session's hops are checked instead (`definition_of_use`).
pub(super) async fn run_goto_definition(
    state: &ServerState,
    params: GotoDefinitionParams,
) -> Result<CallToolResult, McpError> {
    let max_bytes = state.shared.config.scan.max_file_bytes;
    // The index is consulted before the file is read, so a path that is not indexed cannot be
    // probed for existence or contents through the errors below.
    let store = state.store_read().await;
    let Some(entry) = store.lookup(&params.path) else {
        return Err(super::tool_contract::not_indexed_in(
            &store,
            &state.shared.root,
            "code_refs",
            &params.path,
        ));
    };
    let file = super::tool_contract::indexed_file(state, &store, "code_refs", &params.path)?;
    let source = super::blocking::blocking(|| {
        super::freshness::load_fresh("code_refs", &params.path, &file, entry.into(), max_bytes)
    })?;
    let column = match params.column {
        Some(column) => column,
        None => column_of(&source, &params.path, params.line, &params.name)?,
    };

    let pos = line_col_to_offset(&source, params.line, column).ok_or_else(|| {
        McpError::invalid_params(
            format!(
                "`code_refs`: position {}:{} is out of range in `{}`",
                params.line,
                column + 1,
                params.path
            ),
            None,
        )
    })?;

    let refs = store.read_resolved_by_hex(&entry.hash_hex).unwrap_or_else(|error| {
        tracing::debug!(path = %params.path, %error, "goto_definition: resolution blob unreadable — treating as no in-file edge");
        None
    });
    let resolution_ran = refs.is_some();
    let pass_complete = resolution_ran || resolve_pass_complete(&store);
    let intra_def = refs
        .as_ref()
        .and_then(|r| containing_edge(&r.intra, pos))
        .map(|e| e.def_start);
    let Hop {
        found: resolved,
        refused,
    } = crate::engine::definition::definition_of_use(&store, &params.path, pos, intra_def);

    let definition = match resolved {
        Some((def_path, def_start)) => definition_location(state, &store, &params.path, &source, def_path, def_start)?,
        None => None,
    };

    let (line, column) = offset_to_line_col(&source, pos);
    let resolution = match &definition {
        _ if refused => Some(SNAPSHOT_REFUSED.to_string()),
        Some(_) => None,
        None if !resolution_ran => resolution_gap(Some(&entry.language), pass_complete),
        None => None,
    };
    json_result(&GotoDefinitionResponse {
        path: params.path,
        line,
        column: column + 1,
        definition,
        resolution,
    })
}

/// What to report when a file has no resolution blob. The scan writes one only for a file that
/// produced facts, so its absence means one of three things, each a different answer: the language
/// has no resolution engine, the resolve pass has not finished for this index (`pass_complete` is
/// false: every file lacks facts then), or the pass ran and this file bound nothing.
pub(super) fn resolution_gap(language: Option<&str>, pass_complete: bool) -> Option<String> {
    match language {
        Some(language) if crate::intel::resolve::supports(language) => Some(
            if pass_complete {
                NO_RESOLUTION_FACTS
            } else {
                RESOLUTION_NOT_RUN
            }
            .to_string(),
        ),
        Some(language) => Some(format!("unsupported for {language}")),
        None => Some("unsupported for this language".to_string()),
    }
}

/// `resolution` value for a file of a supported language that produced no resolution facts.
pub(super) const NO_RESOLUTION_FACTS: &str = "no resolution facts for this file";

/// `resolution` value when the file has resolution facts and none of them names the pinned
/// definition (nothing in the file refers to it and it is not exported).
pub(super) const NO_DEFINITION_FACTS: &str = "no resolution facts for this definition";

/// `resolution` value when the index's resolve pass did not run to completion.
pub(super) const RESOLUTION_NOT_RUN: &str =
    "resolution has not finished for this index; a running scan will complete it, otherwise run `index_rescan`";

pub(super) use crate::engine::definition::{Hop, SNAPSHOT_REFUSED, resolve_pass_complete};

/// The 0-based byte column where `name` first occurs as a whole identifier on `line` of `source`
/// (`get` in `budget.pop(get)` is the argument, not the tail of `budget`).
fn column_of(source: &[u8], path: &RelPath, line: u32, name: &str) -> Result<u32, McpError> {
    let text = source
        .split(|byte| *byte == b'\n')
        .nth(line.saturating_sub(1) as usize)
        .ok_or_else(|| McpError::invalid_params(format!("`code_refs`: `{path}` has no line {line}"), None))?;
    let finder = memchr::memmem::Finder::new(name.as_bytes());
    crate::textmatch::find_whole(text, &finder)
        .map(|column| column as u32)
        .ok_or_else(|| {
            McpError::invalid_params(
                format!(
                    "`code_refs`: `{name}` does not occur on line {line} of `{path}`. Pass the 1-based `column` \
                     of the reference, or the line it is on."
                ),
                None,
            )
        })
}

/// Build a [`DefinitionLocation`] for a resolved `(def_path, def_start)`. Reuses the already-read
/// `query_source` when the definition is in the queried file; otherwise reads the target file (a
/// cross-file definition). The identifier text is recovered from `def_start` since only the start
/// byte is indexed for cross-file / `locals` edges.
///
/// `None` when a cross-file target is not an indexed file of this repository. A target that was
/// edited or deleted since the scan is an error: `def_start` is an offset into the indexed bytes.
/// `store` is the guard the edge was resolved under, so the target's stamp is of the same scan.
fn definition_location(
    state: &ServerState,
    store: &crate::store::Store,
    query_path: &RelPath,
    query_source: &[u8],
    def_path: RelPath,
    def_start: u32,
) -> Result<Option<DefinitionLocation>, McpError> {
    let located = |source: &[u8], path: RelPath| {
        let (line, column) = offset_to_line_col(source, def_start);
        DefinitionLocation {
            name: identifier_at(source, def_start),
            path,
            line,
            column: column + 1,
        }
    };
    if &def_path == query_path {
        return Ok(Some(located(query_source, def_path)));
    }
    let target = store.lookup(&def_path).and_then(|entry| {
        let file = super::tool_contract::IndexedFiles::of(&state.shared).indexed_path(&def_path)?;
        Some((file, entry))
    });
    let Some((file, entry)) = target else {
        return Ok(None);
    };
    let max_bytes = state.shared.config.scan.max_file_bytes;
    let def_source = super::blocking::blocking(|| {
        super::freshness::load_fresh("code_refs", &def_path, &file, entry.into(), max_bytes)
    })?;
    Ok(Some(located(&def_source, def_path)))
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
#[path = "helpers_intel_tests.rs"]
mod tests;
