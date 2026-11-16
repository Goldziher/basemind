//! Body of `code_read`: one symbol's source body, or an explicit line span of an indexed file.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::ServerState;
use super::freshness::{self, Unloadable};
use super::helpers::{json_result, kind_to_str, parse_kind};
use super::helpers_pick::pick_symbol;
use super::index_route::blocking;
use super::tool_contract::{indexed_file, not_indexed_in};
use super::types_code::{ExpandParams, ExpandResponse, ReadSpanResponse};
use super::types_tools::CodeReadParams;
use crate::query;

/// Maximum body size returned in a single read (128 KiB).
///
/// Symbols and spans larger than this are cut after the last whole line that fits; the response
/// then says `truncated: true` and its `end_line` is the last line returned. Chosen to comfortably
/// cover any hand-written function while bounding the response against generated code or minified
/// JS.
const EXPAND_BODY_CAP: usize = 128 * 1024;

/// `truncated_reason` of a body cut at [`EXPAND_BODY_CAP`].
const SIZE_CAP: &str = "size_cap";

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
    let span = blocking(|| read_lines(&file, line, end_line))
        .map_err(|_| freshness::unloadable("code_read", path, Unloadable::Gone, 0))?;
    let total_lines = span.total_lines;
    if line > total_lines.max(1) {
        return Err(McpError::invalid_params(
            format!("`code_read`: `{path}` has {total_lines} lines; line {line} is out of range"),
            None,
        ));
    }
    let (body, truncated, last) = cap_body(&span.head, span.bytes, line);
    json_result(&ReadSpanResponse {
        path: path.to_string(),
        line,
        end_line: last,
        total_lines,
        omitted_bytes: span.bytes.saturating_sub(body.len()),
        body,
        bytes: span.bytes,
        truncated,
        truncated_reason: truncated.then_some(SIZE_CAP),
    })
}

/// Lines `line..=end_line` of a file, as [`read_lines`] found them.
struct LineSpan {
    /// The span's leading bytes: all of it, or one byte more than [`EXPAND_BODY_CAP`].
    head: Vec<u8>,
    /// Byte length of the whole span, without the last line's terminator.
    bytes: usize,
    total_lines: u32,
}

/// Stream `file` once, keeping the head of lines `line..=end_line` and counting every line.
///
/// A response carries at most [`EXPAND_BODY_CAP`] bytes of the span, so the file is never held
/// whole: ten lines out of a generated file of any size cost its IO and a few kilobytes of RAM.
fn read_lines(file: &std::path::Path, line: u32, end_line: u32) -> std::io::Result<LineSpan> {
    use std::io::Read;

    let mut reader = std::fs::File::open(file)?;
    let mut chunk = vec![0u8; 64 * 1024];
    let mut head: Vec<u8> = Vec::new();
    let (mut bytes, mut current, mut newlines) = (0usize, 1u32, 0u32);
    let mut last_byte = None;
    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        let mut rest = &chunk[..read];
        last_byte = rest.last().copied();
        while !rest.is_empty() {
            let (piece, ends_line) = match memchr::memchr(b'\n', rest) {
                Some(newline) => (&rest[..=newline], true),
                None => (rest, false),
            };
            if (line..=end_line).contains(&current) {
                bytes += piece.len();
                let room = (EXPAND_BODY_CAP + 1).saturating_sub(head.len());
                head.extend_from_slice(&piece[..piece.len().min(room)]);
            }
            if ends_line {
                current = current.saturating_add(1);
                newlines += 1;
            }
            rest = &rest[piece.len()..];
        }
    }
    let total_lines = newlines + u32::from(last_byte.is_some_and(|byte| byte != b'\n'));
    // The last line's terminator is not part of the span: one line is read as `text`, not `text\n`.
    let last = end_line.min(total_lines.max(1));
    if bytes > 0 && last <= newlines {
        bytes -= 1;
        head.truncate(bytes.min(head.len()));
    }
    Ok(LineSpan {
        head,
        bytes,
        total_lines,
    })
}

/// `head`, the leading bytes of a span of `bytes` bytes that starts on `line`, as a response body.
///
/// A span over [`EXPAND_BODY_CAP`] is cut after the last whole line that fits, so the body never
/// ends in a line fragment or half a character; one line longer than the cap is cut at a character
/// boundary. Returns the body, whether it was cut, and the 1-based line it ends on.
fn cap_body(head: &[u8], bytes: usize, line: u32) -> (String, bool, u32) {
    let truncated = bytes > EXPAND_BODY_CAP;
    let kept = if truncated {
        let window = &head[..EXPAND_BODY_CAP.min(head.len())];
        // The byte after the window decides whether its last line is whole.
        let ends_a_line = head.get(window.len()) == Some(&b'\n');
        match memchr::memrchr(b'\n', window) {
            _ if ends_a_line => window,
            Some(newline) => &window[..newline],
            None => {
                let mut end = window.len();
                while end > 0 && head.get(end).is_some_and(|byte| byte & 0xC0 == 0x80) {
                    end -= 1;
                }
                &window[..end]
            }
        }
    } else {
        head
    };
    let last_line = line + memchr::memchr_iter(b'\n', kept).count() as u32;
    (String::from_utf8_lossy(kept).into_owned(), truncated, last_line)
}

