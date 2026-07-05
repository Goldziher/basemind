//! Native Okapi BM25 keyword lane over code chunks (`code-search` feature).
//!
//! A "document" is one [`crate::chunk::CodeChunk`], identified by its content-addressed `chunk_id`
//! (`<source-hash-hex>:<ordinal>`). The scanner tokenizes each chunk's `searchable_text` (symbol +
//! signature + doc + body) and stages the term postings into two Fjall keyspaces via
//! [`crate::index::writer::IndexWriter::upsert_bm25_file`]; this module builds those postings
//! ([`build_chunk_postings`]) and scores queries against them ([`bm25_search`]).
//!
//! ## Scoring
//!
//! Standard Okapi BM25 with the conventional `k1 = 1.2`, `b = 0.75`. Term frequency (`tf`) and
//! document length (`doclen`) are inlined in each posting value, so scoring a query term is a single
//! prefix scan; the posting-list length is that term's document frequency (`df`). The corpus-global
//! `N` (chunk count) and `avgdl` (average document length) come from the `meta`-stored stats that
//! [`crate::index::IndexDb::recompute_bm25_stats`] refreshes at the end of each scan.

use std::cmp::Ordering;

use ahash::{AHashMap, AHashSet};

use crate::chunk::CodeChunk;
use crate::index::{IndexDb, keys};

/// Upper bound on a single token's byte length. Tokens longer than this (minified blobs, base64
/// spilled into a comment) are dropped — they are never real query terms and would bloat the index.
const MAX_TERM_LEN: usize = 80;

/// Okapi BM25 term-frequency saturation parameter. The conventional default.
pub const BM25_K1: f32 = 1.2;
/// Okapi BM25 length-normalization parameter. The conventional default.
pub const BM25_B: f32 = 0.75;

/// One chunk's BM25 posting contribution: its `chunk_id`, document length (total token count), and
/// per-term frequencies. Built by [`build_chunk_postings`] and consumed by the index writer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkPosting {
    /// Content-addressed chunk identity (`<source-hash-hex>:<ordinal>`).
    pub chunk_id: String,
    /// Total number of tokens in the chunk's `searchable_text` (BM25 document length).
    pub doclen: u32,
    /// Distinct `(term, term_frequency)` pairs; every `tf` is `>= 1`.
    pub terms: Vec<(String, u32)>,
}

/// A scored keyword hit: the matching `chunk_id` and its BM25 score (higher is better).
#[derive(Debug, Clone, PartialEq)]
pub struct Bm25Hit {
    pub chunk_id: String,
    pub score: f32,
}

/// Invoke `f` on every lowercased alphanumeric token in `text`. Runs of non-alphanumeric characters
/// separate tokens, so `find_references()` yields `find` + `references` (snake_case splits improve
/// recall). Tokens over [`MAX_TERM_LEN`] bytes are dropped. Unicode-aware via `char::is_alphanumeric`.
fn for_each_token(text: &str, mut f: impl FnMut(&str)) {
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            current.extend(ch.to_lowercase());
        } else if !current.is_empty() {
            if current.len() <= MAX_TERM_LEN {
                f(&current);
            }
            current.clear();
        }
    }
    if !current.is_empty() && current.len() <= MAX_TERM_LEN {
        f(&current);
    }
}

/// Tokenize `text` into `term -> term_frequency` counts (BM25 term frequencies for one document).
fn tokenize_counts(text: &str) -> AHashMap<String, u32> {
    let mut counts: AHashMap<String, u32> = AHashMap::new();
    for_each_token(text, |tok| {
        *counts.entry(tok.to_string()).or_insert(0) += 1;
    });
    counts
}

/// Tokenize a query into its distinct terms (order-independent — each term is scored once).
fn tokenize_query(query: &str) -> Vec<String> {
    let mut seen: AHashSet<String> = AHashSet::new();
    for_each_token(query, |tok| {
        seen.insert(tok.to_string());
    });
    seen.into_iter().collect()
}

/// Build the BM25 postings for a file's chunks. `doclen` is the total token count (with repetition);
/// `terms` are the distinct `(term, tf)` pairs. Called from the scanner's parallel per-file worker.
pub fn build_chunk_postings(chunks: &[CodeChunk]) -> Vec<ChunkPosting> {
    chunks
        .iter()
        .map(|c| {
            let counts = tokenize_counts(&c.searchable_text);
            let doclen: u32 = counts.values().copied().sum();
            let terms: Vec<(String, u32)> = counts.into_iter().collect();
            ChunkPosting {
                chunk_id: c.chunk_id.clone(),
                doclen,
                terms,
            }
        })
        .collect()
}

/// BM25 inverse document frequency for a term appearing in `df` of `n` documents. Uses the
/// `ln(1 + (N - df + 0.5) / (df + 0.5))` form, which stays non-negative for all `df <= N` (unlike
/// the classic form that can go negative for very common terms).
pub fn bm25_idf(n: u64, df: u64) -> f32 {
    let numerator = (n as f32) - (df as f32) + 0.5;
    let denominator = (df as f32) + 0.5;
    (1.0 + numerator / denominator).ln()
}

