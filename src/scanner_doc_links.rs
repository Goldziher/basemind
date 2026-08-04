//! Document→code link production (ADR-0008) for the document scan tier.
//!
//! Split out of `scanner_docs.rs` to keep that module under the 1000-line cap. Derives the raw
//! mentions each document chunk carries — keyword / entity names and repo-relative path citations —
//! and persists them to the LanceDB `doc_links` table. Resolution (name → symbol, path → file) is
//! deferred to the codegraph `documents` build lane; this side stays a deterministic, allocation-lean
//! text scan with no model-based entity linking.

#![cfg(feature = "documents")]

use ahash::AHashSet;
use memchr::memmem;

use crate::config::Config;
use crate::extract::doc::FileMapDoc;
use crate::lance::DocLinkRow;
use crate::scanner_docs::{PendingDocBatch, preset_dim};
use crate::store::Store;

/// Minimum length a token must reach to be considered a path citation — filters single-char noise.
const MIN_PATH_TOKEN_LEN: usize = 3;
/// Maximum length a path-citation token may reach — bounds pathological runs of path-ish characters.
const MAX_PATH_TOKEN_LEN: usize = 200;

/// True when `tok` looks like a repo-relative path citation: made only of path characters and
/// carrying either a `/` separator or a `stem.ext` filename with a 2–8 char alphanumeric extension
/// (e.g. `core.rs`, `src/lib.rs`, `mod_a.py`). Deterministic, allocation-free; resolution to an
/// indexed file happens in the build lane.
fn looks_like_path(tok: &str) -> bool {
    if !(MIN_PATH_TOKEN_LEN..=MAX_PATH_TOKEN_LEN).contains(&tok.len()) {
        return false;
    }
    if !tok
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-'))
    {
        return false;
    }
    let has_slash = tok.contains('/');
    let has_ext = tok.rsplit_once('.').is_some_and(|(stem, ext)| {
        !stem.is_empty() && (2..=8).contains(&ext.len()) && ext.bytes().all(|b| b.is_ascii_alphanumeric())
    });
    (has_slash || has_ext) && tok.bytes().any(|b| b.is_ascii_alphanumeric())
}

/// Split `text` on whitespace + common punctuation and yield the tokens that [`looks_like_path`]
/// accepts, trimming leading/trailing sentence punctuation. Borrows from `text` — no allocation per
/// token.
fn path_tokens(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '`' | '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';' | ':' | '<' | '>' | '|' | '='
            )
    })
    .map(|raw| raw.trim_matches(|c: char| matches!(c, '.' | '!' | '?')))
    .filter(|tok| looks_like_path(tok))
}

/// Derive the persisted document→code links for one extracted document (ADR-0008). For each chunk it
/// emits: a `name` mention for every document-level keyword / entity whose text occurs in that chunk
/// (matched with a reused `memmem` finder), and a `path` mention for every path-like token in the
/// chunk. Deterministic and bounded — keyword / entity counts are capped upstream and each chunk is
/// scanned once. Resolution (name → symbol, path → file) is deferred to the codegraph build lane.
pub(crate) fn doc_links_for(doc: &FileMapDoc, rel: &str, scope: &str) -> Vec<DocLinkRow> {
    let name_finders: Vec<(memmem::Finder<'_>, &str)> = doc
        .keywords
        .iter()
        .map(|k| k.text.as_str())
        .chain(doc.entities.iter().map(|e| e.text.as_str()))
        .filter(|t| !t.is_empty())
        .map(|t| (memmem::Finder::new(t.as_bytes()), t))
        .collect();

    let mut rows: Vec<DocLinkRow> = Vec::new();
    for (idx, chunk) in doc.chunks.iter().enumerate() {
        let chunk_idx = u32::try_from(idx).unwrap_or(u32::MAX);
        let haystack = chunk.text.as_bytes();
        // Dedup within a chunk: a mention repeated in one chunk is one link. `0` = name, `1` = path.
        let mut seen: AHashSet<(u8, &str)> = AHashSet::new();
        for (finder, needle) in &name_finders {
            if finder.find(haystack).is_some() && seen.insert((0, needle)) {
                rows.push(DocLinkRow {
                    scope: scope.to_string(),
                    doc_path: rel.to_string(),
                    chunk_idx,
                    mention_kind: "name".to_string(),
                    mention_value: (*needle).to_string(),
                });
            }
        }
        for tok in path_tokens(&chunk.text) {
            if seen.insert((1, tok)) {
                rows.push(DocLinkRow {
                    scope: scope.to_string(),
                    doc_path: rel.to_string(),
                    chunk_idx,
                    mention_kind: "path".to_string(),
                    mention_value: tok.to_string(),
                });
            }
        }
    }
    rows
}

/// Persist the document→code links (ADR-0008) for every (re)processed document batch. Re-reads each
/// document's already-persisted `.doc.msgpack` blob (mirroring `flush_document_batches`, so no
/// mention payload rides on the metadata-only [`PendingDocBatch`]), derives its links, and writes
/// them under the batch's own doc scope. Opens the LanceStore via the configured preset dim so links
/// persist even when embeddings are disabled — a link is a heuristic over chunk text, not a vector.
pub(crate) fn flush_doc_links(store: &mut Store, config: &Config, batches: &[PendingDocBatch]) {
    if batches.is_empty() {
        return;
    }
    let model = &config.documents.embedding_preset;
    let dim = match preset_dim(model) {
        Ok(dim) => dim,
        Err(error) => {
            tracing::warn!(?error, preset = %model, "doc links: unknown preset; skipping lance write");
            return;
        }
    };
    let lance = match store.lance_or_open(dim, model) {
        Ok(lance) => lance.clone(),
        Err(error) => {
            tracing::warn!(?error, "doc links: open LanceStore failed; skipping");
            return;
        }
    };
    for batch in batches {
        let doc = match store.read_doc_by_hex(&batch.blob_hash) {
            Ok(Some(doc)) => doc,
            Ok(None) => continue,
            Err(error) => {
                tracing::warn!(rel = %batch.rel_path, ?error, "doc links: re-read blob failed; skipping");
                continue;
            }
        };
        let rows = doc_links_for(&doc, &batch.rel_path, &batch.doc_scope);
        if let Err(error) = lance.replace_doc_links(&batch.doc_scope, &batch.rel_path, rows) {
            tracing::warn!(rel = %batch.rel_path, ?error, "doc links: replace failed; doc↔code edges may be stale");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn looks_like_path_accepts_filenames_and_paths_only() {
        assert!(looks_like_path("core.rs"), "bare filename with a 2-char ext");
        assert!(looks_like_path("src/lib.rs"), "slash-separated path");
        assert!(looks_like_path("mod_a.py"), "underscore stem");
        assert!(!looks_like_path("engine"), "a plain word is not a path");
        assert!(!looks_like_path("e.g"), "single-char ext is rejected");
        assert!(!looks_like_path("a/b c"), "embedded space rejects the token");
    }

    #[test]
    fn path_tokens_extracts_citations_from_prose() {
        let toks: Vec<&str> = path_tokens("The engine lives in `core.rs` and src/lib.rs, right?").collect();
        assert_eq!(toks, vec!["core.rs", "src/lib.rs"]);
    }
}
