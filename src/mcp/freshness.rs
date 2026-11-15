//! Whether a working-tree file still holds the content the index describes.
//!
//! An outline's byte spans and rows belong to the bytes that were scanned. Every tool that lays
//! them over the live file asks here first, so one edit is judged the same way everywhere, and a
//! file that was edited, deleted or has outgrown the scan's size limit gets the same answer from
//! each of them.

use std::path::Path;

use rmcp::ErrorData as McpError;

use crate::path::RelPath;

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

/// An indexed file as it is on disk now.
pub(super) struct Live {
    pub bytes: Vec<u8>,
    /// Whether `bytes` are the content the index describes. Indexed offsets apply only then.
    pub fresh: bool,
}

/// Why an indexed file's content could not be loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Unloadable {
    /// Deleted, or no longer readable, since the scan.
    Gone,
    /// Grown past `scan.max_file_bytes` since the scan.
    TooLarge,
}

/// The whole of `file`, refused past `max_bytes`.
///
/// The scan indexes no file over `scan.max_file_bytes`, so a larger one has outgrown its index
/// entry and is never held whole at whatever size it has reached.
pub(super) fn read_bounded(file: &Path, max_bytes: u64) -> std::io::Result<Option<Vec<u8>>> {
    use std::io::Read;

    let mut bytes = Vec::new();
    std::fs::File::open(file)?
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    Ok((bytes.len() as u64 <= max_bytes).then_some(bytes))
}

/// Read `file` whole, bounded by `max_bytes`, and judge it against `stamp`. Blocking IO.
pub(super) fn load(file: &Path, stamp: IndexedStamp<'_>, max_bytes: u64) -> Result<Live, Unloadable> {
    let bytes = read_bounded(file, max_bytes)
        .map_err(|_| Unloadable::Gone)?
        .ok_or(Unloadable::TooLarge)?;
    let fresh = matches_index(stamp, file, &bytes);
    Ok(Live { bytes, fresh })
}

/// [`load`] for a tool that can only answer from the indexed content: an edited file is an error.
pub(super) fn load_fresh(
    tool: &str,
    path: &RelPath,
    file: &Path,
    stamp: IndexedStamp<'_>,
    max_bytes: u64,
) -> Result<Vec<u8>, McpError> {
    match load(file, stamp, max_bytes) {
        Ok(live) if live.fresh => Ok(live.bytes),
        Ok(_) => Err(edited(tool, path)),
        Err(reason) => Err(unloadable(tool, path, reason, max_bytes)),
    }
}

/// The error for an indexed file that cannot be loaded, phrased as the next call to make.
pub(super) fn unloadable(tool: &str, path: &RelPath, reason: Unloadable, max_bytes: u64) -> McpError {
    let message = match reason {
        Unloadable::Gone => format!(
            "`{tool}`: `{path}` is in the index but can no longer be read: it was deleted or moved after the \
             scan. Run `index_rescan {{\"paths\":[\"{path}\"]}}` to bring the index up to date."
        ),
        Unloadable::TooLarge => format!(
            "`{tool}`: `{path}` has grown past {max_bytes} bytes (`scan.max_file_bytes`) since it was indexed \
             and is not read whole. Read a line span with `code_read`, or run `index_rescan \
             {{\"paths\":[\"{path}\"]}}` to drop it from the index."
        ),
    };
    McpError::invalid_params(message, None)
}

/// The outline of `bytes`, the current content of an indexed file that was edited after the scan.
/// Blocking: a tree-sitter parse.
pub(super) fn reparse(
    state: &super::ServerState,
    tool: &str,
    path: &RelPath,
    bytes: &[u8],
) -> Result<crate::extract::FileMapL1, McpError> {
    let cannot = |why: String| {
        McpError::invalid_params(
            format!(
                "`{tool}`: `{path}` changed since it was indexed and {why}. Read a line span with `code_read`, \
                 or run `index_rescan {{\"paths\":[\"{path}\"]}}`."
            ),
            None,
        )
    };
    let language = super::helpers_git_file::detect_lang(state, path)
        .ok_or_else(|| cannot("is no longer in a supported language".to_string()))?;
    crate::extract::l1::extract_l1(language, bytes).map_err(|error| cannot(format!("could not be parsed ({error})")))
}

/// The error for a tool that needs the indexed positions of a file edited after the scan.
pub(super) fn edited(tool: &str, path: &RelPath) -> McpError {
    McpError::invalid_params(
        format!(
            "`{tool}`: `{path}` changed since it was indexed, so the indexed positions no longer match the \
             file. Run `index_rescan {{\"paths\":[\"{path}\"]}}` and retry."
        ),
        None,
    )
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

    #[test]
    fn a_whole_file_read_is_refused_past_its_bound() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("f.txt");
        std::fs::write(&file, b"12345").expect("write");
        assert_eq!(read_bounded(&file, 5).expect("read"), Some(b"12345".to_vec()));
        assert_eq!(read_bounded(&file, 4).expect("read"), None);
    }

    #[test]
    fn a_load_tells_fresh_edited_gone_and_outgrown_apart() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.rs");
        std::fs::write(&file, b"fn a() {}\n").expect("write");
        let (hash_hex, size_bytes, mtime_ns) = stamp_of(b"fn a() {}\n", 0);
        let stamp = IndexedStamp {
            hash_hex: &hash_hex,
            size_bytes,
            mtime_ns,
        };
        assert!(load(&file, stamp, 1024).expect("load").fresh);
        assert_eq!(load(&file, stamp, 4).err(), Some(Unloadable::TooLarge));

        std::fs::write(&file, b"fn b() {}\n").expect("write");
        let live = load(&file, stamp, 1024).expect("load");
        assert!(!live.fresh);
        assert_eq!(live.bytes, b"fn b() {}\n");
        let path = RelPath::from("a.rs");
        let refusal = load_fresh("code_refs", &path, &file, stamp, 1024).expect_err("edited");
        assert!(
            refusal.message.contains("index_rescan {\"paths\":[\"a.rs\"]}"),
            "{}",
            refusal.message
        );

        std::fs::remove_file(&file).expect("remove");
        assert_eq!(load(&file, stamp, 1024).err(), Some(Unloadable::Gone));
        let gone = unloadable("code_read", &path, Unloadable::Gone, 1024);
        assert!(gone.message.contains("index_rescan"), "{}", gone.message);
    }
}
