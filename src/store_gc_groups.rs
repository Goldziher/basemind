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

use crate::store::{CACHE_DIR, LockHolder, VIEWS_DIR, WORKSPACES_DIR, cache_root, read_workspace_marker};
use crate::store_gc::{GcError, dir_size};
use crate::store_layout::{GROUPS_DIR, repo_group_key};

/// What a reap of the group directories removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GroupStoreReap {
    /// Group directories removed.
    pub reaped: usize,
    /// Their on-disk size.
    pub bytes_freed: u64,
}

/// Reap the directory of every group no workspace of the machine-global cache names.
pub fn reap_orphaned_group_stores() -> Result<GroupStoreReap, GcError> {
    reap_orphaned_group_stores_in(&cache_root().join(CACHE_DIR))
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
    for entry in crate::store_gc::read_dir(&groups_dir)?.flatten() {
        let group_dir = entry.path();
        if named.contains(entry.file_name().to_string_lossy().as_ref()) || !group_dir.is_dir() {
            continue;
        }
        let Ok(lock) = crate::store_lock::try_acquire_lock_as(&group_dir, LockHolder::Maintenance) else {
            tracing::debug!(group = %group_dir.display(), "group is locked by another process; skipping");
            continue;
        };
        // One group that cannot be removed (a file held open on Windows, a protected directory) ~keep
        // is left for the next run; it must not keep the other groups, or the blob GC, from running. ~keep
        let removed = dir_size(&group_dir).and_then(|bytes| {
            crate::store_lock::remove_dir_under_lock(&group_dir)
                .map(|()| bytes)
                .map_err(|source| GcError::Io {
                    path: group_dir.clone(),
                    source,
                })
        });
        drop(lock);
        match removed {
            Ok(bytes) => {
                report.reaped += 1;
                report.bytes_freed += bytes;
                tracing::info!(group = %group_dir.display(), bytes, "reaped a group no workspace names");
            }
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
