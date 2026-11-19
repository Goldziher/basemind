//! Body of `code_read`: one symbol's source body, or an explicit line span of an indexed file.

use crate::engine::error::EngineError;
use crate::engine::files::{indexed_file, not_indexed_in};
use crate::engine::freshness::{self, Unloadable};
use crate::engine::pick::pick_symbol;
use crate::engine::ranges::{self, format_spans, parse_spans};
use crate::engine::types::code::{
    CodeReadParams, CodeReadResult, ExpandParams, ExpandResponse, ReadRangeSpan, ReadRangesResponse, ReadSpanResponse,
    kind_to_str, parse_kind,
};
use crate::engine::workspace::Workspace;
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
     several share the name), {path, line, end_line} (a 1-based inclusive line span) or {path, ranges} \
     (several spans, e.g. \"120-148,310\")";

impl Workspace {
    /// `code_read`: a symbol's source body, or a 1-based inclusive line span of an indexed file.
    pub(crate) fn code_read(&self, p: CodeReadParams) -> Result<CodeReadResult, EngineError> {
        code_read(self, p)
    }
}

fn code_read(ws: &Workspace, p: CodeReadParams) -> Result<CodeReadResult, EngineError> {
    let shape_error = |problem: &str| EngineError::invalid_argument(format!("`code_read`: {problem}; {READ_SHAPES}"));
    if let Some(text) = p.ranges {
        if p.symbol.is_some() || p.line.is_some() || p.end_line.is_some() || p.kind.is_some() {
            return Err(shape_error(
                "`ranges` names every span itself and takes none of `symbol`, `line`, `end_line` or `kind`",
            ));
        }
        let spans = parse_spans(&text)?;
        return read_ranges(ws, &p.path, &spans).map(CodeReadResult::Spans);
    }
    match (p.symbol, p.line) {
        (None, None) => Err(shape_error("neither `symbol` nor `line` was given")),
        (Some(name), line) => {
            if p.end_line.is_some() {
                return Err(shape_error("`end_line` applies only to a line span"));
            }
            run_expand(
                ws,
                ExpandParams {
                    path: p.path,
                    name,
                    kind: p.kind,
                    line,
                },
            )
            .map(CodeReadResult::Symbol)
        }
        (None, Some(line)) => {
            if p.kind.is_some() {
                return Err(shape_error("`kind` applies only to a symbol read"));
            }
            read_span(ws, &p.path, line, p.end_line.unwrap_or(line)).map(CodeReadResult::Span)
        }
    }
}

/// Read lines `line..=end_line` of an indexed file (code or document) from the working tree.
fn read_span(
    ws: &Workspace,
    path: &crate::path::RelPath,
    line: u32,
    end_line: u32,
) -> Result<ReadSpanResponse, EngineError> {
    if line == 0 || end_line < line {
        return Err(EngineError::invalid_argument(format!(
            "`code_read`: lines are 1-based and `end_line` must be >= `line` (got {line}..{end_line})"
        )));
    }
    // Only the store's file map is consulted: a span never waits for the in-RAM map to build.
    let file = indexed_file(ws, &ws.store_read(), "code_read", path)?;
    let span =
        read_lines(&file, line, end_line).map_err(|_| freshness::unloadable("code_read", path, Unloadable::Gone, 0))?;
    let total_lines = span.total_lines;
    if line > total_lines.max(1) {
        return Err(EngineError::invalid_argument(format!(
            "`code_read`: `{path}` has {total_lines} lines; line {line} is out of range"
        )));
    }
    let (body, truncated, last) = cap_body(&span.head, span.bytes, line);
    Ok(ReadSpanResponse {
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

/// Read the sorted, merged `spans` of an indexed file in one pass, the 128 KiB cap applying to all
/// of them together. Spans are returned in order until one passes the cap: that one ends at its last
/// whole line that fits (or is left out when it is not the first and no whole line fits), every
/// later span is left out, and `next_ranges` names what was not returned, to pass back as `ranges`.
fn read_ranges(
    ws: &Workspace,
    path: &crate::path::RelPath,
    spans: &[ranges::LineSpan],
) -> Result<ReadRangesResponse, EngineError> {
    let file = indexed_file(ws, &ws.store_read(), "code_read", path)?;
    let read = read_spans(&file, spans).map_err(|_| freshness::unloadable("code_read", path, Unloadable::Gone, 0))?;
    let total_lines = read.total_lines;
    if let Some(past) = spans.iter().find(|span| span.start > total_lines.max(1)) {
        return Err(EngineError::invalid_argument(format!(
            "`code_read`: `{path}` has {total_lines} lines; line {} is out of range",
            past.start
        )));
    }
    let bytes: usize = read.spans.iter().map(|span| span.bytes).sum();
    let (mut left, mut kept, mut truncated) = (EXPAND_BODY_CAP, 0usize, false);
    let mut out = Vec::with_capacity(spans.len());
    let mut rest = Vec::new();
    for (index, (span, got)) in spans.iter().zip(&read.spans).enumerate() {
        let end = span.end.min(total_lines.max(1));
        if truncated {
            rest.push(ranges::LineSpan { start: span.start, end });
            continue;
        }
        if got.bytes > left {
            truncated = true;
            let window = &got.head[..(left + 1).min(got.head.len())];
            if index > 0 && memchr::memchr(b'\n', window).is_none() {
                rest.push(ranges::LineSpan { start: span.start, end });
                continue;
            }
        }
        let (body, _, last) = cap_body_at(&got.head, got.bytes, span.start, left);
        left = left.saturating_sub(body.len());
        kept += if truncated { body.len() } else { got.bytes };
        if last < end && truncated {
            rest.push(ranges::LineSpan { start: last + 1, end });
        }
        out.push(ReadRangeSpan {
            line: span.start,
            end_line: last,
            body,
        });
    }
    Ok(ReadRangesResponse {
        path: path.to_string(),
        total_lines,
        spans: out,
        bytes,
        truncated,
        truncated_reason: truncated.then_some(SIZE_CAP),
        omitted_bytes: bytes.saturating_sub(kept),
        next_ranges: (!rest.is_empty()).then(|| format_spans(&rest)),
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
fn read_lines(file: &std::path::Path, line: u32, end_line: u32) -> std::io::Result<LineSpan> {
    let mut read = read_spans(
        file,
        &[ranges::LineSpan {
            start: line,
            end: end_line,
        }],
    )?;
    let span = read.spans.pop().unwrap_or_default();
    Ok(LineSpan {
        head: span.head,
        bytes: span.bytes,
        total_lines: read.total_lines,
    })
}

/// One requested span of a file, as [`read_spans`] found it.
#[derive(Default)]
struct SpanBytes {
    /// The span's leading bytes: all of it, or at least as many as the cap leaves it.
    head: Vec<u8>,
    /// Byte length of the whole span, without the last line's terminator.
    bytes: usize,
}

/// The requested spans of a file and its line count.
struct FileSpans {
    spans: Vec<SpanBytes>,
    total_lines: u32,
}

/// Stream `file` once, keeping the head of each of the sorted, disjoint `spans` and counting every
/// line.
///
/// A response carries at most [`EXPAND_BODY_CAP`] bytes of all spans together, so the file is never
/// held whole: ten lines out of a generated file of any size cost its IO and a few kilobytes of RAM.
/// Span `k` keeps up to `EXPAND_BODY_CAP + 1 + k` bytes less what the spans before it kept: the
/// earlier spans each also kept a terminator their body drops, so span `k` always holds one byte
/// more than the cap leaves it, which is what [`cap_body_at`] needs to cut at a whole line.
fn read_spans(file: &std::path::Path, spans: &[ranges::LineSpan]) -> std::io::Result<FileSpans> {
    use std::io::Read;

    let mut reader = std::fs::File::open(file)?;
    let mut chunk = vec![0u8; 64 * 1024];
    let mut out: Vec<SpanBytes> = spans.iter().map(|_| SpanBytes::default()).collect();
    let (mut at, mut kept) = (0usize, 0usize);
    let (mut current, mut newlines) = (1u32, 0u32);
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
            while spans.get(at).is_some_and(|span| current > span.end) {
                at += 1;
            }
            if spans.get(at).is_some_and(|span| span.start <= current) {
                let got = &mut out[at];
                got.bytes += piece.len();
                let room = (EXPAND_BODY_CAP + 1 + at).saturating_sub(kept);
                let take = piece.len().min(room);
                got.head.extend_from_slice(&piece[..take]);
                kept += take;
            }
            if ends_line {
                current = current.saturating_add(1);
                newlines += 1;
            }
            rest = &rest[piece.len()..];
        }
    }
    let total_lines = newlines + u32::from(last_byte.is_some_and(|byte| byte != b'\n'));
    // The last line's terminator is not part of a span: one line is read as `text`, not `text\n`.
    for (span, got) in spans.iter().zip(&mut out) {
        let last = span.end.min(total_lines.max(1));
        if got.bytes > 0 && last <= newlines {
            got.bytes -= 1;
            got.head.truncate(got.bytes.min(got.head.len()));
        }
    }
    Ok(FileSpans {
        spans: out,
        total_lines,
    })
}

/// `head`, the leading bytes of a span of `bytes` bytes that starts on `line`, as a response body.
///
/// A span over [`EXPAND_BODY_CAP`] is cut after the last whole line that fits, so the body never
/// ends in a line fragment or half a character; one line longer than the cap is cut at a character
/// boundary. Returns the body, whether it was cut, and the 1-based line it ends on.
fn cap_body(head: &[u8], bytes: usize, line: u32) -> (String, bool, u32) {
    cap_body_at(head, bytes, line, EXPAND_BODY_CAP)
}

/// [`cap_body`] with a cap of `cap` bytes; `head` holds at least `cap + 1` bytes when `bytes > cap`.
fn cap_body_at(head: &[u8], bytes: usize, line: u32, cap: usize) -> (String, bool, u32) {
    let truncated = bytes > cap;
    let kept = if truncated {
        let window = &head[..cap.min(head.len())];
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
fn run_expand(ws: &Workspace, params: ExpandParams) -> Result<ExpandResponse, EngineError> {
    let kind_filter = params
        .kind
        .as_deref()
        .map(parse_kind)
        .transpose()
        .map_err(|e| EngineError::invalid_argument(format!("`code_read`: invalid kind {:?}: {e}", params.kind)))?;

    let (indexed, entry, file) = {
        let store = ws.store_read();
        let not_indexed = || not_indexed_in(&store, &ws.root, "code_read", &params.path);
        let l1 = query::file_outline(&store, &params.path).map_err(|_| not_indexed())?;
        let entry = store.lookup(&params.path).cloned().ok_or_else(not_indexed)?;
        (l1, entry, indexed_file(ws, &store, "code_read", &params.path)?)
    };
    let max_bytes = ws.config.scan.max_file_bytes;
    // The indexed spans locate a symbol only in the bytes that were indexed. In an edited file the
    // symbol is found by parsing what is there now, never by cutting at the old offsets.
    let live = freshness::load(&file, (&entry).into(), max_bytes)
        .map_err(|reason| freshness::unloadable("code_read", &params.path, reason, max_bytes))?;
    let l1 = if live.fresh {
        std::sync::Arc::new(indexed)
    } else {
        freshness::reparse(ws, "code_read", &params.path, &live.bytes)?
    };
    let (file_bytes, stale) = (live.bytes, !live.fresh);
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

    Ok(ExpandResponse {
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
    })
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
    fn several_spans_are_read_in_one_pass_each_without_its_last_terminator() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("f.txt");
        std::fs::write(&file, b"one\ntwo\nthree\nfour\nfive").expect("write");
        let spans = [
            ranges::LineSpan { start: 1, end: 2 },
            ranges::LineSpan { start: 4, end: 9 },
        ];
        let read = read_spans(&file, &spans).expect("read");
        assert_eq!(read.total_lines, 5);
        let got: Vec<(&[u8], usize)> = read.spans.iter().map(|s| (s.head.as_slice(), s.bytes)).collect();
        assert_eq!(got, [(&b"one\ntwo"[..], 7), (&b"four\nfive"[..], 9)]);
    }

    /// Each span after the first keeps one byte more than the cap leaves it, so the cut can tell a
    /// whole last line from a fragment.
    #[test]
    fn a_later_span_keeps_one_byte_past_what_the_cap_leaves_it() {
        let line = "x".repeat(99) + "\n";
        let contents = line.repeat(2_000);
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("f.txt");
        std::fs::write(&file, &contents).expect("write");
        let spans = [
            ranges::LineSpan { start: 1, end: 1_000 },
            ranges::LineSpan {
                start: 1_500,
                end: 2_000,
            },
        ];
        let read = read_spans(&file, &spans).expect("read");
        let first = read.spans[0].bytes;
        assert_eq!(first, 99_999);
        assert_eq!(read.spans[1].bytes, 50_099);
        assert!(read.spans[1].head.len() > EXPAND_BODY_CAP - first);
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
