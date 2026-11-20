//! Reaping the per-view indexes the group index replaced (ADR-0028, ADR-0054).
//!
//! Releases up to 0.33 kept one Fjall database per view, `views/<view>/index.fjall`. This release
//! never reads one: the group index of the repository (`cache/groups/<group>/index.fjall`) holds
//! every view's rows. A writer open of a view deletes its old database, but a worktree nobody opens
//! after the upgrade kept it for good. Once the group index of a workspace's repository exists, the
//! group has adopted every view of the workspace, and this sweep deletes their old databases. Memory
//! notes a release before 0.30 kept in one are moved to the group's notes store first
//! ([`crate::store_gc_live::rescue_legacy_memory`]); a workspace whose notes cannot be moved keeps
//! its databases.
//!
//! An older basemind that is still running may serve a worktree from its old database. Every
//! release that reads one reads the un-namespaced blobs, so while [`Holds::live`] names
//! [`BlobNamespace::Unnamespaced`] nothing is deleted. A workspace whose lock another process holds
//! is skipped.

use std::path::{Path, PathBuf};

use crate::data_root::DataRoot;
use crate::index::INDEX_DIR;
use crate::store::{LockHolder, StoreError, VIEWS_DIR};
use crate::store_gc::{GcError, dir_size, read_dir};
use crate::store_gc_namespaces::{BlobNamespace, Holds};

/// What a reap of the adopted per-view indexes removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct LegacyIndexReap {
    /// Per-view index databases removed.
    pub(crate) reaped: usize,
    /// Their on-disk size.
    pub(crate) bytes_freed: u64,
}

/// Reap the per-view index of every view the group index adopted, under the machine-global cache.
pub(crate) fn reap_adopted_legacy_indexes(data: &DataRoot) -> Result<LegacyIndexReap, GcError> {
    reap_adopted_legacy_indexes_in(&data.workspaces_dir(), &Holds::under(data).live)
}

/// Whether a live basemind process may read a per-view index: one of a release that reads the
/// un-namespaced blobs, all of which kept one database per view, is among the `live` namespaces.
fn legacy_indexes_in_use(live: &[BlobNamespace]) -> bool {
    live.contains(&BlobNamespace::Unnamespaced)
}

/// The view directories of `workspace` that still hold a per-view index.
fn views_with_legacy_index(workspace: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(workspace.join(VIEWS_DIR)) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|view| view.join(INDEX_DIR).is_dir())
        .collect()
}

/// Whether the group index of `workspace`'s repository exists, so the group has adopted its views.
fn adopted_by_group(workspace: &Path) -> bool {
    crate::store_layout::group_dir_of(workspace).is_some_and(|group| group.join(INDEX_DIR).is_dir())
}

/// [`reap_adopted_legacy_indexes`] against an explicit workspaces directory and live namespaces, so
/// tests never touch the machine-global cache nor depend on the processes running on the machine.
pub(crate) fn reap_adopted_legacy_indexes_in(
    workspaces_dir: &Path,
    live: &[BlobNamespace],
) -> Result<LegacyIndexReap, GcError> {
    let mut report = LegacyIndexReap::default();
    if !workspaces_dir.exists() {
        return Ok(report);
    }
    if legacy_indexes_in_use(live) {
        tracing::debug!("a live basemind of a release with per-view indexes is registered; keeping them");
        return Ok(report);
    }
    for entry in read_dir(workspaces_dir)? {
        let workspace = entry
            .map_err(|source| GcError::Io {
                path: workspaces_dir.to_path_buf(),
                source,
            })?
            .path();
        if workspace.is_dir()
            && !views_with_legacy_index(&workspace).is_empty()
            && adopted_by_group(&workspace)
            && let Err(error) = reap_workspace(&workspace, &mut report)
        {
            tracing::warn!(workspace = %workspace.display(), %error, "could not reap its per-view indexes; kept");
        }
    }
    Ok(report)
}

