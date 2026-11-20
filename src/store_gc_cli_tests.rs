//! `basemind cache gc` ([`run_gc`]) over a data home of its own: it reaps what the daemon's sweep
//! reaps, in the same order, and reports it.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::*;
use crate::store::{FileEntry, Index, WORKSPACE_MARKER_FILE, acquire_lock};
use crate::store_layout::{GROUPS_DIR, WorkspaceMarker};

/// Older than the blob sweep's default grace, so a blob only the reaped workspace named goes in the
/// same run.
const AGED: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// One workspace of the fixture: its cache dir, its group's directory and the blob its file map names.
struct Workspace {
    dir: PathBuf,
    group: PathBuf,
    blob: PathBuf,
}

/// A data home holding a live worktree's workspace and the workspace of a worktree that was deleted,
/// each in a group of its own and naming one aged blob.
struct Machine {
    _tmp: tempfile::TempDir,
    data: DataRoot,
    live: Workspace,
    dead: Workspace,
}

fn machine() -> Machine {
    let tmp = tempfile::tempdir().expect("tempdir");
    let data = DataRoot::new(tmp.path().join("data"));
    let live_root = tmp.path().join("live-worktree");
    let dead_root = tmp.path().join("dead-worktree");
    let live = workspace(&data, "key-live", &live_root, "group-live", 'a');
    let dead = workspace(&data, "key-dead", &dead_root, "group-dead", 'b');
    fs::remove_dir_all(&dead_root).expect("delete the worktree");
    Machine {
        _tmp: tmp,
        data,
        live,
        dead,
    }
}

fn workspace(data: &DataRoot, key: &str, root: &Path, group: &str, stem: char) -> Workspace {
    fs::create_dir_all(root).expect("mk worktree");
    let dir = data.workspaces_dir().join(key);
    let working = dir.join(VIEWS_DIR).join("working");
    fs::create_dir_all(&working).expect("mk view");
    let hash_hex = stem.to_string().repeat(64);
    let mut index = Index::empty();
    index.files.insert(
        crate::path::RelPath::from("src/lib.rs"),
        FileEntry {
            hash_hex: hash_hex.clone(),
            language: "rust".to_string(),
            size_bytes: 2,
            mtime: 0,
            extract_epoch: crate::extract::EXTRACT_EPOCH,
            content_key: None,
            lines: 0,
        },
    );
    fs::write(
        working.join(INDEX_FILE),
        rmp_serde::to_vec_named(&index).expect("encode index"),
    )
    .expect("write index");
    let marker = WorkspaceMarker {
        root: root.to_path_buf(),
        updated_unix: 0,
        group: Some(group.to_string()),
    };
    fs::write(
        dir.join(WORKSPACE_MARKER_FILE),
        serde_json::to_vec(&marker).expect("encode marker"),
    )
    .expect("write marker");

    let group_dir = data.cache_dir().join(GROUPS_DIR).join(group);
    let group_index = group_dir.join(crate::index::INDEX_DIR);
    fs::create_dir_all(&group_index).expect("mk group index");
    fs::write(group_index.join("journal"), b"rows").expect("write rows");

    let blobs = data.global_blobs_dir();
    fs::create_dir_all(&blobs).expect("mk blobs");
    let blob = blobs.join(format!("{hash_hex}.fm.msgpack"));
    fs::write(&blob, b"blob").expect("write blob");
    fs::File::options()
        .write(true)
        .open(&blob)
        .and_then(|file| file.set_modified(SystemTime::now() - AGED))
        .expect("age the blob");
    Workspace {
        dir,
        group: group_dir,
        blob,
    }
}

#[test]
fn cache_gc_reaps_a_deleted_worktrees_workspace_its_group_and_the_blobs_it_pinned() {
    let m = machine();

    let report = run_gc(&m.data).expect("cache gc");

    assert_eq!(report.workspaces_reaped, 1, "the deleted worktree's workspace");
    assert!(report.workspace_bytes_freed > 0);
    assert_eq!(report.group_stores_reaped, 1, "the group only that workspace named");
    assert!(report.group_store_bytes_freed > 0);
    assert_eq!(report.removed, 1, "the blob only the reaped workspace named");
    assert!(!m.dead.dir.exists());
    assert!(!m.dead.group.exists());
    assert!(!m.dead.blob.exists());
    assert!(m.live.dir.exists(), "a live worktree's workspace stays");
    assert!(m.live.group.exists(), "so does its group");
    assert!(m.live.blob.exists(), "and the blob it names");

    let state = read_gc_state(&m.data).expect("cache gc records its sweep");
    assert_eq!(state.workspaces_reaped, 1);
    assert_eq!(state.removed, 1);

    let again = run_gc(&m.data).expect("cache gc again");
    assert_eq!(again.workspaces_reaped, 0);
    assert_eq!(again.group_stores_reaped, 0);
    assert_eq!(again.removed, 0);
}

#[test]
fn cache_gc_keeps_a_deleted_worktrees_workspace_another_process_holds() {
    let m = machine();
    let _held = acquire_lock(&m.dead.dir).expect("hold the workspace lock");

    let report = run_gc(&m.data).expect("cache gc");

    assert_eq!(report.workspaces_reaped, 0, "a locked workspace is never reaped");
    assert_eq!(report.group_stores_reaped, 0, "its marker still names its group");
    assert_eq!(report.removed, 0, "its file map still names its blob");
    assert!(m.dead.dir.exists());
    assert!(m.dead.group.exists());
    assert!(m.dead.blob.exists());
}
