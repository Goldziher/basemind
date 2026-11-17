//! One index database per repository group (ADR-0028), shared by every view of every worktree of
//! the group.
//!
//! A process holds at most one handle per group directory: [`GroupDb::shared`] hands out the live
//! one, and only opens the database when no handle is alive. Each view of the group reads and writes
//! its own rows under a 4-byte view id ([`super::scoped`]); the rows the group shares by content (the
//! grep bloom) carry no prefix. The group's own rows live in `meta` under [`GROUP_META`], which no
//! view prefix can start with: view ids are allocated upward from 1 and never reach `0xFF000000`.
//!
//! Durability per view: the view's seal generation is a row under its prefix, so a lost journal
//! tail is detected per view and never discards the group. A database that was wiped or replaced
//! gets a new [`GroupDb::incarnation`]; a file map naming another one describes rows that are gone.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

use ahash::AHashMap;
use fjall::{Database, Keyspace, PersistMode};

use super::scoped::Scoped;
use super::view::Part;
use super::{IndexDb, IndexError};

/// Leading byte of every group-level `meta` row.
const GROUP_META: u8 = 0xFF;
/// `meta` row holding the group database's incarnation.
const META_INCARNATION: &[u8] = b"\xffgroup/incarnation";
/// `meta` row holding the next view id to allocate.
const META_NEXT_VIEW: &[u8] = b"\xffgroup/next_view";
/// Prefix of the view records: `<prefix><workspace key>/<view>` -> view id (`u32`, big endian).
const META_VIEW_RECORD: &[u8] = b"\xffgroup/view/";
/// The first view id. 0 is never allocated, so a file map's default `view_id` names no view.
const FIRST_VIEW_ID: u32 = 1;

/// Keyspaces the group shares by content: no view prefix.
const SHARED_PARTS: [Part; 1] = [Part::GrepBloom];

/// Keeps the cross-process group lock held for as long as the database is open.
pub(crate) type GroupLock = Box<dyn Send + Sync>;

/// The open group database. See the module docs.
pub(crate) struct GroupDb {
    dir: PathBuf,
    db: Database,
    keyspaces: AHashMap<&'static str, Keyspace>,
    embeddings: Keyspace,
    incarnation: u64,
    /// Serialises view-id allocation within the process; the lock serialises it across processes.
    allocation: Mutex<()>,
    _lock: GroupLock,
}

/// One view of a group database: the handle that keeps it open and the view's id.
#[derive(Clone)]
pub(crate) struct GroupView {
    pub(crate) group: Arc<GroupDb>,
    pub(crate) view_id: u32,
}

/// Every group database this process holds, by directory.
fn registry() -> &'static Mutex<AHashMap<PathBuf, Weak<GroupDb>>> {
    static REGISTRY: std::sync::OnceLock<Mutex<AHashMap<PathBuf, Weak<GroupDb>>>> = std::sync::OnceLock::new();
    REGISTRY.get_or_init(Mutex::default)
}

/// A view's record key in group `meta`.
fn view_record(workspace_key: &str, view: &str) -> Vec<u8> {
    let mut key = META_VIEW_RECORD.to_vec();
    key.extend_from_slice(workspace_key.as_bytes());
    key.push(b'/');
    key.extend_from_slice(view.as_bytes());
    key
}

fn read_u32(bytes: &[u8]) -> Option<u32> {
    <[u8; 4]>::try_from(bytes).ok().map(u32::from_be_bytes)
}

fn read_u64(bytes: &[u8]) -> Option<u64> {
    <[u8; 8]>::try_from(bytes).ok().map(u64::from_be_bytes)
}

/// A fresh incarnation: distinct from every earlier one of the directory with overwhelming
/// probability, and never 0, which a file map from before group indexes carries.
fn new_incarnation(dir: &Path) -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let mut seed = dir.as_os_str().as_encoded_bytes().to_vec();
    seed.extend_from_slice(&nanos.to_be_bytes());
    seed.extend_from_slice(&std::process::id().to_be_bytes());
    let digest = crate::hashing::hash_bytes(&seed);
    u64::from_be_bytes(digest[..8].try_into().expect("a blake3 digest has 8 bytes")).max(1)
}

impl GroupDb {
    /// The handle this process already holds for `dir`, if any.
    pub(crate) fn shared(dir: &Path) -> Option<Arc<Self>> {
        registry()
            .lock()
            .expect("group registry")
            .get(dir)
            .and_then(Weak::upgrade)
    }

    /// The handle for `dir`: the one this process holds, or a new open made under `lock`, which the
    /// caller took for the group. A held handle makes `lock` redundant, and it is dropped.
    pub(crate) fn open_shared(
        dir: &Path,
        lock: impl FnOnce() -> Result<GroupLock, IndexError>,
    ) -> Result<Arc<Self>, IndexError> {
        let mut registry = registry().lock().expect("group registry");
        if let Some(held) = registry.get(dir).and_then(Weak::upgrade) {
            return Ok(held);
        }
        let opened = Arc::new(Self::open(dir, lock()?)?);
        registry.retain(|_, handle| handle.strong_count() > 0);
        registry.insert(dir.to_path_buf(), Arc::downgrade(&opened));
        Ok(opened)
    }

