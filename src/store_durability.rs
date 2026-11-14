//! Recovery for a store whose Fjall index fell behind its file map after it was opened without the
//! writer open's check: a store opened read-only and then scanned into in place.

use super::{HashRefs, Store, StoreError};

impl Store {
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
    /// the index lost. Returns whether it did; the caller must then run a full scan.
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
        Ok(true)
    }
}
