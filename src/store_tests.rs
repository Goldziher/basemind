use super::*;

#[test]
fn locked_display_names_the_serve_holder() {
    let err = StoreError::Locked {
        path: PathBuf::from("/repo/.basemind/.lock"),
        holder: None,
    };
    let msg = err.to_string();
    assert!(
        msg.contains("serve"),
        "Locked message should name the `serve` holder, got: {msg}"
    );
    assert!(
        msg.contains("watch"),
        "Locked message should still mention `watch`, got: {msg}"
    );
}

#[test]
fn locked_message_names_actual_holder_from_sidecar() {
    let err = StoreError::Locked {
        path: PathBuf::from("/repo/.basemind/.lock"),
        holder: Some(LockMeta {
            command: "basemind scan".to_string(),
            pid: 4321,
            acquired_unix: 1_700_000_000,
        }),
    };
    let msg = err.to_string();
    assert!(
        msg.contains("basemind scan"),
        "message should name the actual holder command, got: {msg}"
    );
    assert!(msg.contains("4321"), "message should name the holder pid, got: {msg}");
}

#[test]
fn second_acquisition_names_first_holders_command() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let basemind_dir = tmp.path().join(".basemind");
    std::fs::create_dir_all(&basemind_dir).expect("mkdir");

    let _held = acquire_lock_as(&basemind_dir, LockHolder::Scan).expect("first lock");
    let err = acquire_lock_as(&basemind_dir, LockHolder::Serve)
        .expect_err("second acquisition must fail while the first holds the lock");
    assert!(err.is_lock_contention(), "must be a contention error");
    let msg = err.to_string();
    assert!(
        msg.contains("basemind scan"),
        "second error should name the FIRST holder (scan), got: {msg}"
    );
}

#[test]
fn open_read_only_errors_on_never_scanned_named_view() {
    init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    let err = match Store::open_read_only(tmp.path(), "rev-deadbee") {
        Ok(_) => panic!("named unscanned view must error, not silently open empty"),
        Err(e) => e,
    };
    assert!(
        matches!(&err, StoreError::ViewNotScanned { view } if view == "rev-deadbee"),
        "expected ViewNotScanned, got: {err:?}"
    );
    assert!(
        err.to_string().contains("rev-deadbee"),
        "error names the view, got: {err}"
    );
}

#[test]
fn read_only_open_keeps_the_code_index_when_another_process_holds_the_memory_store() {
    init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    drop(Store::open(tmp.path(), VIEW_WORKING).expect("writer creates the view"));
    // A raw handle stands in for the daemon: a separate process holding the group's lock.
    let memory_dir = memory_store_dir(tmp.path());
    std::fs::create_dir_all(&memory_dir).expect("memory dir");
    let _daemon = fjall::Database::builder(&memory_dir).open().expect("hold memory store");

    let store = Store::open_read_only(tmp.path(), VIEW_WORKING).expect("read-only open");

    let db = store
        .index_db
        .as_ref()
        .expect("code index survives a held memory store");
    assert!(
        db.memory_detached(),
        "memory is reported unavailable, not silently empty"
    );
}

#[test]
fn open_read_only_allows_unscanned_working_view() {
    init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = Store::open_read_only(tmp.path(), VIEW_WORKING).expect("working view opens even when never scanned");
    assert!(store.index.files.is_empty(), "empty working index");
}

#[test]
fn ensure_dirs_lets_a_read_only_opened_store_flush_on_first_scan() {
    init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut store =
        Store::open_read_only(tmp.path(), VIEW_WORKING).expect("working view opens even when never scanned");
    assert!(
        !store.view_dir.exists(),
        "open_read_only must not create view_dir on a never-scanned workspace"
    );
    store
        .ensure_dirs()
        .expect("ensure_dirs creates the missing directories");
    assert!(store.view_dir.exists(), "ensure_dirs must create view_dir");
    store
        .flush()
        .expect("flush must not ENOENT once ensure_dirs has materialized view_dir");
}

#[test]
fn open_writer_creates_named_view_for_first_scan() {
    init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = Store::open(tmp.path(), "rev-cafe000").expect("writer creates a named view on first scan");
    assert!(store.view_dir.exists(), "named view dir created by writer");
}

#[test]
fn fs2_advisory_lock_is_lock_contention() {
    let err = StoreError::Locked {
        path: PathBuf::from("/repo/.basemind/.lock"),
        holder: None,
    };
    assert!(err.is_lock_contention());
}

#[test]
fn fjall_internal_lock_is_lock_contention() {
    let err = StoreError::Index(IndexError::Fjall(fjall::Error::Locked));
    assert!(err.is_lock_contention());
}

