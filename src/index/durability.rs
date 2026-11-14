//! The scan-generation stamp that ties the Fjall index to the file map (`index.msgpack`).
//!
//! Batch commits reach the OS but are not fsynced, while the file map is. After a power loss the
//! file map could therefore name a file as indexed and unchanged whose rows never reached the disk,
//! and every later scan skipped that file. [`IndexDb::seal_scan`] closes the window from the index
//! side: it stamps a generation and fsyncs the journal in one batch, and the caller writes the same
//! number into the file map only afterwards. An index whose stamp is below the file map's lost the
//! tail of its journal and is rebuilt (see `Store::open_with_holder`).

use std::path::Path;

use fjall::PersistMode;

use super::{INDEX_DIR, IndexDb, IndexError};

const META_SCAN_GENERATION: &[u8] = b"scan_generation";

impl IndexDb {
    /// The generation the last [`Self::seal_scan`] stamped; `0` for an index no scan has sealed.
    pub fn scan_generation(&self) -> Result<u64, IndexError> {
        Ok(self
            .meta
            .get(META_SCAN_GENERATION)?
            .and_then(|bytes| <[u8; 8]>::try_from(&bytes[..]).ok())
            .map_or(0, u64::from_be_bytes))
    }

    /// Stamp the generation after both `file_map_generation` and the stored one, and fsync the
    /// journal in the same batch: every commit made through this handle before the call is on disk
    /// once it returns. Returns the stamped generation, for the caller to record in the file map.
    ///
    /// One fsync per call, so this belongs at the end of a scan's writes, never in the per-file loop.
    pub fn seal_scan(&self, file_map_generation: u64) -> Result<u64, IndexError> {
        let generation = file_map_generation.max(self.scan_generation()?) + 1;
        let mut batch = self.db.batch().durability(Some(PersistMode::SyncAll));
        batch.insert(&self.meta, META_SCAN_GENERATION, generation.to_be_bytes());
        let sealed = batch.commit().map_err(IndexError::from);
        if let Err(error) = &sealed {
            self.record_commit_error(error);
        }
        sealed.map(|()| generation)
    }
}

/// Delete the index database under `view_dir`. The caller must have dropped every handle to it.
pub(crate) fn discard_index(view_dir: &Path) -> Result<(), IndexError> {
    let dir = view_dir.join(INDEX_DIR);
    std::fs::remove_dir_all(&dir).map_err(|source| IndexError::Io { path: dir, source })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_scan_advances_past_both_sides_and_survives_a_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = IndexDb::open(dir.path()).expect("open");
        assert_eq!(db.scan_generation().expect("read"), 0, "an unsealed index reads 0");
        assert_eq!(db.seal_scan(0).expect("seal"), 1);
        assert_eq!(db.seal_scan(1).expect("seal"), 2);
        assert_eq!(
            db.seal_scan(9).expect("seal"),
            10,
            "a file map ahead of the index is overtaken"
        );
        assert_eq!(
            db.seal_scan(0).expect("seal"),
            11,
            "a file map behind the index never rewinds the stamp"
        );
        drop(db);
        let reopened = IndexDb::open(dir.path()).expect("reopen");
        assert_eq!(reopened.scan_generation().expect("read"), 11);
    }
}
