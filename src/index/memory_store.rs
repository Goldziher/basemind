//! The per-repo-group store for user-authored state: `memory_by_key`, `memory_archive` and
//! `proposals`.
//!
//! These keyspaces used to live in each view's `index.fjall`, so budget eviction of a cold workspace
//! deleted its memory and every seeded worktree forked its own copy. They now live in one Fjall
//! database per repo group (`cache/repos/<group>/memory.fjall`, see
//! [`crate::store_layout::memory_store_dir`]), shared by every worktree of the clone and never
//! listed by any garbage collector.
//!
//! Fjall's directory lock is exclusive, and the daemon holds several workspaces of one group in the
//! same process, so [`open_shared`] hands out one [`MemoryDb`] per directory per process.
//!
//! [`migrate_legacy`] folds the rows an older build left in a view's own keyspaces into the group
//! store, last writer wins by the record's timestamp, then drops those keyspaces from the view.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

use fjall::{Database, Keyspace, PersistMode};
use serde::Deserialize;

use super::{FJALL_MAX_JOURNAL_BYTES, FJALL_WORKER_THREADS, IndexError, open_keyspace};

const MEMORY_CACHE_BYTES: u64 = 8 * 1_024 * 1_024;
const MEMORY_BY_KEY: &str = "memory_by_key";
const MEMORY_ARCHIVE: &str = "memory_archive";
const PROPOSALS: &str = "proposals";

/// An open group memory database plus its three keyspaces.
pub(crate) struct MemoryDb {
    db: Database,
    pub(crate) memory_by_key: Keyspace,
    pub(crate) memory_archive: Keyspace,
    pub(crate) proposals: Keyspace,
}

impl MemoryDb {
    fn open(dir: &Path) -> Result<Self, IndexError> {
        std::fs::create_dir_all(dir).map_err(|source| IndexError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let db = Database::builder(dir)
            .cache_size(MEMORY_CACHE_BYTES)
            .worker_threads(FJALL_WORKER_THREADS)
            .max_journaling_size(FJALL_MAX_JOURNAL_BYTES)
            .open()?;
        let memory_by_key = open_keyspace(&db, MEMORY_BY_KEY)?;
        let memory_archive = open_keyspace(&db, MEMORY_ARCHIVE)?;
        let proposals = open_keyspace(&db, PROPOSALS)?;
        Ok(Self {
            db,
            memory_by_key,
            memory_archive,
            proposals,
        })
    }

    /// Names of the keyspaces this database holds.
    #[cfg(test)]
    pub(crate) fn keyspace_names(&self) -> Vec<String> {
        self.db.list_keyspace_names().iter().map(ToString::to_string).collect()
    }
}

static OPEN_STORES: Mutex<Option<HashMap<PathBuf, Weak<MemoryDb>>>> = Mutex::new(None);

/// Open (or join) the memory database at `dir`. Every caller in this process asking for the same
/// directory gets the same handle, so several worktrees of one repo group never fight over the
/// Fjall lock.
pub(crate) fn open_shared(dir: &Path) -> Result<Arc<MemoryDb>, IndexError> {
    let key = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let mut guard = OPEN_STORES.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let stores = guard.get_or_insert_with(HashMap::new);
    stores.retain(|_, weak| weak.strong_count() > 0);
    if let Some(open) = stores.get(&key).and_then(Weak::upgrade) {
        return Ok(open);
    }
    let opened = Arc::new(MemoryDb::open(dir)?);
    stores.insert(key, Arc::downgrade(&opened));
    Ok(opened)
}

/// The only fields the merge needs; both `MemoryRecord` and `ProposalRecord` are stored as named
/// msgpack maps, so unknown fields are skipped and this decodes either.
#[derive(Deserialize, Default)]
struct Stamp {
    #[serde(default)]
    created_at: i64,
    #[serde(default)]
    updated_at: i64,
}

/// A record's last-write time: `updated_at` for memory rows, `created_at` for proposals (which
/// carry no `updated_at`). Undecodable bytes sort oldest so a readable row always wins.
fn stamp_of(bytes: &[u8]) -> i64 {
    rmp_serde::from_slice::<Stamp>(bytes)
        .map(|stamp| stamp.updated_at.max(stamp.created_at))
        .unwrap_or(i64::MIN)
}

/// Whether `incoming` should replace `existing`: the newer timestamp wins, ties break on the larger
/// payload so the outcome never depends on which worktree migrated first.
fn incoming_wins(incoming: &[u8], existing: &[u8]) -> bool {
    (stamp_of(incoming), incoming) > (stamp_of(existing), existing)
}

fn merge_keyspace(from: &Keyspace, into: &Keyspace) -> Result<(), IndexError> {
    for guard in from.iter() {
        let (key, value) = guard.into_inner()?;
        let replace = match into.get(&key)? {
            Some(existing) => incoming_wins(&value, &existing),
            None => true,
        };
        if replace {
            into.insert(key.to_vec(), value.to_vec())?;
        }
    }
    Ok(())
}

/// Fold the memory rows an older build left in `view_db` into `memory`, then drop those keyspaces
/// from the view. Safe to repeat and safe when several worktrees migrate: a view with no legacy
/// keyspaces is a no-op, and the merge is commutative. The group store is persisted before the
/// view's copy is deleted, so a crash between the two leaves duplicates, never a loss.
pub(crate) fn migrate_legacy(view_db: &Database, memory: &MemoryDb) -> Result<(), IndexError> {
    let targets = [
        (MEMORY_BY_KEY, &memory.memory_by_key),
        (MEMORY_ARCHIVE, &memory.memory_archive),
        (PROPOSALS, &memory.proposals),
    ];
    for (name, target) in targets {
        if !view_db.keyspace_exists(name) {
            continue;
        }
        let legacy = open_keyspace(view_db, name)?;
        merge_keyspace(&legacy, target)?;
        memory.db.persist(PersistMode::SyncAll)?;
        view_db.delete_keyspace(legacy)?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "memory_store_tests.rs"]
mod tests;
