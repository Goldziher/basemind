//! Exact / symbol lane for hybrid code search (`code-search` feature).
//!
//! basemind's differentiator over a pure vector+keyword stack: a scope-aware symbol index. When a
//! query is a bare identifier (a symbol name), this lane resolves it against the `symbols_by_name`
//! index to the symbols that *define* it, maps each definition to its owning code chunk, and returns
//! those chunk ids ranked exact-name-first. RRF then lets the defining chunk win ties against merely
//! lexical or semantic co-occurrences.
//!
//! This lane reads chunk sidecars (blocking disk I/O) — run it off the async reactor.

use ahash::{AHashMap, AHashSet};

use crate::chunk::{CodeChunk, CodeChunkBlob};
use crate::index::IndexDb;
use crate::path::RelPath;
use crate::store::Store;

/// Is `query` a single identifier token (a bare symbol name) rather than a natural-language phrase?
/// Only identifier-shaped queries fire the exact lane. Requires length ≥ 2, an ASCII letter or `_`
/// first, and only ASCII alphanumerics / `_` throughout (so no whitespace, dots, or punctuation).
/// Non-ASCII-identifier queries simply skip this lane and fall back to the keyword/vector lanes.
pub fn is_identifier_query(query: &str) -> bool {
    let q = query.trim();
    if q.len() < 2 {
        return false;
    }
    let mut bytes = q.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return false;
    }
    bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// The chunk whose byte span most tightly contains `byte_offset` (`byte_start <= offset < byte_end`).
/// Nested symbols (a method inside a class) produce overlapping chunk spans, so the tightest
/// (smallest) containing span is the most specific owner. Returns its `chunk_id`, or `None` when the
/// offset lands in a gap no chunk covers (e.g. a whitespace hole dropped during chunking).
pub fn owning_chunk_id(chunks: &[CodeChunk], byte_offset: u32) -> Option<&str> {
    chunks
        .iter()
        .filter(|c| c.byte_start <= byte_offset && byte_offset < c.byte_end)
        .min_by_key(|c| c.byte_end - c.byte_start)
        .map(|c| c.chunk_id.as_str())
}

/// Resolve an identifier-shaped `query` to the chunk ids owning the matching symbol definitions,
/// ranked exact-name matches ahead of longer prefix matches (`parseConfig` before `parseConfigFile`).
/// Returns an empty vec for non-identifier queries or when nothing resolves. `cap` bounds the symbol
/// scan. Reads chunk sidecars — blocking I/O, so call off the async reactor.
pub fn exact_lane_chunk_ids(store: &Store, db: &IndexDb, query: &str, cap: usize) -> Vec<String> {
    if !is_identifier_query(query) {
        return Vec::new();
    }
    let matches = db.symbols_by_name_lookup(query, cap);
    if matches.is_empty() {
        return Vec::new();
    }
    // Read each file's chunk sidecar at most once per query.
    let mut blob_cache: AHashMap<RelPath, Option<CodeChunkBlob>> = AHashMap::new();
    // Two rank tiers: exact-name matches first, then longer prefix matches. Dedup chunk ids across
    // both (a chunk resolved by an exact match must not reappear in the prefix tier).
    let mut exact: Vec<String> = Vec::new();
    let mut prefix: Vec<String> = Vec::new();
    let mut seen: AHashSet<String> = AHashSet::new();
    for (name, _kind, rel, start_byte) in matches {
        let blob = blob_cache.entry(rel.clone()).or_insert_with(|| {
            store
                .lookup(rel.as_bytes())
                .and_then(|e| store.read_chunks_by_hex(&e.hash_hex).ok().flatten())
        });
        let Some(blob) = blob.as_ref() else {
            continue;
        };
        let Some(chunk_id) = owning_chunk_id(&blob.chunks, start_byte) else {
            continue;
        };
        if !seen.insert(chunk_id.to_string()) {
            continue;
        }
        if name == query {
            exact.push(chunk_id.to_string());
        } else {
            prefix.push(chunk_id.to_string());
        }
    }
    exact.extend(prefix);
    exact
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(chunk_id: &str, byte_start: u32, byte_end: u32) -> CodeChunk {
        CodeChunk {
            chunk_id: chunk_id.to_string(),
            path: "src/lib.rs".to_string(),
            lang: "rust".to_string(),
            kind: None,
            symbol: None,
            signature: None,
            doc: None,
            byte_start,
            byte_end,
            line_start: 1,
            line_end: 1,
            text: String::new(),
            searchable_text: String::new(),
        }
    }

    #[test]
    fn identifier_query_accepts_bare_symbols_rejects_phrases() {
        assert!(is_identifier_query("parseConfig"));
        assert!(is_identifier_query("parse_config"));
        assert!(is_identifier_query("_private"));
        assert!(is_identifier_query("Factory2"));
        // Rejections: phrases, too short, leading digit, punctuation/paths, whitespace.
        assert!(!is_identifier_query("parse config"));
        assert!(!is_identifier_query("a"));
        assert!(!is_identifier_query("2fast"));
        assert!(!is_identifier_query("foo.bar"));
        assert!(!is_identifier_query("find the parser"));
        assert!(!is_identifier_query(""));
    }

    #[test]
    fn owning_chunk_prefers_tightest_containing_span() {
        // A class chunk [0,100) with a nested method chunk [20,40); offset 30 is in both.
        let chunks = [chunk("h:0", 0, 100), chunk("h:1", 20, 40)];
        assert_eq!(
            owning_chunk_id(&chunks, 30),
            Some("h:1"),
            "method span is tighter than class span"
        );
        assert_eq!(
            owning_chunk_id(&chunks, 5),
            Some("h:0"),
            "only the class span covers offset 5"
        );
    }

    #[test]
    fn owning_chunk_none_for_uncovered_offset() {
        let chunks = [chunk("h:0", 0, 10), chunk("h:1", 50, 60)];
        // Offset 30 falls in the gap between the two chunks (a whitespace hole).
        assert_eq!(owning_chunk_id(&chunks, 30), None);
        // Half-open: byte_end is exclusive.
        assert_eq!(owning_chunk_id(&chunks, 10), None);
        assert_eq!(owning_chunk_id(&chunks, 0), Some("h:0"));
    }
}
