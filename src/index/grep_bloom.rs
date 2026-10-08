//! Per-file trigram bloom filters: the candidate PREFILTER behind `code grep` (ADR-0012).
//!
//! Each indexed file gets one row in the `grep_bloom` keyspace (key = repo-relative path) holding a
//! small bloom filter over the file's byte trigrams plus the `(size, mtime_ns)` stamp the bloom was
//! built under. A query extracts the literals every match MUST contain, turns them into trigram
//! hashes and tests each row; a file the bloom rejects, whose stamp still matches the live file,
//! cannot contain a match and is never opened. A bloom can only say "definitely absent", so it
//! decides which files to READ, never what a match is: the real regex still runs on every survivor.
//!
//! Everything here errs towards "candidate": a missing, older-version or malformed row, a changed
//! stamp, a pattern with no usable literal, an unreadable file. Only an intact row that was built
//! from the file as it is now and lacks a required trigram skips the file.

use std::path::Path;

use rayon::prelude::*;
use regex_syntax::hir::literal::{ExtractKind, Extractor};

use super::IndexDb;
use crate::path::RelPath;

/// Row layout revision (first byte). A row with any other value is treated as absent, so a future
/// layout change needs no index wipe: old rows read as candidates until the file is next scanned.
const ROW_VERSION: u8 = 1;
/// `version(1) | size(8) | mtime_ns(8)`, then the bit array.
const HEADER_BYTES: usize = 17;
/// Bloom bytes per source byte is `1 / BYTES_DIVISOR` (ADR-0012 sizing: 1/8 keeps the candidate set
/// within ~2x of the exact trigram answer on the armis corpus at 12.8% of corpus bytes; 1/16 was
/// up to ~8x worse on selective needles).
const BYTES_DIVISOR: usize = 8;
/// Floor so a tiny file still gets a usable filter.
const MIN_BLOOM_BYTES: usize = 64;
/// Ceiling per row. Past it a filter is saturated and rejects nothing, so a bigger row would only
/// cost index space; the file simply stays a near-permanent candidate.
const MAX_BLOOM_BYTES: usize = 256 * 1024;
/// Bits set per trigram.
const HASHES: u64 = 2;
/// A literal shorter than this has no trigram to test.
const MIN_LITERAL_BYTES: usize = 3;

#[inline]
fn mix(trigram: u32) -> u64 {
    let mut x = u64::from(trigram).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^= x >> 32;
    x = x.wrapping_mul(0xD6E8_FEB8_6659_FD93);
    x ^ (x >> 32)
}

/// Bit position of hash `i` for a mixed trigram, in `0..nbits` (`nbits <= 2^32`).
#[inline]
fn position(mixed: u64, i: u64, nbits: u64) -> u64 {
    let h1 = mixed & 0xFFFF_FFFF;
    let h2 = (mixed >> 32) | 1;
    ((h1.wrapping_add(i.wrapping_mul(h2)) & 0xFFFF_FFFF) * nbits) >> 32
}

#[inline]
fn trigram(w: &[u8]) -> u32 {
    (u32::from(w[0]) << 16) | (u32::from(w[1]) << 8) | u32::from(w[2])
}

/// Build the stored row for a file whose bytes the caller already holds, stamped with the
/// `(size, mtime_ns)` it was read under.
pub fn build_row(bytes: &[u8], size: u64, mtime_ns: i64) -> Vec<u8> {
    let bloom_bytes = (bytes.len() / BYTES_DIVISOR).clamp(MIN_BLOOM_BYTES, MAX_BLOOM_BYTES);
    let nbits = (bloom_bytes * 8) as u64;
    let mut row = vec![0u8; HEADER_BYTES + bloom_bytes];
    row[0] = ROW_VERSION;
    row[1..9].copy_from_slice(&size.to_le_bytes());
    row[9..17].copy_from_slice(&mtime_ns.to_le_bytes());
    let bits = &mut row[HEADER_BYTES..];
    for w in bytes.windows(3) {
        let mixed = mix(trigram(w));
        for i in 0..HASHES {
            let p = position(mixed, i, nbits);
            bits[(p >> 3) as usize] |= 1 << (p & 7);
        }
    }
    row
}

