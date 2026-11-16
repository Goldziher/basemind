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

    first.notes().insert(b"k".to_vec(), row("from-a", 5)).unwrap();

    let seen = second.notes().get(b"k").unwrap().expect("visible from the other view");
    assert_eq!(value_of(&seen), "from-a");
}

#[test]
fn memory_survives_the_view_being_deleted() {
    let tmp = tempfile::tempdir().unwrap();
    let memory_dir = tmp.path().join("repos/g/memory.fjall");
    let view = tmp.path().join("ws/views/working");
    {
        let db = IndexDb::open_with_memory(&view, &memory_dir).unwrap();
        db.notes().insert(b"k".to_vec(), row("kept", 5)).unwrap();
    }
    std::fs::remove_dir_all(tmp.path().join("ws")).unwrap();

    let db = IndexDb::open_with_memory(&tmp.path().join("ws2/views/working"), &memory_dir).unwrap();
    assert_eq!(value_of(&db.notes().get(b"k").unwrap().unwrap()), "kept");
}

#[test]
fn migration_merges_conflicting_rows_by_timestamp() {
    let tmp = tempfile::tempdir().unwrap();
    let memory_dir = tmp.path().join("repos/g/memory.fjall");
    {
        let store = open_shared(&memory_dir, Version::Stamp).unwrap();
        store
            .memory_by_key
            .insert(b"both-store-newer".to_vec(), row("store", 20))
            .unwrap();
        store
            .memory_by_key
            .insert(b"both-view-newer".to_vec(), row("store", 10))
            .unwrap();
        store
            .memory_by_key
            .insert(b"store-only".to_vec(), row("store", 1))
            .unwrap();
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

    let a = IndexDb::open_with_memory(&view_a, &memory_dir).unwrap();
    let b = IndexDb::open_with_memory(&view_b, &memory_dir).unwrap();

    let get = |key: &[u8]| value_of(&a.notes().get(key).unwrap().expect("row present"));
    assert_eq!(get(b"both-store-newer"), "store");
    assert_eq!(get(b"both-view-newer"), "view-a");
    assert_eq!(get(b"store-only"), "store");
    assert_eq!(get(b"view-only"), "view-b", "the newer of two views wins");
    drop((a, b));

    assert!(
        !legacy_keyspace_exists(&view_a, "memory_by_key"),
        "view copy is cleared"
    );
    assert!(
        !legacy_keyspace_exists(&view_b, "memory_by_key"),
        "view copy is cleared"
    );
}

#[test]
fn migration_is_idempotent_across_reopens() {
    let tmp = tempfile::tempdir().unwrap();
    let memory_dir = tmp.path().join("repos/g/memory.fjall");
    let view = tmp.path().join("v");
    plant_legacy(&view, "memory_by_key", &[("k", row("old", 4))]);
    IndexDb::open_with_memory(&view, &memory_dir).unwrap();
    {
        let store = open_shared(&memory_dir, Version::Stamp).unwrap();
        store.memory_by_key.insert(b"k".to_vec(), row("newer", 8)).unwrap();
    }

    let db = IndexDb::open_with_memory(&view, &memory_dir).unwrap();

    assert_eq!(
        value_of(&db.notes().get(b"k").unwrap().unwrap()),
        "newer",
        "a second open must not re-import the cleared view rows over newer writes"
    );
}

/// A group store written before the audit and mining features were removed holds two more
/// keyspaces. It must open, keep serving `memory_by_key`, and keep the retired rows on disk.
#[test]
fn a_store_holding_the_retired_keyspaces_opens_and_keeps_them() {
    const RETIRED: [&str; 2] = ["memory_archive", "proposals"];
    let tmp = tempfile::tempdir().unwrap();
    let memory_dir = tmp.path().join("repos/g/memory.fjall");
    std::fs::create_dir_all(&memory_dir).unwrap();
    {
        let old = Database::builder(&memory_dir).open().expect("open old store");
        for name in ["memory_by_key", RETIRED[0], RETIRED[1]] {
            let ks = old
                .keyspace(name, KeyspaceCreateOptions::default)
                .expect("old keyspace");
            ks.insert(b"k".to_vec(), row(name, 5)).expect("plant row");
            ks.rotate_memtable_and_wait().expect("flush to disk");
        }
    }

    {
        let db = IndexDb::open_with_memory(&tmp.path().join("ws/views/working"), &memory_dir).unwrap();
        assert_eq!(value_of(&db.notes().get(b"k").unwrap().unwrap()), "memory_by_key");
        db.notes().insert(b"new".to_vec(), row("written", 9)).unwrap();
    }

    let reopened = IndexDb::open_with_memory(&tmp.path().join("ws/views/working"), &memory_dir).unwrap();
    assert_eq!(value_of(&reopened.notes().get(b"new").unwrap().unwrap()), "written");
    drop(reopened);

    let raw = Database::builder(&memory_dir).open().expect("reopen through fjall");
    for name in RETIRED {
        assert!(raw.keyspace_exists(name), "{name} must not be dropped");
        let ks = raw
            .keyspace(name, KeyspaceCreateOptions::default)
            .expect("retired keyspace");
        assert_eq!(value_of(&ks.get(b"k").unwrap().expect("row kept")), name);
    }
}

/// The retired keyspaces an older build left in a view's own database are left where they are.
#[test]
fn migration_leaves_a_views_retired_keyspaces_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let memory_dir = tmp.path().join("repos/g/memory.fjall");
    let view = tmp.path().join("v");
    plant_legacy(&view, "proposals", &[("p", row("proposal", 3))]);
    plant_legacy(&view, "memory_by_key", &[("k", row("mem", 3))]);

    let db = IndexDb::open_with_memory(&view, &memory_dir).unwrap();
    assert_eq!(value_of(&db.notes().get(b"k").unwrap().unwrap()), "mem");
    drop(db);

    assert!(!legacy_keyspace_exists(&view, "memory_by_key"));
    assert!(legacy_keyspace_exists(&view, "proposals"));
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

fn stamp_version(memory_dir: &Path, version: u32) {
    let raw = Database::builder(memory_dir).open().expect("open through fjall");
    let meta = raw
        .keyspace(STORE_META, KeyspaceCreateOptions::default)
        .expect("meta keyspace");
    meta.insert(STORE_VER_KEY, version.to_be_bytes()).expect("stamp");
    raw.persist(PersistMode::SyncAll).expect("persist");
}

/// A store written before the version existed gains one at its first open, and keeps its notes.
#[test]
fn an_unversioned_memory_store_is_stamped_on_open_and_keeps_its_notes() {
    let tmp = tempfile::tempdir().unwrap();
    let memory_dir = tmp.path().join("repos/g/memory.fjall");
    std::fs::create_dir_all(&memory_dir).unwrap();
    {
        let old = Database::builder(&memory_dir).open().expect("open old store");
        let notes = old
            .keyspace(MEMORY_BY_KEY, KeyspaceCreateOptions::default)
            .expect("notes keyspace");
        notes.insert(b"k".to_vec(), row("kept", 5)).expect("plant note");
        old.persist(PersistMode::SyncAll).expect("persist");
    }
    assert_eq!(stored_version(&memory_dir), None);

    let db = IndexDb::open_with_memory(&tmp.path().join("ws/views/working"), &memory_dir).unwrap();
    assert_eq!(value_of(&db.notes().get(b"k").unwrap().unwrap()), "kept");
    drop(db);

    assert_eq!(stored_version(&memory_dir), Some(MEMORY_STORE_VER));
}

/// A read-only open deletes, migrates and wipes nothing: rows an older build left in the view stay
/// where they are, and the group store gains none, until a writer open folds them in.
#[test]
fn a_read_only_open_leaves_a_views_legacy_notes_for_the_writer_open_to_migrate() {
    let tmp = tempfile::tempdir().unwrap();
    let memory_dir = tmp.path().join("repos/g/memory.fjall");
    let view = tmp.path().join("ws/views/working");
    plant_legacy(&view, MEMORY_BY_KEY, &[("k", row("legacy", 5))]);

    {
        let read_only = IndexDb::open_tolerating_held_memory(&view, &memory_dir).unwrap();
        assert!(
            read_only.notes().get(b"k").unwrap().is_none(),
            "a read-only open migrated a note"
        );
    }
    assert!(
        legacy_keyspace_exists(&view, MEMORY_BY_KEY),
        "a read-only open dropped the view's keyspace"
    );

    let writer = IndexDb::open_with_memory(&view, &memory_dir).unwrap();
    assert_eq!(
        value_of(&writer.notes().get(b"k").unwrap().expect("migrated")),
        "legacy"
    );
}

/// The version a store carries on disk, read through fjall alone.
fn stored_version(memory_dir: &Path) -> Option<u32> {
    let db = Database::builder(memory_dir).open().expect("open store");
    if !db.keyspace_exists(STORE_META) {
        return None;
    }
    let meta = db
        .keyspace(STORE_META, KeyspaceCreateOptions::default)
        .expect("meta keyspace");
    let bytes = meta.get(STORE_VER_KEY).expect("read")?;
    Some(u32::from_be_bytes(
        <[u8; 4]>::try_from(bytes.as_ref()).expect("four bytes"),
    ))
}

/// The notes of a newer basemind are not ours to read. That refuses the notes, and nothing else:
/// the code index of every worktree in the group still opens, for a writer and for a reader.
#[test]
fn a_newer_memory_store_detaches_memory_and_leaves_the_code_index_open() {
    let tmp = tempfile::tempdir().unwrap();
    let memory_dir = tmp.path().join("repos/g/memory.fjall");
    let view = tmp.path().join("ws/views/working");
    {
        let db = IndexDb::open_with_memory(&view, &memory_dir).expect("first open");
        db.notes().insert(b"k".to_vec(), row("kept", 5)).unwrap();
        db.memory().unwrap().db.persist(PersistMode::SyncAll).unwrap();
    }
    stamp_version(&memory_dir, MEMORY_STORE_VER + 1);

    for who in ["writer", "reader"] {
        let db = open_released(|| match who {
            "writer" => IndexDb::open_with_memory(&view, &memory_dir),
            _ => IndexDb::open_tolerating_held_memory(&view, &memory_dir),
        });
        assert_eq!(
            db.memory().err(),
            Some(crate::index::MemoryUnavailable::NewerStore {
                stored: MEMORY_STORE_VER + 1,
                current: MEMORY_STORE_VER,
            }),
            "{who}"
        );
        assert!(db.memory_detached(), "{who}");
        drop(db);
    }
    assert_eq!(
        stored_version(&memory_dir),
        Some(MEMORY_STORE_VER + 1),
        "the stamp is not lowered"
    );
    let raw = Database::builder(&memory_dir).open().expect("open through fjall");
    let notes = raw
        .keyspace(MEMORY_BY_KEY, KeyspaceCreateOptions::default)
        .expect("notes keyspace");
    assert_eq!(value_of(&notes.get(b"k").unwrap().expect("note kept")), "kept");
}

/// A view of an older index schema is wiped on a writer open. Memory rows of the old layout still
/// in it could not be moved into a store this binary must not write, so the open is refused rather
/// than wiping them.
#[test]
fn a_view_holding_old_memory_rows_is_not_wiped_while_the_store_is_newer() {
    let tmp = tempfile::tempdir().unwrap();
    let memory_dir = tmp.path().join("repos/g/memory.fjall");
    let view = tmp.path().join("v");
    drop(open_shared(&memory_dir, Version::Stamp).expect("create the store"));
    stamp_version(&memory_dir, MEMORY_STORE_VER + 1);
    plant_legacy(&view, "memory_by_key", &[("k", row("only copy", 4))]);
    {
        let db = Database::builder(view.join(INDEX_DIR)).open().expect("view db");
        let meta = db.keyspace("meta", KeyspaceCreateOptions::default).expect("meta");
        meta.insert(b"schema_ver", (crate::index::INDEX_SCHEMA_VER - 1).to_be_bytes())
            .expect("stamp an older schema");
    }

    let opened = IndexDb::open_with_memory(&view, &memory_dir);

    assert!(
        matches!(opened, Err(crate::index::IndexError::NewerMemoryStore { .. })),
        "the open that would wipe the rows is refused"
    );
    assert!(
        legacy_keyspace_exists(&view, "memory_by_key"),
        "the rows are still there"
    );
}

/// A read-only open holds a store to its version by looking: it stamps nothing and creates no
/// keyspace for a version. An absent store it does create, empty, with its notes keyspace. The
/// writer that opens the store stamps it.
#[test]
fn a_reader_never_stamps_the_store_and_the_writer_that_joins_it_does() {
    let tmp = tempfile::tempdir().unwrap();
    let memory_dir = tmp.path().join("repos/g/memory.fjall");

    drop(open_shared(&memory_dir, Version::Check).expect("reader"));
    {
        let db = Database::builder(&memory_dir).open().expect("open store");
        assert!(
            !db.keyspace_exists(STORE_META),
            "a reader made no keyspace for a version"
        );
        assert!(db.keyspace_exists(MEMORY_BY_KEY), "the empty notes keyspace is there");
    }
    assert_eq!(stored_version(&memory_dir), None, "a reader wrote no version");

    let reader = open_shared(&memory_dir, Version::Check).expect("reader");
    let writer = open_shared(&memory_dir, Version::Stamp).expect("a writer joins the open handle");
    assert!(Arc::ptr_eq(&reader, &writer), "one handle per process");
    drop((reader, writer));
    assert_eq!(stored_version(&memory_dir), Some(MEMORY_STORE_VER));
}

/// A handle whose memory is held by another process holds no store at all: nothing is created in
/// the view directory to stand in for it.
#[test]
fn a_reader_without_memory_creates_nothing_in_the_view() {
    let tmp = tempfile::tempdir().unwrap();
    let memory_dir = tmp.path().join("repos/g/memory.fjall");
    let view = tmp.path().join("ws/views/working");
    drop(IndexDb::open_with_memory(&view, &memory_dir).expect("first open"));
    stamp_version(&memory_dir, MEMORY_STORE_VER + 1);
    let stale = view.join(RETIRED_PLACEHOLDER_DIR);
    std::fs::create_dir_all(&stale).expect("what an older build left");
    let before = dir_names(&view);

    drop(open_released(|| {
        IndexDb::open_tolerating_held_memory(&view, &memory_dir)
    }));
    assert_eq!(dir_names(&view), before, "the reader added and removed nothing");

    drop(open_released(|| IndexDb::open_with_memory(&view, &memory_dir)));
    assert!(!stale.exists(), "the writer removes the retired stand-in");
}

/// Open an index whose directory another handle in this test has just released: fjall lets go of
/// its lock a moment after the handle drops.
fn open_released(open: impl Fn() -> Result<IndexDb, crate::index::IndexError>) -> IndexDb {
    let started = std::time::Instant::now();
    loop {
        match open() {
            Err(crate::index::IndexError::Fjall(fjall::Error::Locked))
                if started.elapsed() < std::time::Duration::from_secs(20) =>
            {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            opened => return opened.expect("the code index must open"),
        }
    }
}

fn dir_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("list")
        .map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}
