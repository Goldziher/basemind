//! Hits that `code_symbols` and a `code_refs` name scan list from the index, laid against the file
//! as it is now. Their positions belong to the scanned bytes: a hit in a file edited since the scan
//! is flagged `stale`, and a definition is moved to where the file now has it.

use ahash::AHashMap;

use super::ServerState;
use super::freshness::{self, IndexedStamp};
use super::tool_contract::IndexedFiles;
use super::types::{ReferenceHit, SearchHitView};
use crate::path::RelPath;

/// A listed file as it is on disk now.
enum Disk {
    /// Holds the content the index describes.
    Fresh,
    /// Edited since the scan; the bytes it holds now.
    Edited(Vec<u8>),
    /// Deleted, outgrown `scan.max_file_bytes`, or leading out of the repository.
    Unreadable,
}

/// Judge `path` against its index entry. A file whose size and timestamp are the indexed ones is
/// fresh on one `stat`, as the scanner judges it; any other file is read and hashed.
fn disk(state: &ServerState, cache: &super::MapCache, path: &RelPath) -> Disk {
    let Some(meta) = cache.meta(path) else {
        return Disk::Fresh;
    };
    let files = IndexedFiles::of(state);
    let Some(file) = files.indexed_path(path) else {
        return Disk::Unreadable;
    };
    let Ok(stat) = std::fs::metadata(&file) else {
        return Disk::Unreadable;
    };
    if meta.mtime_ns != 0 && stat.len() == meta.size_bytes && crate::scanner_file::mtime_nanos(&stat) == meta.mtime_ns {
        return Disk::Fresh;
    }
    match freshness::load(&file, IndexedStamp::from(meta), files.max_file_bytes) {
        Ok(live) if live.fresh => Disk::Fresh,
        Ok(live) => Disk::Edited(live.bytes),
        Err(_) => Disk::Unreadable,
    }
}

/// Flag each `code_symbols` hit in a file edited since the scan, and move it to the line its
/// definition (same name and kind, nearest the indexed line) has in the file now. Blocking IO.
pub(super) fn follow_symbols(state: &ServerState, cache: &super::MapCache, hits: &mut [SearchHitView]) {
    // `None`: fresh. `Some(outline)`: stale, with the file's outline now when it could be parsed.
    type Judged = Option<Option<std::sync::Arc<crate::extract::FileMapL1>>>;
    let mut judged: AHashMap<RelPath, Judged> = AHashMap::new();
    for hit in hits {
        let outline = judged
            .entry(hit.path.clone())
            .or_insert_with(|| match disk(state, cache, &hit.path) {
                Disk::Fresh => None,
                Disk::Edited(bytes) => Some(freshness::reparse(state, "code_symbols", &hit.path, &bytes).ok()),
                Disk::Unreadable => Some(None),
            });
        let Some(outline) = outline else {
            continue;
        };
        hit.stale = true;
        let Some(moved) = outline.as_ref().and_then(|l1| {
            l1.symbols
                .iter()
                .filter(|symbol| symbol.name == hit.name && super::helpers::kind_to_str(symbol.kind) == hit.kind)
                .min_by_key(|symbol| (symbol.start_row + 1).abs_diff(hit.line))
        }) else {
            continue;
        };
        hit.line = moved.start_row + 1;
        hit.signature.clone_from(&moved.signature);
    }
}

/// Flag each call site listed in a file edited since the scan. Its line is the indexed one: a call
/// site has no name of its own to find it again by. Blocking IO.
pub(super) fn flag_call_sites(state: &ServerState, cache: &super::MapCache, hits: &mut [ReferenceHit]) {
    let mut judged: AHashMap<RelPath, bool> = AHashMap::new();
    for hit in hits {
        hit.stale = *judged
            .entry(hit.path.clone())
            .or_insert_with(|| !matches!(disk(state, cache, &hit.path), Disk::Fresh));
    }
}
