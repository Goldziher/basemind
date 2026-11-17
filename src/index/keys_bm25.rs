//! Keys of the BM25 keyword lane (ADR-0028 rule 2).
//!
//! A posting is shared by every view of a repository group that holds the same chunk: it is keyed
//! by term, then by a 16-byte [`Bm25Key`] of the content and everything else that shapes its chunks
//! (language, extract epoch, chunk settings), then by the chunk's ordinal. Two worktrees with
//! different `[code_search]` chunk settings therefore never overwrite each other's postings.
//!
//! Each view keeps its own forward rows in `code_bm25_by_path`, keyed by path: they say which
//! chunks the view holds (the view's live keys, read through [`super::bm25_live`]), give the view
//! its own BM25 totals, make a re-scan's delete O(prefix), and name the postings a retired chunk
//! leaves behind.

use super::keys::{read_len_prefixed_ref, write_len_prefixed};
use crate::path::RelPath;

/// Bytes of a [`Bm25Key`].
pub const BM25_KEY_LEN: usize = 16;

/// The key a file's BM25 rows are written under. See the module docs.
pub type Bm25Key = [u8; BM25_KEY_LEN];

/// The [`Bm25Key`] of a content: 16 bytes of blake3 over the content hash, the language its chunks
/// were cut for, the extract epoch of the outline they were cut from and the chunk-settings digest
/// ([`crate::config::rules::code_digest`]), each length-prefixed.
pub fn bm25_content_key(hash_hex: &str, language: &str, chunk_digest: &str) -> Bm25Key {
    let mut input = Vec::with_capacity(32 + hash_hex.len() + language.len() + chunk_digest.len());
    input.extend_from_slice(b"basemind/bm25_key/1\0");
    input.extend_from_slice(&crate::extract::EXTRACT_EPOCH.to_be_bytes());
    for part in [hash_hex, language, chunk_digest] {
        input.extend_from_slice(&u32::try_from(part.len()).unwrap_or(u32::MAX).to_be_bytes());
        input.extend_from_slice(part.as_bytes());
    }
    let digest = crate::hashing::hash_bytes(&input);
    let mut key = [0u8; BM25_KEY_LEN];
    key.copy_from_slice(&digest[..BM25_KEY_LEN]);
    key
}

/// The content hash and ordinal of a chunk id (`<hash>:<ordinal>`).
pub fn split_chunk_id(chunk_id: &str) -> Option<(&str, u32)> {
    let (hash, ordinal) = chunk_id.rsplit_once(':')?;
    Some((hash, ordinal.parse().ok()?))
}

/// `code_bm25_postings`: `u16:len(term) ‖ term ‖ key:16 ‖ ordinal:u32_be`.
///
/// Returns `None` when `term` exceeds 65535 bytes (the tokenizer caps terms far below it; the guard
/// skips a pathological token rather than panicking inside a rayon `par_iter`).
pub fn code_bm25_posting(term: &str, key: &Bm25Key, ordinal: u32) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(2 + term.len() + BM25_KEY_LEN + 4);
    write_len_prefixed(&mut out, term.as_bytes())?;
    out.extend_from_slice(key);
    out.extend_from_slice(&ordinal.to_be_bytes());
    Some(out)
}

/// Prefix bytes for "every chunk containing `term`".
pub fn code_bm25_postings_prefix(term: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + term.len());
    let _ = write_len_prefixed(&mut out, term.as_bytes());
    out
}

/// Decode `(key, ordinal)` from a `code_bm25_postings` key, skipping the term without allocating.
pub fn parse_code_bm25_posting_chunk(key: &[u8]) -> Option<(Bm25Key, u32)> {
    let mut c = 0;
    read_len_prefixed_ref(key, &mut c)?;
    chunk_tail(key.get(c..)?)
}

/// Decode `(term, key, ordinal)` from a `code_bm25_postings` key.
pub fn parse_code_bm25_posting(key: &[u8]) -> Option<(String, Bm25Key, u32)> {
    let mut c = 0;
    let term = std::str::from_utf8(read_len_prefixed_ref(key, &mut c)?)
        .ok()?
        .to_owned();
    let (content, ordinal) = chunk_tail(key.get(c..)?)?;
    Some((term, content, ordinal))
}

/// `key:16 ‖ ordinal:u32_be`, exactly.
fn chunk_tail(tail: &[u8]) -> Option<(Bm25Key, u32)> {
    if tail.len() != BM25_KEY_LEN + 4 {
        return None;
    }
    let key: Bm25Key = tail[..BM25_KEY_LEN].try_into().ok()?;
    let ordinal = u32::from_be_bytes(tail[BM25_KEY_LEN..].try_into().ok()?);
    Some((key, ordinal))
}

/// Encode a `code_bm25_postings` value: `tf:u32_be ‖ doclen:u32_be`, so one term-prefix scan
/// carries everything the scorer needs.
pub fn code_bm25_posting_value(tf: u32, doclen: u32) -> [u8; 8] {
    let mut out = [0u8; 8];
    out[..4].copy_from_slice(&tf.to_be_bytes());
    out[4..].copy_from_slice(&doclen.to_be_bytes());
    out
}

/// Decode `(tf, doclen)` from a `code_bm25_postings` value.
pub fn parse_code_bm25_posting_value(value: &[u8]) -> Option<(u32, u32)> {
    if value.len() < 8 {
        return None;
    }
    let tf = u32::from_be_bytes([value[0], value[1], value[2], value[3]]);
    let doclen = u32::from_be_bytes([value[4], value[5], value[6], value[7]]);
    Some((tf, doclen))
}

