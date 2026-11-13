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
    let store = Store::open_read_only(tmp.path(), VIEW_WORKING).expect("working view opens even when never scanned");
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
