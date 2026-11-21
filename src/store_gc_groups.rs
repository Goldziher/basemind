//! Reaping the derived state of repository groups no workspace belongs to any more.
//!
//! A group's directory (`cache/groups/<group>/`) holds its index (ADR-0028) and its retrieval store
//! (`lance/`, ADR-0037 rule 3), every row and vector of the group's files, so it is often the largest
//! thing a deleted repository leaves behind. The workspace reaper ([`crate::store_gc_workspace`])
//! removes the workspaces of a deleted worktree; once no workspace marker names the group, its
//! directory is reaped too, under the group lock, the way a workspace is
//! ([`crate::store_lock::remove_dir_under_lock`]). The policy is as conservative as the workspace
//! reaper's: a workspace whose group cannot be told (no marker but index state, or a marker that
//! names no group for a root that is gone) could belong to any group, and then nothing is reaped. A
//! group whose lock another process holds is skipped. The memory notes (`cache/repos/<group>/`)
//! are not derived state and stay.

use std::path::Path;

use ahash::AHashSet;

use crate::data_root::DataRoot;
use crate::store::{LockHolder, VIEWS_DIR, WORKSPACES_DIR, read_workspace_marker};
use crate::store_gc::{GcError, dir_size};
use crate::store_layout::{GROUPS_DIR, group_of_workspace, repo_group_key};

/// What a reap of the group directories removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GroupStoreReap {
    /// Group directories removed.
    pub reaped: usize,
    /// Their on-disk size.
    pub bytes_freed: u64,
}

/// Reap the directory of every group no workspace of the machine-global cache names.
pub fn reap_orphaned_group_stores(data: &DataRoot) -> Result<GroupStoreReap, GcError> {
    reap_orphaned_group_stores_in(&data.cache_dir())
}

/// The groups the workspaces under `workspaces_dir` belong to; `None` when one of them could belong
/// to any group.
fn named_groups(workspaces_dir: &Path) -> Option<AHashSet<String>> {
    let mut named = AHashSet::new();
    let Ok(entries) = std::fs::read_dir(workspaces_dir) else {
        // No workspaces at all names no group; one that cannot be listed could name any. ~keep
        return (!workspaces_dir.exists()).then_some(named);
    };
    for entry in entries.flatten() {
        let workspace = entry.path();
        if !workspace.is_dir() {
            continue;
        }
        match read_workspace_marker(&workspace) {
            Some(marker) => match marker.group {
                Some(group) => {
                    named.insert(group);
                }
                None if marker.root.exists() => {
                    named.insert(repo_group_key(&marker.root));
                }
                None => return None,
            },
            None if workspace.join(VIEWS_DIR).exists() => return None,
            None => {}
        }
    }
    Some(named)
}

/// Whether the workspaces under `workspaces_dir` name `group`, or one of them could.
fn group_may_be_named(workspaces_dir: &Path, group: &str) -> bool {
    named_groups(workspaces_dir).is_none_or(|named| named.contains(group))
}

/// Whether a workspace under `workspaces_dir` that still holds a view belongs to `group`, or may:
/// one whose group cannot be told could belong to any. A workspace with nothing under `views/`
/// (evicted, or never scanned) has no file map naming the group's rows.
pub(crate) fn group_in_use(workspaces_dir: &Path, group: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(workspaces_dir) else {
        return workspaces_dir.exists();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|workspace| std::fs::read_dir(workspace.join(VIEWS_DIR)).is_ok_and(|mut views| views.next().is_some()))
        .any(|workspace| group_of_workspace(&workspace).is_none_or(|named| named == group))
}

/// Remove the directory of `group` (its index and retrieval stores, all rebuildable) when no
/// workspace that holds a view belongs to it, for cache-budget eviction. Taken under the group
/// lock and checked again once it is held; a group whose lock another process holds, or that
/// cannot be removed, is kept. Returns the bytes freed, `0` when kept.
///
/// The memory notes are not under it (`cache/repos/<group>/`). A workspace scanned later opens a
/// new incarnation of the group, which no file map names, so nothing reads rows of the old one.
pub(crate) fn evict_idle_group(workspaces_dir: &Path, group: &str) -> Result<u64, GcError> {
    let Some(cache) = workspaces_dir.parent() else {
        return Ok(0);
    };
    let group_dir = cache.join(GROUPS_DIR).join(group);
    if !group_dir.is_dir() || group_in_use(workspaces_dir, group) {
        return Ok(0);
    }
    let Ok(lock) = crate::store_lock::try_acquire_lock_as(&group_dir, LockHolder::Maintenance) else {
        tracing::debug!(group = %group_dir.display(), "over-budget group is locked; keeping it");
        return Ok(0);
    };
    // A scan may have written a view of the group between the check and the lock. ~keep
    if group_in_use(workspaces_dir, group) {
        return Ok(0);
    }
    let bytes = dir_size(&group_dir)?;
    let removed = crate::store_lock::remove_dir_under_lock(&group_dir);
    drop(lock);
    match removed {
        Ok(()) => {
            tracing::info!(group = %group_dir.display(), bytes, "evicted a group no hosted view uses to enforce the size budget");
            Ok(bytes)
        }
        Err(error) => {
            tracing::warn!(group = %group_dir.display(), %error, "could not evict an idle group; kept");
            Ok(0)
        }
    }
}

