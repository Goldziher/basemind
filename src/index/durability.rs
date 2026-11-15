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

use fjall::{Keyspace, PersistMode};

use super::view::Part;
use super::{INDEX_DIR, INDEX_SCHEMA_VER, IndexDb, IndexError, META_SCHEMA_VER};

const META_SCAN_GENERATION: &[u8] = b"scan_generation";
/// Name prefix of an index directory renamed aside by [`discard_dir`] and not yet deleted.
pub(crate) const DISCARD_PREFIX: &str = "index.fjall.discard-";

/// Sealed journals fjall keeps only until every keyspace they cover has flushed. `meta` and the
/// other lightly written keyspaces never fill a memtable on their own, so without a flush of every
/// keyspace one straggler keeps each sealed journal (64 MiB preallocated) alive for good.
const RECLAIM_SEALED_JOURNALS: usize = 1;

/// Flush the active memtable of each of `keyspaces` to disk and wait for it. Stops at the first
/// error. A journal is deleted once every keyspace it covers has flushed past it, so this releases
/// the sealed ones.
pub(super) fn rotate_memtables<'a>(keyspaces: impl IntoIterator<Item = &'a Keyspace>) -> Result<(), IndexError> {
    for keyspace in keyspaces {
        keyspace.rotate_memtable_and_wait()?;
    }
    Ok(())
}