/// The msgpack file map matches this binary but the fjall index carries a later partition revision:
/// a read-only consumer must surface the upgrade error instead of degrading to blob-only reads.
#[test]
fn open_read_only_reports_a_fjall_index_written_by_a_newer_basemind() {
    init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    let view_dir = {
        let mut store = Store::open(tmp.path(), VIEW_WORKING).expect("writer creates the view");
        store.flush().expect("flush the file map");
        store.view_dir.clone()
    };
    let newer = crate::index::INDEX_SCHEMA_VER + 1;
    {
        let db = fjall::Database::builder(view_dir.join(crate::index::INDEX_DIR))
            .open()
            .expect("open raw index");
        let meta = db
            .keyspace("meta", fjall::KeyspaceCreateOptions::default)
            .expect("meta keyspace");
        meta.insert(b"schema_ver", newer.to_be_bytes())
            .expect("stamp a newer schema version");
    }

    let Err(error) = Store::open_read_only(tmp.path(), VIEW_WORKING) else {
        panic!("a newer index must not read as empty");
    };

    assert!(
        matches!(
            &error,
            StoreError::Index(IndexError::NewerSchema { stored, .. }) if *stored == newer
        ),
        "expected the newer-schema error, got: {error}"
    );
    assert!(
        error.to_string().contains("upgrade basemind"),
        "the error must be actionable, got: {error}"
    );
}

#[test]
fn check_schema_separates_an_older_blob_schema_from_a_newer_one() {
    assert!(check_schema(SCHEMA_VER).is_ok());
    assert!(matches!(
        check_schema(SCHEMA_VER - 1),
        Err(StoreError::SchemaMismatch { found, expected }) if found == SCHEMA_VER - 1 && expected == SCHEMA_VER
    ));
    assert!(matches!(
        check_schema(SCHEMA_VER + 1),
        Err(StoreError::NewerSchema { stored, current }) if stored == SCHEMA_VER + 1 && current == SCHEMA_VER
    ));
}

#[test]
fn schema_mismatch_is_not_lock_contention() {
    let err = StoreError::SchemaMismatch { found: 1, expected: 2 };
    assert!(!err.is_lock_contention());
}

/// An `index.msgpack` written before `doc_files`, `embed_policy` and the per-document digest
/// fields existed must still decode, with the new fields at their neutral values.
#[test]
fn index_written_before_the_additive_fields_still_decodes() {
    let legacy = serde_json::json!({
        "schema_ver": SCHEMA_VER,
        "files": {},
    });
    let bytes = rmp_serde::to_vec_named(&legacy).unwrap();
    let index: Index = rmp_serde::from_slice(&bytes).expect("legacy index decodes");
    assert!(index.doc_files.is_empty());
    assert_eq!(index.embed_policy, "");
    assert!(index.doc_tombstones.is_empty());

    let legacy = serde_json::json!({
        "schema_ver": SCHEMA_VER,
        "files": {},
        "doc_files": {
            "docs/a.pdf": {"hash_hex": "ab", "embedding_preset": "fast", "size_bytes": 3, "mtime": 7},
        },
    });
    let bytes = rmp_serde::to_vec_named(&legacy).unwrap();
    let index: Index = rmp_serde::from_slice(&bytes).expect("legacy doc entry decodes");
    let entry = index.doc_files.values().next().expect("one doc entry");
    assert!(!entry.embedded && !entry.embed_attempted);
    assert_eq!(entry.config_digest, "");
}