/// Remove the per-view indexes of one adopted `workspace` under its lock, once its old memory notes
/// are in the group's notes store. A view whose index cannot be removed is kept and logged; an error
/// keeps the rest of the workspace's indexes, never those of another workspace.
fn reap_workspace(workspace: &Path, report: &mut LegacyIndexReap) -> Result<(), GcError> {
    let lock = match crate::store_lock::try_acquire_lock_as(workspace, LockHolder::Maintenance) {
        Ok(lock) => lock,
        Err(StoreError::Locked { .. }) => {
            tracing::debug!(workspace = %workspace.display(), "workspace is locked; keeping its per-view indexes");
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    if !crate::store_gc_live::rescue_legacy_memory(workspace) {
        return Ok(());
    }
    for view in views_with_legacy_index(workspace) {
        let bytes = dir_size(&view.join(INDEX_DIR))?;
        match crate::index::durability::discard_index(&view) {
            Ok(()) => {
                report.reaped += 1;
                report.bytes_freed += bytes;
                tracing::info!(view = %view.display(), bytes, "removed a per-view index the group index adopted");
            }
            Err(error) => {
                tracing::warn!(view = %view.display(), %error, "could not remove a per-view index; kept");
            }
        }
    }
    drop(lock);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{INDEX_FILE, Index, ensure_workspace_marker, read_workspace_marker};
    use crate::store_layout::GROUPS_DIR;
    use std::fs;

    /// A cache with one workspace whose root exists, keyed `key`: `<tmp>/cache/workspaces/<key>/`.
    struct Cache {
        tmp: tempfile::TempDir,
        workspace: PathBuf,
    }

    impl Cache {
        fn new() -> Self {
            let tmp = tempfile::tempdir().expect("tempdir");
            let root = tmp.path().join("repo");
            fs::create_dir_all(&root).expect("mk root");
            let workspace = tmp.path().join("cache").join("workspaces").join("key");
            fs::create_dir_all(&workspace).expect("mk workspace");
            ensure_workspace_marker(&workspace, &root);
            assert!(
                read_workspace_marker(&workspace).is_some(),
                "fixture must land a marker"
            );
            Self { tmp, workspace }
        }

        fn workspaces(&self) -> PathBuf {
            self.tmp.path().join("cache").join("workspaces")
        }

        /// Give `view` a file map and a per-view index, as a release up to 0.33 left them.
        fn plant_legacy_view(&self, view: &str) -> PathBuf {
            let view_dir = self.workspace.join(VIEWS_DIR).join(view);
            fs::create_dir_all(&view_dir).expect("mk view");
            fs::write(
                view_dir.join(INDEX_FILE),
                rmp_serde::to_vec_named(&Index::empty()).expect("encode index"),
            )
            .expect("write the file map");
            drop(
                fjall::Database::builder(view_dir.join(INDEX_DIR))
                    .open()
                    .expect("create the view's index"),
            );
            view_dir
        }

        /// Create the group index of the workspace's repository: the group adopts its views.
        fn adopt(&self) {
            let group = read_workspace_marker(&self.workspace)
                .and_then(|marker| marker.group)
                .expect("the marker names the group");
            let index = self
                .tmp
                .path()
                .join("cache")
                .join(GROUPS_DIR)
                .join(group)
                .join(INDEX_DIR);
            fs::create_dir_all(index).expect("mk group index");
        }
    }

    #[test]
    fn one_pass_removes_every_adopted_view_s_index_and_keeps_its_file_map() {
        let cache = Cache::new();
        let working = cache.plant_legacy_view("working");
        let staged = cache.plant_legacy_view("staged");
        cache.adopt();

        let report = reap_adopted_legacy_indexes_in(&cache.workspaces(), &[]).expect("reap");

        assert_eq!(report.reaped, 2);
        assert!(report.bytes_freed > 0, "the removed databases' bytes are counted");
        for view in [&working, &staged] {
            assert!(!view.join(INDEX_DIR).exists(), "{} kept its index", view.display());
            assert!(view.join(INDEX_FILE).exists(), "{} lost its file map", view.display());
        }
        let again = reap_adopted_legacy_indexes_in(&cache.workspaces(), &[]).expect("reap again");
        assert_eq!(again, LegacyIndexReap::default());
    }

    /// A row of `a_live_basemind_with_per_view_indexes_keeps_them`: the case, the live namespaces,
    /// and how many indexes one pass removes.
    type LiveCase = (&'static str, &'static [BlobNamespace], usize);

    #[test]
    fn a_live_basemind_with_per_view_indexes_keeps_them() {
        let cases: &[LiveCase] = &[
            ("a live 0.33 daemon", &[BlobNamespace::Unnamespaced], 0),
            (
                "a live 0.33 beside a live 0.34",
                &[BlobNamespace::Schema(34), BlobNamespace::Unnamespaced],
                0,
            ),
            ("only a live 0.34", &[BlobNamespace::Schema(34)], 1),
            ("nothing live", &[], 1),
        ];
        for (case, live, reaped) in cases {
            let cache = Cache::new();
            let working = cache.plant_legacy_view("working");
            cache.adopt();

            let report = reap_adopted_legacy_indexes_in(&cache.workspaces(), live).expect("reap");

            assert_eq!(report.reaped, *reaped, "{case}");
            assert_eq!(working.join(INDEX_DIR).exists(), *reaped == 0, "{case}");
        }
    }

    #[test]
    fn a_view_no_group_index_adopted_keeps_its_index() {
        let cache = Cache::new();
        let working = cache.plant_legacy_view("working");

        let report = reap_adopted_legacy_indexes_in(&cache.workspaces(), &[]).expect("reap");

        assert_eq!(report, LegacyIndexReap::default());
        assert!(working.join(INDEX_DIR).is_dir());
    }

    #[test]
    fn a_locked_workspace_keeps_its_indexes() {
        let cache = Cache::new();
        let working = cache.plant_legacy_view("working");
        cache.adopt();
        let held = crate::store::acquire_lock(&cache.workspace).expect("hold the workspace lock");

        let report = reap_adopted_legacy_indexes_in(&cache.workspaces(), &[]).expect("reap");
        drop(held);

        assert_eq!(report, LegacyIndexReap::default());
        assert!(working.join(INDEX_DIR).is_dir());
    }

    /// A release before 0.30 kept memory notes in each view's index. They are the only copy, so
    /// they move to the group's notes store before the index goes.
    #[test]
    fn an_adopted_view_s_old_notes_move_to_the_group_store_before_its_index_goes() {
        crate::store::init_isolated_cache();
        let cache = Cache::new();
        let working = cache.plant_legacy_view("working");
        {
            let db = fjall::Database::builder(working.join(INDEX_DIR))
                .open()
                .expect("open the view's index");
            db.keyspace("memory_by_key", fjall::KeyspaceCreateOptions::default)
                .expect("the notes keyspace of a 0.29 view")
                .insert(b"k".to_vec(), b"only copy".to_vec())
                .expect("write a note");
        }
        cache.adopt();

        let report = reap_adopted_legacy_indexes_in(&cache.workspaces(), &[]).expect("reap");

        assert_eq!(report.reaped, 1);
        assert!(!working.join(INDEX_DIR).exists());
        let root = read_workspace_marker(&cache.workspace).expect("marker").root;
        let memory_dir = crate::store_layout::memory_store_dir(&root);
        let reader = cache.tmp.path().join("reader");
        let db = crate::index::IndexDb::open_with_memory(&reader, &memory_dir).expect("open the group store");
        assert_eq!(db.notes().get(b"k").unwrap().as_deref(), Some(&b"only copy"[..]));
    }
}
