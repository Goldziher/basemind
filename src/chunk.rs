//! Code chunk model + chunker for the semantic code-search tier (`code-search` feature).
//!
//! Chunks are derived from **cached** L1/L2 extraction plus the source bytes — there is no
//! second tree-sitter parse. The chunker produces:
//!
//! 1. one chunk per L1 symbol span `[start_byte, end_byte)`, carrying the symbol's signature
//!    (from L1) and a docstring proximity-joined from an L2 [`DocComment`] that sits immediately
//!    before the symbol, and
//! 2. "gap" chunks over the complement of the union of all symbol spans — module-level code,
//!    inter-symbol regions — computed by pure interval arithmetic over the sorted spans.
//!
//! Oversized chunks are split into overlapping windows using the same `max_characters` /
//! `overlap` knobs the document tier uses. Every chunk carries a content-addressed
//! [`CodeChunk::chunk_id`] (`<source-hash>:<ordinal>`) that is stable across re-scans of
//! identical content.
//!
//! The whole module is gated on `feature = "code-search"` so nothing is dead in a default build.

#![cfg(feature = "code-search")]

use serde::{Deserialize, Serialize};

use crate::extract::{DocComment, FileMapL1, FileMapL2, Symbol, SymbolKind};

/// Chunk-sizing knobs. Mirrors the document tier's `max_characters` / `overlap`.
#[derive(Debug, Clone, Copy)]
pub struct ChunkOptions {
    /// Maximum chunk size in characters. Chunks longer than this are split into
    /// overlapping windows.
    pub max_characters: usize,
    /// Overlap between adjacent split windows, in characters.
    pub overlap: usize,
}

impl Default for ChunkOptions {
    fn default() -> Self {
        Self {
            max_characters: 1500,
            overlap: 200,
        }
    }
}

/// One retrievable unit of source code. Content-addressed, serde-round-trippable, and stored
/// both in the `.chunk.msgpack` sidecar (with its embedding) and — as pointer columns — in the
/// LanceDB `code_chunks` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeChunk {
    /// `<source-hash-hex>:<within-file ordinal>` — stable + content-addressed.
    pub chunk_id: String,
    /// Repository-relative path, forward-slash separated.
    pub path: String,
    /// Tree-sitter language pack name (from L1).
    pub lang: String,
    /// Symbol kind (`function`, `method`, `struct`, …) for a symbol chunk; `module` for a
    /// module-level gap chunk. `None` only for an unclassifiable span.
    pub kind: Option<String>,
    /// The symbol name for a symbol chunk; `None` for a gap chunk.
    pub symbol: Option<String>,
    /// Signature line from the L1 outline, when the symbol carried one.
    pub signature: Option<String>,
    /// Docstring proximity-joined from an L2 doc comment immediately preceding the symbol.
    pub doc: Option<String>,
    pub byte_start: u32,
    pub byte_end: u32,
    /// 1-based inclusive line range.
    pub line_start: u32,
    pub line_end: u32,
    /// The raw chunk text (the source slice).
    pub text: String,
    /// `symbol + signature + doc + text` concatenation — the lexical field a BM25 index will
    /// score in Phase 2. Populated now so the sidecar blob does not need a re-write later.
    pub searchable_text: String,
}

/// Content-addressed sidecar payload: a file's chunks plus their embeddings, keyed by source
/// hash under `<hash>.chunk.msgpack`. This IS the persistent embedding cache — an unchanged
/// file's hash hits this blob and skips re-chunk + re-embed.
///
/// `embeddings[i]` is the vector for `chunks[i]` (parallel arrays). `embeddings` is empty when
/// embeddings were disabled at scan time. `embedding_dim` is `0` in that case.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodeChunkBlob {
    /// Blob schema version — bound to the release minor exactly like the other blob tiers, so a
    /// bump wipes stale-schema chunk blobs on re-extract.
    pub schema_ver: u16,
    pub embedding_dim: u16,
    pub chunks: Vec<CodeChunk>,
    pub embeddings: Vec<Vec<f32>>,
}

/// A chunk before line numbers / ids / searchable_text are finalized.
struct RawChunk {
    kind: Option<&'static str>,
    symbol: Option<String>,
    signature: Option<String>,
    doc: Option<String>,
    byte_start: usize,
    byte_end: usize,
    text: String,
}