/// The group directories under `cache_dir` that the workspaces in `workspaces` belong to, each
/// once.
pub(crate) fn group_dirs_of(cache_dir: &Path, workspaces: &[std::path::PathBuf]) -> Vec<std::path::PathBuf> {
    let groups: AHashSet<String> = workspaces.iter().filter_map(|ws| group_of_workspace(ws)).collect();
    groups
        .into_iter()
        .map(|group| cache_dir.join(GROUPS_DIR).join(group))
        .collect()
}

/// [`reap_orphaned_group_stores`] against an explicit `cache/` directory, so tests never touch the
/// machine-global cache.
pub(crate) fn reap_orphaned_group_stores_in(cache_dir: &Path) -> Result<GroupStoreReap, GcError> {
    let mut report = GroupStoreReap::default();
    let groups_dir = cache_dir.join(GROUPS_DIR);
    if !groups_dir.is_dir() {
        return Ok(report);
    }
    let Some(named) = named_groups(&cache_dir.join(WORKSPACES_DIR)) else {
        tracing::debug!("a workspace's group is unknown; keeping every group directory");
        return Ok(report);
    };
    crate::store_lock::purge_reaped(&groups_dir);
    let workspaces_dir = cache_dir.join(WORKSPACES_DIR);
    for entry in crate::store_gc::read_dir(&groups_dir)?.flatten() {
        let group_dir = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if named.contains(&name) || !group_dir.is_dir() {
            continue;
        }
        let Ok(lock) = crate::store_lock::try_acquire_lock_as(&group_dir, LockHolder::Maintenance) else {
            tracing::debug!(group = %group_dir.display(), "group is locked by another process; skipping");
            continue;
        };
        // One group that cannot be removed (a file held open on Windows, a protected directory) ~keep
        // is left for the next run; it must not keep the other groups, or the blob GC, from running. ~keep
        let removed = if group_may_be_named(&workspaces_dir, &name) {
            // A scan registered a workspace of the group between the snapshot and the lock. ~keep
            tracing::debug!(group = %group_dir.display(), "a workspace now names the group; keeping it");
            Ok(None)
        } else {
            dir_size(&group_dir).and_then(|bytes| {
                crate::store_lock::remove_dir_under_lock(&group_dir)
                    .map(|()| Some(bytes))
                    .map_err(|source| GcError::Io {
                        path: group_dir.clone(),
                        source,
                    })
            })
        };
        drop(lock);
        match removed {
            Ok(Some(bytes)) => {
                report.reaped += 1;
                report.bytes_freed += bytes;
                tracing::info!(group = %group_dir.display(), bytes, "reaped a group no workspace names");
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(group = %group_dir.display(), %error, "could not reap a group no workspace names; kept");
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;
    use crate::store_layout::GROUP_LANCE_DIR;

    /// `cache/` with a group store per name in `groups`, each holding a few bytes.
    fn cache_with_groups(groups: &[&str]) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cache = tmp.path().join("cache");
        for group in groups {
            let store = cache
                .join(GROUPS_DIR)
                .join(group)
                .join(GROUP_LANCE_DIR)
                .join("model-768-x");
            fs::create_dir_all(&store).expect("mk store");
            fs::write(store.join("rows"), b"rows").expect("write rows");
        }
        (tmp, cache)
    }

    fn workspace(cache: &Path, key: &str, marker: Option<&str>) -> PathBuf {
        let dir = cache.join(WORKSPACES_DIR).join(key);
        fs::create_dir_all(dir.join(VIEWS_DIR)).expect("mk workspace");
        if let Some(marker) = marker {
            fs::write(dir.join(crate::store::WORKSPACE_MARKER_FILE), marker).expect("write marker");
        }
        dir
    }

    fn store_of(cache: &Path, group: &str) -> PathBuf {
        cache.join(GROUPS_DIR).join(group).join(GROUP_LANCE_DIR)
    }

    /// The reaper names the groups once, then takes each group's lock: a workspace registered in
    /// between must be seen under the lock.
    #[test]
    fn a_group_a_workspace_registered_since_the_snapshot_names_is_kept_under_the_lock() {
        let (_tmp, cache) = cache_with_groups(&["late"]);
        let workspaces = cache.join(WORKSPACES_DIR);
        assert!(!group_may_be_named(&workspaces, "late"), "nothing names it yet");

        workspace(&cache, "w", Some(r#"{"root":"/gone","updated_unix":0,"group":"late"}"#));

        assert!(group_may_be_named(&workspaces, "late"), "the new workspace names it");
        assert!(!group_may_be_named(&workspaces, "other"));
    }

    #[test]
    fn the_store_of_a_group_no_workspace_names_is_reaped() {
        let (_tmp, cache) = cache_with_groups(&["live", "gone"]);
        workspace(&cache, "w", Some(r#"{"root":"/gone","updated_unix":0,"group":"live"}"#));

        let report = reap_orphaned_group_stores_in(&cache).expect("reap");

        assert_eq!(report.reaped, 1);
        assert!(report.bytes_freed > 0);
        assert!(store_of(&cache, "live").exists(), "a named group keeps its store");
        assert!(
            !cache.join(GROUPS_DIR).join("gone").exists(),
            "no workspace names it: the whole group directory goes"
        );
    }

    /// A group's index is reaped with the group: a deleted repository leaves no group directory.
    #[test]
    fn the_index_of_a_group_no_workspace_names_is_reaped_with_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cache = tmp.path().join("cache");
        for group in ["live", "gone"] {
            let index = cache.join(GROUPS_DIR).join(group).join(crate::index::INDEX_DIR);
            fs::create_dir_all(&index).expect("mk group index");
            fs::write(index.join("journal"), b"rows").expect("write rows");
        }
        let memory = cache.join(crate::store_layout::REPOS_DIR).join("gone");
        fs::create_dir_all(&memory).expect("mk memory");
        workspace(&cache, "w", Some(r#"{"root":"/gone","updated_unix":0,"group":"live"}"#));

        let report = reap_orphaned_group_stores_in(&cache).expect("reap");

        assert_eq!(report.reaped, 1);
        assert!(
            cache
                .join(GROUPS_DIR)
                .join("live")
                .join(crate::index::INDEX_DIR)
                .exists()
        );
        assert!(!cache.join(GROUPS_DIR).join("gone").exists());
        assert!(memory.exists(), "memory notes are not derived state");
    }

    #[test]
    fn a_workspace_of_unknown_group_keeps_every_store() {
        let (_tmp, cache) = cache_with_groups(&["gone"]);
        workspace(&cache, "markerless", None);
        assert_eq!(reap_orphaned_group_stores_in(&cache).expect("reap").reaped, 0);

        let (_tmp, cache) = cache_with_groups(&["gone"]);
        workspace(&cache, "old", Some(r#"{"root":"/no/such/root","updated_unix":0}"#));
        assert_eq!(reap_orphaned_group_stores_in(&cache).expect("reap").reaped, 0);
        assert!(store_of(&cache, "gone").exists());
    }

    /// `cache stats` counts the group's code index (ADR-0028 moved the code index out of the
    /// workspace) in its own field and in the total, and the components still reconcile.
    #[test]
    fn cache_stats_count_the_group_index() {
        use crate::store_cache_admin::{BlobStorePaths, cache_stats_in};
        let (_tmp, cache) = cache_with_groups(&["g"]);
        let index = cache.join(GROUPS_DIR).join("g").join(crate::index::INDEX_DIR);
        fs::create_dir_all(&index).expect("mk group index");
        fs::write(index.join("journal"), vec![7u8; 64 * 1024]).expect("write rows");
        let basemind_dir = workspace(&cache, "w", Some(r#"{"root":"/gone","updated_unix":0,"group":"g"}"#));
        let blobs = BlobStorePaths {
            root: cache.join("blobs"),
            schema: crate::extract::SCHEMA_VER,
            gc_state: cache.join("gc-state.json"),
            repos: cache.join("repos"),
        };
        let index_bytes = dir_size(&index).expect("size");

        let stats = cache_stats_in(&basemind_dir, &blobs).expect("stats");

        assert_eq!(stats.group_index_bytes, index_bytes, "{stats:?}");
        assert!(
            stats.total_bytes >= dir_size(&basemind_dir).expect("size") + index_bytes,
            "{stats:?}"
        );
        let components = stats.blobs_bytes
            + stats.views_bytes
            + stats.group_index_bytes
            + stats.lance_bytes
            + stats.memory_vectors_bytes
            + stats.git_cache_bytes
            + stats.telemetry_bytes
            + stats.git_history_bytes;
        assert_eq!(stats.total_bytes, components + stats.other_bytes, "{stats:?}");
    }

    /// `cache stats` counts the group's retrieval store; `cache clear lance` and `cache clear all`
    /// remove it, and leave the group index beside it.
    #[cfg(feature = "intelligence")]
    #[test]
    fn cache_stats_count_and_clears_remove_the_group_store() {
        use crate::store_cache_admin::{BlobStorePaths, CacheComponent, cache_stats_in, clear_component_in};
        for component in [CacheComponent::Lance, CacheComponent::All] {
            let (_tmp, cache) = cache_with_groups(&["g"]);
            let index = cache.join(GROUPS_DIR).join("g").join(crate::index::INDEX_DIR);
            fs::create_dir_all(&index).expect("mk group index");
            let basemind_dir = workspace(&cache, "w", Some(r#"{"root":"/gone","updated_unix":0,"group":"g"}"#));
            let blobs = BlobStorePaths {
                root: cache.join("blobs"),
                schema: crate::extract::SCHEMA_VER,
                gc_state: cache.join("gc-state.json"),
                repos: cache.join("repos"),
            };
            let store_bytes = dir_size(&store_of(&cache, "g")).expect("size");

            let stats = cache_stats_in(&basemind_dir, &blobs).expect("stats");
            assert!(stats.lance_bytes >= store_bytes && store_bytes > 0, "{stats:?}");
            assert!(stats.total_bytes >= stats.lance_bytes);

            clear_component_in(&basemind_dir, component, &blobs).expect("clear");
            assert!(!store_of(&cache, "g").exists(), "{component:?} removes the group store");
            assert!(index.exists(), "the group index stays");
        }
    }

    /// The group's retrieval store is written by whichever process holds the group lock, for any
    /// worktree of the group: a clear from a sibling worktree whose own lock is free is refused
    /// while that lock is held, and the store stays.
    #[cfg(feature = "intelligence")]
    #[test]
    fn a_clear_of_the_group_store_under_a_held_group_lock_is_refused() {
        use crate::store_cache_admin::{BlobStorePaths, CacheComponent, clear_component_in};
        for component in [CacheComponent::Lance, CacheComponent::All] {
            let (_tmp, cache) = cache_with_groups(&["g"]);
            let basemind_dir = workspace(&cache, "w", Some(r#"{"root":"/gone","updated_unix":0,"group":"g"}"#));
            let blobs = BlobStorePaths {
                root: cache.join("blobs"),
                schema: crate::extract::SCHEMA_VER,
                gc_state: cache.join("gc-state.json"),
                repos: cache.join("repos"),
            };
            let _host = crate::store_lock::acquire_lock(&cache.join(GROUPS_DIR).join("g")).expect("group lock");

            let refused = clear_component_in(&basemind_dir, component, &blobs);

            assert!(
                matches!(refused, Err(GcError::Store(crate::store::StoreError::Locked { .. }))),
                "{component:?}: {refused:?}"
            );
            assert!(store_of(&cache, "g").exists(), "{component:?} kept the group store");
            assert!(basemind_dir.exists(), "{component:?} kept the workspace");
        }
    }

    /// A group that cannot be removed (a held file on Windows, a protected directory) is logged and
    /// left; the reaper goes on with the others and the caller's blob GC still runs.
    #[cfg(unix)]
    #[test]
    fn a_group_that_cannot_be_removed_does_not_stop_the_reaper() {
        use std::os::unix::fs::PermissionsExt;
        let (tmp, cache) = cache_with_groups(&["stuck", "gone"]);
        let protected = store_of(&cache, "stuck").join("model-768-x");
        fs::set_permissions(&protected, fs::Permissions::from_mode(0o555)).expect("chmod");

        let report = reap_orphaned_group_stores_in(&cache);

        for dir in dirs_under(tmp.path()) {
            let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o755));
        }
        let report = report.expect("one group failing is not the reaper failing");
        assert_eq!(report.reaped, 1);
        assert!(!cache.join(GROUPS_DIR).join("gone").exists());
    }

    /// Every directory under `dir`, for restoring permissions before the tempdir is removed.
    #[cfg(unix)]
    fn dirs_under(dir: &Path) -> Vec<PathBuf> {
        let mut out = vec![dir.to_path_buf()];
        let mut at = 0;
        while at < out.len() {
            if let Ok(entries) = fs::read_dir(&out[at]) {
                out.extend(entries.flatten().map(|entry| entry.path()).filter(|path| path.is_dir()));
            }
            at += 1;
        }
        out
    }

    #[test]
    fn a_locked_group_is_skipped() {
        let (_tmp, cache) = cache_with_groups(&["busy"]);
        let _held = crate::store_lock::acquire_lock(&cache.join(GROUPS_DIR).join("busy")).expect("lock");

        assert_eq!(reap_orphaned_group_stores_in(&cache).expect("reap").reaped, 0);
        assert!(store_of(&cache, "busy").exists());
    }
}
