//! Flushing and compacting the git-history database after a sync. A full rebuild compacts every
//! keyspace once; an append, which may add a single commit, only flushes and merges when what it
//! left behind is worth the rewrite.

use fjall::Keyspace;

use super::{GitHistoryError, LocalDb};

/// Unflushed memtable bytes an append may leave behind. Below it the rows stay in the memtables
/// and the journal, which fjall flushes on its own as they fill; above it the append flushes them,
/// so a daemon holding many repositories does not keep every keyspace's memtable growing toward
/// its 8 MiB ceiling.
pub(super) const APPEND_WRITE_BUFFER_BYTES: u64 = 8 * 1_024 * 1_024;

/// On-disk size up to which a keyspace holding more than one table is merged back into one after
/// an append. Each flush adds a level-0 table and leveled compaction merges them only at four, so a
/// small keyspace would otherwise hold several tables, each an open descriptor; a larger keyspace
/// is left to fjall's own compaction, where a major one would rewrite it on every flush.
const SMALL_KEYSPACE_BYTES: u64 = 16 * 1_024 * 1_024;

impl LocalDb {
    fn keyspaces(&self) -> [&Keyspace; 8] {
        [
            &self.meta,
            &self.commit_by_ord,
            &self.ord_by_sha,
            &self.path_id_by_path,
            &self.path_by_id,
            &self.path_to_ords,
            &self.commit_text_by_ord,
            &self.term_to_ords,
        ]
    }

    /// Flush every keyspace's memtable to a disk segment and major-compact it. After a bulk build
    /// the data otherwise lingers in the write-ahead journal (hundreds of MB on a deep repo); this
    /// reclaims it into minimal, query-ready segments. Seconds on a deep history, so only a full
    /// rebuild runs it; an append runs [`Self::settle_append`].
    pub(super) fn compact(&self) -> Result<(), GitHistoryError> {
        for keyspace in self.keyspaces() {
            keyspace.rotate_memtable_and_wait()?;
            keyspace.major_compact()?;
        }
        Ok(())
    }

    /// The cheap end of an append: sync the journal, so the stamped head is durable as it was when
    /// every sync flushed; once more than `flush_above` bytes sit unflushed, flush every memtable;
    /// then merge each small keyspace holding more than one table. An append of a few commits
    /// changes no table and rewrites none. A failure only logs: the head is already stamped and the
    /// rows stay in the memtables and the journal.
    pub(super) fn settle_append(&self, flush_above: u64) {
        if let Err(error) = self.db.persist(fjall::PersistMode::SyncAll) {
            tracing::warn!(%error, "git-history journal sync failed; the append is durable at the next flush");
        }
        if self.db.write_buffer_size() > flush_above {
            for keyspace in self.keyspaces() {
                if let Err(error) = keyspace.rotate_memtable_and_wait() {
                    tracing::warn!(%error, "git-history memtable flush failed; its rows stay in memory");
                    break;
                }
            }
        }
        for keyspace in self.keyspaces() {
            if keyspace.table_count() > 1
                && keyspace.disk_space() <= SMALL_KEYSPACE_BYTES
                && let Err(error) = keyspace.major_compact()
            {
                tracing::warn!(%error, "git-history keyspace compaction failed; its tables merge later");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::git_history::GitHistoryIndex;

    /// Appends that each flush (the threshold at zero) add a table per keyspace they wrote; the
    /// small keyspaces are merged back, so the count stays at one however many appends run.
    #[test]
    fn flushing_appends_keep_each_small_keyspace_at_one_table() {
        let dir = tempfile::tempdir().expect("tempdir");
        let index = GitHistoryIndex::open(dir.path()).expect("open");
        let db = index.local();
        for append in 0u32..6 {
            for keyspace in db.keyspaces() {
                for row in 0u32..64 {
                    let key = [append.to_be_bytes(), row.to_be_bytes()].concat();
                    keyspace.insert(key, vec![7u8; 256]).expect("insert");
                }
            }
            db.settle_append(0);
            assert_eq!(db.db.write_buffer_size(), 0, "append {append}: every memtable flushed");
            for keyspace in db.keyspaces() {
                assert_eq!(
                    keyspace.table_count(),
                    1,
                    "append {append}: {:?} holds more than one table",
                    keyspace.path()
                );
            }
        }
    }

    /// Below the threshold an append leaves its rows in the memtables and the tables untouched.
    #[test]
    fn a_small_append_under_the_threshold_flushes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let index = GitHistoryIndex::open(dir.path()).expect("open");
        let db = index.local();
        db.commit_by_ord.insert([1u8; 4], [2u8; 64]).expect("insert");
        db.settle_append(super::APPEND_WRITE_BUFFER_BYTES);
        assert!(db.db.write_buffer_size() > 0, "the row stays in the memtable");
        assert_eq!(db.commit_by_ord.table_count(), 0, "no table was written");
    }
}
