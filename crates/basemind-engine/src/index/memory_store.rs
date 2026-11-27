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
use std::sync::{Arc, Mutex, OnceLock, Weak};

use fjall::{Database, Keyspace, PersistMode};
use serde::Deserialize;

use super::{FJALL_MAX_JOURNAL_BYTES, FJALL_WORKER_THREADS, IndexError, open_keyspace};

const MEMORY_CACHE_BYTES: u64 = 8 * 1_024 * 1_024;
const MEMORY_BY_KEY: &str = "memory_by_key";

/// What an open does about the store's version.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Version {
    /// A writer: refuse a newer store, stamp one that carries no version.
    Stamp,
    /// A reader: refuse a newer store and write no version.
    Check,
}

/// The Fjall database behind a [`MemoryDb`] once the store exists on disk.
struct Opened {
    db: Database,
    memory_by_key: Keyspace,
}

/// The group memory store. An existing store is opened with the handle; an absent one is created
/// by the first write ([`MemoryDb::notes_for_write`]) and reads of it answer empty until then, so a
/// session that never saves a note leaves no database (and no 64 MiB preallocated journal) behind.
pub struct MemoryDb {
    dir: PathBuf,
    opened: OnceLock<Opened>,
    /// Serializes the open, so two threads never both take the Fjall lock on the directory.
    opening: Mutex<()>,
    /// Whether a writer open has held this handle to its version.
    stamped: std::sync::atomic::AtomicBool,
    /// How many times [`MemoryDb::sync`] ran.
    #[cfg(all(test, feature = "memory"))]
    pub(crate) syncs: std::sync::atomic::AtomicU32,
}

/// Whether a store exists at `dir`. A store is only ever created by a write, so an absent directory
/// is an empty store.
fn store_exists(dir: &Path) -> bool {
    dir.is_dir()
}

impl MemoryDb {
    fn open(dir: &Path, version: Version) -> Result<Self, IndexError> {
        let handle = Self {
            dir: dir.to_path_buf(),
            opened: OnceLock::new(),
            opening: Mutex::new(()),
            stamped: std::sync::atomic::AtomicBool::new(false),
            #[cfg(all(test, feature = "memory"))]
            syncs: std::sync::atomic::AtomicU32::new(0),
        };
        if store_exists(dir) {
            handle.open_now(version)?;
        }
        Ok(handle)
    }

    /// Open the store, creating it when it does not exist yet.
    fn open_now(&self, version: Version) -> Result<&Opened, IndexError> {
        if let Some(opened) = self.opened.get() {
            return Ok(opened);
        }
        let _opening = self.opening.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(opened) = self.opened.get() {
            return Ok(opened);
        }
        std::fs::create_dir_all(&self.dir).map_err(|source| IndexError::Io {
            path: self.dir.clone(),
            source,
        })?;
        let db = Database::builder(&self.dir)
            .cache_size(MEMORY_CACHE_BYTES)
            .worker_threads(FJALL_WORKER_THREADS)
            .max_journaling_size(FJALL_MAX_JOURNAL_BYTES)
            .open()?;
        check_version(&db, version)?;
        let memory_by_key = open_keyspace(&db, MEMORY_BY_KEY)?;
        self.stamped
            .store(version == Version::Stamp, std::sync::atomic::Ordering::Release);
        Ok(self.opened.get_or_init(|| Opened { db, memory_by_key }))
    }

    /// The notes keyspace, or `None` while the store does not exist: nothing was ever saved, and a
    /// read creates nothing. A store another process created since this handle was made is opened.
    #[cfg(any(test, feature = "memory"))]
    pub(crate) fn notes(&self) -> Result<Option<&Keyspace>, IndexError> {
        if let Some(opened) = self.opened.get() {
            return Ok(Some(&opened.memory_by_key));
        }
        if !store_exists(&self.dir) {
            return Ok(None);
        }
        Ok(Some(&self.open_now(Version::Check)?.memory_by_key))
    }

    /// The notes keyspace for a write, creating the store (stamped with its version) when absent.
    #[cfg(any(test, feature = "memory"))]
    pub(crate) fn notes_for_write(&self) -> Result<&Keyspace, IndexError> {
        Ok(&self.open_now(Version::Stamp)?.memory_by_key)
    }

    /// Stamp a handle a reader opened first, for the writer now joining it. A store that does not
    /// exist yet is stamped by the write that creates it.
    fn hold_to_version(&self, version: Version) -> Result<(), IndexError> {
        use std::sync::atomic::Ordering;
        let Some(opened) = self.opened.get() else {
            return Ok(());
        };
        if version == Version::Stamp && !self.stamped.load(Ordering::Acquire) {
            check_version(&opened.db, Version::Stamp)?;
            self.stamped.store(true, Ordering::Release);
        }
        Ok(())
    }

    /// Fsync the journal. A note exists nowhere else, so unlike the code index (rebuilt from
    /// source by a scan) a write that only reached the OS is lost for good by a power failure. A
    /// store that does not exist holds nothing to sync.
    #[cfg(feature = "memory")]
    pub(crate) fn sync(&self) -> Result<(), IndexError> {
        #[cfg(test)]
        self.syncs.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        match self.opened.get() {
            Some(opened) => Ok(opened.db.persist(PersistMode::SyncAll)?),
            None => Ok(()),
        }
    }

