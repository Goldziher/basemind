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
    let mut index_db = match open_index_with_retry(root, view_dir) {
        // A damaged database is rebuildable cache, and left in place it fails every open until
        // someone clears the cache by hand. Only this open, which holds the writer lock, may
        // replace it; the checks below then find an unsealed index and order the rebuild. ~keep
        Err(crate::index::IndexError::Corrupt(error)) => {
            tracing::warn!(view, %error, "code index is damaged; discarding it for a rebuild");
            crate::index::durability::discard_index(view_dir)?;
            open_index_with_retry(root, view_dir)?
        }
        opened => opened?,
    };
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
    let sealed = index_db.scan_generation()?;
    index.generation = index.generation.min(sealed);
    // An index some scan sealed holds what the file map says, whatever it holds: a repository whose
    // files yield no symbols has no symbol rows to lose. Only an index nothing ever sealed (a
    // replaced one, or one from before generations) is judged by looking empty. ~keep
    let never_sealed = sealed == 0 && index.generation == 0;
    let index_rebuild_needed = !index.files.is_empty()
        && (index_db.wiped_on_open() || (never_sealed && index_db.view().symbols_index_is_empty()));
    if index_rebuild_needed {
        index.empty_for_rebuild();
    }
    // An earlier process emptied the file map and died before its rebuild scan finished. ~keep
    Ok((index_db, index_rebuild_needed || index.rebuild_pending))
}

/// Remove the file map of a view written for an earlier blob store layout, and the index built
/// from it: the index is of the current schema, so nothing else would empty it, and rows of files
/// deleted since would outlive the map that knew of them. The caller holds the workspace lock.
pub(super) fn discard_view_of_a_stale_layout(view_dir: &Path) -> Result<(), StoreError> {
    super::wipe_view(view_dir)?;
    if view_dir.join(crate::index::INDEX_DIR).exists() {
        crate::index::durability::discard_index(view_dir)?;
    }
    Ok(())
}

/// What a reader is told when [`Store::rescan_owed`]: one line, naming the command that fixes it.
pub const RESCAN_OWED: &str = "the code index of this workspace must be rebuilt (an older basemind wrote it, or a \
     rebuild was interrupted): run `basemind scan` (MCP: `index_rescan`). Until then symbol, reference and call \
     queries answer from cached blobs or come back empty";

impl Index {
    /// Forget every file so a full scan re-indexes them, and record that it must. The hashes the
    /// files held stay pinned for the blob sweep: the rebuild reads those blobs, and an emptied
    /// map would otherwise leave them unreferenced for as long as the rebuild takes.
    pub(super) fn empty_for_rebuild(&mut self) {
        self.rebuild_pins
            .extend(self.files.drain().map(|(_, entry)| entry.hash_hex));
        self.rebuild_pins.sort_unstable();
        self.rebuild_pins.dedup();
        self.rebuild_pending = true;
    }

    /// A full pass re-indexed every file: the rebuild is done and its pins are released.
    pub(crate) fn rebuild_done(&mut self) {
        self.rebuild_pending = false;
        self.rebuild_pins = Vec::new();
    }
}

impl Store {
    /// Whether the index cannot answer until a scan rebuilds it: this open found an index it must
    /// not serve, or the file map records a rebuild that never finished.
    pub fn rescan_owed(&self) -> bool {
        self.index_rebuild_needed || self.index.rebuild_pending
    }

    /// Write the file map [`open_index_for_writer`] emptied, before any scan can die over the full
    /// one (see the module docs). [`Store::index_rebuild_needed`] stays set for the scan that follows.
    pub(super) fn persist_file_map_emptied_on_open(&mut self) -> Result<(), StoreError> {
        if self.index_rebuild_needed {
            self.flush()?;
        }
        Ok(())
    }

    /// Whether this store was opened holding the workspace lock, or has taken it since.
    pub(crate) fn holds_writer_lock(&self) -> bool {
        self._lock.is_some()
    }

    /// Whether an incremental scan would be wrong for this store: a rebuild is pending, the file
    /// map lists nothing, or its blobs are to be verified, which only a full pass does. The pending rebuild is read from the file map itself
    /// ([`Index::rebuild_pending`]), so it outlives a process that died between writing the emptied
    /// map and finishing the rebuild scan, whatever else the map still lists: the rebuild empties
    /// `files` only, and a store with documents would otherwise pass for scanned.
    pub fn needs_full_scan(&self) -> bool {
        crate::scan_evidence::settle_dead_hosts();
        self.index_rebuild_needed
            || self.index.rebuild_pending
            || (self.index.files.is_empty() && self.index.doc_files.is_empty())
            || (self.layered_read.is_none() && crate::scan_evidence::blobs_need_verifying(&self.basemind_dir))
    }

    /// Open, for the scan about to write into this store, the index a read-only open left alone
    /// for being of an older schema. This is where that index is replaced: the same recovery a
    /// writer open takes, file map emptied and written at once. A no-op for every other store.
    ///
    /// The recovery wipes and rewrites, so it runs under the workspace lock a writer open holds.
    /// A store opened read-only has none: it is taken here and kept, and the scan is refused with
    /// [`StoreError::Locked`] while another process holds it.
    pub(crate) fn open_index_owed_to_scan(&mut self) -> Result<(), StoreError> {
        if self.index_db.is_some() || !self.index_rebuild_needed || self.layered_read.is_some() {
            return Ok(());
        }
        if self._lock.is_none() {
            self._lock = Some(super::acquire_lock_as(&self.basemind_dir, super::LockHolder::Rescan)?);
        }
        if matches!(
            super::read_index(&self.view_dir),
            Err(StoreError::StaleBlobLayout { .. })
        ) {
            discard_view_of_a_stale_layout(&self.view_dir)?;
        }
        let (index_db, _) = open_index_for_writer(&self.root, &self.view, &self.view_dir, &mut self.index)?;
        self.hash_refs = HashRefs::of(&self.index.files);
        self.index_db = Some(index_db);
        self.flush()
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
        self.index.empty_for_rebuild();
        self.hash_refs = HashRefs::of(&self.index.files);
        self.index.generation = 0;
        self.flush()?;
        Ok(true)
    }
}
