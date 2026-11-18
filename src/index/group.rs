//! One index database per repository group (ADR-0028), shared by every view of every worktree of
//! the group.
//!
//! A process holds at most one handle per group directory: [`GroupDb::shared`] hands out the live
//! one, and only opens the database when no handle is alive. Each view of the group reads and writes
//! its own rows under a 4-byte view id ([`super::scoped`]); the rows the group shares by content
//! (symbols, calls, BM25 postings, the grep bloom) carry no prefix. The group's own rows live in
//! `meta` under [`GROUP_META`], which no view prefix can start with: view ids are allocated upward
//! from 1 and never reach `0xFF000000`.
//!
//! Durability per view: the view's seal generation is a row under its prefix, so a lost journal
//! tail is detected per view and never discards the group. A database that was wiped or replaced
//! gets a new [`GroupDb::incarnation`]; a file map naming another one describes rows that are gone.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

use ahash::AHashMap;
use fjall::{Database, Keyspace, PersistMode};

use super::bm25_live::{Bm25Gate, Bm25LiveCache};
use super::group_sweep::PendingContents;
use super::live_keys::LiveKeysCell;
use super::name_dict::NameDict;
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
pub(super) const SHARED_PARTS: [Part; 7] = [
    Part::SymbolsByPath,
    Part::SymbolsByName,
    Part::CallsByPath,
    Part::CallsByCallee,
    Part::GrepBloom,
    Part::CodeBm25Postings,
    Part::CodeBm25Holders,
];

/// `meta` row holding the layout of the group's rows ([`GROUP_FORMAT`]).
const META_FORMAT: &[u8] = b"\xffgroup/format";
/// The layout of the rows in a group database. Bumped when a stage of ADR-0028 changes a key shape
/// within an unreleased minor: an open that finds another value (or none, in a database that has an
/// incarnation) discards the database and creates it anew with a new incarnation, so every view's
/// file map names a gone incarnation and its next scan rebuilds it. 2: BM25 postings shared, keyed by content. 3:
/// symbol and call rows shared, keyed by content. 4: BM25 holders; postings written once per key. 5: one posting row per term and
/// content, packing its chunks (ADR-0041). 6: one call name row per callee and content, packing its
/// starts (ADR-0043).
const GROUP_FORMAT: u32 = 6;

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
    /// A view's retirement holds it too, and so does every open's claim of an id
    /// ([`Self::claim_view`]), so no open gets an id being retired.
    pub(super) allocation: Mutex<()>,
    /// Each view's BM25 live-key cache, shared by every handle on the view and dropped with the last.
    bm25_live: Mutex<AHashMap<u32, Weak<Bm25LiveCache>>>,
    /// Each view's BM25 pass ledger, shared and dropped as `bm25_live` is.
    #[cfg(feature = "code-search")]
    bm25_pass: Mutex<AHashMap<u32, Weak<super::bm25_pass::Bm25PassLedger>>>,
    /// Orders the BM25 posting sweep against posting commits, across every view of the group.
    bm25_gate: Bm25Gate,
    /// Each view's live content keys, shared by every handle on the view and dropped with the last.
    live: Mutex<AHashMap<u32, Weak<LiveKeysCell>>>,
    /// Distinct callee names of the shared `calls_by_callee`, shared by every view.
    callee_names: Arc<NameDict>,
    /// Content keys each view's writer staged that no file map it wrote names yet: live for the
    /// group sweep ([`super::group_sweep`]).
    pub(super) pending: Mutex<PendingContents>,
    _lock: GroupLock,
}

/// One view of a group database: the handle that keeps it open and the view's id.
#[derive(Clone)]
pub(crate) struct GroupView {
    pub(crate) group: Arc<GroupDb>,
    pub(crate) view_id: u32,
}

/// A view id held for an open; see [`GroupDb::claim_view`].
pub(crate) struct ViewClaim {
    id: u32,
    live: Arc<LiveKeysCell>,
}

