//! Hits that `code_symbols` and `code_refs` list from the index, laid against the file as it is now.
//! Their positions belong to the scanned bytes: a hit in a file edited since the scan is flagged
//! `stale`, and a definition is moved to where the file now has it.

use ahash::AHashMap;

use crate::engine::MapCache;
use crate::engine::files::IndexedFiles;
use crate::engine::freshness::{self, IndexedStamp};
use crate::engine::types::code::{SearchHitView, kind_to_str};
use crate::engine::types::refs::{ImplementationHit, ReferenceHit};
use crate::engine::workspace::Workspace;
use crate::path::RelPath;

/// A hit listed from the index at its indexed position, which [`flag_listed`] can mark `stale`.
pub(crate) trait Listed {
    fn path(&self) -> &RelPath;
    fn set_stale(&mut self, stale: bool);
}

impl Listed for ReferenceHit {
    fn path(&self) -> &RelPath {
        &self.path
    }

    fn set_stale(&mut self, stale: bool) {
        self.stale = stale;
    }
}

impl Listed for ImplementationHit {
    fn path(&self) -> &RelPath {
        &self.path
    }

    fn set_stale(&mut self, stale: bool) {
        self.stale = stale;
    }
}

/// Flag each hit listed in a file edited since the scan: a call site of a name scan or of a pinned
/// definition's callers, or an implementation. Its line is the indexed one: such a hit has no name
/// of its own to find it again by. Blocking IO, one judgement per file.
pub(crate) fn flag_listed<H: Listed>(ws: &Workspace, cache: &MapCache, hits: &mut [H]) {
    let mut judged: AHashMap<RelPath, bool> = AHashMap::new();
    for hit in hits {
        let stale = *judged
            .entry(hit.path().clone())
            .or_insert_with(|| !matches!(disk(ws, cache, hit.path()), Disk::Fresh));
        hit.set_stale(stale);
    }
}

/// A listed file as it is on disk now.
pub(crate) enum Disk {
    /// Holds the content the index describes.
    Fresh,
    /// Edited since the scan; the bytes it holds now.
    Edited(Vec<u8>),
    /// Deleted, outgrown `scan.max_file_bytes`, or leading out of the repository.
    Unreadable,
}

/// Judge `path` against its index entry. A file whose size and timestamp are the indexed ones is
/// fresh on one `stat`, as the scanner judges it; any other file is read and hashed.
pub(crate) fn disk(ws: &Workspace, cache: &MapCache, path: &RelPath) -> Disk {
    let Some(meta) = cache.meta(path) else {
        return Disk::Fresh;
    };
    let files = IndexedFiles::of(ws);
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
pub(crate) fn follow_symbols(ws: &Workspace, cache: &MapCache, hits: &mut [SearchHitView]) {
    // `None`: fresh. `Some(outline)`: stale, with the file's outline now when it could be parsed.
    type Judged = Option<Option<std::sync::Arc<crate::extract::FileMapL1>>>;
    let mut judged: AHashMap<RelPath, Judged> = AHashMap::new();
    for hit in hits {
        let outline = judged
            .entry(hit.path.clone())
            .or_insert_with(|| match disk(ws, cache, &hit.path) {
                Disk::Fresh => None,
                Disk::Edited(bytes) => Some(freshness::reparse(ws, "code_symbols", &hit.path, &bytes).ok()),
                Disk::Unreadable => Some(None),
            });
        let Some(outline) = outline else {
            continue;
        };
        hit.stale = true;
        let Some(moved) = outline.as_ref().and_then(|l1| {
            l1.symbols
                .iter()
                .filter(|symbol| symbol.name == hit.name && kind_to_str(symbol.kind) == hit.kind)
                .min_by_key(|symbol| (symbol.start_row + 1).abs_diff(hit.line))
        }) else {
            continue;
        };
        hit.line = moved.start_row + 1;
        hit.signature.clone_from(&moved.signature);
    }
}