/// A decoded row: the stamp it was built under and its bit array.
struct Row<'a> {
    size: u64,
    mtime_ns: i64,
    bits: &'a [u8],
}

fn parse_row(row: &[u8]) -> Option<Row<'_>> {
    if row.len() <= HEADER_BYTES || row[0] != ROW_VERSION {
        return None;
    }
    Some(Row {
        size: u64::from_le_bytes(row[1..9].try_into().ok()?),
        mtime_ns: i64::from_le_bytes(row[9..17].try_into().ok()?),
        bits: &row[HEADER_BYTES..],
    })
}

/// The `(size, mtime_ns)` stamp a stored row was built under, or `None` for an absent / foreign row.
pub fn row_stamp(row: &[u8]) -> Option<(u64, i64)> {
    parse_row(row).map(|r| (r.size, r.mtime_ns))
}

pub use crate::scanner_file::mtime_nanos;

/// What a regex requires of a file, as trigram hashes: every group must pass, and a group passes when
/// ANY of its alternatives has ALL its trigrams in the bloom.
#[derive(Debug)]
pub struct Needle {
    groups: Vec<Vec<Vec<u64>>>,
}

impl Needle {
    /// Derive the required-trigram groups from a regex source, or `None` when nothing safe can be
    /// required (the caller then sweeps every file).
    ///
    /// Two necessary conditions are extracted with `regex-syntax`'s literal [`Extractor`]: the set
    /// of literals one of which every match STARTS with, and the set one of which every match ENDS
    /// with. A set is usable only when it is finite, non-empty and every member is at least
    /// [`MIN_LITERAL_BYTES`] long; one empty or short member means some match may contain no
    /// trigram at all, and an infinite set means no bound. Case-insensitive flags and classes are
    /// expanded by the extractor into their case variants (or make the set unusable when they would
    /// explode), so they need no special handling here.
    pub fn compile(pattern: &str) -> Option<Self> {
        let hir = regex_syntax::ParserBuilder::new().build().parse(pattern).ok()?;
        let groups: Vec<Vec<Vec<u64>>> = [ExtractKind::Prefix, ExtractKind::Suffix]
            .into_iter()
            .filter_map(|kind| {
                let mut extractor = Extractor::new();
                extractor.kind(kind);
                let seq = extractor.extract(&hir);
                let literals = seq.literals()?;
                if literals.is_empty() || literals.iter().any(|l| l.as_bytes().len() < MIN_LITERAL_BYTES) {
                    return None;
                }
                Some(
                    literals
                        .iter()
                        .map(|l| {
                            let mut hashes: Vec<u64> = l.as_bytes().windows(3).map(|w| mix(trigram(w))).collect();
                            hashes.sort_unstable();
                            hashes.dedup();
                            hashes
                        })
                        .collect(),
                )
            })
            .collect();
        (!groups.is_empty()).then_some(Self { groups })
    }

    /// True when the bloom `bits` may belong to a file satisfying every group.
    fn passes(&self, bits: &[u8]) -> bool {
        let nbits = (bits.len() * 8) as u64;
        let has = |mixed: u64| {
            (0..HASHES).all(|i| {
                let p = position(mixed, i, nbits);
                bits[(p >> 3) as usize] & (1 << (p & 7)) != 0
            })
        };
        self.groups
            .iter()
            .all(|group| group.iter().any(|alt| alt.iter().all(|&h| has(h))))
    }
}

impl IndexDb {
    /// Fetch the stored bloom row for `rel`, if any.
    pub fn grep_bloom_row(&self, rel: &RelPath) -> Option<fjall::Slice> {
        self.grep_bloom.get(rel.as_bytes()).ok().flatten()
    }

    /// Delete `rel`'s row without touching the rest of its index entries: how a test models an index
    /// built before the keyspace existed.
    #[doc(hidden)]
    pub fn drop_grep_bloom_row_for_test(&self, rel: &RelPath) {
        self.grep_bloom.remove(rel.as_bytes()).expect("remove bloom row");
    }

    /// True when `rel` has an intact row stamped `(size, mtime_ns)`: nothing to (re)build.
    pub fn grep_bloom_current(&self, rel: &RelPath, size: u64, mtime_ns: i64) -> bool {
        self.grep_bloom_row(rel)
            .and_then(|row| row_stamp(&row))
            .is_some_and(|stamp| stamp == (size, mtime_ns))
    }

