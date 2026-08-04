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

/// Bounds the stack buffer used to case-fold the head of each comment for marker matching. Must be
/// at least the longest marker keyword (`RATIONALE`, 9 bytes); the `(...)` owner tag and colon that
/// may follow are matched over the full body, not the case-folded head.
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

/// The marker table, built once. Needles are the uppercase keyword only (no trailing `:`); the
/// colon — and an optional `(owner)` tag before it — is matched separately by [`marker_tail`] so
/// `TODO(alice):` classifies. Matching case-folds the comment head so classification is
/// case-insensitive without per-comment allocation.
static MARKERS: LazyLock<[Marker; 8]> = LazyLock::new(|| {
    [
        marker(b"RATIONALE", RationaleKind::Why),
        marker(b"WHY", RationaleKind::Why),
        marker(b"NOTE", RationaleKind::Note),
        marker(b"TODO", RationaleKind::Todo),
        marker(b"FIXME", RationaleKind::Fixme),
        marker(b"HACK", RationaleKind::Hack),
        marker(b"XXX", RationaleKind::Hack),
        marker(b"SAFETY", RationaleKind::Safety),
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
    classify_text(raw, node.start_byte() as u32)
}

/// Classify the raw text of a comment span (delimiters included). Multi-line block comments have
/// each line's continuation syntax stripped and the surviving content joined with a single space,
/// so a marker on a `* WHY: ...` body line — the most common `/** */` shape — is still matched and
/// citations on any line are still scanned.
///
/// The single-line case stays fully borrowed (zero heap allocation); only a multi-line comment
/// allocates one joined `String`, and only that path pays it.
fn classify_text(raw: &str, start_byte: u32) -> Option<RationaleRecord> {
    if memchr::memchr(b'\n', raw.as_bytes()).is_none() {
        return record_from_cleaned(clean_line(raw), start_byte);
    }
    let mut joined = String::new();
    for line in raw.lines() {
        let cleaned = clean_line(line);
        if cleaned.is_empty() {
            continue;
        }
        if !joined.is_empty() {
            joined.push(' ');
        }
        joined.push_str(cleaned);
    }
    record_from_cleaned(&joined, start_byte)
}

/// Build a [`RationaleRecord`] from already-cleaned comment content: match a leading marker, then
/// scan the remainder for citations. Returns `None` when neither is present.
fn record_from_cleaned(body: &str, start_byte: u32) -> Option<RationaleRecord> {
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
        start_byte,
        citations,
    })
}

/// Strip one comment line's syntax, leaving its textual content. Leading opener/continuation
/// characters (`/`, `#`, `*`, `-`, `;`, and the inner-doc `!` of `//!` / `/*!`) plus surrounding
/// whitespace are removed; trailing stripping is limited to genuine block-close syntax (`*/` and
/// block-padding `*`) so real content ending in `-`, `;`, `#` … is preserved.
fn clean_line(line: &str) -> &str {
    let leader = |c: char| matches!(c, '/' | '#' | '*' | '-' | ';' | '!');
    let s = line.trim();
    let s = s.trim_start_matches(leader).trim_start();
    let s = s.trim_end();
    let s = s.strip_suffix("*/").map(str::trim_end).unwrap_or(s);
    s.trim_end_matches('*').trim_end()
}

/// Match a rationale marker at the start of `body` (case-insensitive), returning the kind and the
/// remaining text after the marker punctuation. Case-folds only the head into a stack buffer, then
/// probes each marker's prewarmed `Finder` for a position-0 hit; the required `:` — and an optional
/// `(owner)` tag before it, e.g. `TODO(alice):` — is verified by [`marker_tail`] over the full body.
fn match_marker(body: &str) -> Option<(RationaleKind, &str)> {
    let bytes = body.as_bytes();
    let head_len = bytes.len().min(MAX_MARKER_LEN);
    let mut head = [0u8; MAX_MARKER_LEN];
    for (dst, &src) in head[..head_len].iter_mut().zip(&bytes[..head_len]) {
        *dst = src.to_ascii_uppercase();
    }
    let head = &head[..head_len];
    for m in MARKERS.iter() {
        if m.finder.find(head) == Some(0)
            && let Some(rest) = marker_tail(bytes, m.len)
        {
            return Some((m.kind, &body[rest..]));
        }
    }
    None
}

