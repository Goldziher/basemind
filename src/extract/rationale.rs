//! Inline rationale extraction (ADR-0009).
//!
//! During L1 extraction the parsed tree is walked once for comment nodes; each comment is
//! classified against a fixed set of rationale markers (`WHY:` / `NOTE:` / `TODO:` / `FIXME:` /
//! `HACK:` / `XXX:` / `SAFETY:`) and scanned for decision-record citations (`ADR-0007`,
//! `RFC-2119`). The resulting [`RationaleRecord`]s populate [`super::FileMapL1::rationale`], which
//! the read-side graph build consumes to construct `Annotates` / `Cites` edges.
//!
//! Performance: marker matching runs against a stack-allocated, ASCII-uppercased head of each
//! comment via `memchr::memmem::Finder`s built exactly once (a `LazyLock` table), and comment-node
//! detection reuses a single `Finder` for the `"comment"` substring — no per-file allocation of the
//! matcher and no re-parse (the caller passes the already-parsed tree root).

use std::sync::LazyLock;

use memchr::memmem::Finder;
use tree_sitter::Node;

use super::{RationaleKind, RationaleRecord};

/// The longest recognized marker (`"RATIONALE:"`, 10 bytes). Bounds the stack buffer used to
/// case-fold the head of each comment for marker matching.
const MAX_MARKER_LEN: usize = 10;

/// Upper bound on the stored note text, in bytes (truncated on a UTF-8 char boundary).
const MAX_TEXT_LEN: usize = 200;

/// A compiled marker matcher: its `memmem::Finder`, byte length, and the kind it denotes.
struct Marker {
    finder: Finder<'static>,
    len: usize,
    kind: RationaleKind,
}

fn marker(needle: &'static [u8], kind: RationaleKind) -> Marker {
    Marker {
        finder: Finder::new(needle),
        len: needle.len(),
        kind,
    }
}

/// The marker table, built once. Needles are uppercase; matching case-folds the comment head so the
/// classification is case-insensitive without per-comment allocation.
static MARKERS: LazyLock<[Marker; 8]> = LazyLock::new(|| {
    [
        marker(b"RATIONALE:", RationaleKind::Why),
        marker(b"WHY:", RationaleKind::Why),
        marker(b"NOTE:", RationaleKind::Note),
        marker(b"TODO:", RationaleKind::Todo),
        marker(b"FIXME:", RationaleKind::Fixme),
        marker(b"HACK:", RationaleKind::Hack),
        marker(b"XXX:", RationaleKind::Hack),
        marker(b"SAFETY:", RationaleKind::Safety),
    ]
});

/// Reusable `Finder` for spotting comment node kinds — any tree-sitter kind whose name contains
/// `"comment"` (covers `line_comment` / `block_comment` / `comment` across grammars).
static COMMENT_KIND: LazyLock<Finder<'static>> = LazyLock::new(|| Finder::new(b"comment"));

