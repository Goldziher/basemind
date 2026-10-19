//! Per-file trigram bloom filters: the candidate PREFILTER behind `code grep` (ADR-0012).
//!
//! Each distinct file content gets one row in the `grep_bloom` keyspace (key = hex content hash, so
//! two paths with identical bytes share a row) holding a small bloom filter over the content's byte
//! trigrams. A query extracts the literals every match MUST contain, turns them into trigram hashes
//! and tests each row; a file whose indexed hash selects a rejecting bloom, and whose live
//! `(size, mtime_ns)` still equals the stamp it was indexed under, cannot contain a match and is
//! never opened. A bloom can only say "definitely absent", so it
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

/// Row layout revision (first byte). A row with any other value is treated as absent, so a layout
/// change needs no index wipe: old rows read as candidates until a scan sweeps and rebuilds them.
/// 1 was the path-keyed row carrying a `(size, mtime_ns)` stamp; 2 is the content-hash-keyed row.
const ROW_VERSION: u8 = 2;
/// `version(1)`, then the bit array.
const HEADER_BYTES: usize = 1;
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

/// Build the stored row for content the caller already holds.
pub fn build_row(bytes: &[u8]) -> Vec<u8> {
    let bloom_bytes = (bytes.len() / BYTES_DIVISOR).clamp(MIN_BLOOM_BYTES, MAX_BLOOM_BYTES);
    let nbits = (bloom_bytes * 8) as u64;
    let mut row = vec![0u8; HEADER_BYTES + bloom_bytes];
    row[0] = ROW_VERSION;
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

fn parse_row(row: &[u8]) -> Option<&[u8]> {
    (row.len() > HEADER_BYTES && row[0] == ROW_VERSION).then(|| &row[HEADER_BYTES..])
}

pub use crate::scanner_file::mtime_nanos;

/// One file a grep may test: its path, the content hash it was indexed under, and the
/// `(size, mtime_ns)` stamp of that indexing. The stamp is what the live file is held to, so a file
/// edited since the scan is never skipped on the strength of its old content's bloom.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GrepEntry {
    pub rel: RelPath,
    pub hash_hex: String,
    pub size_bytes: u64,
    pub mtime_ns: i64,
}

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
    /// Fetch the stored bloom row for the content `hash_hex`, if any.
    pub fn grep_bloom_row(&self, hash_hex: &str) -> Option<fjall::Slice> {
        self.grep_bloom.get(hash_hex.as_bytes()).ok().flatten()
    }

    /// Delete the row for `hash_hex`: how a test models an index built before the keyspace existed.
    #[doc(hidden)]
    pub fn drop_grep_bloom_row_for_test(&self, hash_hex: &str) {
        self.grep_bloom.remove(hash_hex.as_bytes()).expect("remove bloom row");
    }

    /// Number of stored rows, one per distinct content.
    #[doc(hidden)]
    pub fn grep_bloom_row_count(&self) -> usize {
        self.grep_bloom.iter().count()
    }

    /// True when `hash_hex` has an intact row: nothing to (re)build.
    pub fn grep_bloom_current(&self, hash_hex: &str) -> bool {
        self.grep_bloom_row(hash_hex)
            .is_some_and(|row| parse_row(&row).is_some())
    }

    /// Delete every row whose hash is not in `live` or whose layout is not [`ROW_VERSION`], returning
    /// how many were dropped. Rows are shared by every path holding the content, so only a hash no
    /// indexed file references may go. Path-keyed rows from before the content-hash re-key are never
    /// in `live`, and older-version rows are unreadable, so both leave here.
    pub fn sweep_grep_bloom(&self, live: &ahash::AHashSet<&str>) -> Result<usize, super::IndexError> {
        let mut writer = self.writer();
        let mut dropped = 0;
        for guard in self.grep_bloom.iter() {
            let (key, row) = guard.into_inner()?;
            if parse_row(&row).is_some() && std::str::from_utf8(&key).is_ok_and(|hash| live.contains(hash)) {
                continue;
            }
            writer.remove_grep_bloom(&key);
            dropped += 1;
        }
        writer.commit()?;
        Ok(dropped)
    }

    /// For each of `entries`, whether the file can be SKIPPED by a grep for `needle`: its content has
    /// an intact row, the row rejects the needle, and the live file still has the stamp the entry was
    /// indexed under. Rows are fetched and dropped one at a time, in parallel; nothing is held
    /// between files.
    pub fn grep_skip_verdicts(&self, root: &Path, entries: &[GrepEntry], needle: &Needle) -> Vec<bool> {
        skip_verdicts(|hash| self.grep_bloom_row(hash), root, entries, needle)
    }
}

