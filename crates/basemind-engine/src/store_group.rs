//! The store side of the group index (ADR-0028): the group lock, and the checks that tie a view's
//! file map to the group database.
//!
//! A file map names the incarnation of the group database it was written against and the id the
//! group gave its view. A map naming another incarnation or id, or a seal generation the view never
//! reached, describes rows that are gone: the map is emptied and the view's rows are cleared. The
//! group database and every other view stay.

use std::path::Path;
use std::sync::Arc;

use super::{Index, LockHolder, StoreError, acquire_lock_as, memory_store_dir};
use crate::index::group::{GroupDb, GroupLock};
use crate::index::{IndexDb, IndexError};
use crate::store_layout::{group_db_dir_in, group_index_dir, workspace_key};

/// File in the group directory naming the checkout that keyed it, for diagnostics.
const GROUP_MARKER_FILE: &str = "group.json";

/// Take the group lock for `holder`, as a guard the group database keeps.
fn take_lock(dir: &Path, holder: LockHolder) -> Result<GroupLock, StoreError> {
    std::fs::create_dir_all(dir).map_err(|source| StoreError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    Ok(Box::new(acquire_lock_as(dir, holder)?))
}

/// [`GroupDb::open_shared`] on this binary's database in `group_dir`, under the group lock taken for
/// `holder`. A lock held by another process is reported as the store's [`StoreError::Locked`],
/// naming the holder.
fn open_locked(group_dir: &Path, holder: LockHolder) -> Result<Arc<GroupDb>, StoreError> {
    let mut refused = None;
    let opened = GroupDb::open_shared(&group_db_dir_in(group_dir), || {
        take_lock(group_dir, holder).map_err(|error| {
            refused = Some(error);
            IndexError::Fjall(fjall::Error::Locked)
        })
    });
    match (opened, refused) {
        (Err(_), Some(error)) => Err(error),
        (opened, _) => Ok(opened?),
    }
}

/// [`open_locked`], retrying a fjall `Locked`: with the group lock held, fjall's own directory lock
/// can only be held by a handle shutting down.
fn open_with_retry(group_dir: &Path, holder: LockHolder) -> Result<Arc<GroupDb>, StoreError> {
    let mut attempt = 0;
    loop {
        match open_locked(group_dir, holder) {
            Err(StoreError::Index(IndexError::Fjall(fjall::Error::Locked))) if attempt < super::INDEX_OPEN_RETRIES => {
                attempt += 1;
                std::thread::sleep(super::INDEX_OPEN_BACKOFF);
            }
            opened => return opened,
        }
    }
}

/// Record which checkout keyed the group, once. Best-effort: diagnostics only.
fn write_group_marker(dir: &Path, root: &Path) {
    let path = dir.join(GROUP_MARKER_FILE);
    if path.exists() {
        return;
    }
    let main_root = crate::git::Repo::discover(root)
        .map(|repo| repo.main_worktree_root())
        .unwrap_or_else(|_| root.to_path_buf());
    let marker = serde_json::json!({ "main_root": main_root.to_string_lossy() });
    let _ = std::fs::write(path, marker.to_string());
}

/// The group database of `root`, opened for a writer: this process's handle, or a new one under the
/// group lock. A database found damaged is rebuildable cache and is replaced.
fn open_group_for_writer(root: &Path, holder: LockHolder) -> Result<Arc<GroupDb>, StoreError> {
    let dir = group_index_dir(root);
    let group = match open_with_retry(&dir, holder) {
        Err(StoreError::Index(IndexError::Corrupt(error))) => {
            tracing::warn!(%error, dir = %dir.display(), "group index is damaged; discarding it for a rebuild");
            crate::index::durability::discard_index(&group_db_dir_in(&dir))?;
            open_with_retry(&dir, holder)?
        }
        opened => opened?,
    };
    write_group_marker(&dir, root);
    Ok(group)
}

/// Whether `index` was written against `db`'s view of `group`: the same incarnation and view id,
/// and no seal generation beyond the view's.
fn file_map_matches(index: &Index, group: &GroupDb, db: &IndexDb, view_id: u32) -> Result<bool, StoreError> {
    Ok(index.group_incarnation == group.incarnation()
        && index.view_id == view_id
        && index.generation <= db.scan_generation()?)
}

/// Point `index` at `db`'s view, emptying it (and clearing the view's rows) when it describes rows
/// the view does not hold. Returns whether a full scan must rebuild the view.
fn reconcile(index: &mut Index, group: &GroupDb, db: &IndexDb, view_id: u32, view: &str) -> Result<bool, StoreError> {
    let mut rebuild = false;
    if !file_map_matches(index, group, db, view_id)? {
        if !index.files.is_empty() || index.generation > 0 {
            tracing::warn!(
                view,
                "file map does not match the group index (another incarnation, view or generation); rebuilding the view"
            );
        }
        db.clear_for_rebuild()?;
        rebuild = !index.files.is_empty();
        if rebuild {
            index.empty_for_rebuild();
        }
        index.generation = 0;
    }
    index.group_incarnation = group.incarnation();
    index.view_id = view_id;
    Ok(rebuild || index.rebuild_pending)
}

/// The writer open of a view in the group layout: the group database, the view's id (allocated on
/// first use) and the reconciled file map. The view's database of its own, left by the per-view
/// layout is replaced by the group index. Notes an older build kept in it move to the group's notes
/// store first; while that store cannot take them, the database is kept. The database is not
/// deleted here: the returned flag says it is ready to go ([`Store::reap_legacy_index`]), which a
/// scan does once its free-space check has passed, so a refusal leaves the old release's index.
pub(super) fn open_index_for_writer(
    root: &Path,
    view: &str,
    view_dir: &Path,
    index: &mut Index,
    holder: LockHolder,
) -> Result<(IndexDb, bool, bool), StoreError> {
    let group = open_group_for_writer(root, holder)?;
    let claim = group.claim_view(&workspace_key(root), view)?;
    let view_id = claim.id();
    let db = group.open_claimed(claim, &memory_store_dir(root), false)?;
    let mut legacy_adopted = false;
    if view_dir.join(crate::index::INDEX_DIR).exists() {
        if db.adopt_legacy_view_notes(view_dir)? {
            legacy_adopted = true;
        } else {
            tracing::warn!(
                view,
                "the notes store cannot take this view's notes yet; its old index is kept until it can"
            );
        }
    }
    let rebuild = reconcile(index, &group, &db, view_id, view)?;
    Ok((db, rebuild, legacy_adopted))
}

/// How a read-only open may reach the group database when its process holds no handle on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReadAccess {
    /// Only through a handle the process already holds: a long-lived reader (`serve` beside a
    /// writer) must never hold the group lock, or every writer of the group would wait on it.
    HeldHandle,
    /// Also by taking a free group lock without waiting and opening the database for reading only,
    /// for the length of one CLI command run with no daemon (ADR-0036 rule 3).
    BriefLock,
}