/// An `index.msgpack` written before `embed_policy`, `DocEntry::{embedded, embed_attempted,
/// config_digest}` existed must still decode, with those fields at their neutral defaults -- the
/// defaults the scanner's upgrade path (purge-without-reflush, cheap re-check) relies on.
#[test]
fn a_pre_digest_index_decodes_with_neutral_defaults() {
    #[derive(Serialize)]
    struct LegacyDoc {
        hash_hex: String,
        embedding_preset: String,
        size_bytes: u64,
        mtime: i64,
    }
    #[derive(Serialize)]
    struct LegacyIndex {
        schema_ver: u16,
        files: AHashMap<RelPath, FileEntry>,
        doc_files: AHashMap<RelPath, LegacyDoc>,
    }
    let mut files = AHashMap::new();
    files.insert(
        RelPath::from("a.rs"),
        FileEntry {
            hash_hex: "h1".to_string(),
            language: "rust".to_string(),
            size_bytes: 3,
            mtime: 7,
            extract_epoch: crate::extract::EXTRACT_EPOCH,
        },
    );
    let mut doc_files = AHashMap::new();
    doc_files.insert(
        RelPath::from("d.pdf"),
        LegacyDoc {
            hash_hex: "h2".to_string(),
            embedding_preset: "balanced".to_string(),
            size_bytes: 9,
            mtime: 0,
        },
    );
    let bytes = rmp_serde::to_vec_named(&LegacyIndex {
        schema_ver: SCHEMA_VER,
        files,
        doc_files,
    })
    .expect("encode legacy index");

    let index: Index = rmp_serde::from_slice(&bytes).expect("legacy index decodes");

    assert!(index.embed_policy.is_empty(), "no recorded policy");
    let doc = index.doc_files.get(&RelPath::from("d.pdf")).expect("doc entry kept");
    assert_eq!(doc.hash_hex, "h2");
    assert!(!doc.embedded && !doc.embed_attempted);
    assert!(doc.config_digest.is_empty());
    assert_eq!(index.files.len(), 1);
}

/// A store without an index handle writes no index rows, so flushing it must not withdraw the claim
/// its file map makes about the index another handle maintains.
#[test]
fn flush_without_an_index_handle_keeps_the_file_map_generation() {
    init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    let sealed = {
        let mut store = Store::open(tmp.path(), VIEW_WORKING).expect("writer creates the view");
        store.flush().expect("flush");
        store.flush().expect("flush");
        store.index.generation
    };
    assert_eq!(sealed, 2);

    let mut blobs_only = Store::open_read_only_no_index(tmp.path(), VIEW_WORKING).expect("open without the index");
    assert!(blobs_only.index_db.is_none());
    blobs_only.flush().expect("flush");
    let on_disk = read_index(&blobs_only.view_dir)
        .expect("read")
        .expect("file map exists");
    assert_eq!(on_disk.generation, sealed);
}

/// An index a scan sealed holds what its file map says, even with no symbol rows: a repository whose
/// files define nothing is not emptied and rescanned by every writer open.
#[test]
fn a_sealed_index_without_symbols_is_not_rebuilt_on_every_open() {
    use crate::index::view::IndexView;

    init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("notes.rs"), b"// nothing to index\n").expect("write");
    {
        let mut store = Store::open(dir.path(), VIEW_WORKING).expect("open");
        crate::scanner::scan(
            dir.path(),
            &mut store,
            &crate::config::ConfigV1::with_defaults(),
            crate::scanner::ScanSource::WorkingTree,
            crate::scanner::EmbedMode::Deferred,
        )
        .expect("scan");
        assert_eq!(store.index.files.len(), 1, "the fixture file must be indexed");
        assert!(
            store.index_view().expect("index").symbols_index_is_empty(),
            "the fixture must yield no symbol rows"
        );
    }

    let store = Store::open(dir.path(), VIEW_WORKING).expect("reopen");

    assert!(!store.index_rebuild_needed, "a sealed index is not a lost one");
    assert_eq!(store.index.files.len(), 1, "the file map must keep its files");
}

/// Overwrite every table file under `index_dir` with zeroes of the same length. Returns how many.
fn damage_tables(index_dir: &std::path::Path) -> usize {
    let mut damaged = 0;
    let mut dirs = vec![index_dir.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).expect("read_dir").flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if dir.file_name().is_some_and(|name| name == "tables") {
                let len = std::fs::metadata(&path).expect("stat").len() as usize;
                std::fs::write(&path, vec![0_u8; len]).expect("damage");
                damaged += 1;
            }
        }
    }
    damaged
}

/// A damaged index database is rebuildable cache. The writer open replaces it and orders a
/// rebuild; a read-only open never discards anything.
#[test]
fn a_writer_open_rebuilds_a_damaged_index_and_a_read_only_open_leaves_it() {
    init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::write(root.join("a.rs"), b"pub fn damaged_index_fixture() {}\n").expect("write");
    {
        let mut store = Store::open(root, VIEW_WORKING).expect("open");
        crate::scanner::scan(
            root,
            &mut store,
            &crate::config::ConfigV1::with_defaults(),
            crate::scanner::ScanSource::WorkingTree,
            crate::scanner::EmbedMode::Deferred,
        )
        .expect("scan");
        assert!(
            store.index_db.as_ref().expect("index").reclaim_journal(),
            "flush to tables"
        );
    }
    let index_dir = workspace_cache_dir(root)
        .join(VIEWS_DIR)
        .join(VIEW_WORKING)
        .join(crate::index::INDEX_DIR);
    assert!(
        damage_tables(&index_dir) > 0,
        "the fixture must hold table files to damage"
    );

    let read_only = Store::open_read_only(root, VIEW_WORKING).expect("a read-only open degrades");
    assert!(read_only.index_db.is_none(), "the damaged index is not served");
    assert_eq!(read_only.index.files.len(), 1, "and nothing was discarded");
    drop(read_only);
    assert!(
        damage_tables(&index_dir) > 0,
        "the read-only open left the damaged database on disk"
    );

    let store = Store::open(root, VIEW_WORKING).expect("the writer open replaces the damaged index");
    assert!(store.index_rebuild_needed, "and orders the rebuild");
    assert!(store.index.files.is_empty());
}

