//! The scan-generation stamp that ties the Fjall index to the file map (`index.msgpack`).
//!
//! Batch commits reach the OS but are not fsynced, while the file map is. After a power loss the
//! file map could therefore name a file as indexed and unchanged whose rows never reached the disk,
//! and every later scan skipped that file. [`IndexDb::seal_scan`] closes the window from the index
//! side: it stamps a generation and fsyncs the journal in one batch, and the caller writes the same
//! number into the file map only afterwards. An index whose stamp is below the file map's lost the
//! tail of its journal and is rebuilt: discarded by a writer open (`Store::open_with_holder`),
//! emptied in place by a scan into a store that was opened without one
//! (`Store::rebuild_index_if_behind`). [`IndexDb::seal_scan`] refuses to stamp such an index, so no
//! path can make the two sides agree again without that rebuild.

use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

use fjall::PersistMode;

use super::view::Part;
use super::{INDEX_DIR, INDEX_SCHEMA_VER, IndexDb, IndexError, META_SCHEMA_VER};

const META_SCAN_GENERATION: &[u8] = b"scan_generation";
/// Name prefix of an index directory renamed aside by [`discard_dir`] and not yet deleted.
const DISCARD_PREFIX: &str = "index.fjall.discard-";

impl IndexDb {
    /// The generation the last [`Self::seal_scan`] stamped; `0` for an index no scan has sealed.
    pub fn scan_generation(&self) -> Result<u64, IndexError> {
        Ok(self
            .meta
            .get(META_SCAN_GENERATION)?
            .and_then(|bytes| <[u8; 8]>::try_from(&bytes[..]).ok())
            .map_or(0, u64::from_be_bytes))
    }

    /// Stamp the next generation and fsync the journal in the same batch: every commit made through
    /// this handle before the call is on disk once it returns. Returns the stamped generation, for
    /// the caller to record in the file map.
    ///
    /// Refuses a file map ahead of the index ([`IndexError::BehindFileMap`]): stamping past it would
    /// make the two sides agree over rows the index lost.
    ///
    /// One fsync per call, so this belongs at the end of a scan's writes, never in the per-file loop.
    pub fn seal_scan(&self, file_map_generation: u64) -> Result<u64, IndexError> {
        let stored = self.scan_generation()?;
        if file_map_generation > stored {
            return Err(IndexError::BehindFileMap {
                index: stored,
                file_map: file_map_generation,
            });
        }
        let generation = stored + 1;
        let mut batch = self.db.batch().durability(Some(PersistMode::SyncAll));
        batch.insert(&self.meta, META_SCAN_GENERATION, generation.to_be_bytes());
        let sealed = batch.commit().map_err(IndexError::from);
        if let Err(error) = &sealed {
            self.record_commit_error(error);
        }
        sealed?;
        self.synced_seqno.store(self.db.seqno(), Ordering::Release);
        Ok(generation)
    }

    /// Fsync the journal if anything was written since the last [`Self::seal_scan`] or call of this,
    /// and report whether it did. For the rows a scan writes after its seal (the resolve pass, BM25
    /// statistics): the file map already calls their files unchanged, so no later scan rewrites them.
    ///
    /// At most one fsync per call, so once at the end of a scan, never per file or per batch.
    pub fn sync_tail(&self) -> Result<bool, IndexError> {
        let seqno = self.db.seqno();
        if seqno == self.synced_seqno.load(Ordering::Acquire) {
            return Ok(false);
        }
        let synced = self.db.persist(PersistMode::SyncAll).map_err(IndexError::from);
        if let Err(error) = &synced {
            self.record_commit_error(error);
        }
        synced?;
        self.synced_seqno.store(seqno, Ordering::Release);
        Ok(true)
    }

    #[cfg(test)]
    pub(crate) fn has_unsynced_tail(&self) -> bool {
        self.db.seqno() != self.synced_seqno.load(Ordering::Acquire)
    }

    /// Empty every keyspace in place, leaving a current-schema index no scan has sealed. The
    /// in-place form of [`discard_index`], for a store that cannot drop every clone of its handle.
    pub(crate) fn clear_for_rebuild(&self) -> Result<(), IndexError> {
        for part in Part::ALL {
            self.keyspace(part).clear()?;
        }
        self.meta.insert(META_SCHEMA_VER, INDEX_SCHEMA_VER.to_be_bytes())?;
        Ok(())
    }
}

/// Delete the index database under `view_dir`. The caller must have dropped every handle to it.
pub(crate) fn discard_index(view_dir: &Path) -> Result<(), IndexError> {
    discard_dir(&view_dir.join(INDEX_DIR))
}