/// The group database opened for a one-shot query: under the group lock taken without waiting, by
/// an open that writes no row. `None` when the lock is held (the holder writes; serve from blobs),
/// the database does not exist, or no writer has opened it yet.
fn open_group_for_query(dir: &Path) -> Result<Option<Arc<GroupDb>>, StoreError> {
    let db_dir = group_db_dir_in(dir);
    if !db_dir.join(crate::index::INDEX_DIR).is_dir() {
        return Ok(None);
    }
    let lock = match super::try_acquire_lock_as(dir, LockHolder::Query) {
        Ok(lock) => lock,
        Err(StoreError::Locked { holder: Some(meta), .. })
            if crate::store_lock::embed_yield::held_by_worker(&meta.command) =>
        {
            // The background pass asks to be waited for rather than answered around: it gives the
            // lock up at its next file boundary.
            match crate::store_lock::embed_yield::wait_for_worker(dir, || {
                super::try_acquire_lock_as(dir, LockHolder::Query)
            }) {
                Ok(lock) => lock,
                Err(StoreError::Locked { .. }) => return Ok(None),
                Err(error) => return Err(error),
            }
        }
        Err(StoreError::Locked { .. }) => return Ok(None),
        Err(error) => return Err(error),
    };
    match GroupDb::open_shared_for_reading(&db_dir, Box::new(lock)) {
        Ok(group) => Ok(group),
        Err(IndexError::Fjall(fjall::Error::Locked)) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// The read-only open of a view. Opening the group database as a writer does (journal flush,
/// schema stamp, incarnation, wipe) is never done here: an open without the workspace lock writes
/// nothing (ADR-0030 rule 26). `access` says whether a process holding no handle may open the
/// database for reading under a briefly held group lock. `Ok(None)` serves from blobs: no handle,
/// the view was never scanned into the group, or its file map does not match the group. The second
/// value says whether a scan owes the view a rebuild.
pub(super) fn open_index_read_only(
    root: &Path,
    view: &str,
    index: &Index,
    access: ReadAccess,
) -> Result<(Option<IndexDb>, bool), StoreError> {
    let dir = group_index_dir(root);
    let held = GroupDb::shared(&group_db_dir_in(&dir));
    let group = match (held, access) {
        (Some(group), _) => group,
        (None, ReadAccess::HeldHandle) => return Ok((None, false)),
        (None, ReadAccess::BriefLock) => match open_group_for_query(&dir) {
            Ok(Some(group)) => group,
            Ok(None) => return Ok((None, false)),
            Err(StoreError::Index(IndexError::NeedsRescan { .. })) => return Ok((None, !index.files.is_empty())),
            Err(error) => return Err(error),
        },
    };
    let Some(claim) = group.claim_existing_view(&workspace_key(root), view)? else {
        return Ok((None, !index.files.is_empty()));
    };
    let view_id = claim.id();
    let db = group.open_claimed(claim, &memory_store_dir(root), true)?;
    // A view with no file map yet claims nothing, so it is read as it stands, as a view with a
    // database of its own is. ~keep
    let claims_nothing = index.group_incarnation == 0 && index.files.is_empty() && index.generation == 0;
    if !claims_nothing && !file_map_matches(index, &group, &db, view_id)? {
        return Ok((None, !index.files.is_empty()));
    }
    Ok((Some(db), false))
}

/// How long a reopen waits for the other handles on the group database to be dropped.
const REOPEN_RELEASE_WAIT: std::time::Duration = std::time::Duration::from_secs(2);
const REOPEN_RELEASE_POLL: std::time::Duration = std::time::Duration::from_millis(20);

/// A fresh handle on `view` of `root`'s group, for a store whose handle was poisoned and dropped. A
/// handle still open elsewhere in this process would be handed back by the registry, so the reopen
/// waits for every one to drop and fails with [`IndexError::StillOpen`] when one outlives the wait.
pub(super) fn reopen_view(root: &Path, view: &str) -> Result<IndexDb, IndexError> {
    let dir = group_index_dir(root);
    let deadline = std::time::Instant::now() + REOPEN_RELEASE_WAIT;
    let db_dir = group_db_dir_in(&dir);
    while GroupDb::shared(&db_dir).is_some() {
        if std::time::Instant::now() >= deadline {
            return Err(IndexError::StillOpen { dir: db_dir });
        }
        std::thread::sleep(REOPEN_RELEASE_POLL);
    }
    let group = open_with_retry(&dir, LockHolder::Maintenance).map_err(|error| match error {
        StoreError::Index(error) => error,
        StoreError::Locked { .. } => IndexError::Fjall(fjall::Error::Locked),
        other => IndexError::Io {
            path: dir.clone(),
            source: std::io::Error::other(other.to_string()),
        },
    })?;
    let claim = group.claim_view(&workspace_key(root), view)?;
    group.open_claimed(claim, &memory_store_dir(root), false)
}
