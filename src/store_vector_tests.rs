use super::tests::{rescan, scanned_view};
use super::*;

/// A vector store that will not open (here a file in its place; in the field another open holding
/// its lock past the wait) is opened once per scan pass: the pass's later opens fail at once
/// instead of each waiting again, and the next pass tries again.
#[cfg(feature = "intelligence")]
#[test]
fn a_vector_store_that_would_not_open_is_tried_once_per_scan_pass() {
    init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::write(root.join("a.rs"), b"pub fn vector_store_fixture() {}\n").expect("write");
    let mut store = Store::open(root, VIEW_WORKING).expect("open");
    let lance_dir = crate::lance::code_store_dir(&store.root, "model", 4);
    std::fs::create_dir_all(lance_dir.parent().expect("stores dir")).expect("mk stores dir");
    std::fs::write(&lance_dir, b"not a directory").expect("block the store");
    assert!(store.lance_or_open(4, "model").is_err());

    std::fs::remove_file(&lance_dir).expect("unblock the store");
    assert!(
        store.lance_or_open(4, "model").is_err(),
        "the failure stands for the rest of the pass"
    );
    assert!(!lance_dir.exists(), "and nothing was opened");

    crate::scanner::scan(
        root,
        &mut store,
        &crate::config::Config::with_defaults(),
        crate::scanner::ScanSource::WorkingTree,
        crate::scanner::EmbedMode::Deferred,
        crate::scan_retrieval::Retrieval::inert(),
    )
    .expect("scan");
    assert!(store.lance_or_open(4, "model").is_ok(), "the next pass opens it");
}

/// The code-chunk store is the group's: opening it creates the group store and removes the
/// workspace's own `lance/`, whose rows an earlier build keyed by path.
#[cfg(feature = "intelligence")]
#[test]
fn opening_the_code_store_opens_the_groups_and_removes_the_workspaces_old_one() {
    init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let mut store = Store::open(dir.path(), VIEW_WORKING).expect("open");
    let legacy = store.basemind_dir.join(LANCE_DIR);
    std::fs::create_dir_all(&legacy).expect("mk legacy store");
    std::fs::write(legacy.join("meta.json"), b"{}").expect("write legacy meta");

    let opened = store.lance_or_open(4, "model").expect("open").dir().to_path_buf();

    assert_eq!(opened, crate::lance::code_store_dir(&store.root, "model", 4));
    assert!(opened.starts_with(crate::lance::code_stores_dir(&store.root)));
    assert!(!legacy.exists(), "the workspace's path-keyed store is gone");
    assert!(store.code_stores_exist());
}

/// A torn or undecodable file map failed every writer open, `scan` included, until a daemon GC or a
/// cache clear removed it. The open now discards the view and the index it fed, and a scan rebuilds.
#[test]
fn a_writer_open_rebuilds_a_view_whose_file_map_does_not_decode() {
    use crate::index::view::IndexView;

    init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let view_dir = scanned_view(root);
    std::fs::write(view_dir.join(INDEX_FILE), b"\xff\xff torn \x00").expect("tear the file map");

    let mut store = Store::open(root, VIEW_WORKING).expect("the writer open replaces the torn map");
    assert!(store.index.files.is_empty());
    let view = store.index_view().expect("index");
    assert!(view.symbols_index_is_empty(), "the index it fed is discarded too");
    rescan(root, &mut store).expect("scan");
    assert_eq!(store.index.files.len(), 1);
}

/// The status sidecar counted the machine-wide blob namespace with a directory listing on every
/// watcher batch, so its number was every workspace's. It counts this workspace's contents.
#[test]
fn the_status_sidecar_counts_this_workspaces_distinct_contents() {
    init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    std::fs::write(root.join("a.rs"), b"pub fn same() {}\n").expect("write");
    std::fs::write(root.join("b.rs"), b"pub fn same() {}\n").expect("write");
    std::fs::write(root.join("c.rs"), b"pub fn other() {}\n").expect("write");
    let mut store = Store::open(root, VIEW_WORKING).expect("open");
    std::fs::create_dir_all(&store.blobs_dir).expect("mk blobs");
    std::fs::write(store.blobs_dir.join(format!("{}.fm.msgpack", "f".repeat(64))), b"x")
        .expect("another workspace's blob");

    rescan(root, &mut store).expect("scan");

    let sidecar = read_status_sidecar(&store.basemind_dir).expect("sidecar");
    assert_eq!((sidecar.file_count, sidecar.blob_count), (3, 2));
}