impl ViewClaim {
    /// The claimed view's id.
    pub(crate) fn id(&self) -> u32 {
        self.id
    }
}

/// Every group database this process holds, by directory.
fn registry() -> &'static Mutex<AHashMap<PathBuf, Weak<GroupDb>>> {
    static REGISTRY: std::sync::OnceLock<Mutex<AHashMap<PathBuf, Weak<GroupDb>>>> = std::sync::OnceLock::new();
    REGISTRY.get_or_init(Mutex::default)
}

/// A view's record key in group `meta`.
pub(super) fn view_record(workspace_key: &str, view: &str) -> Vec<u8> {
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

/// Whether the database holds rows no open of this build may read: a format other than
/// [`GROUP_FORMAT`], or rows without the incarnation that every writer stamps before its first row
/// (what a crash in an older build's in-place clear left).
fn holds_rows_of_another_layout(db: &IndexDb) -> Result<bool, IndexError> {
    let meta = db.keyspace(Part::Meta).raw();
    if meta.get(META_INCARNATION)?.is_some() {
        return Ok(meta.get(META_FORMAT)?.as_deref().and_then(read_u32) != Some(GROUP_FORMAT));
    }
    for part in Part::ALL.into_iter().filter(|part| *part != Part::Meta) {
        if !db.keyspace(part).raw().is_empty()? {
            return Ok(true);
        }
    }
    Ok(false)
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

    /// A handle for reading only, opened under `lock`, which the caller took for the group: the one
    /// this process holds, or a new open that writes nothing. `None` when the database holds no
    /// incarnation yet (no writer has opened it, so no view's rows are in it).
    pub(crate) fn open_shared_for_reading(dir: &Path, lock: GroupLock) -> Result<Option<Arc<Self>>, IndexError> {
        let mut registry = registry().lock().expect("group registry");
        if let Some(held) = registry.get(dir).and_then(Weak::upgrade) {
            return Ok(Some(held));
        }
        let fresh = super::IndexDb::open_group_for_reading(dir)?;
        let meta = fresh.keyspace(Part::Meta).raw();
        let Some(incarnation) = meta.get(META_INCARNATION)?.as_deref().and_then(read_u64) else {
            return Ok(None);
        };
        if meta.get(META_FORMAT)?.as_deref().and_then(read_u32) != Some(GROUP_FORMAT) {
            // Rows of another layout: a writer rebuilds them; a reader must not parse them. ~keep
            return Ok(None);
        }
        let opened = Arc::new(Self::from_open(dir, fresh, incarnation, lock));
        registry.retain(|_, handle| handle.strong_count() > 0);
        registry.insert(dir.to_path_buf(), Arc::downgrade(&opened));
        Ok(Some(opened))
    }

    fn from_open(dir: &Path, fresh: IndexDb, incarnation: u64, lock: GroupLock) -> Self {
        let mut keyspaces = AHashMap::new();
        for part in Part::ALL {
            keyspaces.insert(part.name(), fresh.keyspace(part).raw().clone());
        }
        Self {
            dir: dir.to_path_buf(),
            db: fresh.db.clone(),
            keyspaces,
            embeddings: fresh.embeddings.clone(),
            incarnation,
            allocation: Mutex::new(()),
            bm25_live: Mutex::default(),
            #[cfg(feature = "code-search")]
            bm25_pass: Mutex::default(),
            bm25_gate: Bm25Gate::default(),
            live: Mutex::default(),
            callee_names: Arc::default(),
            pending: Mutex::default(),
            _lock: lock,
        }
    }

    fn open(dir: &Path, lock: GroupLock) -> Result<Self, IndexError> {
        let mut fresh = super::IndexDb::open_without_memory(dir)?;
        let mut wiped_on_open = fresh.wiped_on_open();
        if holds_rows_of_another_layout(&fresh)? {
            // Discarded whole, as a schema mismatch is: emptying it keyspace by keyspace would leave,
            // after a crash, rows of the old layout under view ids the next open allocates again. ~keep
            tracing::info!(dir = %dir.display(), "group index rows are of another layout; rebuilding it");
            drop(fresh);
            super::durability::discard_dir(&dir.join(super::INDEX_DIR))?;
            fresh = super::IndexDb::open_without_memory(dir)?;
            wiped_on_open = true;
        }
        let meta = fresh.keyspace(Part::Meta).raw().clone();
        let stored = meta.get(META_INCARNATION)?.as_deref().and_then(read_u64);
        let current_format = meta.get(META_FORMAT)?.as_deref().and_then(read_u32) == Some(GROUP_FORMAT);
        let incarnation = match stored {
            Some(incarnation) if !wiped_on_open && current_format => incarnation,
            _ => {
                let incarnation = new_incarnation(dir);
                let mut batch = fresh.db.batch().durability(Some(PersistMode::SyncAll));
                batch.insert(&meta, META_INCARNATION, incarnation.to_be_bytes());
                batch.insert(&meta, META_FORMAT, GROUP_FORMAT.to_be_bytes());
                batch.insert(&meta, super::META_SCHEMA_VER, super::INDEX_SCHEMA_VER.to_be_bytes());
                batch.commit()?;
                incarnation
            }
        };
        Ok(Self::from_open(dir, fresh, incarnation, lock))
    }

    /// The incarnation of the database: a new one after every wipe or replacement.
    pub(crate) fn incarnation(&self) -> u64 {
        self.incarnation
    }

    pub(super) fn meta(&self) -> &Keyspace {
        &self.keyspaces[Part::Meta.name()]
    }

    /// The fjall keyspace of `part`, every view's rows.
    pub(super) fn keyspace(&self, part: Part) -> &Keyspace {
        &self.keyspaces[part.name()]
    }

    /// The database every keyspace lives in.
    pub(super) fn database(&self) -> &Database {
        &self.db
    }

    /// The id of `(workspace_key, view)`, if the group has allocated one.
    pub(crate) fn view_id(&self, workspace_key: &str, view: &str) -> Result<Option<u32>, IndexError> {
        Ok(self
            .meta()
            .get(view_record(workspace_key, view))?
            .as_deref()
            .and_then(read_u32))
    }

    /// The id of `(workspace_key, view)`, allocating one when it has none, unclaimed: for tests. An
    /// open takes [`Self::claim_view`].
    #[cfg(test)]
    pub(crate) fn allocate_view_id(&self, workspace_key: &str, view: &str) -> Result<u32, IndexError> {
        let _serial = self.allocation.lock().expect("view allocation");
        self.allocate_locked(workspace_key, view)
    }

    /// The id of `(workspace_key, view)`, allocated when it has none, claimed for an open: its
    /// live-key cell is taken while the allocation lock is held, and the group sweep retires a view
    /// only under that lock and only when no cell of it is held. So the id the open gets is never
    /// retired under it.
    pub(crate) fn claim_view(&self, workspace_key: &str, view: &str) -> Result<ViewClaim, IndexError> {
        let _serial = self.allocation.lock().expect("view allocation");
        let id = self.allocate_locked(workspace_key, view)?;
        Ok(ViewClaim {
            id,
            live: self.live_keys_cell(id),
        })
    }

    /// [`Self::claim_view`] for a reader: `None` when the view has no id, none is allocated.
    pub(crate) fn claim_existing_view(&self, workspace_key: &str, view: &str) -> Result<Option<ViewClaim>, IndexError> {
        let _serial = self.allocation.lock().expect("view allocation");
        Ok(self.view_id(workspace_key, view)?.map(|id| ViewClaim {
            id,
            live: self.live_keys_cell(id),
        }))
    }

    /// The id of `(workspace_key, view)`, allocating one with a synced commit when it has none. An id
    /// is never reused: the counter only grows, and a record outlives the rows it names until the
    /// group sweep removes both. The caller holds the allocation lock.
    fn allocate_locked(&self, workspace_key: &str, view: &str) -> Result<u32, IndexError> {
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

    /// A handle on the rows of view `view_id`, taking its live-key cell without the allocation lock:
    /// for tests that open a view no sweep runs against.
    #[cfg(test)]
    pub(crate) fn open_view(
        self: &Arc<Self>,
        view_id: u32,
        memory_dir: &Path,
        tolerate_held_memory: bool,
    ) -> Result<IndexDb, IndexError> {
        let claim = ViewClaim {
            id: view_id,
            live: self.live_keys_cell(view_id),
        };
        self.open_claimed(claim, memory_dir, tolerate_held_memory)
    }

    /// A handle on the rows of the claimed view, sharing this database. The memory store opens as
    /// for a view database of its own.
    pub(crate) fn open_claimed(
        self: &Arc<Self>,
        claim: ViewClaim,
        memory_dir: &Path,
        tolerate_held_memory: bool,
    ) -> Result<IndexDb, IndexError> {
        let ViewClaim { id: view_id, live } = claim;
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
            callee_names: Arc::clone(&self.callee_names),
            implementations_by_trait: scoped(Part::ImplementationsByTrait),
            trait_names: Arc::default(),
            implementations_by_path: scoped(Part::ImplementationsByPath),
            refs_by_def: scoped(Part::RefsByDef),
            refs_by_path: scoped(Part::RefsByPath),
            import_targets: scoped(Part::ImportTargets),
            importers_by_probe: scoped(Part::ImportersByProbe),
            code_bm25_postings: scoped(Part::CodeBm25Postings),
            code_bm25_by_path: scoped(Part::CodeBm25ByPath),
            code_bm25_holders: scoped(Part::CodeBm25Holders),
            bm25_live: per_view(&self.bm25_live, view_id),
            bm25_gate: Arc::clone(&self.bm25_gate),
            live,
            #[cfg(feature = "code-search")]
            bm25_pass: per_view(&self.bm25_pass, view_id),
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

    /// The live-key cell of view `view_id`, as [`per_view`] hands it out.
    fn live_keys_cell(&self, view_id: u32) -> Arc<LiveKeysCell> {
        per_view(&self.live, view_id)
    }

    /// Views whose live-key cell is still held by an open handle.
    pub(crate) fn live_key_views(&self) -> Vec<u32> {
        let mut cells = self.live.lock().expect("live key cells");
        cells.retain(|_, cell| cell.strong_count() > 0);
        cells.keys().copied().collect()
    }

    /// Drop the per-view caches of a view whose rows are gone.
    pub(super) fn forget_view(&self, view_id: u32) {
        self.bm25_live.lock().expect("bm25 live caches").remove(&view_id);
        #[cfg(feature = "code-search")]
        self.bm25_pass.lock().expect("bm25 pass ledgers").remove(&view_id);
    }

    /// Views whose BM25 live-key cache is still held by an open handle.
    #[cfg(test)]
    pub(crate) fn bm25_cache_views(&self) -> Vec<u32> {
        let mut caches = self.bm25_live.lock().expect("bm25 live caches");
        caches.retain(|_, cache| cache.strong_count() > 0);
        caches.keys().copied().collect()
    }
}

/// View `view_id`'s entry of a per-view map: the one its open handles share, or a new one when none
/// is open. Entries no open handle holds are dropped here, so a long-lived process (the daemon,
/// hosting one view of the group) does not keep the state of every view it ever opened.
fn per_view<T: Default>(entries: &Mutex<AHashMap<u32, Weak<T>>>, view_id: u32) -> Arc<T> {
    let mut entries = entries.lock().expect("per-view entries");
    entries.retain(|_, entry| entry.strong_count() > 0);
    if let Some(entry) = entries.get(&view_id).and_then(Weak::upgrade) {
        return entry;
    }
    let entry = Arc::default();
    entries.insert(view_id, Arc::downgrade(&entry));
    entry
}

#[cfg(test)]
#[path = "group_tests.rs"]
mod tests;