    fn open(dir: &Path, lock: GroupLock) -> Result<Self, IndexError> {
        let fresh = super::IndexDb::open_without_memory(dir)?;
        let wiped_on_open = fresh.wiped_on_open();
        let db = fresh.db.clone();
        let mut keyspaces = AHashMap::new();
        for part in Part::ALL {
            keyspaces.insert(part.name(), fresh.keyspace(part).raw().clone());
        }
        let embeddings = fresh.embeddings.clone();
        let meta = keyspaces[Part::Meta.name()].clone();
        let incarnation = match meta.get(META_INCARNATION)?.as_deref().and_then(read_u64) {
            Some(incarnation) if !wiped_on_open => incarnation,
            _ => {
                let incarnation = new_incarnation(dir);
                let mut batch = db.batch().durability(Some(PersistMode::SyncAll));
                batch.insert(&meta, META_INCARNATION, incarnation.to_be_bytes());
                batch.commit()?;
                incarnation
            }
        };
        Ok(Self {
            dir: dir.to_path_buf(),
            db,
            keyspaces,
            embeddings,
            incarnation,
            allocation: Mutex::new(()),
            _lock: lock,
        })
    }

    /// The incarnation of the database: a new one after every wipe or replacement.
    pub(crate) fn incarnation(&self) -> u64 {
        self.incarnation
    }

    fn meta(&self) -> &Keyspace {
        &self.keyspaces[Part::Meta.name()]
    }

    /// The id of `(workspace_key, view)`, if the group has allocated one.
    pub(crate) fn view_id(&self, workspace_key: &str, view: &str) -> Result<Option<u32>, IndexError> {
        Ok(self
            .meta()
            .get(view_record(workspace_key, view))?
            .as_deref()
            .and_then(read_u32))
    }

    /// The id of `(workspace_key, view)`, allocating one with a synced commit when it has none. An id
    /// is never reused: the counter only grows, and a record outlives the rows it names until the
    /// group sweep removes both.
    pub(crate) fn allocate_view_id(&self, workspace_key: &str, view: &str) -> Result<u32, IndexError> {
        let _serial = self.allocation.lock().expect("view allocation");
        if let Some(id) = self.view_id(workspace_key, view)? {
            return Ok(id);
        }
        let id = self
            .meta()
            .get(META_NEXT_VIEW)?
            .as_deref()
            .and_then(read_u32)
            .unwrap_or(FIRST_VIEW_ID);
        let next = id.checked_add(1).filter(|next| next.to_be_bytes()[0] != GROUP_META);
        let next = next.ok_or_else(|| IndexError::Io {
            path: self.dir.clone(),
            source: std::io::Error::other("the group index has allocated every view id; delete it to rebuild"),
        })?;
        let mut batch = self.db.batch().durability(Some(PersistMode::SyncAll));
        batch.insert(self.meta(), META_NEXT_VIEW, next.to_be_bytes());
        batch.insert(self.meta(), view_record(workspace_key, view), id.to_be_bytes());
        batch.commit()?;
        Ok(id)
    }

    /// Every view record of the group: `(workspace key, view, view id)`.
    pub(crate) fn views(&self) -> Result<Vec<(String, String, u32)>, IndexError> {
        let mut views = Vec::new();
        for guard in self.meta().prefix(META_VIEW_RECORD) {
            let (key, value) = guard.into_inner()?;
            let Some(record) = key
                .get(META_VIEW_RECORD.len()..)
                .and_then(|rest| std::str::from_utf8(rest).ok())
            else {
                continue;
            };
            if let (Some((workspace, view)), Some(id)) = (record.split_once('/'), read_u32(&value)) {
                views.push((workspace.to_owned(), view.to_owned(), id));
            }
        }
        Ok(views)
    }

    /// A handle on the rows of view `view_id`, sharing this database. The memory store opens as
    /// for a view database of its own.
    pub(crate) fn open_view(
        self: &Arc<Self>,
        view_id: u32,
        memory_dir: &Path,
        tolerate_held_memory: bool,
    ) -> Result<IndexDb, IndexError> {
        let memory = super::open_memory(memory_dir, tolerate_held_memory)?;
        let scoped = |part: Part| {
            let keyspace = self.keyspaces[part.name()].clone();
            if SHARED_PARTS.contains(&part) {
                Scoped::whole(keyspace)
            } else {
                Scoped::view(keyspace, view_id)
            }
        };
        Ok(IndexDb {
            db: self.db.clone(),
            meta: scoped(Part::Meta),
            symbols_by_path: scoped(Part::SymbolsByPath),
            symbols_by_name: scoped(Part::SymbolsByName),
            calls_by_path: scoped(Part::CallsByPath),
            calls_by_callee: scoped(Part::CallsByCallee),
            callee_names: Arc::default(),
            imports_by_module: scoped(Part::ImportsByModule),
            imports_by_path: scoped(Part::ImportsByPath),
            implementations_by_trait: scoped(Part::ImplementationsByTrait),
            trait_names: Arc::default(),
            implementations_by_path: scoped(Part::ImplementationsByPath),
            refs_by_def: scoped(Part::RefsByDef),
            refs_by_path: scoped(Part::RefsByPath),
            import_targets: scoped(Part::ImportTargets),
            importers_by_probe: scoped(Part::ImportersByProbe),
            code_bm25_postings: scoped(Part::CodeBm25Postings),
            code_bm25_by_path: scoped(Part::CodeBm25ByPath),
            grep_bloom: scoped(Part::GrepBloom),
            embeddings: self.embeddings.clone(),
            fault: Arc::default(),
            synced_seqno: Arc::default(),
            memory,
            // A wipe of the group database is told to each view by the incarnation its file map
            // names, not by this flag. ~keep
            wiped_on_open: false,
            group: Some(GroupView {
                group: Arc::clone(self),
                view_id,
            }),
        })
    }
}

#[cfg(test)]
#[path = "group_tests.rs"]
mod tests;