/// A file map of a newer blob layout was accepted and rewritten as this binary's layout, over blobs
/// it does not know where to find. It is refused as a newer basemind's, like a newer schema.
#[test]
fn a_file_map_of_a_newer_blob_layout_is_refused() {
    init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    let view_dir = scanned_view(tmp.path());
    let mut newer = read_index(&view_dir).expect("read").expect("a file map");
    newer.blob_layout = crate::store_layout::BLOB_LAYOUT + 1;
    std::fs::write(
        view_dir.join(INDEX_FILE),
        rmp_serde::to_vec_named(&newer).expect("encode"),
    )
    .expect("write");

    let Err(error) = Store::open(tmp.path(), VIEW_WORKING) else {
        panic!("a newer blob layout must not open");
    };
    assert!(matches!(error, StoreError::NewerSchema { .. }), "{error}");
}

/// A file map whose version peek reads a newer blob layout belongs to a newer basemind, whether or
/// not the rest of it decodes as this binary's: a newer layout may change any other field. It is
/// refused, never discarded as damaged.
#[test]
fn an_undecodable_file_map_of_a_newer_blob_layout_is_refused_not_rebuilt() {
    #[derive(serde::Serialize)]
    struct Newer {
        schema_ver: u16,
        files: &'static str,
        blob_layout: u8,
    }
    init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    let view_dir = scanned_view(tmp.path());
    let newer = rmp_serde::to_vec_named(&Newer {
        schema_ver: crate::extract::SCHEMA_VER,
        files: "a field this binary reads as a map",
        blob_layout: crate::store_layout::BLOB_LAYOUT + 1,
    })
    .expect("encode");
    std::fs::write(view_dir.join(INDEX_FILE), &newer).expect("write");

    assert!(matches!(read_index(&view_dir), Err(StoreError::NewerSchema { .. })));
    let Err(error) = Store::open(tmp.path(), VIEW_WORKING) else {
        panic!("a newer blob layout must not open");
    };
    assert!(matches!(error, StoreError::NewerSchema { .. }), "{error}");
    assert_eq!(
        std::fs::read(view_dir.join(INDEX_FILE)).expect("the map is still there"),
        newer,
        "the newer map is left as it was"
    );
    assert!(
        crate::store_layout::group_index_dir(tmp.path())
            .join(crate::index::INDEX_DIR)
            .exists(),
        "and so is the group index"
    );
}

/// A map whose version cannot even be read may be a newer basemind's: a writer open sets it aside
/// rather than deleting it, and rebuilds.
#[test]
fn a_file_map_whose_version_cannot_be_read_is_set_aside_not_deleted() {
    init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    let view_dir = scanned_view(tmp.path());
    let torn = b"\xff\xff torn \x00";
    std::fs::write(view_dir.join(INDEX_FILE), torn).expect("tear the file map");

    let store = Store::open(tmp.path(), VIEW_WORKING).expect("the writer open rebuilds");
    assert!(store.index.files.is_empty());
    let aside: Vec<Vec<u8>> = std::fs::read_dir(&view_dir)
        .expect("read view dir")
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("index.msgpack.unreadable-")
        })
        .map(|entry| std::fs::read(entry.path()).expect("read the set-aside map"))
        .collect();
    assert_eq!(
        aside,
        vec![torn.to_vec()],
        "the unreadable map is kept aside, byte for byte"
    );
}

