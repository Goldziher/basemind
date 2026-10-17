use std::path::Path;
use std::process::Command;

use fjall::{Database, KeyspaceCreateOptions};
use serde::Serialize;

use super::*;
use crate::index::{INDEX_DIR, IndexDb};
use crate::store_layout::{repo_group_key, workspace_key};

#[derive(Serialize)]
struct Row {
    value: &'static str,
    created_at: i64,
    updated_at: i64,
}

fn row(value: &'static str, updated_at: i64) -> Vec<u8> {
    rmp_serde::to_vec_named(&Row {
        value,
        created_at: 1,
        updated_at,
    })
    .expect("encode row")
}

fn value_of(bytes: &[u8]) -> String {
    #[derive(serde::Deserialize)]
    struct Back {
        value: String,
    }
    rmp_serde::from_slice::<Back>(bytes).expect("decode row").value
}

/// Plant rows in `keyspace` of a view database at `view_dir`, the way a pre-group build left them.
fn plant_legacy(view_dir: &Path, keyspace: &str, rows: &[(&str, Vec<u8>)]) {
    let dir = view_dir.join(INDEX_DIR);
    std::fs::create_dir_all(&dir).expect("mk index dir");
    let db = Database::builder(&dir).open().expect("open legacy db");
    let ks = db
        .keyspace(keyspace, KeyspaceCreateOptions::default)
        .expect("legacy keyspace");
    for (key, value) in rows {
        ks.insert(key.as_bytes().to_vec(), value.clone()).expect("plant row");
    }
}

fn legacy_keyspace_exists(view_dir: &Path, keyspace: &str) -> bool {
    let db = Database::builder(view_dir.join(INDEX_DIR)).open().expect("reopen view");
    db.keyspace_exists(keyspace)
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

#[test]
fn two_views_of_one_group_share_one_memory_store() {
    let tmp = tempfile::tempdir().unwrap();
    let memory_dir = tmp.path().join("repos/g/memory.fjall");
    let first = IndexDb::open_with_memory(&tmp.path().join("a"), &memory_dir).unwrap();
    let second = IndexDb::open_with_memory(&tmp.path().join("b"), &memory_dir).unwrap();

    first.memory_by_key.insert(b"k".to_vec(), row("from-a", 5)).unwrap();

    let seen = second.memory_by_key.get(b"k").unwrap().expect("visible from the other view");
    assert_eq!(value_of(&seen), "from-a");
}

#[test]
fn memory_survives_the_view_being_deleted() {
    let tmp = tempfile::tempdir().unwrap();
    let memory_dir = tmp.path().join("repos/g/memory.fjall");
    let view = tmp.path().join("ws/views/working");
    {
        let db = IndexDb::open_with_memory(&view, &memory_dir).unwrap();
        db.memory_by_key.insert(b"k".to_vec(), row("kept", 5)).unwrap();
    }
    std::fs::remove_dir_all(tmp.path().join("ws")).unwrap();

    let db = IndexDb::open_with_memory(&tmp.path().join("ws2/views/working"), &memory_dir).unwrap();
    assert_eq!(value_of(&db.memory_by_key.get(b"k").unwrap().unwrap()), "kept");
}

#[test]
fn migration_merges_conflicting_rows_by_timestamp() {
    let tmp = tempfile::tempdir().unwrap();
    let memory_dir = tmp.path().join("repos/g/memory.fjall");
    {
        let store = open_shared(&memory_dir).unwrap();
        store.memory_by_key.insert(b"both-store-newer".to_vec(), row("store", 20)).unwrap();
        store.memory_by_key.insert(b"both-view-newer".to_vec(), row("store", 10)).unwrap();
        store.memory_by_key.insert(b"store-only".to_vec(), row("store", 1)).unwrap();
    }
    let view_a = tmp.path().join("a");
    let view_b = tmp.path().join("b");
    plant_legacy(
        &view_a,
        "memory_by_key",
        &[
            ("both-store-newer", row("view-a", 5)),
            ("both-view-newer", row("view-a", 30)),
            ("view-only", row("view-a", 7)),
        ],
    );
    plant_legacy(&view_b, "memory_by_key", &[("view-only", row("view-b", 9))]);
    plant_legacy(&view_b, "proposals", &[("p", row("proposal", 3))]);

    let a = IndexDb::open_with_memory(&view_a, &memory_dir).unwrap();
    let b = IndexDb::open_with_memory(&view_b, &memory_dir).unwrap();

    let get = |key: &[u8]| value_of(&a.memory_by_key.get(key).unwrap().expect("row present"));
    assert_eq!(get(b"both-store-newer"), "store");
    assert_eq!(get(b"both-view-newer"), "view-a");
    assert_eq!(get(b"store-only"), "store");
    assert_eq!(get(b"view-only"), "view-b", "the newer of two views wins");
    assert_eq!(value_of(&b.proposals.get(b"p").unwrap().unwrap()), "proposal");
    drop((a, b));

    assert!(!legacy_keyspace_exists(&view_a, "memory_by_key"), "view copy is cleared");
    assert!(!legacy_keyspace_exists(&view_b, "proposals"), "view copy is cleared");
}

#[test]
fn migration_is_idempotent_across_reopens() {
    let tmp = tempfile::tempdir().unwrap();
    let memory_dir = tmp.path().join("repos/g/memory.fjall");
    let view = tmp.path().join("v");
    plant_legacy(&view, "memory_archive", &[("k", row("old", 4))]);
    IndexDb::open_with_memory(&view, &memory_dir).unwrap();
    {
        let store = open_shared(&memory_dir).unwrap();
        store.memory_archive.insert(b"k".to_vec(), row("newer", 8)).unwrap();
    }

    let db = IndexDb::open_with_memory(&view, &memory_dir).unwrap();

    assert_eq!(
        value_of(&db.memory_archive.get(b"k").unwrap().unwrap()),
        "newer",
        "a second open must not re-import the cleared view rows over newer writes"
    );
}

#[test]
fn linked_worktrees_share_a_group_and_non_git_roots_use_their_workspace_key() {
    let tmp = tempfile::tempdir().unwrap();
    let main = tmp.path().join("main");
    std::fs::create_dir_all(&main).unwrap();
    git(&main, &["init", "-q"]);
    git(&main, &["commit", "-q", "--allow-empty", "-m", "init"]);
    let linked = tmp.path().join("linked");
    git(&main, &["worktree", "add", "-q", linked.to_str().unwrap()]);

    assert_eq!(repo_group_key(&main), repo_group_key(&linked));
    assert_ne!(workspace_key(&main), workspace_key(&linked));

    let plain = tmp.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    assert_eq!(repo_group_key(&plain), workspace_key(&plain));
}
