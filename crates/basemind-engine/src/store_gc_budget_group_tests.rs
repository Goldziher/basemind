//! Budget eviction of repository-group directories (ADR-0028, ADR-0037): an idle group counts
//! against the budget and goes with its last workspace; a used or locked group stays.

use std::fs;
use std::time::Duration;

use super::tests::seed_workspace_aged;
use super::*;
use crate::store::VIEWS_DIR;

/// A day, for ages and the hot floor.
const DAY_AGO: Duration = Duration::from_secs(24 * 3600);

/// Seed `cache/groups/<group>/index.fjall` (beside `workspaces`) with `bytes` of rows.
fn seed_group(workspaces: &Path, group: &str, bytes: usize) -> PathBuf {
    let dir = workspaces
        .parent()
        .expect("cache dir")
        .join(crate::store_layout::GROUPS_DIR)
        .join(group);
    fs::create_dir_all(dir.join(crate::index::INDEX_DIR)).expect("mk group index");
    fs::write(dir.join(crate::index::INDEX_DIR).join("journal"), vec![7u8; bytes]).expect("write rows");
    dir
}

/// Point `workspace`'s marker at `group`, keeping the marker's age. The marker's mtime is activity:
/// rewritten now, every workspace would be as recently used as the newest, and on a kernel whose
/// file clock ticks coarsely (Linux) the most recently used one would fall to directory order.
fn name_group(workspace: &Path, group: &str) {
    let path = workspace.join(WORKSPACE_MARKER_FILE);
    let aged = fs::metadata(&path).and_then(|m| m.modified()).expect("marker mtime");
    let mut marker = read_workspace_marker(workspace).expect("marker");
    marker.group = Some(group.to_string());
    fs::write(&path, serde_json::to_vec(&marker).expect("encode marker")).expect("write marker");
    fs::File::options()
        .write(true)
        .open(&path)
        .and_then(|f| f.set_modified(aged))
        .expect("keep the marker's age");
}

/// The group directory (the code index and the retrieval stores, ADR-0028 and ADR-0037) counts
/// against the budget, and goes with the last workspace of its group that held a view.
#[test]
fn an_idle_group_counts_against_the_budget_and_goes_with_its_last_workspace() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let workspaces = tmp.path().join("workspaces");
    let blobs = tmp.path().join("blobs");
    fs::create_dir_all(&blobs).expect("mk blobs");
    let cold = seed_workspace_aged(&workspaces, "key-cold", &"a".repeat(64), 10 * DAY_AGO);
    let newest = seed_workspace_aged(&workspaces, "key-newest", &"b".repeat(64), Duration::ZERO);
    name_group(&cold, "cold-group");
    name_group(&newest, "live-group");
    // The budget fits everything but the idle group: the most recently used workspace and its group ~keep
    // are protected, so they remain after any eviction. ~keep
    let live = seed_group(&workspaces, "live-group", 4096);
    let groups = tmp.path().join(crate::store_layout::GROUPS_DIR);
    let budget =
        dir_size(&workspaces).expect("size") + dir_size(&blobs).expect("size") + dir_size(&groups).expect("size");
    let idle = seed_group(&workspaces, "cold-group", 1 << 20);

    let report = enforce_cache_budget_in(&workspaces, &blobs, budget, DAY_AGO, Duration::ZERO).expect("enforce");

    assert!(report.total_bytes_before >= budget + (1 << 20), "{report:?}");
    assert_eq!(report.groups_evicted, 1, "{report:?}");
    assert!(report.group_bytes_freed >= 1 << 20, "{report:?}");
    assert!(!idle.exists(), "the group no workspace holding a view uses is evicted");
    assert!(!cold.join(VIEWS_DIR).exists());
    assert!(live.exists(), "the group of the most recently used workspace stays");
    assert!(newest.join(VIEWS_DIR).exists());
    assert!(report.total_bytes_after <= budget, "{report:?}");
}

/// A group another workspace still holds a view of stays when one of its workspaces is evicted:
/// that view's file map names the group's rows.
#[test]
fn a_group_a_remaining_view_uses_is_kept() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let workspaces = tmp.path().join("workspaces");
    let blobs = tmp.path().join("blobs");
    fs::create_dir_all(&blobs).expect("mk blobs");
    let cold = seed_workspace_aged(&workspaces, "key-cold", &"a".repeat(64), 10 * DAY_AGO);
    let newest = seed_workspace_aged(&workspaces, "key-newest", &"b".repeat(64), Duration::ZERO);
    name_group(&cold, "shared");
    name_group(&newest, "shared");
    let group = seed_group(&workspaces, "shared", 1 << 20);

    let report = enforce_cache_budget_in(&workspaces, &blobs, 0, DAY_AGO, Duration::ZERO).expect("enforce");

    assert!(!cold.join(VIEWS_DIR).exists(), "the cold workspace is evicted");
    assert_eq!(report.groups_evicted, 0, "{report:?}");
    assert!(group.exists(), "the most recently used workspace still reads the group");
}

/// A group whose lock a writer holds is kept, even when no workspace holds a view of it.
#[test]
fn a_locked_idle_group_is_kept() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let workspaces = tmp.path().join("workspaces");
    let blobs = tmp.path().join("blobs");
    fs::create_dir_all(&blobs).expect("mk blobs");
    let cold = seed_workspace_aged(&workspaces, "key-cold", &"a".repeat(64), 10 * DAY_AGO);
    let newest = seed_workspace_aged(&workspaces, "key-newest", &"b".repeat(64), Duration::ZERO);
    name_group(&cold, "cold-group");
    name_group(&newest, "live-group");
    let group = seed_group(&workspaces, "cold-group", 1 << 20);
    let _writer = acquire_lock(&group).expect("group lock");

    let report = enforce_cache_budget_in(&workspaces, &blobs, 0, DAY_AGO, Duration::ZERO).expect("enforce");

    assert!(!cold.join(VIEWS_DIR).exists(), "the cold workspace is evicted");
    assert_eq!(report.groups_evicted, 0, "{report:?}");
    assert!(group.join(crate::index::INDEX_DIR).exists());
}
