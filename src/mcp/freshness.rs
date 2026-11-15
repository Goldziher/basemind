//! Whether a working-tree file still holds the content the index describes.
//!
//! An outline's byte spans and rows belong to the bytes that were scanned. Every tool that lays
//! them over the live file asks here first, so one edit is judged the same way everywhere.

use std::path::Path;

/// What the index recorded about a file's content when it scanned it.
#[derive(Clone, Copy)]
pub(super) struct IndexedStamp<'a> {
    pub hash_hex: &'a str,
    pub size_bytes: u64,
    /// Nanoseconds since the epoch; `0` when the scan could not trust the timestamp.
    pub mtime_ns: i64,
}

impl<'a> From<&'a crate::store::FileEntry> for IndexedStamp<'a> {
    fn from(entry: &'a crate::store::FileEntry) -> Self {
        Self {
            hash_hex: &entry.hash_hex,
            size_bytes: entry.size_bytes,
            mtime_ns: entry.mtime,
        }
    }
}

impl<'a> From<&'a super::l1_cache::FileMeta> for IndexedStamp<'a> {
    fn from(meta: &'a super::l1_cache::FileMeta) -> Self {
        Self {
            hash_hex: &meta.hash_hex,
            size_bytes: meta.size_bytes,
            mtime_ns: meta.mtime_ns,
        }
    }
}

/// True when `bytes`, just read from `file`, are the content `stamp` was taken from.
///
/// A different size settles it. The same size and timestamp is the scanner's own "unchanged" test.
/// Anything else is decided by the content hash, so an edit that keeps the size is never taken for
/// the indexed content.
pub(super) fn matches_index(stamp: IndexedStamp<'_>, file: &Path, bytes: &[u8]) -> bool {
    if bytes.len() as u64 != stamp.size_bytes {
        return false;
    }
    let live_mtime = std::fs::metadata(file)
        .ok()
        .map(|metadata| crate::scanner_file::mtime_nanos(&metadata));
    if stamp.mtime_ns != 0 && live_mtime == Some(stamp.mtime_ns) {
        return true;
    }
    let hash = crate::hashing::hex_buf(&crate::hashing::hash_bytes(bytes));
    crate::hashing::hex_str(&hash) == stamp.hash_hex
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp_of(bytes: &[u8], mtime_ns: i64) -> (String, u64, i64) {
        (
            crate::hashing::hex(&crate::hashing::hash_bytes(bytes)),
            bytes.len() as u64,
            mtime_ns,
        )
    }

    #[test]
    fn an_edit_that_keeps_the_size_is_not_the_indexed_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.rs");
        std::fs::write(&file, b"fn aa() {}\nfn b() {}\n").expect("write");
        let (hash_hex, size_bytes, mtime_ns) = stamp_of(b"fn aa() {}\nfn b() {}\n", 0);
        let stamp = IndexedStamp {
            hash_hex: &hash_hex,
            size_bytes,
            mtime_ns,
        };
        assert!(matches_index(stamp, &file, b"fn aa() {}\nfn b() {}\n"));
        assert!(!matches_index(stamp, &file, b"fn a() {}\nfn bb() {}\n"));
        assert!(!matches_index(stamp, &file, b"fn aa() {}\nfn b() {}\n\n"));
    }

    #[test]
    fn a_matching_size_and_timestamp_is_trusted_without_hashing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.rs");
        std::fs::write(&file, b"same").expect("write");
        let live = crate::scanner_file::mtime_nanos(&std::fs::metadata(&file).expect("metadata"));
        let stamp = IndexedStamp {
            hash_hex: "not the hash of the bytes",
            size_bytes: 4,
            mtime_ns: live,
        };
        assert!(matches_index(stamp, &file, b"same"));
        let unknown = IndexedStamp { mtime_ns: 0, ..stamp };
        assert!(
            !matches_index(unknown, &file, b"same"),
            "an unknown timestamp proves nothing"
        );
    }
}