/// One term's BM25 contribution to a document's score, given the term's `idf`, its frequency `tf` in
/// the document, the document length `doclen`, and the corpus average document length `avgdl`.
pub fn bm25_term_score(tf: u32, doclen: u32, avgdl: f32, idf: f32) -> f32 {
    let tf = tf as f32;
    let norm = 1.0 - BM25_B + BM25_B * (doclen as f32) / avgdl;
    idf * (tf * (BM25_K1 + 1.0)) / (tf + BM25_K1 * norm)
}

/// Score `query` against the BM25 keyword index in `db` and return the top `limit` chunk hits,
/// highest score first (ties broken by `chunk_id` for a stable order). Returns empty when the index
/// is empty, the query has no tokens, or no chunk matches.
pub fn bm25_search(db: &IndexDb, query: &str, limit: usize) -> Vec<Bm25Hit> {
    let terms = tokenize_query(query);
    if terms.is_empty() || limit == 0 {
        return Vec::new();
    }
    let Some((n, total_len)) = db.bm25_stats() else {
        return Vec::new();
    };
    if n == 0 {
        return Vec::new();
    }
    let avgdl = (total_len as f32 / n as f32).max(1.0);

    let mut scores: AHashMap<String, f32> = AHashMap::new();
    for term in &terms {
        // First collect this term's postings so its document frequency (the list length) is known
        // before scoring — BM25's idf needs df up front.
        let mut postings: Vec<(String, u32, u32)> = Vec::new();
        for guard in db.code_bm25_postings.prefix(keys::code_bm25_postings_prefix(term)) {
            let Ok((k, v)) = guard.into_inner() else { continue };
            if let (Some(chunk_id), Some((tf, doclen))) = (
                keys::parse_code_bm25_posting_chunk_id(&k),
                keys::parse_code_bm25_posting_value(&v),
            ) {
                postings.push((chunk_id.to_string(), tf, doclen));
            }
        }
        if postings.is_empty() {
            continue;
        }
        let idf = bm25_idf(n, postings.len() as u64);
        for (chunk_id, tf, doclen) in postings {
            *scores.entry(chunk_id).or_insert(0.0) += bm25_term_score(tf, doclen, avgdl, idf);
        }
    }

    let mut hits: Vec<Bm25Hit> = scores
        .into_iter()
        .map(|(chunk_id, score)| Bm25Hit { chunk_id, score })
        .collect();
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.chunk_id.cmp(&b.chunk_id))
    });
    hits.truncate(limit);
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(chunk_id: &str, searchable_text: &str) -> CodeChunk {
        CodeChunk {
            chunk_id: chunk_id.to_string(),
            path: "src/lib.rs".to_string(),
            lang: "rust".to_string(),
            kind: None,
            symbol: None,
            signature: None,
            doc: None,
            byte_start: 0,
            byte_end: 0,
            line_start: 1,
            line_end: 1,
            text: searchable_text.to_string(),
            searchable_text: searchable_text.to_string(),
        }
    }

    #[test]
    fn tokenizer_splits_snake_case_and_lowercases() {
        let counts = tokenize_counts("fn find_references(Spawn) spawn spawn");
        assert_eq!(counts.get("find"), Some(&1));
        assert_eq!(counts.get("references"), Some(&1));
        assert_eq!(counts.get("spawn"), Some(&3)); // "Spawn" lowercased + two "spawn"
        assert_eq!(counts.get("fn"), Some(&1));
    }

    #[test]
    fn tokenizer_drops_oversized_tokens() {
        let huge = "x".repeat(MAX_TERM_LEN + 1);
        let counts = tokenize_counts(&format!("keep {huge} keep"));
        assert_eq!(counts.get("keep"), Some(&2));
        assert!(counts.get(huge.as_str()).is_none(), "oversized token must be dropped");
    }

    #[test]
    fn build_postings_reports_doclen_as_total_token_count() {
        let postings = build_chunk_postings(&[chunk("h:0", "alpha beta alpha")]);
        assert_eq!(postings.len(), 1);
        assert_eq!(postings[0].chunk_id, "h:0");
        assert_eq!(postings[0].doclen, 3, "three tokens total incl. the repeat");
        let alpha = postings[0].terms.iter().find(|(t, _)| t == "alpha").unwrap();
        assert_eq!(alpha.1, 2, "alpha appears twice");
    }

    #[test]
    fn idf_is_non_negative_even_for_ubiquitous_terms() {
        // A term in every document (df == n) must not produce a negative idf.
        assert!(bm25_idf(100, 100) >= 0.0);
        // Rarer terms score strictly higher than common ones.
        assert!(bm25_idf(100, 1) > bm25_idf(100, 50));
    }

    #[test]
    fn term_score_rewards_higher_tf_and_penalizes_length() {
        let idf = bm25_idf(100, 10);
        // More occurrences of the term → higher score.
        assert!(bm25_term_score(5, 100, 100.0, idf) > bm25_term_score(1, 100, 100.0, idf));
        // Same tf in a longer-than-average document → lower score.
        assert!(bm25_term_score(3, 400, 100.0, idf) < bm25_term_score(3, 50, 100.0, idf));
    }
}
