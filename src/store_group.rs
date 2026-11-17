//! The store side of the group index (ADR-0028): which layout a store opens, the group lock, and
//! the checks that tie a view's file map to the group database.
//!
//! The layout is chosen by `BASEMIND_INDEX_GROUP=1` while the group index is being built; without it
//! every view keeps a database of its own. Nothing here is a released format.
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
use crate::store_layout::{group_index_dir, workspace_key};

/// The environment switch for the group index layout.
pub const INDEX_GROUP_ENV: &str = "BASEMIND_INDEX_GROUP";

/// File in the group directory naming the checkout that keyed it, for diagnostics.
const GROUP_MARKER_FILE: &str = "group.json";

/// Whether stores open the group index layout.
pub fn group_layout() -> bool {
    std::env::var_os(INDEX_GROUP_ENV).is_some_and(|value| value == "1")
}

/// Take the group lock for `holder`, as a guard the group database keeps.
fn take_lock(dir: &Path, holder: LockHolder) -> Result<GroupLock, StoreError> {
    std::fs::create_dir_all(dir).map_err(|source| StoreError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    Ok(Box::new(acquire_lock_as(dir, holder)?))
}

/// [`GroupDb::open_shared`] under the group lock taken for `holder`. A lock held by another process
/// is reported as the store's [`StoreError::Locked`], naming the holder.
fn open_locked(dir: &Path, holder: LockHolder) -> Result<Arc<GroupDb>, StoreError> {
    let mut refused = None;
    let opened = GroupDb::open_shared(dir, || {
        take_lock(dir, holder).map_err(|error| {
            refused = Some(error);
            IndexError::Fjall(fjall::Error::Locked)
        })
    });
    match (opened, refused) {
        (Err(_), Some(error)) => Err(error),
        (opened, _) => Ok(opened?),
    }
}

/// [`open_locked`], retrying a fjall `Locked` the way [`super::open_index_with_retry`] does: with
/// the group lock held, fjall's own directory lock can only be held by a handle shutting down.
fn open_with_retry(dir: &Path, holder: LockHolder) -> Result<Arc<GroupDb>, StoreError> {
    let mut attempt = 0;
    loop {
        match open_locked(dir, holder) {
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
            crate::index::durability::discard_index(&dir)?;
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
/// layout, is deleted: the group index replaces it.
pub(super) fn open_index_for_writer(
    root: &Path,
    view: &str,
    view_dir: &Path,
    index: &mut Index,
    holder: LockHolder,
) -> Result<(IndexDb, bool), StoreError> {
    if view_dir.join(crate::index::INDEX_DIR).exists() {
        crate::index::durability::discard_index(view_dir)?;
    }
    let group = open_group_for_writer(root, holder)?;
    let view_id = group.allocate_view_id(&workspace_key(root), view)?;
    let db = group.open_view(view_id, &memory_store_dir(root), false)?;
    let rebuild = reconcile(index, &group, &db, view_id, view)?;
    Ok((db, rebuild))
}

/// The read-only open of a view in the group layout. Uses this process's handle when it has one;
/// otherwise takes the group lock only if it is free. `Ok(None)` serves from blobs: the lock is held
/// elsewhere, the view was never scanned into the group, or its file map does not match the group.
/// The second value says whether a scan owes the view a rebuild.
pub(super) fn open_index_read_only(
    root: &Path,
    view: &str,
    index: &Index,
) -> Result<(Option<IndexDb>, bool), StoreError> {
    let dir = group_index_dir(root);
    let group = match GroupDb::shared(&dir) {
        Some(group) => group,
        None if !dir.exists() || crate::store_lock::writer_lock_is_held(&dir) => return Ok((None, false)),
        None => match open_locked(&dir, LockHolder::Maintenance) {
            Ok(group) => group,
            Err(error) if error.is_lock_contention() => return Ok((None, false)),
            Err(error @ StoreError::Index(IndexError::NewerSchema { .. })) => return Err(error),
            Err(error) => {
                tracing::warn!(%error, "read-only group index open failed; degrading to blob-only reads");
                return Ok((None, false));
            }
        },
    };
    let Some(view_id) = group.view_id(&workspace_key(root), view)? else {
        return Ok((None, !index.files.is_empty()));
    };
    let db = group.open_view(view_id, &memory_store_dir(root), true)?;
    if !file_map_matches(index, &group, &db, view_id)? {
        return Ok((None, !index.files.is_empty()));
    }
    Ok((Some(db), false))
}

/// A fresh handle on `view` of `root`'s group, for a store whose handle was poisoned. While another
/// handle on the group is alive this is that same database.
pub(super) fn reopen_view(root: &Path, view: &str) -> Result<IndexDb, IndexError> {
    let dir = group_index_dir(root);
    let group = open_with_retry(&dir, LockHolder::Maintenance).map_err(|error| match error {
        StoreError::Index(error) => error,
        StoreError::Locked { .. } => IndexError::Fjall(fjall::Error::Locked),
        other => IndexError::Io {
            path: dir.clone(),
            source: std::io::Error::other(other.to_string()),
        },
    })?;
    let view_id = group.allocate_view_id(&workspace_key(root), view)?;
    group.open_view(view_id, &memory_store_dir(root), false)
}