/// Resolve one symbol by `(path, name[, kind][, line])` and return its raw source body. The outline
/// is the indexed one while the file still holds the indexed content, else a fresh parse of the
/// file as it is now.
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

    let (indexed, entry, file) = {
        let store = state.shared.store.read().await;
        let not_indexed = || not_indexed_in(&store, &state.shared.root, "code_read", &params.path);
        let l1 = query::file_outline(&store, &params.path).map_err(|_| not_indexed())?;
        let entry = store.lookup(&params.path).cloned().ok_or_else(not_indexed)?;
        (l1, entry, indexed_file(state, &store, "code_read", &params.path)?)
    };
    let max_bytes = state.shared.config.scan.max_file_bytes;
    // The indexed spans locate a symbol only in the bytes that were indexed. In an edited file the
    // symbol is found by parsing what is there now, never by cutting at the old offsets.
    let (file_bytes, stale, l1) = blocking(|| {
        let live = freshness::load(&file, (&entry).into(), max_bytes)
            .map_err(|reason| freshness::unloadable("code_read", &params.path, reason, max_bytes))?;
        let l1 = if live.fresh {
            std::sync::Arc::new(indexed)
        } else {
            freshness::reparse(state, "code_read", &params.path, &live.bytes)?
        };
        Ok::<_, McpError>((live.bytes, !live.fresh, l1))
    })?;
    let symbol = pick_symbol(
        "code_read",
        &params.path,
        None,
        &l1.symbols,
        Some(&file_bytes),
        &params.name,
        kind_filter,
        params.line,
    )?
    .symbol;

    let start = symbol.start_byte as usize;
    let end = (symbol.end_byte as usize).min(file_bytes.len());
    let raw = file_bytes.get(start..end).unwrap_or(&[]);

    let full_bytes = raw.len();
    let (body, truncated, end_line) = cap_body(raw, full_bytes, symbol.start_row + 1);

    let response = ExpandResponse {
        path: params.path.to_string(),
        name: symbol.name.clone(),
        kind: kind_to_str(symbol.kind).to_string(),
        line: symbol.start_row + 1,
        end_line,
        omitted_bytes: full_bytes.saturating_sub(body.len()),
        body,
        bytes: full_bytes,
        truncated,
        truncated_reason: truncated.then_some(SIZE_CAP),
        stale,
    };

    json_result(&response)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span_of(contents: &[u8], line: u32, end_line: u32) -> (Vec<u8>, usize, u32) {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("f.txt");
        std::fs::write(&file, contents).expect("write");
        let span = read_lines(&file, line, end_line).expect("read");
        (span.head, span.bytes, span.total_lines)
    }

    #[test]
    fn a_span_excludes_its_last_terminator_and_counts_every_line() {
        assert_eq!(span_of(b"one\ntwo\nthree\n", 2, 2), (b"two".to_vec(), 3, 3));
        assert_eq!(
            span_of(b"one\ntwo\nthree\n", 1, 3),
            (b"one\ntwo\nthree".to_vec(), 13, 3)
        );
        assert_eq!(span_of(b"one\ntwo\nthree", 3, 9), (b"three".to_vec(), 5, 3));
        assert_eq!(span_of(b"one\n\nthree\n", 2, 2), (Vec::new(), 0, 3));
        assert_eq!(span_of(b"", 1, 1), (Vec::new(), 0, 0));
        assert_eq!(span_of(b"one\n", 5, 6), (Vec::new(), 0, 1));
    }

    #[test]
    fn a_span_longer_than_the_cap_keeps_only_its_head() {
        let line = "x".repeat(99) + "\n";
        let contents = line.repeat(3_000);
        let (head, bytes, total_lines) = span_of(contents.as_bytes(), 1, 3_000);
        assert_eq!(head.len(), EXPAND_BODY_CAP + 1);
        assert_eq!(bytes, contents.len() - 1);
        assert_eq!(total_lines, 3_000);

        let (body, truncated, last_line) = cap_body(&head, bytes, 1);
        assert!(truncated);
        assert_eq!(
            body.len(),
            (EXPAND_BODY_CAP / 100) * 100 - 1,
            "whole lines, no trailing newline"
        );
        assert_eq!(last_line as usize, EXPAND_BODY_CAP / 100);
    }

    /// The cap can fall exactly between a line and its terminator. That line fits and is kept.
    #[test]
    fn a_line_that_ends_exactly_at_the_cap_is_kept() {
        let contents = "y".repeat(128) + "\n" + &("x".repeat(127) + "\n").repeat(1_100);
        assert_eq!(
            contents.as_bytes()[EXPAND_BODY_CAP],
            b'\n',
            "a terminator sits right after the cap"
        );

        let (head, bytes, _) = span_of(contents.as_bytes(), 1, u32::MAX);
        let (body, truncated, last_line) = cap_body(&head, bytes, 1);
        assert!(truncated);
        assert_eq!(body.len(), EXPAND_BODY_CAP, "the whole window is whole lines");
        assert!(
            body.ends_with(&"x".repeat(127)),
            "the last fitting line is there in full"
        );
        assert_eq!(last_line, 1_024);
        assert_eq!(body.lines().count(), 1_024);
    }

    #[test]
    fn one_line_longer_than_the_cap_is_cut_at_a_character_boundary() {
        let contents = "é".repeat(EXPAND_BODY_CAP);
        let (head, bytes, _) = span_of(contents.as_bytes(), 1, 1);
        let (body, truncated, last_line) = cap_body(&head, bytes, 7);
        assert!(truncated);
        assert_eq!(body.len(), EXPAND_BODY_CAP);
        assert!(body.chars().all(|c| c == 'é'));
        assert_eq!(last_line, 7);

        let shifted = format!("a{contents}");
        let (head, bytes, _) = span_of(shifted.as_bytes(), 1, 1);
        let (body, _, _) = cap_body(&head, bytes, 1);
        assert_eq!(body.len(), EXPAND_BODY_CAP - 1, "the cap falls inside a character");
    }
}
