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
//! When the cache is over its size budget, budget enforcement deletes every per-view index left,
//! adopted or not, before it evicts any workspace ([`evict_legacy_indexes_in`]): this release reads
//! none of them, and a release up to 0.33 that opens a view whose database is gone rebuilds it.
//! `cache stats` counts the ones left on the machine ([`legacy_indexes_in`]).
//!
//! An older basemind that is still running may serve a worktree from its old database. Every
//! release that reads one reads the un-namespaced blobs, so while [`Holds::live`] names
//! [`BlobNamespace::Unnamespaced`], or a live basemind's namespace cannot be told
//! ([`Holds::unidentified`]), nothing is deleted. A workspace whose lock another process holds is
//! skipped, and so is one holding a view a newer basemind wrote.

use std::path::{Path, PathBuf};

use crate::data_root::DataRoot;
use crate::index::INDEX_DIR;
use crate::store::{LockHolder, StoreError, VIEWS_DIR};
use crate::store_gc::{GcError, dir_size, read_dir};
use crate::store_gc_namespaces::{BlobNamespace, Holds};

/// What a reap of per-view indexes removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct LegacyIndexReap {
    /// Per-view index databases removed.
    pub(crate) reaped: usize,
    /// Their on-disk size.
    pub(crate) bytes_freed: u64,
}

/// The per-view indexes left under a workspaces directory.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct LegacyIndexes {
    /// Per-view index databases.
    pub(crate) count: usize,
    /// Their on-disk size.
    pub(crate) bytes: u64,
}

/// Reap the per-view index of every view the group index adopted, under the machine-global cache.
pub(crate) fn reap_adopted_legacy_indexes(data: &DataRoot) -> Result<LegacyIndexReap, GcError> {
    reap_adopted_legacy_indexes_in(&data.workspaces_dir(), &Holds::under(data))
}

/// Whether a live basemind process may read a per-view index: one of a release that reads the
/// un-namespaced blobs, all of which kept one database per view, is among the live namespaces, or
/// one runs whose namespace cannot be told.
fn legacy_indexes_in_use(holds: &Holds) -> bool {
    holds.unidentified || holds.live.contains(&BlobNamespace::Unnamespaced)
}

/// The paths in `dir`; none when `dir` does not exist. Any other error reading it is returned, so a
/// walk never passes over what it could not list.
fn paths_in(dir: &Path) -> Result<Vec<PathBuf>, GcError> {
    let io = |source| GcError::Io {
        path: dir.to_path_buf(),
        source,
    };
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(io(source)),
    };
    entries
        .map(|entry| entry.map(|entry| entry.path()).map_err(io))
        .collect()
}

/// The view directories of `workspace` that still hold a per-view index.
fn views_with_legacy_index(workspace: &Path) -> Result<Vec<PathBuf>, GcError> {
    let mut views = paths_in(&workspace.join(VIEWS_DIR))?;
    views.retain(|view| view.join(INDEX_DIR).is_dir());
    Ok(views)
}

/// Whether the group index of `workspace`'s repository exists, so the group has adopted its views.
fn adopted_by_group(workspace: &Path) -> bool {
    crate::store_layout::group_dir_of(workspace).is_some_and(|group| group.join(INDEX_DIR).is_dir())
}

/// [`reap_adopted_legacy_indexes`] against an explicit workspaces directory and holds, so tests
/// never touch the machine-global cache nor depend on the processes running on the machine. A
/// workspace holding a view a newer basemind wrote is that version's, as in budget enforcement.
pub(crate) fn reap_adopted_legacy_indexes_in(workspaces_dir: &Path, holds: &Holds) -> Result<LegacyIndexReap, GcError> {
    reap_legacy_indexes_where(workspaces_dir, holds, |workspace| {
        adopted_by_group(workspace) && !crate::store_gc_budget::holds_a_newer_view(workspace)
    })
}

/// Reap the per-view index of every view under `workspaces_dir`, whether the group index adopted it
/// or not, except in the workspaces `spared` names. Budget enforcement runs it before it evicts any
/// workspace, with the same liveness guard and workspace lock as [`reap_adopted_legacy_indexes`].
pub(crate) fn evict_legacy_indexes_in(
    workspaces_dir: &Path,
    holds: &Holds,
    spared: &[PathBuf],
) -> Result<LegacyIndexReap, GcError> {
    reap_legacy_indexes_where(workspaces_dir, holds, |workspace| {
        !spared.iter().any(|dir| dir == workspace)
    })
}

/// The per-view indexes left in the workspaces under `workspaces_dir`, and their size. Reads the
/// directory tree only; touches nothing.
pub(crate) fn legacy_indexes_in(workspaces_dir: &Path) -> Result<LegacyIndexes, GcError> {
    let mut found = LegacyIndexes::default();
    for workspace in paths_in(workspaces_dir)? {
        if !workspace.is_dir() {
            continue;
        }
        for view in views_with_legacy_index(&workspace)? {
            found.count += 1;
            found.bytes += dir_size(&view.join(INDEX_DIR))?;
        }
    }
    Ok(found)
}

