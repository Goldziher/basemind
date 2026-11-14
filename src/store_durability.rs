//! Recovery for a store whose Fjall index fell behind its file map: the writer open's check, and
//! its in-place form for a store opened read-only and then scanned into.
//!
//! Either way the rebuilt index counts its generations from 0 again, so the emptied file map is
//! written at once. A file map that still listed every file would otherwise wait on disk for the new
//! count to reach its generation (each flush seals the index before the write that can fail) and then
//! vouch for a rebuild that never finished.

use std::path::Path;

use super::{HashRefs, Index, Store, StoreError, open_index_with_retry};
use crate::index::IndexDb;
use crate::index::view::IndexView;

/// Open the Fjall index for a writer, discarding one that is behind `index` (the file map just read).
/// Returns the handle and whether the index must be rebuilt by a full scan, in which case `index`
/// lists no files any more.
pub(super) fn open_index_for_writer(
    root: &Path,
    view: &str,
    view_dir: &Path,
    index: &mut Index,
) -> Result<(IndexDb, bool), StoreError> {
    let mut index_db = open_index_with_retry(root, view_dir)?;
    // The file map is fsynced only after the index sealed the same generation, so an index
    // stamped below it lost the tail of its journal. Which rows went is unknowable, so the whole
    // index is discarded and the empty-index rebuild below takes over. ~keep
    if !index_db.wiped_on_open() && index.generation > index_db.scan_generation()? {
        tracing::warn!(
            view,
            "code index is older than its file map (unclean shutdown); rebuilding it"
        );
        drop(index_db);
        crate::index::durability::discard_index(view_dir)?;
        index_db = open_index_with_retry(root, view_dir)?;
    }
    // A replaced index starts unsealed, and its emptied file map vouches for nothing. ~keep
    index.generation = index.generation.min(index_db.scan_generation()?);
    let index_rebuild_needed =
        !index.files.is_empty() && (index_db.wiped_on_open() || index_db.view().symbols_index_is_empty());
    if index_rebuild_needed {
        index.files.clear();
    }
    Ok((index_db, index_rebuild_needed))
}

impl Store {
    /// Write the file map [`open_index_for_writer`] emptied, before any scan can die over the full
    /// one (see the module docs). [`Store::index_rebuild_needed`] stays set for the scan that follows.
    pub(super) fn persist_file_map_emptied_on_open(&mut self) -> Result<(), StoreError> {
        if self.index_rebuild_needed {
            self.flush()?;
        }
        Ok(())
    }

    /// Whether an incremental scan would be wrong for this store: a rebuild is pending, or the file
    /// map lists nothing. The second case is the first after a process died between writing the
    /// emptied file map and finishing the rebuild scan: the next open sees an empty map, so
    /// [`Store::index_rebuild_needed`] is unset, and scanning only the given paths would leave the
    /// rest of the tree unindexed.
    pub fn needs_full_scan(&self) -> bool {
        self.index_rebuild_needed || (self.index.files.is_empty() && self.index.doc_files.is_empty())
    }

    /// Whether the file map vouches for a scan generation the index never sealed (see
    /// [`crate::index::durability`]). `false` for a store with no index handle.
    pub(crate) fn index_is_behind_file_map(&self) -> Result<bool, StoreError> {
        match self.index_db.as_ref() {
            Some(db) => Ok(self.index.generation > db.scan_generation()?),
            None => Ok(false),
        }
    }

    /// Take the recovery [`Store::open_with_holder`] takes for an index behind its file map, on a
    /// store that is already open: empty the index and forget the file map, so the scan that follows
    /// re-indexes every file (from its cached blob) instead of skipping it as unchanged over rows
    /// the index lost. The emptied file map is written before returning (see the module docs).
    /// Returns whether it did; the caller must then run a full scan.
    ///
    /// The index is emptied in place rather than discarded: clones of the handle may be live, and a
    /// reopen fails while one is.
    pub(crate) fn rebuild_index_if_behind(&mut self) -> Result<bool, StoreError> {
        if !self.index_is_behind_file_map()? {
            return Ok(false);
        }
        tracing::warn!(
            view = %self.view,
            "code index is older than its file map (unclean shutdown); rebuilding it"
        );
        if let Some(db) = self.index_db.as_ref() {
            db.clear_for_rebuild()?;
        }
        self.index.files.clear();
        self.hash_refs = HashRefs::of(&self.index.files);
        self.index.generation = 0;
        self.flush()?;
        Ok(true)
    }
}