    /// For each of `paths`, whether the file can be SKIPPED by a grep for `needle`: it has an intact
    /// row, the row rejects the needle, and the live file still has the stamp the row was built
    /// under. Rows are fetched and dropped one at a time, in parallel; nothing is held between files.
    pub fn grep_skip_verdicts(&self, root: &Path, paths: &[&RelPath], needle: &Needle) -> Vec<bool> {
        paths
            .par_iter()
            .map(|rel| {
                let Some(row) = self.grep_bloom_row(rel) else {
                    return false;
                };
                let Some(row) = parse_row(&row) else {
                    return false;
                };
                if needle.passes(row.bits) {
                    return false;
                }
                // The bloom describes the file as scanned; the sweep reads it as it is now. Only a
                // file still carrying the scan-time stamp may be trusted to lack the literal.
                std::fs::metadata(root.join(rel.to_path_buf()))
                    .is_ok_and(|m| m.len() == row.size && mtime_nanos(&m) == row.mtime_ns)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_for(text: &str) -> Vec<u8> {
        build_row(text.as_bytes(), text.len() as u64, 7)
    }

    fn rejects(pattern: &str, text: &str) -> bool {
        let needle = Needle::compile(pattern).expect("usable needle");
        let row = row_for(text);
        !needle.passes(parse_row(&row).expect("row").bits)
    }

    #[test]
    fn a_present_literal_is_never_rejected() {
        let text = "fn main() { let post_processing = 1; }\n".repeat(50);
        assert!(!rejects("post_processing", &text));
        assert!(!rejects(r"post_processing\s*=", &text));
        assert!(!rejects("(?i)POST_PROCESSING", &text));
        assert!(!rejects("zzz_absent|post_processing", &text));
    }

    #[test]
    fn an_absent_literal_is_rejected() {
        assert!(rejects("eq9fsw", "fn main() { println!(\"hello\"); }"));
    }

    #[test]
    fn patterns_without_a_required_trigram_sweep_everything() {
        for pattern in [".*", r"\w+", "ab", "a|bcd", "(?i)k", "^", r"\d{3}"] {
            assert!(Needle::compile(pattern).is_none(), "{pattern} must not be prefiltered");
        }
    }

    #[test]
    fn case_insensitive_literals_match_every_casing() {
        let needle = Needle::compile("(?i)needle").expect("expanded to case variants");
        for text in ["xx NEEDLE xx", "xx needle xx", "xx NeEdLe xx"] {
            assert!(needle.passes(parse_row(&row_for(text)).unwrap().bits), "{text}");
        }
    }

    #[test]
    fn unicode_literals_are_matched_on_their_utf8_bytes() {
        let needle = Needle::compile("caf\u{e9}s").expect("literal");
        assert!(needle.passes(parse_row(&row_for("two caf\u{e9}s here")).unwrap().bits));
    }

    #[test]
    fn foreign_or_short_rows_do_not_parse() {
        let mut row = row_for("hello world");
        assert!(parse_row(&row).is_some());
        row[0] = ROW_VERSION + 1;
        assert!(parse_row(&row).is_none());
        assert!(parse_row(&[ROW_VERSION; HEADER_BYTES]).is_none());
        assert!(parse_row(&[]).is_none());
    }

    #[test]
    fn the_stamp_round_trips() {
        let row = build_row(b"abcdef", 6, -12);
        assert_eq!(row_stamp(&row), Some((6, -12)));
    }

    #[test]
    fn row_size_tracks_the_file_within_its_bounds() {
        assert_eq!(build_row(b"abc", 3, 0).len(), HEADER_BYTES + MIN_BLOOM_BYTES);
        let big = vec![b'x'; 8 * 1024];
        assert_eq!(build_row(&big, 8192, 0).len(), HEADER_BYTES + 1024);
        let huge = vec![b'x'; 4 * 1024 * 1024];
        assert_eq!(build_row(&huge, 0, 0).len(), HEADER_BYTES + MAX_BLOOM_BYTES);
    }
}