    /// Persist the store, when it exists, whatever the build's features.
    #[cfg(test)]
    pub(crate) fn sync_for_test(&self) {
        if let Some(opened) = self.opened.get() {
            opened.db.persist(PersistMode::SyncAll).expect("persist");
        }
    }

    /// The open database, creating the store if needed.
    #[cfg(test)]
    pub(crate) fn db_for_test(&self) -> &Database {
        &self.open_now(Version::Stamp).expect("open the store").db
    }

    /// Names of the keyspaces this database holds, the one for its version aside: that one is not
    /// sized by the index table.
    #[cfg(test)]
    pub(crate) fn keyspace_names(&self) -> Vec<String> {
        self.db_for_test()
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
    let key = registry_key(dir);
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

/// The key `dir` is registered under: its canonical form, or for a store not created yet its nearest
/// existing ancestor's canonical form plus the rest. Both resolve the same (`/var` → `/private/var`
/// on macOS) before and after the store is created, so one store never sits under two keys and a
/// second open never hits the Fjall lock.
fn registry_key(dir: &Path) -> PathBuf {
    let mut missing = Vec::new();
    let mut existing = dir;
    loop {
        if let Ok(canonical) = existing.canonicalize() {
            return missing.iter().rev().fold(canonical, |path, name| path.join(name));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                missing.push(name.to_os_string());
                existing = parent;
            }
            _ => return dir.to_path_buf(),
        }
    }
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

/// [`holds_legacy_rows`] for the index under `view_dir`, opened on its own without a notes store.
/// For a collector that must not remove the notes with the view; the caller holds the workspace
/// lock.
pub(crate) fn view_holds_legacy_rows(view_dir: &Path) -> Result<bool, IndexError> {
    let db = Database::builder(view_dir.join(super::INDEX_DIR))
        .worker_threads(FJALL_WORKER_THREADS)
        .open()?;
    Ok(holds_legacy_rows(&db))
}

impl super::IndexDb {
    /// Whether this index still holds notes of the layout from before the group store: a writer
    /// open that met a newer notes store leaves them where they are.
    pub(crate) fn holds_legacy_memory(&self) -> bool {
        holds_legacy_rows(&self.db)
    }

    /// Move the notes in the database a view kept under `view_dir` before the group index into
    /// this handle's notes store ([`migrate_legacy`]). `false` when that database holds notes this
    /// handle has no store to take: the caller must keep it. The caller holds the workspace lock.
    pub(crate) fn adopt_legacy_view_notes(&self, view_dir: &Path) -> Result<bool, IndexError> {
        let view_db = Database::builder(view_dir.join(super::INDEX_DIR))
            .worker_threads(FJALL_WORKER_THREADS)
            .open()?;
        if !holds_legacy_rows(&view_db) {
            return Ok(true);
        }
        match self.memory.as_deref() {
            Ok(memory) => migrate_legacy(&view_db, memory).map(|()| true),
            Err(_) => Ok(false),
        }
    }
}

#[cfg(feature = "memory")]
impl super::IndexDb {
    /// [`rescope`] this handle's notes store to `scope`. `0` when the handle has no store.
    pub(crate) fn rescope_notes(&self, scope: &str) -> Result<usize, IndexError> {
        match self.memory.as_deref() {
            Ok(memory) => rescope(memory, scope),
            Err(_) => Ok(0),
        }
    }
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

/// File every note stored under a scope other than `scope` under `scope`, and return how many
/// were moved. The store is keyed by the main worktree's path, so every scope in it belongs to this
/// one repository: a note filed under an `origin` that has since changed (or appeared, or gone)
/// would otherwise be hidden. A note already present under `scope` is replaced only by a newer one
/// ([`incoming_wins`]), so the outcome never depends on the order scopes are met. The merged rows
/// are persisted before the old ones are removed: a crash between the two leaves duplicates, never
/// a loss.
#[cfg(any(test, feature = "memory"))]
pub(crate) fn rescope(memory: &MemoryDb, scope: &str) -> Result<usize, IndexError> {
    use super::keys::{memory_by_key, parse_memory_by_key};

    let Some(notes) = memory.notes()? else {
        return Ok(0);
    };
    let mut foreign = Vec::new();
    for guard in notes.iter() {
        let (key, value) = guard.into_inner()?;
        let Some((from, vis_byte, owner, note)) = parse_memory_by_key(&key) else {
            continue;
        };
        if from != scope {
            foreign.push((
                key.to_vec(),
                memory_by_key(scope, vis_byte, &owner, &note),
                value.to_vec(),
            ));
        }
    }
    if foreign.is_empty() {
        return Ok(0);
    }
    let opened = memory.open_now(Version::Stamp)?;
    for (_, target, value) in &foreign {
        let replace = match opened.memory_by_key.get(target)? {
            Some(existing) => incoming_wins(value, &existing),
            None => true,
        };
        if replace {
            opened.memory_by_key.insert(target.clone(), value.clone())?;
        }
    }
    opened.db.persist(PersistMode::SyncAll)?;
    for (old, _, _) in &foreign {
        opened.memory_by_key.remove(old.clone())?;
    }
    opened.db.persist(PersistMode::SyncAll)?;
    Ok(foreign.len())
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
    let opened = memory.open_now(Version::Stamp)?;
    merge_keyspace(&legacy, &opened.memory_by_key)?;
    opened.db.persist(PersistMode::SyncAll)?;
    view_db.delete_keyspace(legacy)?;
    Ok(())
}

#[cfg(test)]
#[path = "memory_store_tests.rs"]
mod tests;
