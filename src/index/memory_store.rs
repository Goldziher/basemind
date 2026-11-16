//! The per-repo-group store for user-authored state: `memory_by_key`.
//!
//! A store written by an older build also holds the `memory_archive` and `proposals` keyspaces of
//! the removed audit and mining features. They are never opened and never deleted.
//!
//! The memory keyspace used to live in each view's `index.fjall`, so budget eviction of a cold workspace
//! deleted its memory and every seeded worktree forked its own copy. They now live in one Fjall
//! database per repo group (`cache/repos/<group>/memory.fjall`, see
//! [`crate::store_layout::memory_store_dir`]), shared by every worktree of the clone and never
//! listed by any garbage collector.
//!
//! Fjall's directory lock is exclusive, and the daemon holds several workspaces of one group in the
//! same process, so [`open_shared`] hands out one [`MemoryDb`] per directory per process.
//!
//! [`migrate_legacy`] folds the rows an older build left in a view's own `memory_by_key` keyspace
//! into the group store, last writer wins by the record's timestamp, then drops that keyspace from
//! the view.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

use fjall::{Database, Keyspace, PersistMode};
use serde::Deserialize;

use super::{FJALL_MAX_JOURNAL_BYTES, FJALL_WORKER_THREADS, IndexError, open_keyspace};

const MEMORY_CACHE_BYTES: u64 = 8 * 1_024 * 1_024;
const MEMORY_BY_KEY: &str = "memory_by_key";

/// What an open does about the store's version.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Version {
    /// A writer: refuse a newer store, stamp one that carries no version.
    Stamp,
    /// A reader: refuse a newer store and write no version. An absent store is still created,
    /// empty, with its notes keyspace.
    Check,
}

/// An open group memory database plus its keyspace.
pub(crate) struct MemoryDb {
    db: Database,
    pub(crate) memory_by_key: Keyspace,
    /// Whether a writer open has held this handle to its version.
    stamped: std::sync::atomic::AtomicBool,
    /// How many times [`MemoryDb::sync`] ran.
    #[cfg(all(test, feature = "memory"))]
    pub(crate) syncs: std::sync::atomic::AtomicU32,
}

