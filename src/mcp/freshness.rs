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

/// The index entry of `path` and the outline of the content that entry names.
///
/// A rescan replaces the store's entry and publishes the in-RAM map as two steps, so an outline
/// taken from the map and a stamp taken from the store can describe different scans of one file:
/// the live bytes then match the stamp while the spans laid over them belong to the older content.
/// Here the outline is looked up by the entry's own content hash, so the stamp a file is judged
/// against and the facts served for it are one snapshot.
pub(super) fn indexed_outline(
    store: &crate::store::Store,
    cache: &super::MapCache,
    path: &RelPath,
) -> Option<(crate::store::FileEntry, std::sync::Arc<crate::extract::FileMapL1>)> {
    let entry = store.lookup(path)?.clone();
    let l1 = cache.outline_of_content(&entry.hash_hex).or_else(|| {
        store
            .read_l1_by_hex(&entry.hash_hex)
            .ok()
            .flatten()
            .map(std::sync::Arc::new)
    })?;
    Some((entry, l1))
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

/// The outline of `bytes` in `language`, parsed once per content: it is kept in the outline cache
/// under the content's git blob id, the key a revision's blob has there, so paging a stale outline
/// or reading several symbols of one edited file costs one parse. Blocking: a tree-sitter parse.
fn outline_of(
    cache: &super::OutlineCache,
    language: crate::lang::LangId,
    bytes: &[u8],
) -> Result<std::sync::Arc<crate::extract::FileMapL1>, crate::extract::ExtractError> {
    let key = gix::objs::compute_hash(gix::hash::Kind::Sha1, gix::objs::Kind::Blob, bytes)
        .ok()
        .map(|oid| (oid, language));
    if let Some(entry) = key.as_ref().and_then(|key| cache.get(key)) {
        return Ok(std::sync::Arc::clone(&entry.map));
    }
    let map = std::sync::Arc::new(crate::extract::l1::extract_l1(language, bytes)?);
    if let Some(key) = key {
        let entry = super::OutlineEntry {
            map: std::sync::Arc::clone(&map),
            source: std::sync::Arc::new(bytes.to_vec()),
        };
        cache.put(key, std::sync::Arc::new(entry));
    }
    Ok(map)
}

/// The outline of `bytes`, the current content of an indexed file that was edited after the scan.
pub(super) fn reparse(
    state: &super::ServerState,
    tool: &str,
    path: &RelPath,
    bytes: &[u8],
) -> Result<std::sync::Arc<crate::extract::FileMapL1>, McpError> {
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
    outline_of(&state.shared.outline_cache, language, bytes)
        .map_err(|error| cannot(format!("could not be parsed ({error})")))
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

    #[test]
    fn an_edited_file_is_parsed_once_per_content() {
        let _ = crate::lang::ensure_grammars().expect("grammar bootstrap");
        let cache = crate::mcp::OutlineCache::with_bounds(4, u64::MAX);
        let rust = crate::lang::intern("rust").expect("rust grammar");
        let first = outline_of(&cache, rust, b"fn a() {}\n").expect("parse");
        let again = outline_of(&cache, rust, b"fn a() {}\n").expect("parse");
        assert!(
            std::sync::Arc::ptr_eq(&first, &again),
            "the same bytes are not parsed twice"
        );
        let edited = outline_of(&cache, rust, b"fn b() {}\n").expect("parse");
        assert_eq!(edited.symbols[0].name, "b", "other bytes are other content");
        assert_eq!(cache.held().0, 2);
    }

    /// The window between a rescan's store write and its map publish: the store names the new
    /// content while the in-RAM map still lists the old one.
    #[test]
    fn the_outline_served_is_the_one_the_stamp_names_when_the_map_lags_the_store() {
        use crate::store::{Store, VIEW_WORKING};

        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let path = RelPath::from("a.rs");
        let file = root.join("a.rs");
        let config = crate::config::ConfigV1::with_defaults();
        let scan = |store: &mut Store| {
            crate::scanner::scan(
                root,
                store,
                &config,
                crate::scanner::ScanSource::WorkingTree,
                crate::scanner::EmbedMode::Inline,
            )
            .expect("scan");
        };
        let before = b"pub fn before() {}\n";
        let after = b"// moved down\npub fn after() {}\n";

        std::fs::write(&file, before).expect("write");
        let mut store = Store::open(root, VIEW_WORKING).expect("open store");
        scan(&mut store);
        let lagging = crate::mcp::MapCache::build(&store, 0);
        std::fs::write(&file, after).expect("write");
        scan(&mut store);

        let names = |l1: &crate::extract::FileMapL1| l1.symbols.iter().map(|s| s.name.clone()).collect::<Vec<_>>();
        assert_eq!(
            names(&lagging.get(&path).expect("map")),
            ["before"],
            "the map was not republished"
        );
        let (entry, l1) = indexed_outline(&store, &lagging, &path).expect("indexed");
        assert!(matches_index((&entry).into(), &file, after), "the stamp is the store's");
        assert_eq!(names(&l1), ["after"], "and so is the outline laid over those bytes");
        assert_eq!(l1.symbols[0].start_row, 1);
    }
}