/// Reap the per-view indexes of every workspace under `workspaces_dir` that `eligible` accepts,
/// unless a live basemind may read them.
fn reap_legacy_indexes_where(
    workspaces_dir: &Path,
    holds: &Holds,
    eligible: impl Fn(&Path) -> bool,
) -> Result<LegacyIndexReap, GcError> {
    let mut report = LegacyIndexReap::default();
    if !workspaces_dir.exists() {
        return Ok(report);
    }
    if legacy_indexes_in_use(holds) {
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
        if !workspace.is_dir() {
            continue;
        }
        let has_legacy_index = match views_with_legacy_index(&workspace) {
            Ok(views) => !views.is_empty(),
            Err(error) => {
                tracing::warn!(workspace = %workspace.display(), %error, "could not list its views; kept its per-view indexes");
                false
            }
        };
        if has_legacy_index
            && eligible(workspace.as_path())
            && let Err(error) = reap_workspace(&workspace, &mut report)
        {
            tracing::warn!(workspace = %workspace.display(), %error, "could not reap its per-view indexes; kept");
        }
    }
    Ok(report)
}

/// Remove the per-view indexes of one `workspace` under its lock, once its old memory notes
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
    // Measured before the rescue opens the databases, which rewrites their journals. ~keep
    let measured = views_with_legacy_index(workspace)?
        .into_iter()
        .map(|view| Ok((dir_size(&view.join(INDEX_DIR))?, view)))
        .collect::<Result<Vec<_>, GcError>>()?;
    if !crate::store_gc_live::rescue_legacy_memory(workspace) {
        return Ok(());
    }
    for (bytes, view) in measured {
        match crate::index::durability::discard_index(&view) {
            Ok(()) => {
                report.reaped += 1;
                report.bytes_freed += bytes;
                tracing::info!(view = %view.display(), bytes, "removed the per-view index of a release up to 0.33");
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
    use crate::store_cache_admin::{BlobStorePaths, cache_stats_in};
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
            plant_legacy_view_in(&self.workspace, view)
        }

        /// Where `cache stats` reads the machine-global stores of this cache.
        fn stores(&self) -> BlobStorePaths {
            let cache = self.tmp.path().join("cache");
            BlobStorePaths {
                root: cache.join("blobs"),
                schema: crate::extract::SCHEMA_VER,
                gc_state: cache.join("gc-state.json"),
                repos: cache.join(crate::store::REPOS_DIR),
            }
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

    /// Give `view` of `workspace` a file map and a per-view index, as a release up to 0.33 left them.
    fn plant_legacy_view_in(workspace: &Path, view: &str) -> PathBuf {
        let view_dir = workspace.join(VIEWS_DIR).join(view);
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

    #[test]
    fn one_pass_removes_every_adopted_view_s_index_and_keeps_its_file_map() {
        let cache = Cache::new();
        let working = cache.plant_legacy_view("working");
        let staged = cache.plant_legacy_view("staged");
        cache.adopt();

        let report = reap_adopted_legacy_indexes_in(&cache.workspaces(), &Holds::default()).expect("reap");

        assert_eq!(report.reaped, 2);
        assert!(report.bytes_freed > 0, "the removed databases' bytes are counted");
        for view in [&working, &staged] {
            assert!(!view.join(INDEX_DIR).exists(), "{} kept its index", view.display());
            assert!(view.join(INDEX_FILE).exists(), "{} lost its file map", view.display());
        }
        let again = reap_adopted_legacy_indexes_in(&cache.workspaces(), &Holds::default()).expect("reap again");
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

            let holds = Holds {
                live: live.to_vec(),
                ..Holds::default()
            };
            let report = reap_adopted_legacy_indexes_in(&cache.workspaces(), &holds).expect("reap");

            assert_eq!(report.reaped, *reaped, "{case}");
            assert_eq!(working.join(INDEX_DIR).exists(), *reaped == 0, "{case}");
        }
    }

    /// A live basemind whose namespace cannot be told may be a release up to 0.33 reading its
    /// per-view index, so none is removed while it runs.
    #[test]
    fn a_live_basemind_of_an_unknown_build_keeps_them() {
        let cache = Cache::new();
        let working = cache.plant_legacy_view("working");
        cache.adopt();
        let holds = Holds {
            unidentified: true,
            ..Holds::default()
        };

        let adopted = reap_adopted_legacy_indexes_in(&cache.workspaces(), &holds).expect("reap");
        let evicted = evict_legacy_indexes_in(&cache.workspaces(), &holds, &[]).expect("evict");

        assert_eq!(
            (adopted, evicted),
            (LegacyIndexReap::default(), LegacyIndexReap::default())
        );
        assert!(working.join(INDEX_DIR).is_dir());
    }

    /// A workspace holding a view a newer basemind wrote is that version's: its per-view indexes
    /// stay, adopted or not, as budget enforcement spares it.
    #[test]
    fn a_workspace_holding_a_newer_view_keeps_its_indexes() {
        let cache = Cache::new();
        let working = cache.plant_legacy_view("working");
        cache.adopt();
        let mut newer = Index::empty();
        newer.schema_ver = crate::extract::SCHEMA_VER + 1;
        let newer_view = cache.workspace.join(VIEWS_DIR).join("newer");
        fs::create_dir_all(&newer_view).expect("mk newer view");
        fs::write(
            newer_view.join(INDEX_FILE),
            rmp_serde::to_vec_named(&newer).expect("encode index"),
        )
        .expect("write the newer file map");

        let report = reap_adopted_legacy_indexes_in(&cache.workspaces(), &Holds::default()).expect("reap");

        assert_eq!(report, LegacyIndexReap::default());
        assert!(working.join(INDEX_DIR).is_dir());
    }

    /// A views directory that cannot be listed is an error to `cache stats`, not a workspace with
    /// no per-view index; a cache with no workspaces directory has none.
    #[test]
    fn a_views_directory_that_cannot_be_listed_is_an_error() {
        let cache = Cache::new();
        assert_eq!(
            legacy_indexes_in(&cache.tmp.path().join("absent")).expect("no workspaces"),
            LegacyIndexes::default()
        );
        fs::write(cache.workspace.join(VIEWS_DIR), b"not a directory").expect("plant a file as views");

        assert!(legacy_indexes_in(&cache.workspaces()).is_err());
    }

    #[test]
    fn a_view_no_group_index_adopted_keeps_its_index() {
        let cache = Cache::new();
        let working = cache.plant_legacy_view("working");

        let report = reap_adopted_legacy_indexes_in(&cache.workspaces(), &Holds::default()).expect("reap");

        assert_eq!(report, LegacyIndexReap::default());
        assert!(working.join(INDEX_DIR).is_dir());
    }

    #[test]
    fn a_locked_workspace_keeps_its_indexes() {
        let cache = Cache::new();
        let working = cache.plant_legacy_view("working");
        cache.adopt();
        let held = crate::store::acquire_lock(&cache.workspace).expect("hold the workspace lock");

        let report = reap_adopted_legacy_indexes_in(&cache.workspaces(), &Holds::default()).expect("reap");
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

        let report = reap_adopted_legacy_indexes_in(&cache.workspaces(), &Holds::default()).expect("reap");

        assert_eq!(report.reaped, 1);
        assert!(!working.join(INDEX_DIR).exists());
        let root = read_workspace_marker(&cache.workspace).expect("marker").root;
        let memory_dir = crate::store_layout::memory_store_dir(&root);
        let reader = cache.tmp.path().join("reader");
        let db = crate::index::IndexDb::open_with_memory(&reader, &memory_dir).expect("open the group store");
        assert_eq!(db.notes().get(b"k").unwrap().as_deref(), Some(&b"only copy"[..]));
    }

    /// Over budget, every per-view index goes, adopted by the group index or not, except in a
    /// workspace the pass spares.
    #[test]
    fn budget_eviction_removes_every_per_view_index_but_a_spared_workspace_s() {
        let cache = Cache::new();
        let working = cache.plant_legacy_view("working");
        let other = cache.workspaces().join("other");
        let spared = plant_legacy_view_in(&other, "working");

        let report = evict_legacy_indexes_in(&cache.workspaces(), &Holds::default(), std::slice::from_ref(&other))
            .expect("evict");

        assert_eq!(report.reaped, 1);
        assert!(!working.join(INDEX_DIR).exists(), "the unadopted view's index goes");
        assert!(working.join(INDEX_FILE).exists(), "its file map stays");
        assert!(spared.join(INDEX_DIR).is_dir(), "the spared workspace keeps its index");
    }

    /// `cache stats` counts the per-view indexes left in every workspace on the machine; after one
    /// pass only the one in a workspace no group index adopted is left.
    #[test]
    fn cache_stats_count_the_per_view_indexes_left_on_the_machine() {
        let cache = Cache::new();
        let working = cache.plant_legacy_view("working");
        let staged = cache.plant_legacy_view("staged");
        let other = plant_legacy_view_in(&cache.workspaces().join("other"), "working");
        cache.adopt();
        let size = |view: &Path| dir_size(&view.join(INDEX_DIR)).expect("size");
        let (working_bytes, staged_bytes, other_bytes) = (size(&working), size(&staged), size(&other));

        let before = cache_stats_in(&cache.workspace, &cache.stores()).expect("stats");
        reap_adopted_legacy_indexes_in(&cache.workspaces(), &Holds::default()).expect("reap");
        let after = cache_stats_in(&cache.workspace, &cache.stores()).expect("stats");

        assert_eq!(
            (before.legacy_indexes, before.legacy_index_bytes),
            (3, working_bytes + staged_bytes + other_bytes)
        );
        assert_eq!((after.legacy_indexes, after.legacy_index_bytes), (1, other_bytes));
    }
}