/// [`IndexDb::grep_skip_verdicts`] over any row source, so a layered view shares the verdict logic.
pub(crate) fn skip_verdicts(
    row_of: impl Fn(&str) -> Option<fjall::Slice> + Sync,
    root: &Path,
    entries: &[GrepEntry],
    needle: &Needle,
) -> Vec<bool> {
    entries
        .par_iter()
        .map(|entry| {
            let Some(row) = row_of(&entry.hash_hex) else {
                return false;
            };
            let Some(bits) = parse_row(&row) else {
                return false;
            };
            if needle.passes(bits) {
                return false;
            }
            // The bloom describes the content as indexed; the sweep reads the file as it is now.
            // Only a file still carrying its indexed stamp may be trusted to lack the literal.
            std::fs::metadata(root.join(entry.rel.to_path_buf()))
                .is_ok_and(|m| m.len() == entry.size_bytes && mtime_nanos(&m) == entry.mtime_ns)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_for(text: &str) -> Vec<u8> {
        build_row(text.as_bytes())
    }

    fn rejects(pattern: &str, text: &str) -> bool {
        let needle = Needle::compile(pattern).expect("usable needle");
        let row = row_for(text);
        !needle.passes(parse_row(&row).expect("row"))
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
            assert!(needle.passes(parse_row(&row_for(text)).unwrap()), "{text}");
        }
    }

    #[test]
    fn unicode_literals_are_matched_on_their_utf8_bytes() {
        let needle = Needle::compile("caf\u{e9}s").expect("literal");
        assert!(needle.passes(parse_row(&row_for("two caf\u{e9}s here")).unwrap()));
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
    fn row_size_tracks_the_file_within_its_bounds() {
        assert_eq!(build_row(b"abc").len(), HEADER_BYTES + MIN_BLOOM_BYTES);
        let big = vec![b'x'; 8 * 1024];
        assert_eq!(build_row(&big).len(), HEADER_BYTES + 1024);
        let huge = vec![b'x'; 4 * 1024 * 1024];
        assert_eq!(build_row(&huge).len(), HEADER_BYTES + MAX_BLOOM_BYTES);
    }

    fn open_db() -> (tempfile::TempDir, IndexDb) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = IndexDb::open(dir.path()).expect("index");
        (dir, db)
    }

    fn put(db: &IndexDb, hash_hex: &str, text: &str) {
        let mut writer = db.writer();
        writer.upsert_grep_bloom(hash_hex, row_for(text));
        writer.commit().expect("commit");
    }

    #[test]
    fn a_row_round_trips_under_its_content_hash() {
        let (_dir, db) = open_db();
        put(&db, "aa11", "fn alpha() {}");
        assert_eq!(
            db.grep_bloom_row("aa11").as_deref(),
            Some(row_for("fn alpha() {}").as_slice())
        );
        assert!(db.grep_bloom_current("aa11"));
        assert!(!db.grep_bloom_current("bb22"));
    }

    #[test]
    fn a_hash_that_prefixes_another_reads_only_its_own_row() {
        let (_dir, db) = open_db();
        put(&db, "ab", "short key");
        put(&db, "abc", "longer key");
        assert_eq!(
            db.grep_bloom_row("ab").as_deref(),
            Some(row_for("short key").as_slice())
        );
        assert_eq!(
            db.grep_bloom_row("abc").as_deref(),
            Some(row_for("longer key").as_slice())
        );
        assert!(db.grep_bloom_row("a").is_none());
    }

    #[test]
    fn the_sweep_drops_only_hashes_no_live_file_references() {
        let (_dir, db) = open_db();
        for hash in ["h1", "h2", "h3"] {
            put(&db, hash, hash);
        }
        let live: ahash::AHashSet<&str> = ["h1", "h3"].into_iter().collect();
        assert_eq!(db.sweep_grep_bloom(&live).expect("sweep"), 1);
        assert!(db.grep_bloom_current("h1") && db.grep_bloom_current("h3"));
        assert!(!db.grep_bloom_current("h2"));
        assert_eq!(
            db.sweep_grep_bloom(&live).expect("sweep"),
            0,
            "a second sweep is a no-op"
        );
    }

    #[test]
    fn two_paths_with_the_same_content_share_one_row_and_both_skip() {
        let (_dir, db) = open_db();
        let root = tempfile::tempdir().expect("root");
        let text = "fn unrelated() { println!(\"hello\"); }\n";
        let mut entries = Vec::new();
        for name in ["a.rs", "b.rs"] {
            std::fs::write(root.path().join(name), text).expect("write");
            let meta = std::fs::metadata(root.path().join(name)).expect("stat");
            entries.push(GrepEntry {
                rel: RelPath::from(name),
                hash_hex: "same".to_owned(),
                size_bytes: meta.len(),
                mtime_ns: mtime_nanos(&meta),
            });
        }
        put(&db, "same", text);
        put(&db, "same", text);
        assert_eq!(db.grep_bloom_row_count(), 1);
        let needle = Needle::compile("zzz_absent_literal").expect("needle");
        assert_eq!(db.grep_skip_verdicts(root.path(), &entries, &needle), vec![true, true]);
    }

    #[test]
    fn a_live_file_that_moved_off_its_indexed_stamp_is_a_candidate() {
        let (_dir, db) = open_db();
        let root = tempfile::tempdir().expect("root");
        std::fs::write(root.path().join("a.rs"), "fn unrelated() {}\n").expect("write");
        let meta = std::fs::metadata(root.path().join("a.rs")).expect("stat");
        put(&db, "h", "fn unrelated() {}\n");
        let needle = Needle::compile("zzz_absent_literal").expect("needle");
        let entry = |size_bytes| GrepEntry {
            rel: RelPath::from("a.rs"),
            hash_hex: "h".to_owned(),
            size_bytes,
            mtime_ns: mtime_nanos(&meta),
        };
        assert_eq!(
            db.grep_skip_verdicts(root.path(), &[entry(meta.len())], &needle),
            vec![true]
        );
        assert_eq!(
            db.grep_skip_verdicts(root.path(), &[entry(meta.len() + 1)], &needle),
            vec![false]
        );
    }
}