/// Delete the database directory `dir` so that a crash at any point leaves either the whole
/// database or none of it at `dir`: a half-deleted one can be neither recovered nor created over.
/// It is renamed aside first, the rename made durable, and only then deleted; a deletion that does
/// not finish is completed by [`sweep_discarded`] on a later open.
pub(super) fn discard_dir(dir: &Path) -> Result<(), IndexError> {
    static SEQUENCE: AtomicU32 = AtomicU32::new(0);
    let aside = dir.with_file_name(format!(
        "{DISCARD_PREFIX}{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::rename(dir, &aside).map_err(|source| IndexError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    if let Some(parent) = dir.parent()
        && let Err(error) = crate::store_base::sync_directory(parent)
    {
        tracing::warn!(%error, "could not fsync the view directory after setting the index aside");
    }
    if let Err(error) = std::fs::remove_dir_all(&aside) {
        tracing::warn!(%error, path = %aside.display(), "discarded index not fully deleted; the next open retries");
    }
    Ok(())
}

/// Delete what an interrupted [`discard_dir`] left under `view_dir`. Best-effort: a leftover costs
/// disk space only.
pub(super) fn sweep_discarded(view_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(view_dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(DISCARD_PREFIX)
            && let Err(error) = std::fs::remove_dir_all(entry.path())
        {
            tracing::warn!(%error, path = %entry.path().display(), "could not delete a discarded index");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_scan_advances_one_generation_at_a_time_and_survives_a_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = IndexDb::open(dir.path()).expect("open");
        assert_eq!(db.scan_generation().expect("read"), 0, "an unsealed index reads 0");
        assert_eq!(db.seal_scan(0).expect("seal"), 1);
        assert_eq!(db.seal_scan(1).expect("seal"), 2);
        assert_eq!(
            db.seal_scan(0).expect("seal"),
            3,
            "a file map behind the index never rewinds the stamp"
        );
        drop(db);
        let reopened = IndexDb::open(dir.path()).expect("reopen");
        assert_eq!(reopened.scan_generation().expect("read"), 3);
    }

    /// Stamping past a file map that is ahead would make an index that lost rows look current.
    #[test]
    fn seal_scan_refuses_a_file_map_ahead_of_the_index() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = IndexDb::open(dir.path()).expect("open");
        assert_eq!(db.seal_scan(0).expect("seal"), 1);
        let refused = db.seal_scan(5).expect_err("the index is four generations behind");
        assert!(
            matches!(refused, IndexError::BehindFileMap { index: 1, file_map: 5 }),
            "got {refused:?}"
        );
        assert_eq!(db.scan_generation().expect("read"), 1, "a refused seal stamps nothing");
    }

    #[test]
    fn sync_tail_syncs_only_what_was_written_after_the_seal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = IndexDb::open(dir.path()).expect("open");
        db.seal_scan(0).expect("seal");
        assert!(!db.has_unsynced_tail());
        assert!(!db.sync_tail().expect("sync"), "nothing was written after the seal");

        db.set_import_probes_complete(true).expect("a write after the seal");
        assert!(db.has_unsynced_tail());
        assert!(db.sync_tail().expect("sync"));
        assert!(!db.has_unsynced_tail());
        assert!(
            !db.sync_tail().expect("sync"),
            "the tail is synced once, not on every call"
        );
    }

    #[test]
    fn clear_for_rebuild_empties_the_index_and_its_generation_but_keeps_the_schema() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = IndexDb::open(dir.path()).expect("open");
        db.keyspace(Part::SymbolsByPath).insert("row", "value").expect("insert");
        db.seal_scan(0).expect("seal");

        db.clear_for_rebuild().expect("clear");
        assert!(db.keyspace(Part::SymbolsByPath).iter().next().is_none());
        assert_eq!(db.scan_generation().expect("read"), 0);
        drop(db);
        assert_eq!(
            super::super::peek_index_schema_ver(dir.path()).expect("peek"),
            Some(INDEX_SCHEMA_VER),
            "the schema stamp survived the clear"
        );
    }

    fn discarded_siblings(view_dir: &Path) -> Vec<String> {
        std::fs::read_dir(view_dir)
            .expect("read view dir")
            .map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(DISCARD_PREFIX))
            .collect()
    }

    #[test]
    fn discard_index_removes_the_database_and_leaves_nothing_aside() {
        let dir = tempfile::tempdir().expect("tempdir");
        drop(IndexDb::open(dir.path()).expect("open"));
        discard_index(dir.path()).expect("discard");
        assert!(!dir.path().join(INDEX_DIR).exists());
        assert_eq!(discarded_siblings(dir.path()), Vec::<String>::new());
    }

    /// A crash between the rename and the delete leaves the database aside; the next open removes it.
    #[test]
    fn an_index_left_aside_by_an_interrupted_discard_is_swept_on_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = IndexDb::open(dir.path()).expect("open");
        db.seal_scan(0).expect("seal");
        drop(db);
        std::fs::rename(
            dir.path().join(INDEX_DIR),
            dir.path().join(format!("{DISCARD_PREFIX}4242-0")),
        )
        .expect("simulate the crash");

        let reopened = IndexDb::open(dir.path()).expect("open over a half-finished discard");
        assert_eq!(
            reopened.scan_generation().expect("read"),
            0,
            "a fresh index took its place"
        );
        assert_eq!(discarded_siblings(dir.path()), Vec::<String>::new());
    }
}