/// Walk the parsed tree once for comment nodes and classify each into a [`RationaleRecord`].
///
/// A comment is recorded when it carries a rationale marker OR contains at least one decision-record
/// citation (a bare `// see ADR-7` still yields a `Cites` edge, classified as [`RationaleKind::Note`]).
/// Records are returned in ascending `start_byte` order so downstream consumers see document order.
pub(crate) fn extract_rationale(root: Node, source: &[u8]) -> Vec<RationaleRecord> {
    let comment_finder = &*COMMENT_KIND;
    let mut out: Vec<RationaleRecord> = Vec::new();
    let mut cursor = root.walk();
    let mut stack: Vec<Node> = vec![root];
    while let Some(node) = stack.pop() {
        if comment_finder.find(node.kind().as_bytes()).is_some() {
            if let Some(record) = classify_comment(node, source) {
                out.push(record);
            }
            // Comment nodes carry no child structure we care about — do not descend.
            continue;
        }
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    out.sort_by_key(|record| record.start_byte);
    out
}

/// Classify a single comment node, returning `None` when it carries neither a marker nor a citation.
fn classify_comment(node: Node, source: &[u8]) -> Option<RationaleRecord> {
    let raw = node.utf8_text(source).ok()?;
    let body = strip_comment_delimiters(raw);

    let (kind, note) = match match_marker(body) {
        Some((kind, rest)) => (Some(kind), rest),
        None => (None, body),
    };
    let text = note.trim();
    let citations = extract_citations(text);

    if kind.is_none() && citations.is_empty() {
        return None;
    }

    Some(RationaleRecord {
        kind: kind.unwrap_or(RationaleKind::Note),
        text: cap_text(text),
        start_byte: node.start_byte() as u32,
        citations,
    })
}

/// Strip leading/trailing comment syntax (`//`, `#`, `/*`, `*/`, `--`, `;`, block-continuation `*`)
/// and surrounding whitespace, leaving the comment's textual content.
fn strip_comment_delimiters(raw: &str) -> &str {
    let is_delim = |c: char| matches!(c, '/' | '#' | '*' | '-' | ';');
    let trimmed = raw.trim();
    let trimmed = trimmed.trim_start_matches(is_delim);
    let trimmed = trimmed.trim_end_matches(is_delim);
    trimmed.trim()
}

/// Match a rationale marker at the start of `body` (case-insensitive), returning the kind and the
/// remaining text after the marker. Case-folds only the head into a stack buffer, then probes each
/// marker's prewarmed `Finder` for a position-0 hit.
fn match_marker(body: &str) -> Option<(RationaleKind, &str)> {
    let bytes = body.as_bytes();
    let head_len = bytes.len().min(MAX_MARKER_LEN);
    let mut head = [0u8; MAX_MARKER_LEN];
    for (dst, &src) in head[..head_len].iter_mut().zip(&bytes[..head_len]) {
        *dst = src.to_ascii_uppercase();
    }
    let head = &head[..head_len];
    for m in MARKERS.iter() {
        if m.finder.find(head) == Some(0) {
            return Some((m.kind, &body[m.len..]));
        }
    }
    None
}

/// Find and normalize every ADR/RFC citation in `text`. Recognizes `ADR-1`, `ADR 0001`, `adr-42`,
/// `RFC2119`, `RFC-2119` and normalizes each to `format!("{PREFIX}-{number:04}")`. Duplicates within
/// one note are collapsed; document order of first occurrence is preserved.
fn extract_citations(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut seen: ahash::AHashSet<String> = ahash::AHashSet::new();
    let mut i = 0usize;
    while i + 3 <= bytes.len() {
        let head = &bytes[i..i + 3];
        let prefix = if head.eq_ignore_ascii_case(b"ADR") {
            Some("ADR")
        } else if head.eq_ignore_ascii_case(b"RFC") {
            Some("RFC")
        } else {
            None
        };
        let Some(prefix) = prefix else {
            i += 1;
            continue;
        };
        let boundary_before = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
        // Consume an optional separator: spaces and/or a single hyphen.
        let mut j = i + 3;
        while j < bytes.len() && bytes[j] == b' ' {
            j += 1;
        }
        if j < bytes.len() && bytes[j] == b'-' {
            j += 1;
            while j < bytes.len() && bytes[j] == b' ' {
                j += 1;
            }
        }
        let digits_start = j;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        if boundary_before
            && j > digits_start
            && let Ok(number) = text[digits_start..j].parse::<u32>()
        {
            let id = format!("{prefix}-{number:04}");
            if seen.insert(id.clone()) {
                out.push(id);
            }
            i = j;
            continue;
        }
        i += 1;
    }
    out
}

/// Truncate `text` to at most [`MAX_TEXT_LEN`] bytes on a UTF-8 char boundary and own it.
fn cap_text(text: &str) -> String {
    if text.len() <= MAX_TEXT_LEN {
        return text.to_string();
    }
    let mut end = MAX_TEXT_LEN;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_line_and_block_delimiters() {
        assert_eq!(strip_comment_delimiters("// WHY: keep it"), "WHY: keep it");
        assert_eq!(strip_comment_delimiters("# NOTE: py"), "NOTE: py");
        assert_eq!(strip_comment_delimiters("/* HACK: x */"), "HACK: x");
        assert_eq!(strip_comment_delimiters("-- rationale"), "rationale");
        assert_eq!(strip_comment_delimiters(";; lisp"), "lisp");
    }

    #[test]
    fn match_marker_is_case_insensitive_and_returns_remainder() {
        assert_eq!(
            match_marker("why: lower case"),
            Some((RationaleKind::Why, " lower case"))
        );
        assert_eq!(
            match_marker("RATIONALE: because"),
            Some((RationaleKind::Why, " because"))
        );
        assert_eq!(match_marker("Fixme: bug"), Some((RationaleKind::Fixme, " bug")));
        assert_eq!(match_marker("XXX: shortcut"), Some((RationaleKind::Hack, " shortcut")));
        assert_eq!(match_marker("not a marker"), None);
    }

    #[test]
    fn normalizes_all_citation_forms_and_dedupes() {
        assert_eq!(
            extract_citations("see ADR-1, adr 0001, RFC2119 and RFC-2119"),
            vec!["ADR-0001".to_string(), "RFC-2119".to_string()]
        );
        assert_eq!(extract_citations("no refs here"), Vec::<String>::new());
        // A prefix embedded in a larger word must not match.
        assert_eq!(extract_citations("PADRE-1"), Vec::<String>::new());
    }
}