/// Scan `root` once and return its working view directory.
fn scanned_view(root: &Path) -> std::path::PathBuf {
    std::fs::write(root.join("a.rs"), b"pub fn older_index_fixture() {}\n").expect("write");
    let mut store = Store::open(root, VIEW_WORKING).expect("open");
    rescan(root, &mut store).expect("scan");
    store.view_dir.clone()
}

fn rescan(root: &Path, store: &mut Store) -> Result<crate::scanner::ScanReport, crate::scanner::ScanError> {
    crate::scanner::scan(
        root,
        store,
        &crate::config::ConfigV1::with_defaults(),
        crate::scanner::ScanSource::WorkingTree,
        crate::scanner::EmbedMode::Deferred,
    )
}

/// The schema stamp of the raw index under `view_dir`, after writing `write` to it if given.
fn index_stamp(view_dir: &Path, write: Option<u32>) -> Option<Vec<u8>> {
    let db = fjall::Database::builder(view_dir.join(crate::index::INDEX_DIR))
        .open()
        .expect("open raw index");
    let meta = db
        .keyspace("meta", fjall::KeyspaceCreateOptions::default)
        .expect("meta keyspace");
    if let Some(ver) = write {
        meta.insert(b"schema_ver", ver.to_be_bytes()).expect("stamp");
        db.persist(fjall::PersistMode::SyncAll).expect("persist");
    }
    meta.get(b"schema_ver").expect("read stamp").map(|bytes| bytes.to_vec())
}

/// A read-only open changes nothing in an index of an older schema and says a rescan is owed; the
/// scan that follows, into the same store, is what replaces the index.
#[test]
fn a_read_only_open_leaves_an_older_index_for_the_scan_to_replace() {
    use crate::index::view::IndexView;

    init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let view_dir = scanned_view(root);
    let older = crate::index::INDEX_SCHEMA_VER - 1;
    index_stamp(&view_dir, Some(older));

    let mut store = Store::open_read_only(root, VIEW_WORKING).expect("read-only open");
    assert!(store.index_db.is_none(), "an older index is not opened");
    assert!(store.index_rebuild_needed, "and the caller is told a rescan is owed");
    assert_eq!(store.index.files.len(), 1, "the file map is untouched");
    assert_eq!(
        index_stamp(&view_dir, None),
        Some(older.to_be_bytes().to_vec()),
        "nothing was wiped or stamped"
    );

    rescan(root, &mut store).expect("scan");
    assert!(!store.index_rebuild_needed);
    let view = store.index_view().expect("the scan opened the index");
    assert_eq!(view.symbols_by_name_lookup("older_index_fixture", 4).len(), 1);
}

/// Replacing the index is a writer's act. A store opened read-only holds no workspace lock, so the
/// scan that would replace the index takes it first, and is refused while another process has it.
#[test]
fn the_scan_that_replaces_an_older_index_takes_the_workspace_lock() {
    init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let view_dir = scanned_view(root);
    let older = crate::index::INDEX_SCHEMA_VER - 1;
    index_stamp(&view_dir, Some(older));
    let mut store = Store::open_read_only(root, VIEW_WORKING).expect("read-only open");
    assert!(store.index_rebuild_needed);

    let other_writer = acquire_lock_as(&store.basemind_dir, LockHolder::Scan).expect("another writer");
    let refused = rescan(root, &mut store);
    assert!(
        matches!(
            refused,
            Err(crate::scanner::ScanError::Store(StoreError::Locked { .. }))
        ),
        "{refused:?}"
    );
    assert_eq!(
        index_stamp(&view_dir, None),
        Some(older.to_be_bytes().to_vec()),
        "the index another writer may be using was not touched"
    );
    assert_eq!(store.index.files.len(), 1, "nor the file map");

    drop(other_writer);
    rescan(root, &mut store).expect("the scan runs once the lock is free");
    assert!(
        writer_lock_is_held(&store.basemind_dir),
        "and holds the lock it took for as long as the store lives"
    );
}

