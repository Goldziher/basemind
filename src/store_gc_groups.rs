//! Reaping the derived state of repository groups no workspace belongs to any more.
//!
//! A group's directory (`cache/groups/<group>/`) holds its index (ADR-0028) and its retrieval store
//! (`lance/`, ADR-0037 rule 3), every row and vector of the group's files, so it is often the largest
//! thing a deleted repository leaves behind. The workspace reaper ([`crate::store_gc_workspace`])
//! removes the workspaces of a deleted worktree; once no workspace marker names the group, its
//! directory is reaped too, under the group lock, the way a workspace is
//! (`crate::store_lock::remove_dir_under_lock`). The policy is as conservative as the workspace
//! reaper's: a workspace whose group cannot be told (no marker but index state, or a marker that
//! names no group for a root that is gone) could belong to any group, and then nothing is reaped. A
//! group whose lock another process holds is skipped. The memory notes (`cache/repos/<group>/`)
//! are not derived state and stay.

use std::path::Path;

use ahash::AHashSet;

use crate::data_root::DataRoot;
use crate::store::{LockHolder, VIEWS_DIR, WORKSPACES_DIR, read_workspace_marker};
use crate::store_gc::{GcError, dir_size};
use crate::store_gc_namespaces::{BlobNamespace, Holds};
use crate::store_layout::{GROUPS_DIR, LAST_UNNAMESPACED_GROUP_MINOR, group_of_workspace, repo_group_key};

/// What a reap of the group directories removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GroupStoreReap {
    /// Group directories removed, and the group indexes of older releases.
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

/// What a release minor older than the running one left in a group directory: its group database
/// directory, and who may still read it.
struct SupersededIndex {
    /// The directory removed: `v<minor>/`, or the legacy `index.fjall` of the releases up to
    /// [`LAST_UNNAMESPACED_GROUP_MINOR`].
    path: std::path::PathBuf,
    /// Whether a live process of `holds` reads it.
    in_use: fn(&Holds, u16) -> bool,
    /// The minor a namespaced directory is of; unused by the legacy one.
    minor: u16,
}

fn namespace_in_use(holds: &Holds, minor: u16) -> bool {
    holds.live.contains(&BlobNamespace::Schema(minor))
}

/// The legacy index was read by every release up to [`LAST_UNNAMESPACED_GROUP_MINOR`], and by those
/// before the blob namespaces too.
fn legacy_in_use(holds: &Holds, _minor: u16) -> bool {
    holds.live.iter().any(|namespace| match namespace {
        BlobNamespace::Unnamespaced => true,
        BlobNamespace::Schema(minor) => *minor <= LAST_UNNAMESPACED_GROUP_MINOR,
    })
}

/// The group databases in `group_dir` that a release older than `current` keeps there.
fn superseded_indexes(group_dir: &Path, current: u16) -> Vec<SupersededIndex> {
    if current <= LAST_UNNAMESPACED_GROUP_MINOR {
        return Vec::new();
    }
    let mut found = Vec::new();
    let legacy = group_dir.join(crate::index::INDEX_DIR);
    if legacy.is_dir() {
        found.push(SupersededIndex {
            path: legacy,
            in_use: legacy_in_use,
            minor: LAST_UNNAMESPACED_GROUP_MINOR,
        });
    }
    for entry in std::fs::read_dir(group_dir).into_iter().flatten().flatten() {
        let minor = entry
            .file_name()
            .to_str()
            .and_then(crate::store_gc_namespaces::namespace_schema)
            .filter(|minor| (LAST_UNNAMESPACED_GROUP_MINOR + 1..current).contains(minor));
        if let Some(minor) = minor.filter(|_| entry.path().is_dir()) {
            found.push(SupersededIndex {
                path: entry.path(),
                in_use: namespace_in_use,
                minor,
            });
        }
    }
    found
}

/// The newest modification time of any file under `dir`, `None` when there is none.
fn newest_write(dir: &Path) -> Option<std::time::SystemTime> {
    let mut newest = None;
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in std::fs::read_dir(&next).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if let Ok(modified) = entry.metadata().and_then(|meta| meta.modified()) {
                newest = newest.max(Some(modified));
            }
        }
    }
    newest
}