impl MemoryDb {
    fn open(dir: &Path, version: Version) -> Result<Self, IndexError> {
        std::fs::create_dir_all(dir).map_err(|source| IndexError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let db = Database::builder(dir)
            .cache_size(MEMORY_CACHE_BYTES)
            .worker_threads(FJALL_WORKER_THREADS)
            .max_journaling_size(FJALL_MAX_JOURNAL_BYTES)
            .open()?;
        check_version(&db, version)?;
        let memory_by_key = open_keyspace(&db, MEMORY_BY_KEY)?;
        Ok(Self {
            db,
            memory_by_key,
            stamped: std::sync::atomic::AtomicBool::new(version == Version::Stamp),
            #[cfg(all(test, feature = "memory"))]
            syncs: std::sync::atomic::AtomicU32::new(0),
        })
    }

    /// Stamp a handle a reader opened first, for the writer now joining it.
    fn hold_to_version(&self, version: Version) -> Result<(), IndexError> {
        use std::sync::atomic::Ordering;
        if version == Version::Stamp && !self.stamped.load(Ordering::Acquire) {
            check_version(&self.db, Version::Stamp)?;
            self.stamped.store(true, Ordering::Release);
        }
        Ok(())
    }

    /// Fsync the journal. A note exists nowhere else, so unlike the code index (rebuilt from
    /// source by a scan) a write that only reached the OS is lost for good by a power failure.
    #[cfg(feature = "memory")]
    pub(crate) fn sync(&self) -> Result<(), IndexError> {
        #[cfg(test)]
        self.syncs.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Ok(self.db.persist(PersistMode::SyncAll)?)
    }

    /// Names of the keyspaces this database holds, the one for its version aside: that one is not
    /// sized by the index table.
    #[cfg(test)]
    pub(crate) fn keyspace_names(&self) -> Vec<String> {
        self.db
            .list_keyspace_names()
            .iter()
            .map(ToString::to_string)
            .filter(|name| name != STORE_META)
            .collect()
    }
}

/// Layout version of the memory-notes store, independent of the release: the notes are
/// user-authored, so no release may discard them. A layout change raises this and adds a forward
/// migration to [`check_version`]; nothing ever wipes the store.
pub(crate) const MEMORY_STORE_VER: u32 = 1;
const STORE_META: &str = "store_meta";
const STORE_VER_KEY: &[u8] = b"store_ver";

/// Hold the store to its version. A newer one is refused untouched. A store with none was written
/// before the version existed and has the layout of version 1: a writer stamps it, a reader takes
/// it as it is and writes no version, not even the keyspace the version lives in.
fn check_version(db: &Database, version: Version) -> Result<(), IndexError> {
    if version == Version::Check && !db.keyspace_exists(STORE_META) {
        return Ok(());
    }
    // Not one of the index's keyspaces, so not sized by their table: it holds one small key.
    let meta = db.keyspace(STORE_META, || super::index_keyspace_options(super::MEMTABLE_COLD_BYTES))?;
    let stored = meta
        .get(STORE_VER_KEY)?
        .and_then(|bytes| <[u8; 4]>::try_from(bytes.as_ref()).ok())
        .map(u32::from_be_bytes);
    match stored {
        Some(stored) if stored > MEMORY_STORE_VER => Err(IndexError::NewerMemoryStore {
            stored,
            current: MEMORY_STORE_VER,
        }),
        Some(stored) if stored == MEMORY_STORE_VER => Ok(()),
        _ if version == Version::Check => Ok(()),
        // An older layout is migrated forward here, one step per version, before the stamp.
        _ => {
            meta.insert(STORE_VER_KEY, MEMORY_STORE_VER.to_be_bytes())?;
            Ok(db.persist(PersistMode::SyncAll)?)
        }
    }
}

/// Stamp the store at `dir` as written by a basemind one layout ahead of this one.
#[cfg(all(test, feature = "memory"))]
pub(crate) fn stamp_newer_for_test(dir: &Path) {
    let db = Database::builder(dir).open().expect("open the store");
    let meta = db
        .keyspace(STORE_META, fjall::KeyspaceCreateOptions::default)
        .expect("meta keyspace");
    meta.insert(STORE_VER_KEY, (MEMORY_STORE_VER + 1).to_be_bytes())
        .expect("stamp");
    db.persist(PersistMode::SyncAll).expect("persist");
}

static OPEN_STORES: Mutex<Option<HashMap<PathBuf, Weak<MemoryDb>>>> = Mutex::new(None);

/// Open (or join) the memory database at `dir`. Every caller in this process asking for the same
/// directory gets the same handle, so several worktrees of one repo group never fight over the
/// Fjall lock.
pub(crate) fn open_shared(dir: &Path, version: Version) -> Result<Arc<MemoryDb>, IndexError> {
    // Create the directory before canonicalising: a path that does not exist yet canonicalises to
    // itself, and to its resolved form (`/var` → `/private/var` on macOS) once it does, which would
    // register one store under two keys and make the second open hit the Fjall lock.
    std::fs::create_dir_all(dir).map_err(|source| IndexError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    let key = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let mut guard = OPEN_STORES.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let stores = guard.get_or_insert_with(HashMap::new);
    stores.retain(|_, weak| weak.strong_count() > 0);
    if let Some(open) = stores.get(&key).and_then(Weak::upgrade) {
        open.hold_to_version(version)?;
        return Ok(open);
    }
    let opened = Arc::new(MemoryDb::open(dir, version)?);
    stores.insert(key, Arc::downgrade(&opened));
    Ok(opened)
}

/// Directory under a view where builds before this one kept an empty stand-in store for a handle
/// without memory. Nothing creates it any more; a writer open removes what is left of it.
const RETIRED_PLACEHOLDER_DIR: &str = "memory-detached.fjall";

/// Remove the stand-in store an older build left in `view_dir`. Best effort.
pub(crate) fn remove_retired_placeholder(view_dir: &Path) {
    let dir = view_dir.join(RETIRED_PLACEHOLDER_DIR);
    if dir.exists() {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Whether `view_db` still holds memory rows of the layout from before the group store.
pub(crate) fn holds_legacy_rows(view_db: &Database) -> bool {
    view_db.keyspace_exists(MEMORY_BY_KEY)
}

/// The only fields the merge needs; a `MemoryRecord` is stored as a named msgpack map, so unknown
/// fields are skipped.
#[derive(Deserialize, Default)]
struct Stamp {
    #[serde(default)]
    created_at: i64,
    #[serde(default)]
    updated_at: i64,
}

/// A record's last-write time. Undecodable bytes sort oldest so a readable row always wins.
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

/// Fold the memory rows an older build left in `view_db` into `memory`, then drop that keyspace
/// from the view. Safe to repeat and safe when several worktrees migrate: a view with no legacy
/// keyspace is a no-op, and the merge is commutative. The group store is persisted before the
/// view's copy is deleted, so a crash between the two leaves duplicates, never a loss.
pub(crate) fn migrate_legacy(view_db: &Database, memory: &MemoryDb) -> Result<(), IndexError> {
    if !holds_legacy_rows(view_db) {
        return Ok(());
    }
    let legacy = open_keyspace(view_db, MEMORY_BY_KEY)?;
    merge_keyspace(&legacy, &memory.memory_by_key)?;
    memory.db.persist(PersistMode::SyncAll)?;
    view_db.delete_keyspace(legacy)?;
    Ok(())
}

#[cfg(test)]
#[path = "memory_store_tests.rs"]
mod tests;
