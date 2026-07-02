//! Key encoders/decoders for the git-history Fjall partitions.
//!
//! Layout mirrors `crate::index::keys`: fixed-width integer keys are big-endian (so a range scan
//! is chronological/numeric), and the one variable-length key (`path_id_by_path`) is `u16`
//! length-prefixed exactly like the symbol index. The helpers are duplicated here (six trivial
//! lines) rather than shared, to keep `git_history` decoupled from `index`.

use crate::git::ChangeKind;
use crate::path::RelPath;

// ── fixed `gh_meta` rows ─────────────────────────────────────────────────────
pub const META_SCHEMA_VER: &[u8] = b"schema_ver";
pub const META_LAST_HEAD: &[u8] = b"last_indexed_head"; // 20 raw sha bytes
pub const META_NEXT_ORD: &[u8] = b"next_commit_ord"; // u32 be
pub const META_NEXT_PATH_ID: &[u8] = b"next_path_id"; // u32 be
pub const META_ROOT_SHA: &[u8] = b"root_sha"; // 20 raw sha bytes
pub const META_COMMIT_COUNT: &[u8] = b"commit_count"; // u32 be

/// `u16:len ‖ bytes`. Returns `None` past the 64 KiB ceiling so a pathological path is skipped
/// (and falls back to the live walk) rather than panicking inside a rayon worker.
fn write_len_prefixed(out: &mut Vec<u8>, bytes: &[u8]) -> Option<()> {
    let len = u16::try_from(bytes.len()).ok()?;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
    Some(())
}

/// `gh_commit_by_ord` / `gh_path_by_id` / `gh_path_to_ords` key: a bare `u32` big-endian.
pub fn u32_key(value: u32) -> [u8; 4] {
    value.to_be_bytes()
}

/// Parse a 4-byte big-endian `u32` key/value. `None` on wrong length.
pub fn parse_u32(bytes: &[u8]) -> Option<u32> {
    let arr: [u8; 4] = bytes.try_into().ok()?;
    Some(u32::from_be_bytes(arr))
}

/// `gh_path_id_by_path` key: `u16:len(rel) ‖ rel_bytes`. `None` only for a path past 64 KiB.
pub fn path_id_by_path_key(rel: &RelPath) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(2 + rel.as_bytes().len());
    write_len_prefixed(&mut out, rel.as_bytes())?;
    Some(out)
}

/// `gh_term_to_ords` key: `field:u8 ‖ term_bytes`. The full search-index posting for one
/// `(field, term)` pair is a point lookup (never a prefix scan), so the term needs no length
/// prefix — the leading field byte plus the exact term bytes fully identify the key. The field
/// byte lets an author-scoped vs message-scoped query hit disjoint keys while a combined query
/// unions both. Terms come pre-capped by the tokenizer, so there is no ceiling check here.
pub fn term_key(field: u8, term: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + term.len());
    out.push(field);
    out.extend_from_slice(term);
    out
}

/// Decode a 40-char hex sha into 20 raw bytes (the `gh_ord_by_sha` key form). `None` on malformed
/// input — callers treat that as "not indexable", never panic.
pub fn sha_hex_to_raw(hex40: &str) -> Option<[u8; 20]> {
    let mut out = [0u8; 20];
    hex::decode_to_slice(hex40, &mut out).ok()?;
    Some(out)
}

/// Render 20 raw sha bytes back to a 40-char lowercase hex string.
pub fn sha_raw_to_hex(sha20: &[u8; 20]) -> String {
    hex::encode(sha20)
}

/// Stable, append-only `ChangeKind` → `u8` mapping. Never reorder existing arms — the byte is
/// persisted in `CommitMeta.files`; new variants extend the tail (mirrors `index::symbol_kind_byte`).
pub fn change_kind_byte(kind: ChangeKind) -> u8 {
    match kind {
        ChangeKind::Added => 0,
        ChangeKind::Modified => 1,
        ChangeKind::Deleted => 2,
        ChangeKind::Renamed => 3,
        // Append-only past this line.
    }
}

/// Inverse of [`change_kind_byte`]. Unknown bytes decode to `Added` (the safest default for a
/// forward-compatible read of a tail variant this build doesn't know).
pub fn change_kind_from_byte(byte: u8) -> ChangeKind {
    match byte {
        1 => ChangeKind::Modified,
        2 => ChangeKind::Deleted,
        3 => ChangeKind::Renamed,
        _ => ChangeKind::Added,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u32_key_round_trips() {
        for value in [0u32, 1, 255, 256, 65_535, 1_000_000, u32::MAX] {
            assert_eq!(parse_u32(&u32_key(value)), Some(value));
        }
        assert_eq!(parse_u32(&[1, 2, 3]), None, "wrong length rejected");
    }

    #[test]
    fn sha_hex_raw_round_trips() {
        let hex = "0a2cad8d74da1107738833adc23ce104835d96cc";
        let raw = sha_hex_to_raw(hex).expect("valid sha");
        assert_eq!(sha_raw_to_hex(&raw), hex);
        assert_eq!(sha_hex_to_raw("not-hex"), None);
        assert_eq!(sha_hex_to_raw("abcd"), None, "wrong length rejected");
    }

    #[test]
    fn change_kind_byte_round_trips_all_variants() {
        for kind in [
            ChangeKind::Added,
            ChangeKind::Modified,
            ChangeKind::Deleted,
            ChangeKind::Renamed,
        ] {
            assert_eq!(change_kind_from_byte(change_kind_byte(kind)), kind);
        }
    }

    #[test]
    fn path_id_key_prefix_isolation() {
        // "Foo" must not be a prefix-collision of "Foobar": length prefix guarantees distinct keys.
        let foo = path_id_by_path_key(&RelPath::from("Foo".as_bytes())).unwrap();
        let foobar = path_id_by_path_key(&RelPath::from("Foobar".as_bytes())).unwrap();
        assert_ne!(foo, foobar);
        assert!(!foobar.starts_with(&foo), "length-prefix prevents prefix spill");
    }

    #[test]
    fn term_key_disjoint_by_field_and_term() {
        // Same term under different fields → distinct keys (author vs message scope never collide).
        assert_ne!(term_key(0, b"fix"), term_key(1, b"fix"));
        // Distinct terms under the same field → distinct keys.
        assert_ne!(term_key(1, b"fix"), term_key(1, b"fixture"));
        // The field byte leads, so an author-field key never equals a message-field key.
        assert_eq!(term_key(0, b"jane")[0], 0);
        assert_eq!(term_key(1, b"jane")[0], 1);
    }

    #[test]
    fn oversized_path_is_rejected() {
        let huge = RelPath::from(vec![b'a'; 70_000].as_slice());
        assert!(path_id_by_path_key(&huge).is_none(), "past u16 ceiling → None");
    }
}