/// Map a [`SymbolKind`] to its stable snake_case string (matches the serde discriminant used
/// everywhere else in the codebase).
fn kind_str(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Function => "function",
        SymbolKind::Method => "method",
        SymbolKind::Struct => "struct",
        SymbolKind::Enum => "enum",
        SymbolKind::Class => "class",
        SymbolKind::Interface => "interface",
        SymbolKind::Trait => "trait",
        SymbolKind::Type => "type",
        SymbolKind::Const => "const",
        SymbolKind::Module => "module",
        SymbolKind::Macro => "macro",
        SymbolKind::Impl => "impl",
        SymbolKind::Namespace => "namespace",
        SymbolKind::Getter => "getter",
        SymbolKind::Setter => "setter",
        SymbolKind::Field => "field",
        SymbolKind::Variable => "variable",
        SymbolKind::EnumVariant => "enum_variant",
        SymbolKind::Constructor => "constructor",
        SymbolKind::Decorator => "decorator",
        SymbolKind::Heading => "heading",
        SymbolKind::Unknown => "unknown",
    }
}

/// 1-based line number of a byte offset, via binary search over precomputed newline offsets.
fn line_of(newlines: &[usize], byte: usize) -> u32 {
    (newlines.partition_point(|&n| n < byte) as u32) + 1
}

/// Proximity-join a doc comment onto a symbol: pick the doc whose `end_byte` is the largest
/// value `<= symbol.start_byte` such that the bytes between the doc and the symbol are only
/// whitespace (i.e. the doc sits immediately above the symbol). Returns the doc text.
fn doc_for_symbol(docs: &[DocComment], sym: &Symbol, source: &str) -> Option<String> {
    let start = sym.start_byte as usize;
    let mut best: Option<&DocComment> = None;
    for d in docs {
        let de = d.end_byte as usize;
        if de > start {
            continue;
        }
        // Only whitespace may separate the doc from the symbol.
        let gap = source.get(de..start).unwrap_or("");
        if !gap.chars().all(char::is_whitespace) {
            continue;
        }
        if best.is_none_or(|b| de > b.end_byte as usize) {
            best = Some(d);
        }
    }
    best.map(|d| d.text.clone())
}

/// Merge a set of `[start, end)` intervals into a sorted, non-overlapping list.
fn merge_intervals(mut intervals: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    intervals.sort_unstable_by_key(|&(s, _)| s);
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(intervals.len());
    for (s, e) in intervals {
        match merged.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => merged.push((s, e)),
        }
    }
    merged
}

/// Split an oversized raw chunk into overlapping windows of at most `max_characters` chars.
/// Byte offsets are recomputed from character offsets within the chunk's own text so the
/// windows point back at exact source ranges. Chunks at or below the cap pass through
/// unchanged.
fn split_oversized(chunk: RawChunk, opts: ChunkOptions) -> Vec<RawChunk> {
    let char_count = chunk.text.chars().count();
    if char_count <= opts.max_characters || opts.max_characters == 0 {
        return vec![chunk];
    }
    // Byte offset of each char boundary in the chunk text, plus the final length.
    let mut boundaries: Vec<usize> = chunk.text.char_indices().map(|(i, _)| i).collect();
    boundaries.push(chunk.text.len());

    let step = opts.max_characters.saturating_sub(opts.overlap).max(1);
    let mut out = Vec::new();
    let mut start_char = 0usize;
    loop {
        let end_char = (start_char + opts.max_characters).min(char_count);
        let bs = boundaries[start_char];
        let be = boundaries[end_char];
        let piece = chunk.text.get(bs..be).unwrap_or("").to_string();
        if !piece.trim().is_empty() {
            out.push(RawChunk {
                kind: chunk.kind,
                symbol: chunk.symbol.clone(),
                signature: chunk.signature.clone(),
                doc: chunk.doc.clone(),
                byte_start: chunk.byte_start + bs,
                byte_end: chunk.byte_start + be,
                text: piece,
            });
        }
        if end_char >= char_count {
            break;
        }
        start_char += step;
    }
    out
}

/// Build `searchable_text` from a chunk's fields — the lexical field a BM25 index will score.
fn searchable_text(symbol: Option<&str>, signature: Option<&str>, doc: Option<&str>, text: &str) -> String {
    let mut parts: Vec<&str> = Vec::with_capacity(4);
    if let Some(s) = symbol {
        parts.push(s);
    }
    if let Some(s) = signature {
        parts.push(s);
    }
    if let Some(d) = doc {
        parts.push(d);
    }
    parts.push(text);
    parts.join("\n")
}

