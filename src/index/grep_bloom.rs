//! Per-file trigram bloom filters: the candidate PREFILTER behind `code grep` (ADR-0012).
//!
//! Each distinct file content gets one row in the `grep_bloom` keyspace (key = hex content hash, so
//! two paths with identical bytes share a row) holding a small bloom filter over the content's byte
//! trigrams. A query extracts the literals every match MUST contain, turns them into trigram hashes
//! and tests each row; a file whose indexed hash selects a rejecting bloom, and whose live
//! `(size, mtime_ns)` still equals the stamp it was indexed under, cannot contain a match and is
//! never opened. A bloom can only say "definitely absent", so it decides which files to READ,
//! never what a match is: the real regex still runs on every survivor.
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

/// `meta` row recording the [`ROW_VERSION`] the keyspace was last version-swept under. Present and
/// equal means every row is current, so a sweep need not read values; set only after a sweep that
/// read them all succeeded.
const META_ROW_SWEPT_VER: &[u8] = b"grep_bloom_swept_ver";

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
    #[cfg(any(test, feature = "test-support"))]
    pub fn drop_grep_bloom_row_for_test(&self, hash_hex: &str) {
        self.grep_bloom.remove(hash_hex.as_bytes()).expect("remove bloom row");
    }

    /// Number of stored rows, one per distinct content.
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-support"))]
    pub fn grep_bloom_row_count(&self) -> usize {
        self.grep_bloom.iter().count()
    }

    /// True when `hash_hex` has an intact row: nothing to (re)build.
    pub fn grep_bloom_current(&self, hash_hex: &str) -> bool {
        self.grep_bloom_row(hash_hex)
            .is_some_and(|row| parse_row(&row).is_some())
    }

    /// Delete every row whose hash is not in `live`, returning how many were dropped. Rows are shared
    /// by every path holding the content, so only a hash no indexed file references may go. Path-keyed
    /// rows from before the content-hash re-key are never in `live`, so they leave here too.
    ///
    /// Row values are read only until the keyspace has been swept once under the current
    /// [`ROW_VERSION`] (recorded in `meta`): that pass also drops rows whose layout is older, which
    /// are unreadable. Afterwards every row is current, so the sweep is key-only and a large index
    /// is not read back in full on each scan.
    pub fn sweep_grep_bloom(&self, live: &ahash::AHashSet<&str>) -> Result<usize, super::IndexError> {
        let versioned = self
            .meta
            .get(META_ROW_SWEPT_VER)?
            .is_some_and(|v| v.first() == Some(&ROW_VERSION));
        let mut writer = self.writer();
        let mut dropped = 0;
        for guard in self.grep_bloom.iter() {
            let (key, current) = if versioned {
                (guard.key()?, true)
            } else {
                let (key, row) = guard.into_inner()?;
                (key, parse_row(&row).is_some())
            };
            if current && std::str::from_utf8(&key).is_ok_and(|hash| live.contains(hash)) {
                continue;
            }
            writer.remove_grep_bloom(&key);
            dropped += 1;
        }
        writer.commit()?;
        if !versioned {
            self.meta.insert(META_ROW_SWEPT_VER, [ROW_VERSION])?;
        }
        Ok(dropped)
    }

    /// Delete the rows of `hashes`, none of which any indexed file references now. The incremental
    /// counterpart of [`Self::sweep_grep_bloom`]: cost is the number of hashes, not the keyspace.
    pub fn remove_grep_bloom_rows<'a>(
        &self,
        hashes: impl IntoIterator<Item = &'a str>,
    ) -> Result<(), super::IndexError> {
        let mut writer = self.writer();
        for hash in hashes {
            writer.remove_grep_bloom(hash.as_bytes());
        }
        writer.commit()
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
            // Only a file still carrying its indexed stamp may be trusted to lack the literal. A
            // stamp of 0 is "unknown" (a git source, or an mtime too close to the clock when it was
            // taken, see `scanner_file::settled_mtime`), and an unavailable live mtime also reads 0,
            // so it must never count as a match.
            entry.mtime_ns != 0
                && std::fs::metadata(root.join(entry.rel.to_path_buf()))
                    .is_ok_and(|m| m.len() == entry.size_bytes && mtime_nanos(&m) == entry.mtime_ns)
        })
        .collect()
}

#[cfg(test)]
#[path = "grep_bloom_tests.rs"]
mod tests;