/// Tell a database that is damaged on disk from one that merely could not be opened right now. Only
/// the first may be discarded: a lock held elsewhere, an I/O error (a full disk, a permission) and a
/// format version this fjall does not know all leave data that is, or may be, perfectly good.
pub(super) fn classify_open_error(error: fjall::Error) -> IndexError {
    use fjall::{Error, LsmError};
    let damaged = match &error {
        Error::JournalRecovery(_)
        | Error::Decompress(_)
        | Error::InvalidTrailer
        | Error::InvalidTag(_)
        | Error::Unrecoverable => true,
        Error::Storage(inner) => !matches!(inner, LsmError::Io(_) | LsmError::InvalidVersion(_)),
        _ => false,
    };
    if damaged {
        IndexError::Corrupt(error)
    } else {
        IndexError::Fjall(error)
    }
}

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
        // Read before the commit: a write landing after the fsync must not be recorded as synced.
        // The batch takes one sequence number, so any other write leaves the two unequal. ~keep
        let synced_seqno = self.db.seqno() + 1;
        let mut batch = self.db.batch().durability(Some(PersistMode::SyncAll));
        batch.insert(&self.meta, META_SCAN_GENERATION, generation.to_be_bytes());
        let sealed = batch.commit().map_err(IndexError::from);
        if let Err(error) = &sealed {
            self.record_commit_error(error);
        }
        sealed?;
        self.synced_seqno.store(synced_seqno, Ordering::Release);
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

    /// Flush every keyspace so the sealed journals can be deleted. Call after [`Self::seal_scan`] and
    /// [`Self::sync_tail`]: the rows are already durable in the journal, so a failure here costs disk
    /// space only and is logged, not returned. Returns whether the flush completed. fjall's worker
    /// thread deletes the released journals after its last flush, so the files and
    /// `journal_count` may trail the return by a moment.
    ///
    /// Waits for every flush, so it belongs at the end of a scan, never in the per-file loop. It
    /// cannot shrink the ACTIVE journal: fjall seals that one only at a memtable flush after it
    /// passes 64 MB, so an index that never wrote that much keeps its one journal however it is
    /// flushed, and flushing it only writes segments sooner.
    pub fn reclaim_journal(&self) -> bool {
        #[cfg(test)]
        {
            self.fault.rotations.fetch_add(1, Ordering::AcqRel);
            if self
                .fault
                .injected_rotate_failures
                .try_update(Ordering::AcqRel, Ordering::Acquire, |left| left.checked_sub(1))
                .is_ok()
            {
                tracing::warn!("index journal reclaim failed: injected");
                return false;
            }
        }
        let keyspaces = Part::ALL
            .into_iter()
            .map(|part| self.keyspace(part))
            .chain(std::iter::once(&self.embeddings));
        rotate_memtables(keyspaces)
            .inspect_err(|error| tracing::warn!(%error, "index journal reclaim failed; the journal stays on disk"))
            .is_ok()
    }

    /// [`Self::reclaim_journal`], but only once fjall holds a sealed journal, i.e. the index has
    /// written past a journal's worth (64 MB) since the last flush: the case a straggler keyspace
    /// keeps alive. Reads fjall's in-memory journal count, so a scan or a watcher batch that has not
    /// costs nothing; a flush of a small index would only add segments and delay the scan.
    pub fn reclaim_journal_if_grown(&self) -> bool {
        self.db.journal_count() > RECLAIM_SEALED_JOURNALS && self.reclaim_journal()
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
pub(crate) fn sweep_discarded(view_dir: &Path) {
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

    fn journal_files(dir: &Path) -> usize {
        std::fs::read_dir(dir.join(INDEX_DIR))
            .expect("read index dir")
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "jnl"))
            .count()
    }

    /// How long [`await_journal_count`] gives fjall's worker thread.
    const WORKER_SETTLE: std::time::Duration = std::time::Duration::from_secs(120);

    /// Waits until fjall counts `expected` journals. Its worker thread seals a journal when it takes
    /// up a flush and deletes a released one after the flush, in both cases after
    /// `rotate_memtable_and_wait` may have returned, and fjall has no call that waits for either.
    /// The count and the journal files change together under fjall's journal lock.
    fn await_journal_count(db: &IndexDb, expected: usize) {
        let deadline = std::time::Instant::now() + WORKER_SETTLE;
        while db.db.journal_count() != expected {
            assert!(
                std::time::Instant::now() < deadline,
                "fjall still counts {} journals, not {expected}",
                db.db.journal_count()
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// Writes past one journal's worth into one keyspace so fjall seals the journal, leaving `meta`
    /// unflushed: the straggler that keeps a sealed journal alive. Returns once the journal is
    /// sealed.
    fn seal_a_journal_with_a_straggler(db: &IndexDb) {
        let rows = db.keyspace(Part::SymbolsByPath);
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        for row in 0..70u32 {
            // Incompressible, or the journal's LZ4 shrinks 70 MB of writes below the seal threshold.
            let value: Vec<u8> = (0..1_024 * 1_024)
                .map(|_| {
                    state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                    (state >> 56) as u8
                })
                .collect();
            rows.insert(row.to_be_bytes(), &value).expect("insert");
        }
        rows.rotate_memtable_and_wait().expect("flush the written keyspace");
        await_journal_count(db, 2);
    }

    #[test]
    fn reclaim_journal_deletes_a_sealed_journal_a_straggler_keyspace_was_holding() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = IndexDb::open(dir.path()).expect("open");
        seal_a_journal_with_a_straggler(&db);
        assert_eq!(
            db.db.journal_count(),
            2,
            "meta never flushed, so the sealed journal stays"
        );
        assert_eq!(journal_files(dir.path()), 2);

        assert!(db.reclaim_journal());
        await_journal_count(&db, 1);
        assert_eq!(journal_files(dir.path()), 1, "only the active journal is left");
        drop(db);

        let reopened = IndexDb::open(dir.path()).expect("reopen");
        assert_eq!(reopened.keyspace(Part::SymbolsByPath).iter().count(), 70);
        assert_eq!(reopened.scan_generation().expect("read"), 0);
    }

    #[test]
    fn reclaim_journal_if_grown_leaves_a_small_journal_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = IndexDb::open(dir.path()).expect("open");
        db.seal_scan(0).expect("seal");
        assert!(!db.reclaim_journal_if_grown(), "no sealed journal, no flush");
        assert_eq!(db.fault.rotations.load(Ordering::Acquire), 0);

        seal_a_journal_with_a_straggler(&db);
        assert!(db.reclaim_journal_if_grown());
        assert_eq!(db.fault.rotations.load(Ordering::Acquire), 1);
        await_journal_count(&db, 1);
    }

    /// The rows are durable before the flush runs, so a flush that fails costs disk space, not data.
    #[test]
    fn a_failed_reclaim_keeps_every_row_and_the_next_one_succeeds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = IndexDb::open(dir.path()).expect("open");
        seal_a_journal_with_a_straggler(&db);
        db.fault.injected_rotate_failures.store(1, Ordering::Release);

        assert!(
            !db.reclaim_journal_if_grown(),
            "the injected failure is reported, not raised"
        );
        assert_eq!(db.db.journal_count(), 2, "nothing was flushed");
        assert!(db.fatal_fault().is_none(), "a failed flush latches nothing");
        assert_eq!(db.keyspace(Part::SymbolsByPath).iter().count(), 70);

        assert!(db.reclaim_journal_if_grown());
        await_journal_count(&db, 1);
    }

    /// Only a database damaged on disk may be discarded and rebuilt: a lock held elsewhere, an I/O
    /// error and a format version this fjall does not know leave data that is, or may be, good.
    #[test]
    fn only_a_database_damaged_on_disk_is_called_corrupt() {
        use fjall::{Error, LsmError};

        let io = || std::io::Error::other("the disk is full");
        let undamaged = [
            Error::Locked,
            Error::Io(io()),
            Error::InvalidVersion(None),
            Error::Poisoned,
            Error::Storage(LsmError::Io(io())),
            Error::Storage(LsmError::InvalidVersion(9)),
        ];
        for error in undamaged {
            let shown = format!("{error:?}");
            assert!(
                matches!(classify_open_error(error), IndexError::Fjall(_)),
                "{shown} is no reason to discard the index"
            );
        }
        let damaged = [
            Error::InvalidTrailer,
            Error::Unrecoverable,
            Error::InvalidTag(("tag", 1)),
            Error::Storage(LsmError::Unrecoverable),
        ];
        for error in damaged {
            let shown = format!("{error:?}");
            assert!(
                matches!(classify_open_error(error), IndexError::Corrupt(_)),
                "{shown} is damage"
            );
        }
    }

    /// A read-only open deletes nothing: an index set aside by an interrupted discard is the
    /// business of the next writer open.
    #[test]
    fn a_read_only_open_leaves_a_discarded_index_for_the_writer_open_to_sweep() {
        let dir = tempfile::tempdir().expect("tempdir");
        let aside = dir.path().join(format!("{DISCARD_PREFIX}4242-0"));
        std::fs::create_dir_all(&aside).expect("mkdir");
        std::fs::write(aside.join("table"), b"rows").expect("write");
        let memory_dir = dir.path().join("memory.fjall");

        drop(IndexDb::open_tolerating_held_memory(dir.path(), &memory_dir).expect("read-only open"));
        assert!(aside.exists(), "a read-only open deleted what a writer set aside");

        drop(IndexDb::open_with_memory(dir.path(), &memory_dir).expect("writer open"));
        assert!(!aside.exists(), "the writer open sweeps it");
    }
}