/// After a keyword match of length `keyword_len` at offset 0, verify the marker punctuation: an
/// optional balanced `(...)` owner tag glued to the keyword, then a `:`. Returns the byte offset
/// just past the colon, or `None` when the colon is absent (so `TODOLIST` and `TODO(x) no colon`
/// are not markers).
fn marker_tail(bytes: &[u8], keyword_len: usize) -> Option<usize> {
    let mut pos = keyword_len;
    if pos < bytes.len() && bytes[pos] == b'(' {
        pos += 1;
        let mut depth = 1usize;
        while pos < bytes.len() && depth > 0 {
            match bytes[pos] {
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ => {}
            }
            pos += 1;
        }
        if depth != 0 {
            return None;
        }
    }
    if pos < bytes.len() && bytes[pos] == b':' {
        Some(pos + 1)
    } else {
        None
    }
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
        // The char after the digit run must not be alphanumeric, mirroring `boundary_before`, so
        // `ADR-00071x` does not truncate to `ADR-0071`.
        let boundary_after = j >= bytes.len() || !bytes[j].is_ascii_alphanumeric();
        if boundary_before
            && boundary_after
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
        assert_eq!(clean_line("// WHY: keep it"), "WHY: keep it");
        assert_eq!(clean_line("# NOTE: py"), "NOTE: py");
        assert_eq!(clean_line("/* HACK: x */"), "HACK: x");
        assert_eq!(clean_line("-- rationale"), "rationale");
        assert_eq!(clean_line(";; lisp"), "lisp");
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

    #[test]
    fn match_marker_accepts_optional_owner_tag() {
        assert_eq!(
            match_marker("TODO(alice): refactor"),
            Some((RationaleKind::Todo, " refactor"))
        );
        assert_eq!(
            match_marker("FIXME(bug-123): broken"),
            Some((RationaleKind::Fixme, " broken"))
        );
        // A parenthetical without a following colon is not a marker.
        assert_eq!(match_marker("TODO(alice) no colon"), None);
    }

    #[test]
    fn citation_requires_trailing_boundary() {
        // Trailing alphanumeric after the digit run must reject the whole citation.
        assert_eq!(extract_citations("ADR-00071x"), Vec::<String>::new());
        assert_eq!(extract_citations("ADR-0071"), vec!["ADR-0071".to_string()]);
        // A trailing punctuation boundary still matches.
        assert_eq!(extract_citations("see ADR-7."), vec!["ADR-0007".to_string()]);
    }

    #[test]
    fn clean_line_preserves_trailing_content_punctuation() {
        assert_eq!(clean_line("// use -1 as sentinel -"), "use -1 as sentinel -");
        assert_eq!(clean_line("# TBD;"), "TBD;");
        assert_eq!(clean_line(" * WHY: keep it"), "WHY: keep it");
    }

    #[test]
    fn classify_text_handles_block_and_doc_comment_shapes() {
        let block =
            classify_text("/**\n * WHY: keep the lock tight; see ADR-0001\n */", 0).expect("block comment record");
        assert_eq!(block.kind, RationaleKind::Why);
        assert_eq!(block.text, "keep the lock tight; see ADR-0001");
        assert_eq!(block.citations, vec!["ADR-0001".to_string()]);

        let inner_block = classify_text("/*!\n * NOTE: crate level; RFC-2119\n */", 0).expect("inner block record");
        assert_eq!(inner_block.kind, RationaleKind::Note);
        assert_eq!(inner_block.citations, vec!["RFC-2119".to_string()]);

        let inner_line = classify_text("//! WHY: module rationale", 0).expect("inner line record");
        assert_eq!(inner_line.kind, RationaleKind::Why);
        assert_eq!(inner_line.text, "module rationale");

        let owner = classify_text("// TODO(alice): refactor", 0).expect("owner-tagged record");
        assert_eq!(owner.kind, RationaleKind::Todo);
        assert_eq!(owner.text, "refactor");
    }
}