/// Rewrite the working view's file map as one written before blobs were namespaced.
fn forge_flat_layout(view_dir: &std::path::Path) {
    let path = view_dir.join(INDEX_FILE);
    let mut map: Index = rmp_serde::from_slice(&std::fs::read(&path).expect("read")).expect("decode");
    map.blob_layout = 0;
    std::fs::write(&path, rmp_serde::to_vec_named(&map).expect("encode")).expect("write");
}

/// A file map written when blobs lay flat in `cache/blobs/` names blobs that are not in this
/// binary's namespace. Its schema is current, so nothing but the layout it records says so. The
/// writer open must not call its files unchanged: it discards the map and the index built from
/// it, and the scan that follows extracts every file again.
#[test]
fn a_file_map_from_before_the_blob_namespaces_is_discarded_by_the_writer_open() {
    use crate::index::view::IndexView as _;

    init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::write(root.join("kept.rs"), b"pub fn kept_layout_fixture() {}\n").expect("write");
    std::fs::write(root.join("gone.rs"), b"pub fn gone_layout_fixture() {}\n").expect("write");
    let scan = |store: &mut Store| {
        crate::scanner::scan(
            root,
            store,
            &crate::config::Config::with_defaults(),
            crate::scanner::ScanSource::WorkingTree,
            crate::scanner::EmbedMode::Deferred,
        )
        .expect("scan")
    };
    let mut store = Store::open(root, VIEW_WORKING).expect("open");
    assert_eq!(scan(&mut store).stats.updated, 2);
    let view_dir = store.view_dir.clone();
    drop(store);
    forge_flat_layout(&view_dir);
    std::fs::remove_file(root.join("gone.rs")).expect("delete a file the old map knew");

    let mut reopened = loop {
        match Store::open(root, VIEW_WORKING) {
            Err(error) if error.is_lock_contention() => std::thread::sleep(std::time::Duration::from_millis(25)),
            opened => break opened.expect("the writer open replaces the stale view"),
        }
    };

    assert!(reopened.index.files.is_empty(), "no file is vouched for");
    assert!(reopened.needs_full_scan());
    assert!(reopened.reset_stale_schema, "the caller is told blobs were displaced");
    assert!(
        reopened
            .index_db
            .as_ref()
            .expect("index")
            .view()
            .symbols_index_is_empty(),
        "the index built from the stale map went with it: rows of a deleted file have no map left to purge them"
    );
    let report = scan(&mut reopened);
    assert_eq!(
        report.stats.skipped_unchanged, 0,
        "nothing is skipped over the stale map"
    );
    assert!(reopened.lookup("kept.rs").is_some() && reopened.lookup("gone.rs").is_none());
    reopened.flush().expect("flush");
    let on_disk = read_index(&view_dir).expect("read").expect("a file map");
    assert_eq!(on_disk.blob_layout, crate::store_layout::BLOB_LAYOUT);
}

/// A read-only open of such a view serves nothing from the stale map, says a rescan is owed and
/// leaves the view as it found it.
#[test]
fn a_file_map_from_before_the_blob_namespaces_reads_as_a_rescan_owed() {
    init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::write(root.join("a.rs"), b"pub fn read_only_layout_fixture() {}\n").expect("write");
    let mut store = Store::open(root, VIEW_WORKING).expect("open");
    crate::scanner::scan(
        root,
        &mut store,
        &crate::config::Config::with_defaults(),
        crate::scanner::ScanSource::WorkingTree,
        crate::scanner::EmbedMode::Deferred,
    )
    .expect("scan");
    let view_dir = store.view_dir.clone();
    drop(store);
    forge_flat_layout(&view_dir);
    let forged = std::fs::read(view_dir.join(INDEX_FILE)).expect("read");

    let read_only = Store::open_read_only(root, VIEW_WORKING).expect("open read-only");

    assert!(read_only.index.files.is_empty());
    assert!(read_only.index_db.is_none(), "the index of the stale map is not served");
    assert!(read_only.rescan_owed());
    assert_eq!(std::fs::read(view_dir.join(INDEX_FILE)).expect("read"), forged);
    assert!(matches!(
        read_index(&view_dir),
        Err(StoreError::StaleBlobLayout { found: 0, .. })
    ));
}

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
    let lance_dir = store.basemind_dir.join(LANCE_DIR);
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
    )
    .expect("scan");
    assert!(store.lance_or_open(4, "model").is_ok(), "the next pass opens it");
}