/// Chunk one file from its cached L1/L2 + source bytes. `hash_hex` is the source content hash
/// used to make each `chunk_id` content-addressed. Returns chunks in byte order; each carries a
/// stable ordinal.
pub fn chunk_file(
    path: &str,
    hash_hex: &str,
    l1: &FileMapL1,
    l2: Option<&FileMapL2>,
    source: &[u8],
    opts: ChunkOptions,
) -> Vec<CodeChunk> {
    // Source is validated UTF-8 by the scanner before chunking; bail to no chunks otherwise.
    let Ok(source) = std::str::from_utf8(source) else {
        return Vec::new();
    };
    let len = source.len();
    let docs: &[DocComment] = l2.map(|m| m.docs.as_slice()).unwrap_or(&[]);

    let mut raws: Vec<RawChunk> = Vec::new();

    // 1. One chunk per symbol span (nested symbols each get their own chunk).
    let mut intervals: Vec<(usize, usize)> = Vec::with_capacity(l1.symbols.len());
    for sym in &l1.symbols {
        let s = sym.start_byte as usize;
        let e = (sym.end_byte as usize).min(len);
        if s >= e {
            continue;
        }
        intervals.push((s, e));
        let text = source.get(s..e).unwrap_or("").to_string();
        if text.trim().is_empty() {
            continue;
        }
        raws.push(RawChunk {
            kind: Some(kind_str(sym.kind)),
            symbol: Some(sym.name.clone()),
            signature: sym.signature.clone(),
            doc: doc_for_symbol(docs, sym, source),
            byte_start: s,
            byte_end: e,
            text,
        });
    }

    // 2. Gap chunks over the complement of the union of symbol spans.
    let merged = merge_intervals(intervals);
    let mut prev_end = 0usize;
    let push_gap = |gs: usize, ge: usize, raws: &mut Vec<RawChunk>| {
        if gs >= ge {
            return;
        }
        let raw = source.get(gs..ge).unwrap_or("");
        // Tighten to the non-whitespace bounds so a gap of blank lines contributes nothing.
        let leading = raw.len() - raw.trim_start().len();
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return;
        }
        let bs = gs + leading;
        let be = bs + trimmed.len();
        raws.push(RawChunk {
            kind: Some("module"),
            symbol: None,
            signature: None,
            doc: None,
            byte_start: bs,
            byte_end: be,
            text: trimmed.to_string(),
        });
    };
    for (s, e) in merged {
        push_gap(prev_end, s, &mut raws);
        prev_end = e;
    }
    push_gap(prev_end, len, &mut raws);

    // 3. Split oversized chunks.
    let mut split: Vec<RawChunk> = Vec::with_capacity(raws.len());
    for raw in raws {
        split.extend(split_oversized(raw, opts));
    }

    // 4. Finalize: sort by byte order, assign ordinals + content-addressed ids + lines.
    split.sort_by(|a, b| a.byte_start.cmp(&b.byte_start).then(a.byte_end.cmp(&b.byte_end)));
    let newlines: Vec<usize> = memchr::memchr_iter(b'\n', source.as_bytes()).collect();
    split
        .into_iter()
        .enumerate()
        .map(|(ordinal, raw)| {
            let line_start = line_of(&newlines, raw.byte_start);
            let last_byte = raw.byte_end.saturating_sub(1).max(raw.byte_start);
            let line_end = line_of(&newlines, last_byte);
            let searchable = searchable_text(
                raw.symbol.as_deref(),
                raw.signature.as_deref(),
                raw.doc.as_deref(),
                &raw.text,
            );
            CodeChunk {
                chunk_id: format!("{hash_hex}:{ordinal}"),
                path: path.to_string(),
                lang: l1.language.clone(),
                kind: raw.kind.map(str::to_string),
                symbol: raw.symbol,
                signature: raw.signature,
                doc: raw.doc,
                byte_start: raw.byte_start as u32,
                byte_end: raw.byte_end as u32,
                line_start,
                line_end,
                text: raw.text,
                searchable_text: searchable,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::{DocComment, FileMapL1, FileMapL2, Symbol, SymbolKind};

    fn sym(name: &str, kind: SymbolKind, start: u32, end: u32, row: u32, sig: Option<&str>) -> Symbol {
        Symbol {
            name: name.to_string(),
            kind,
            start_byte: start,
            end_byte: end,
            start_row: row,
            start_col: 0,
            signature: sig.map(str::to_string),
            decorators: Vec::new(),
        }
    }

    fn l1_with(symbols: Vec<Symbol>, source: &str) -> FileMapL1 {
        FileMapL1 {
            schema_ver: 0,
            language: "rust".to_string(),
            size_bytes: source.len() as u64,
            had_errors: false,
            error_count: 0,
            symbols,
            imports: Vec::new(),
            implementations: Vec::new(),
        }
    }

    #[test]
    fn should_emit_one_chunk_per_symbol_span() {
        let source = "fn alpha() {\n    1\n}\nfn beta() {\n    2\n}\n";
        let a_start = source.find("fn alpha").unwrap() as u32;
        let a_end = (source.find("}\nfn beta").unwrap() + 1) as u32;
        let b_start = source.find("fn beta").unwrap() as u32;
        let b_end = source.rfind('}').unwrap() as u32 + 1;
        let l1 = l1_with(
            vec![
                sym("alpha", SymbolKind::Function, a_start, a_end, 0, Some("fn alpha()")),
                sym("beta", SymbolKind::Function, b_start, b_end, 3, Some("fn beta()")),
            ],
            source,
        );
        let chunks = chunk_file(
            "a.rs",
            "deadbeef",
            &l1,
            None,
            source.as_bytes(),
            ChunkOptions::default(),
        );
        let symbol_chunks: Vec<&CodeChunk> = chunks.iter().filter(|c| c.symbol.is_some()).collect();
        assert_eq!(symbol_chunks.len(), 2, "one chunk per symbol");
        assert_eq!(symbol_chunks[0].symbol.as_deref(), Some("alpha"));
        assert_eq!(symbol_chunks[0].signature.as_deref(), Some("fn alpha()"));
        assert_eq!(symbol_chunks[0].chunk_id, "deadbeef:0");
        assert!(symbol_chunks[0].text.contains("alpha"));
    }

    #[test]
    fn should_emit_gap_chunk_for_module_level_code() {
        // Module-level statement before any symbol.
        let source = "use std::io;\nstatic X: u32 = 1;\nfn f() {\n    0\n}\n";
        let f_start = source.find("fn f").unwrap() as u32;
        let f_end = source.rfind('}').unwrap() as u32 + 1;
        let l1 = l1_with(vec![sym("f", SymbolKind::Function, f_start, f_end, 2, None)], source);
        let chunks = chunk_file("m.rs", "cafe", &l1, None, source.as_bytes(), ChunkOptions::default());
        let gap = chunks.iter().find(|c| c.symbol.is_none()).expect("a gap chunk exists");
        assert_eq!(gap.kind.as_deref(), Some("module"));
        assert!(
            gap.text.contains("use std::io"),
            "gap captures module-level code: {:?}",
            gap.text
        );
        assert!(gap.line_start >= 1);
    }

    #[test]
    fn should_join_doc_comment_immediately_preceding_symbol() {
        let source = "/// docs for alpha\nfn alpha() {\n    1\n}\n";
        let doc_end = (source.find("\nfn alpha").unwrap()) as u32;
        let a_start = source.find("fn alpha").unwrap() as u32;
        let a_end = source.rfind('}').unwrap() as u32 + 1;
        let l1 = l1_with(
            vec![sym("alpha", SymbolKind::Function, a_start, a_end, 1, None)],
            source,
        );
        let l2 = FileMapL2 {
            schema_ver: 0,
            language: "rust".to_string(),
            calls: Vec::new(),
            docs: vec![DocComment {
                text: "/// docs for alpha".to_string(),
                start_byte: 0,
                end_byte: doc_end,
            }],
        };
        let chunks = chunk_file(
            "d.rs",
            "f00d",
            &l1,
            Some(&l2),
            source.as_bytes(),
            ChunkOptions::default(),
        );
        let alpha = chunks.iter().find(|c| c.symbol.as_deref() == Some("alpha")).unwrap();
        assert_eq!(alpha.doc.as_deref(), Some("/// docs for alpha"));
        assert!(alpha.searchable_text.contains("docs for alpha"));
    }

    #[test]
    fn should_split_oversized_symbol_with_overlap() {
        // Build a symbol whose body is 500 chars; cap at 100, overlap 20.
        let body = "x".repeat(500);
        let source = format!("fn big() {{{body}}}");
        let l1 = l1_with(
            vec![sym("big", SymbolKind::Function, 0, source.len() as u32, 0, None)],
            &source,
        );
        let opts = ChunkOptions {
            max_characters: 100,
            overlap: 20,
        };
        let chunks = chunk_file("big.rs", "abcd", &l1, None, source.as_bytes(), opts);
        assert!(
            chunks.len() > 1,
            "oversized chunk split into multiple pieces: {}",
            chunks.len()
        );
        // Each piece is at most max_characters chars.
        for c in &chunks {
            assert!(
                c.text.chars().count() <= 100,
                "piece exceeds cap: {}",
                c.text.chars().count()
            );
        }
        // Ordinals are contiguous and content-addressed.
        assert_eq!(chunks[0].chunk_id, "abcd:0");
        assert_eq!(chunks[1].chunk_id, "abcd:1");
    }

    #[test]
    fn empty_and_whitespace_only_files_yield_no_chunks() {
        let l1 = l1_with(Vec::new(), "   \n\n  ");
        let chunks = chunk_file("blank.rs", "00", &l1, None, b"   \n\n  ", ChunkOptions::default());
        assert!(chunks.is_empty(), "whitespace-only file has no chunks");
    }
}