/// Reap the group indexes of release minors older than `current` ([`crate::store_layout::group_index_namespace`]):
/// two installed releases keep an index each in one group, and a superseded one is garbage only
/// this sweep collects. As for a blob namespace ([`crate::store_gc_namespaces`]) one is kept while a
/// live process reads it, and until nothing has been written into it for [`Holds::hold`]; nothing
/// goes while a live process's release cannot be told. Taken under the group lock, so a release that
/// is writing the group is never raced; a locked group is skipped.
pub(crate) fn reap_superseded_group_indexes_in(
    cache_dir: &Path,
    current: u16,
    holds: &Holds,
) -> Result<GroupStoreReap, GcError> {
    let mut report = GroupStoreReap::default();
    let groups_dir = cache_dir.join(GROUPS_DIR);
    if holds.unidentified || !groups_dir.is_dir() {
        return Ok(report);
    }
    let now = std::time::SystemTime::now();
    for entry in crate::store_gc::read_dir(&groups_dir)?.flatten() {
        let group_dir = entry.path();
        let superseded: Vec<_> = superseded_indexes(&group_dir, current)
            .into_iter()
            .filter(|index| !(index.in_use)(holds, index.minor))
            .filter(|index| {
                newest_write(&index.path)
                    .map(|written| now.duration_since(written).unwrap_or_default())
                    .is_none_or(|age| age >= holds.hold)
            })
            .collect();
        if superseded.is_empty() {
            continue;
        }
        let Ok(lock) = crate::store_lock::try_acquire_lock_as(&group_dir, LockHolder::Maintenance) else {
            tracing::debug!(group = %group_dir.display(), "group is locked by another process; keeping its older indexes");
            continue;
        };
        for index in superseded {
            let bytes = dir_size(&index.path)?;
            match std::fs::remove_dir_all(&index.path) {
                Ok(()) => {
                    report.reaped += 1;
                    report.bytes_freed += bytes;
                    tracing::info!(index = %index.path.display(), bytes, "reaped the group index of an older release");
                }
                Err(error) => {
                    tracing::warn!(index = %index.path.display(), %error, "could not reap the group index of an older release; kept");
                }
            }
        }
        drop(lock);
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

    /// `cache stats` names the repository group directory that holds the memory notes and their
    /// vector stores, and leaves it out for a workspace whose marker names no root.
    #[test]
    fn cache_stats_name_the_memory_directory() {
        use crate::store_cache_admin::{BlobStorePaths, cache_stats_in};
        let (_tmp, cache) = cache_with_groups(&["g"]);
        let blobs = BlobStorePaths {
            root: cache.join("blobs"),
            schema: crate::extract::SCHEMA_VER,
            gc_state: cache.join("gc-state.json"),
            repos: cache.join("repos"),
        };
        let named = workspace(&cache, "w", Some(r#"{"root":"/gone","updated_unix":0,"group":"g"}"#));
        let markerless = workspace(&cache, "markerless", None);

        let stats = cache_stats_in(&named, &blobs).expect("stats");
        let unnamed = cache_stats_in(&markerless, &blobs).expect("stats");

        assert_eq!(
            stats.memory_dir,
            Some(crate::store_layout::repo_group_dir(Path::new("/gone")))
        );
        assert_eq!(unnamed.memory_dir, None);
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

    /// Plant a group database directory under `group` with a file last written `age` ago.
    fn plant_index(cache: &Path, group: &str, rel: &str, age: std::time::Duration) -> PathBuf {
        let dir = cache.join(GROUPS_DIR).join(group).join(rel);
        fs::create_dir_all(&dir).expect("mk index");
        let file = dir.join("journal");
        fs::write(&file, b"rows").expect("write rows");
        fs::File::options()
            .write(true)
            .open(&file)
            .and_then(|file| file.set_modified(std::time::SystemTime::now() - age))
            .expect("age the file");
        dir
    }

    const DAY: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

    fn holds(live: &[BlobNamespace], hold_days: u32) -> Holds {
        Holds {
            live: live.to_vec(),
            unidentified: false,
            hold: DAY * hold_days,
        }
    }

    /// A row of `an_older_releases_group_index_is_reaped_only_when_unused_and_past_the_hold`: the
    /// case, the directory the index is in, the days since its last write, the live namespaces, the
    /// hold in days, the running minor and whether it is reaped.
    type ReapCase = (
        &'static str,
        &'static str,
        u32,
        &'static [BlobNamespace],
        u32,
        u16,
        bool,
    );

    #[test]
    fn an_older_releases_group_index_is_reaped_only_when_unused_and_past_the_hold() {
        use BlobNamespace::{Schema, Unnamespaced};
        let cases: [ReapCase; 9] = [
            ("legacy, idle and past the hold", "index.fjall", 30, &[], 14, 35, true),
            ("legacy, inside the hold", "index.fjall", 3, &[], 14, 35, false),
            (
                "legacy, read by a live 0.34",
                "index.fjall",
                30,
                &[Schema(34)],
                14,
                35,
                false,
            ),
            (
                "legacy, read by a live pre-namespace release",
                "index.fjall",
                30,
                &[Unnamespaced],
                14,
                35,
                false,
            ),
            (
                "legacy, a live release of another namespace",
                "index.fjall",
                30,
                &[Schema(36)],
                14,
                37,
                true,
            ),
            (
                "a namespace, idle and past the hold",
                "v35/index.fjall",
                30,
                &[],
                14,
                36,
                true,
            ),
            (
                "a namespace, read by a live release",
                "v35/index.fjall",
                30,
                &[Schema(35)],
                14,
                36,
                false,
            ),
            (
                "the running minor's namespace is never reaped",
                "v35/index.fjall",
                30,
                &[],
                0,
                35,
                false,
            ),
            (
                "a newer minor's namespace is never reaped",
                "v36/index.fjall",
                30,
                &[],
                0,
                35,
                false,
            ),
        ];
        for (case, rel, days, live, hold_days, current, reaped) in cases {
            let tmp = tempfile::tempdir().expect("tempdir");
            let cache = tmp.path().join("cache");
            let index = plant_index(&cache, "g", rel, DAY * days);
            let lance = cache.join(GROUPS_DIR).join("g").join(GROUP_LANCE_DIR);
            fs::create_dir_all(&lance).expect("mk lance");

            let report = reap_superseded_group_indexes_in(&cache, current, &holds(live, hold_days)).expect("reap");

            assert_eq!(report.reaped, usize::from(reaped), "{case}");
            assert_eq!(index.exists(), !reaped, "{case}");
            assert!(
                lance.exists(),
                "{case}: the retrieval stores are not an index's to take"
            );
        }
    }

    #[test]
    fn the_running_releases_own_index_is_never_reaped() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cache = tmp.path().join("cache");
        let legacy = plant_index(&cache, "g", "index.fjall", DAY * 90);
        assert_eq!(
            reap_superseded_group_indexes_in(&cache, LAST_UNNAMESPACED_GROUP_MINOR, &holds(&[], 0))
                .expect("reap")
                .reaped,
            0
        );
        assert!(
            legacy.exists(),
            "up to the last unnamespaced minor the legacy index is the current one"
        );
    }

    #[test]
    fn an_unidentified_live_process_holds_every_older_index() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cache = tmp.path().join("cache");
        let index = plant_index(&cache, "g", "index.fjall", DAY * 90);
        let unidentified = Holds {
            unidentified: true,
            ..holds(&[], 0)
        };
        assert_eq!(
            reap_superseded_group_indexes_in(&cache, 35, &unidentified)
                .expect("reap")
                .reaped,
            0
        );
        assert!(index.exists());
    }

    #[test]
    fn a_locked_group_keeps_its_older_index() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cache = tmp.path().join("cache");
        let index = plant_index(&cache, "g", "index.fjall", DAY * 90);
        let _held = crate::store_lock::acquire_lock(&cache.join(GROUPS_DIR).join("g")).expect("lock");
        assert_eq!(
            reap_superseded_group_indexes_in(&cache, 35, &holds(&[], 0))
                .expect("reap")
                .reaped,
            0
        );
        assert!(index.exists());
    }
}