/// `code_bm25_by_path`: `u16:len(rel) ‖ rel ‖ key:16 ‖ ordinal:u32_be`, one row per chunk the view
/// holds at `rel`.
pub fn code_bm25_by_path(rel: &RelPath, key: &Bm25Key, ordinal: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + rel.as_bytes().len() + BM25_KEY_LEN + 4);
    let _ = write_len_prefixed(&mut out, rel.as_bytes());
    out.extend_from_slice(key);
    out.extend_from_slice(&ordinal.to_be_bytes());
    out
}

/// Prefix bytes for "every BM25 chunk row of this file".
pub fn code_bm25_by_path_prefix(rel: &RelPath) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + rel.as_bytes().len());
    let _ = write_len_prefixed(&mut out, rel.as_bytes());
    out
}

/// Decode `(rel, key, ordinal)` from a `code_bm25_by_path` key.
pub fn parse_code_bm25_by_path(key: &[u8]) -> Option<(RelPath, Bm25Key, u32)> {
    let mut c = 0;
    let rel = read_len_prefixed_ref(key, &mut c)?;
    let (content, ordinal) = chunk_tail(key.get(c..)?)?;
    Some((RelPath::from(rel), content, ordinal))
}

/// Encode a `code_bm25_by_path` value: `doclen:u32_be ‖ u8:len(hash) ‖ content hash (hex) ‖
/// msgpack(terms)`. The hash turns the view's live keys back into chunk ids, the doclen feeds the
/// view's BM25 totals, and the terms name the chunk's postings for their retirement.
pub fn code_bm25_by_path_value(doclen: u32, hash_hex: &str, terms_msgpack: &[u8]) -> Option<Vec<u8>> {
    let hash_len = u8::try_from(hash_hex.len()).ok()?;
    let mut out = Vec::with_capacity(5 + hash_hex.len() + terms_msgpack.len());
    out.extend_from_slice(&doclen.to_be_bytes());
    out.push(hash_len);
    out.extend_from_slice(hash_hex.as_bytes());
    out.extend_from_slice(terms_msgpack);
    Some(out)
}

/// Decode `(doclen, content hash, msgpack(terms))` from a `code_bm25_by_path` value.
pub fn parse_code_bm25_by_path_value(value: &[u8]) -> Option<(u32, &str, &[u8])> {
    let doclen = u32::from_be_bytes(value.get(..4)?.try_into().ok()?);
    let hash_end = 5 + usize::from(*value.get(4)?);
    let hash = std::str::from_utf8(value.get(5..hash_end)?).ok()?;
    Some((doclen, hash, value.get(hash_end..)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_bm25_posting_roundtrips() {
        let content = bm25_content_key("abcd", "rust", "digest");
        let key = code_bm25_posting("spawn", &content, 3).unwrap();
        assert_eq!(parse_code_bm25_posting(&key), Some(("spawn".to_string(), content, 3)));
        assert_eq!(parse_code_bm25_posting_chunk(&key), Some((content, 3)));
    }

    #[test]
    fn code_bm25_posting_value_roundtrips() {
        let value = code_bm25_posting_value(7, 142);
        assert_eq!(parse_code_bm25_posting_value(&value), Some((7, 142)));
        assert_eq!(parse_code_bm25_posting_value(&value[..7]), None);
    }

    /// A `spawn` posting prefix must not bleed into `spawnblocking`.
    #[test]
    fn code_bm25_posting_prefix_isolates_terms() {
        let content = bm25_content_key("h", "rust", "d");
        let key_spawn = code_bm25_posting("spawn", &content, 1).unwrap();
        let key_spawn_blocking = code_bm25_posting("spawnblocking", &content, 1).unwrap();
        let prefix = code_bm25_postings_prefix("spawn");
        assert!(key_spawn.starts_with(&prefix));
        assert!(!key_spawn_blocking.starts_with(&prefix));
    }

    #[test]
    fn code_bm25_by_path_roundtrips_and_isolates() {
        let content = bm25_content_key("hash", "rust", "d");
        let (rel_a, rel_b) = (RelPath::from("src/foo.rs"), RelPath::from("src/foo.rs.bak"));
        let key = code_bm25_by_path(&rel_a, &content, 0);
        assert_eq!(parse_code_bm25_by_path(&key), Some((rel_a.clone(), content, 0)));
        let prefix_a = code_bm25_by_path_prefix(&rel_a);
        assert!(key.starts_with(&prefix_a));
        assert!(!code_bm25_by_path(&rel_b, &content, 0).starts_with(&prefix_a));
        let value = code_bm25_by_path_value(42, "hash", b"terms").unwrap();
        assert_eq!(parse_code_bm25_by_path_value(&value), Some((42, "hash", &b"terms"[..])));
    }

    #[test]
    fn the_content_key_follows_everything_that_shapes_the_chunks() {
        let base = bm25_content_key("hash", "rust", "digest");
        assert_eq!(base, bm25_content_key("hash", "rust", "digest"));
        assert_ne!(base, bm25_content_key("hash2", "rust", "digest"));
        assert_ne!(base, bm25_content_key("hash", "python", "digest"));
        assert_ne!(base, bm25_content_key("hash", "rust", "digest2"));
        assert_ne!(
            bm25_content_key("ab", "c", "d"),
            bm25_content_key("a", "bc", "d"),
            "parts are length-prefixed"
        );
    }

    #[test]
    fn a_chunk_id_splits_into_hash_and_ordinal() {
        assert_eq!(split_chunk_id("abcd:12"), Some(("abcd", 12)));
        assert_eq!(split_chunk_id("abcd"), None);
        assert_eq!(split_chunk_id("abcd:x"), None);
    }
}