/// A view's own `index.fjall`, left by a build from before the group index, may hold the notes of
/// that time: the writer open moves them into the group's notes store, and the database goes once
/// a scan asks for it ([`Store::reap_legacy_index`]), so an upgrade keeps them.
#[test]
fn the_writer_open_keeps_the_notes_of_an_older_view_index_it_deletes() {
    init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    let view_dir = scanned_view(tmp.path());
    {
        let db = fjall::Database::builder(view_dir.join(crate::index::INDEX_DIR))
            .open()
            .expect("an older view index");
        let notes = db
            .keyspace("memory_by_key", fjall::KeyspaceCreateOptions::default)
            .expect("its notes");
        notes
            .insert(b"older-note", b"kept across the upgrade")
            .expect("write a note");
        db.persist(fjall::PersistMode::SyncAll).expect("persist");
    }

    let mut store = Store::open(tmp.path(), VIEW_WORKING).expect("writer open");
    assert!(
        view_dir.join(crate::index::INDEX_DIR).exists(),
        "the open leaves the older view index for the scan's free-space check to clear"
    );
    store.reap_legacy_index().expect("reap");
    assert!(
        !view_dir.join(crate::index::INDEX_DIR).exists(),
        "the older view index is deleted"
    );
    let memory = store.index_db.as_ref().expect("index").memory().expect("notes store");
    let notes = memory
        .notes()
        .expect("open notes")
        .expect("the group notes store exists");
    assert_eq!(
        notes.get(b"older-note").expect("read").as_deref(),
        Some(&b"kept across the upgrade"[..])
    );
}

/// A reopened index handle publishes the live keys of the file map on disk. Entries the store holds
/// but has not flushed name rows a reader must not see before the map listing them is written.
#[test]
fn a_reopened_index_publishes_the_written_file_map_not_the_unflushed_one() {
    init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(tmp.path().join("a.rs"), b"pub fn reopened_fixture() {}\n").expect("write");
    let mut store = Store::open(tmp.path(), VIEW_WORKING).expect("writer");
    crate::scanner::scan(
        tmp.path(),
        &mut store,
        &crate::config::ConfigV1::with_defaults(),
        crate::scanner::ScanSource::WorkingTree,
        crate::scanner::EmbedMode::Deferred,
        crate::scan_retrieval::Retrieval::inert(),
    )
    .expect("scan");
    let entry = store.lookup("a.rs").cloned().expect("indexed");
    store.upsert("unflushed.rs", entry);

    store.reopen_index_db().expect("reopen");

    let live = store.index_db.as_ref().expect("reopened").live_keys();
    assert!(
        live.key_of(&RelPath::from("a.rs")).is_some(),
        "the written map is published"
    );
    assert!(
        live.key_of(&RelPath::from("unflushed.rs")).is_none(),
        "an entry no written map lists is not"
    );
}

/// A view whose file map does not decode, over a per-view index that still holds the memory notes a
/// release before 0.30 kept there: the writer open moves the notes to the group store before the
/// per-view index goes. Discarding it with the map lost the only copy.
#[test]
fn a_torn_file_map_does_not_discard_the_notes_of_its_per_view_index() {
    init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let working = workspace_cache_dir(root).join(VIEWS_DIR).join(VIEW_WORKING);
    std::fs::create_dir_all(&working).expect("mk view");
    {
        let db = fjall::Database::builder(working.join(crate::index::INDEX_DIR))
            .open()
            .expect("open the view's index");
        let notes = db
            .keyspace("memory_by_key", fjall::KeyspaceCreateOptions::default)
            .expect("the notes keyspace of a 0.29 view");
        notes
            .insert(b"k".to_vec(), b"only copy".to_vec())
            .expect("write a note");
    }
    std::fs::write(working.join(INDEX_FILE), b"\xff\xff torn \x00").expect("tear the file map");

    let store = Store::open(root, VIEW_WORKING).expect("the writer open rebuilds the view");
    drop(store);

    let reader = dir.path().join("reader");
    let db = crate::index::IndexDb::open_with_memory(&reader, &memory_store_dir(root)).expect("open the group store");
    assert_eq!(db.notes().get(b"k").expect("read").as_deref(), Some(&b"only copy"[..]));
}
