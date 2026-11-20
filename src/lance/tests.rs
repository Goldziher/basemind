use super::*;

fn sentinel(dir: &std::path::Path) -> std::path::PathBuf {
    let p = dir.join("sentinel");
    std::fs::write(&p, b"keep-me").unwrap();
    p
}

/// The search predicate must pin the full `(scope, visibility, agent_id)` namespace so
/// an individual search can never surface another agent's or the group's rows, and a
/// group search only sees group rows. We assert the predicate-string construction
/// directly — a full `LanceStore` needs an embedder + on-disk table, far heavier than
/// the isolation invariant under test.
#[test]
fn search_predicate_isolates_namespaces() {
    let group = memory_namespace_predicate("scope-a", "group", "");
    assert_eq!(group, "scope = 'scope-a' AND visibility = 'group' AND agent_id = ''");

    let indiv_a = memory_namespace_predicate("scope-a", "individual", "agent-a");
    assert_eq!(
        indiv_a,
        "scope = 'scope-a' AND visibility = 'individual' AND agent_id = 'agent-a'"
    );

    assert_ne!(group, indiv_a);
    let indiv_b = memory_namespace_predicate("scope-a", "individual", "agent-b");
    assert_ne!(indiv_a, indiv_b);
}

/// The keys predicate appends the key list to the namespace predicate, and a
/// single-quote in any segment is escaped so the literal cannot break out.
#[test]
fn keys_predicate_pins_keys_and_escapes_quotes() {
    let p = memory::memory_keys_predicate("s", "individual", "a", &["o'brien", "k"]);
    assert_eq!(
        p,
        "scope = 's' AND visibility = 'individual' AND agent_id = 'a' AND key IN ('o''brien', 'k')"
    );
}

/// A pre-0.5 `meta.json` (no `schema_ver`) must deserialize as `schema_ver = 0` and,
/// because that differs from the current `VECTOR_STORE_VER`, force a wipe — never a
/// parse error. This is the guard against the memory-table column add faulting at
/// batch-build time on upgrade.
#[test]
fn pre_0_5_meta_without_schema_ver_triggers_wipe() {
    let dir = tempfile::tempdir().unwrap();
    let meta_path = dir.path().join(META_FILE);
    std::fs::write(&meta_path, br#"{"dim":384,"embedding_model":"balanced"}"#).unwrap();
    let keep = sentinel(dir.path());

    let expected = LanceMeta {
        dim: 384,
        embedding_model: "balanced".to_string(),
        schema_ver: VECTOR_STORE_VER,
    };
    assert_ne!(VECTOR_STORE_VER, 0, "current schema ver must differ from the legacy 0");
    wipe_on_mismatch(dir.path(), &meta_path, &expected).unwrap();
    assert!(!keep.exists(), "stale lance dir should have been wiped");
}

/// A matching `meta.json` (same dim, model, and `schema_ver`) leaves the store intact.
#[test]
fn matching_meta_preserves_store() {
    let dir = tempfile::tempdir().unwrap();
    let meta_path = dir.path().join(META_FILE);
    let expected = LanceMeta {
        dim: 384,
        embedding_model: "balanced".to_string(),
        schema_ver: VECTOR_STORE_VER,
    };
    std::fs::write(&meta_path, serde_json::to_vec(&expected).unwrap()).unwrap();
    let keep = sentinel(dir.path());

    wipe_on_mismatch(dir.path(), &meta_path, &expected).unwrap();
    assert!(keep.exists(), "matching meta must not wipe the store");
}

/// A bumped `schema_ver` (e.g. a future minor) on an otherwise-identical store wipes.
#[test]
fn schema_ver_bump_triggers_wipe() {
    let dir = tempfile::tempdir().unwrap();
    let meta_path = dir.path().join(META_FILE);
    let on_disk = LanceMeta {
        dim: 384,
        embedding_model: "balanced".to_string(),
        schema_ver: VECTOR_STORE_VER,
    };
    std::fs::write(&meta_path, serde_json::to_vec(&on_disk).unwrap()).unwrap();
    let keep = sentinel(dir.path());

    let expected = LanceMeta {
        schema_ver: VECTOR_STORE_VER + 1,
        ..on_disk
    };
    wipe_on_mismatch(dir.path(), &meta_path, &expected).unwrap();
    assert!(!keep.exists(), "a schema_ver bump should wipe the store");
}

/// A store written before the document-link table (or the per-path `documents` table) was dropped
/// still holds it. The table is no longer declared, which must not read as a mismatch: the store opens, keeps its
/// `meta.json`, and leaves the old table on disk.
#[test]
fn a_store_holding_the_retired_doc_links_table_opens_without_a_wipe() {
    use lancedb::arrow::arrow_schema::{DataType, Field, Schema};

    let dir = tempfile::tempdir().unwrap();
    drop(LanceStore::open(dir.path(), 4, "test").unwrap());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let connection = lancedb::connect(&dir.path().to_string_lossy()).execute().await.unwrap();
        let schema = Arc::new(Schema::new(vec![
            Field::new("scope", DataType::Utf8, false),
            Field::new("doc_path", DataType::Utf8, false),
        ]));
        connection
            .create_empty_table("doc_links", schema)
            .execute()
            .await
            .unwrap();
    });
    let old_table = dir.path().join("doc_links.lance");
    assert!(old_table.is_dir(), "fixture must hold the retired table");
    let meta_before = std::fs::read(dir.path().join(META_FILE)).unwrap();
    let keep = sentinel(dir.path());

    let store = LanceStore::open(dir.path(), 4, "test").unwrap();

    assert!(keep.exists(), "an undeclared table is not a mismatch: nothing is wiped");
    assert!(old_table.is_dir(), "the retired table is left on disk");
    assert_eq!(std::fs::read(dir.path().join(META_FILE)).unwrap(), meta_before);
    let tables: Vec<String> = store
        .version_counts()
        .unwrap()
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert!(tables.iter().any(|name| name == MEMORY_TABLE), "{tables:?}");
}

/// A `schema_ver` newer than this binary's belongs to a newer basemind sharing the cache: the open
/// fails and the store is left untouched, even when dim and model match.
#[test]
fn newer_schema_ver_on_disk_is_refused_without_wiping() {
    let dir = tempfile::tempdir().unwrap();
    let meta_path = dir.path().join(META_FILE);
    let expected = LanceMeta {
        dim: 384,
        embedding_model: "balanced".to_string(),
        schema_ver: VECTOR_STORE_VER,
    };
    let on_disk = LanceMeta {
        schema_ver: VECTOR_STORE_VER + 1,
        ..expected.clone()
    };
    std::fs::write(&meta_path, serde_json::to_vec(&on_disk).unwrap()).unwrap();
    let keep = sentinel(dir.path());

    let error = wipe_on_mismatch(dir.path(), &meta_path, &expected).unwrap_err();

    let message = error.to_string();
    assert!(
        message.contains("newer basemind") && message.contains("upgrade basemind"),
        "the error must tell the user to upgrade, got: {message}"
    );
    assert!(keep.exists(), "a newer store must not be wiped");
    assert!(meta_path.exists(), "a newer store keeps its meta.json");
}

fn memory_row(key: &str, _text: &str, updated_at: i64) -> MemoryRow {
    MemoryRow {
        scope: "s".into(),
        key: key.into(),
        tags: Vec::new(),
        visibility: "group".into(),
        agent_id: String::new(),
        embedding: vec![0.0; 4],
        created_at: 1,
        updated_at,
    }
}

/// A hit names the write it was embedded from, so the caller can tell a row a newer write left
/// behind from a current one.
#[test]
fn a_memory_hit_carries_the_stamp_of_the_write_it_was_embedded_from() {
    let dir = tempfile::tempdir().unwrap();
    let store = LanceStore::open(dir.path(), 4, "test").unwrap();
    store.upsert_memory(memory_row("k", "old text", 7)).unwrap();

    let hits = store.search_memory("s", "group", "", vec![0.0; 4], 10, &[]).unwrap();

    assert_eq!(hits.len(), 1);
    assert_eq!((hits[0].key.as_str(), hits[0].updated_at), ("k", 7));
}

/// The stamps say which write of each note the table holds a vector for, one namespace at a time.
#[test]
fn memory_stamps_list_one_namespace() {
    let dir = tempfile::tempdir().unwrap();
    let store = LanceStore::open(dir.path(), 4, "test").unwrap();
    assert!(
        store.memory_stamps("s", "group", "").unwrap().stamps.is_empty(),
        "a new table holds no rows"
    );
    store.upsert_memory(memory_row("a", "one", 3)).unwrap();
    store.upsert_memory(memory_row("b", "two", 4)).unwrap();
    let mut other = memory_row("c", "three", 5);
    other.scope = "other".into();
    store.upsert_memory(other).unwrap();

    let found = store.memory_stamps("s", "group", "").unwrap();

    assert_eq!(found.stamps.len(), 2);
    assert_eq!((found.stamps["a"], found.stamps["b"]), (3, 4));
    assert!(found.duplicated.is_empty());
}

/// Upserting the same rows twice leaves one row per key, and one upsert carries many rows.
#[test]
fn a_repeated_batch_upsert_leaves_one_row_per_key() {
    let dir = tempfile::tempdir().unwrap();
    let store = LanceStore::open(dir.path(), 4, "test").unwrap();
    let rows = [memory_row("a", "one", 3), memory_row("b", "two", 4)];

    store.upsert_memory_rows(&rows).unwrap();
    store.upsert_memory_rows(&rows).unwrap();

    let hits = store.search_memory("s", "group", "", vec![0.0; 4], 10, &[]).unwrap();
    assert_eq!(hits.len(), 2, "{hits:?}");
    assert!(store.memory_stamps("s", "group", "").unwrap().duplicated.is_empty());
}

/// Two rows for one key (two writers racing) are reported, so the caller can rewrite the key.
#[test]
fn memory_stamps_name_the_keys_with_more_than_one_row() {
    let dir = tempfile::tempdir().unwrap();
    let store = LanceStore::open(dir.path(), 4, "test").unwrap();
    store.upsert_memory(memory_row("a", "one", 3)).unwrap();
    store.add_memory_rows_unchecked(&[memory_row("a", "one", 3)]).unwrap();

    let found = store.memory_stamps("s", "group", "").unwrap();

    assert!(found.duplicated.contains("a"), "{found:?}");
    store.upsert_memory(memory_row("a", "one", 3)).unwrap();
    assert!(store.memory_stamps("s", "group", "").unwrap().duplicated.is_empty());
}

/// A delete bounded by an instant spares a row written after it: a prune decided from an earlier
/// listing must not remove a note saved since.
#[test]
fn a_bounded_delete_spares_rows_written_later() {
    let dir = tempfile::tempdir().unwrap();
    let store = LanceStore::open(dir.path(), 4, "test").unwrap();
    store.upsert_memory(memory_row("old", "one", 100)).unwrap();
    store.upsert_memory(memory_row("new", "two", 300)).unwrap();

    let deleted = store
        .delete_memory_keys("s", "group", "", &["old", "new"], Some(200))
        .unwrap();

    assert_eq!(deleted, 1);
    let found = store.memory_stamps("s", "group", "").unwrap();
    assert_eq!(found.stamps.keys().collect::<Vec<_>>(), ["new"]);
}

/// A `meta.json` that cannot be parsed (a write cut short) is a mismatch: the store is a cache and
/// is rebuilt; it does not fail the open.
#[test]
fn an_unreadable_meta_is_a_mismatch_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let meta_path = dir.path().join(META_FILE);
    std::fs::write(&meta_path, br#"{"dim":38"#).unwrap();
    let keep = sentinel(dir.path());
    let expected = LanceMeta {
        dim: 384,
        embedding_model: "balanced".to_string(),
        schema_ver: VECTOR_STORE_VER,
    };

    wipe_on_mismatch(dir.path(), &meta_path, &expected).unwrap();

    assert!(!keep.exists() && !meta_path.exists(), "the store is wiped");
    drop(LanceStore::open(dir.path(), 4, "test").unwrap());
    let reread: LanceMeta = serde_json::from_slice(&std::fs::read(&meta_path).unwrap()).unwrap();
    assert_eq!(reread.dim, 4, "and the next open writes a whole meta.json");
    assert!(!dir.path().join("meta.json.tmp").exists());
}

/// `meta.json` is removed last. While it is on disk a wipe that was cut short is seen and finished by
/// the next open.
#[test]
fn a_wipe_that_cannot_finish_leaves_meta_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let meta_path = dir.path().join(META_FILE);
    std::fs::write(&meta_path, b"{}").unwrap();
    let table = sentinel(dir.path());
    let refuse = |_: &Path| Err(std::io::Error::other("the table cannot be removed"));

    let wiped = remove_entries(dir.path(), &meta_path, &refuse);

    assert!(wiped.is_err(), "the failure to remove an entry is reported");
    assert!(
        table.exists() && meta_path.exists(),
        "meta.json outlives an unfinished wipe"
    );
    remove_entries(dir.path(), &meta_path, &remove_entry).unwrap();
    assert!(
        !table.exists() && meta_path.exists(),
        "a wipe that finishes still leaves meta.json to its caller"
    );
}

/// A newer basemind may write a `meta.json` this binary cannot parse. Its version is still read,
/// and the store is refused, never wiped as unreadable.
#[test]
fn a_newer_meta_this_binary_cannot_parse_is_refused_not_wiped() {
    let dir = tempfile::tempdir().unwrap();
    let meta_path = dir.path().join(META_FILE);
    let newer = format!(r#"{{"schema_ver":{},"vectors":{{"dim":384}}}}"#, VECTOR_STORE_VER + 1);
    std::fs::write(&meta_path, &newer).unwrap();
    let keep = sentinel(dir.path());
    let expected = LanceMeta {
        dim: 384,
        embedding_model: "balanced".to_string(),
        schema_ver: VECTOR_STORE_VER,
    };

    let error = wipe_on_mismatch(dir.path(), &meta_path, &expected).unwrap_err();

    assert!(error.to_string().contains("newer basemind"), "{error}");
    assert!(keep.exists(), "a newer store must not be wiped");
    assert_eq!(std::fs::read_to_string(&meta_path).unwrap(), newer);
}

/// Tables beside no `meta.json` were created by an open that died before it recorded their model.
/// The next open, for any model, must not adopt them.
#[test]
fn tables_without_a_meta_are_not_adopted() {
    let dir = tempfile::tempdir().unwrap();
    let store = LanceStore::open(dir.path(), 4, "first").unwrap();
    store.upsert_memory(memory_row("k", "text", 7)).unwrap();
    drop(store);
    std::fs::remove_file(dir.path().join(META_FILE)).unwrap();

    let reopened = LanceStore::open(dir.path(), 4, "second").unwrap();

    assert!(
        reopened.memory_stamps("s", "group", "").unwrap().stamps.is_empty(),
        "rows embedded by an unknown model are gone"
    );
    let meta: LanceMeta = serde_json::from_slice(&std::fs::read(dir.path().join(META_FILE)).unwrap()).unwrap();
    assert_eq!(meta.embedding_model, "second");
}

/// Make `dir` refuse new and removed entries; returns false when the process is not stopped by
/// permissions (running as root), in which case the caller has nothing to observe.
#[cfg(unix)]
fn lock_down(dir: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    let enforced = std::fs::write(dir.join(".probe"), b"").is_err();
    if !enforced {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_file(dir.join(".probe"));
    }
    enforced
}

#[cfg(unix)]
fn unlock(dir: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A wipe that cannot finish must leave `meta.json` in place: a next open that finds the mismatch
/// finishes the wipe, while one that finds no meta adopts the tables the wipe left.
#[cfg(unix)]
#[test]
fn a_wipe_that_fails_on_a_table_keeps_meta_json_for_the_next_open() {
    let dir = tempfile::tempdir().unwrap();
    let meta_path = dir.path().join(META_FILE);
    let old = LanceMeta {
        dim: 8,
        embedding_model: "old".to_string(),
        schema_ver: VECTOR_STORE_VER,
    };
    std::fs::write(&meta_path, serde_json::to_vec(&old).unwrap()).unwrap();
    let stuck = dir.path().join("stuck.lance");
    std::fs::create_dir(&stuck).unwrap();
    std::fs::write(stuck.join("fragment"), b"rows").unwrap();
    if !lock_down(&stuck) {
        return;
    }
    let expected = LanceMeta {
        dim: 4,
        embedding_model: "new".to_string(),
        schema_ver: VECTOR_STORE_VER,
    };

    let wiped = wipe_on_mismatch(dir.path(), &meta_path, &expected);

    unlock(&stuck);
    assert!(wiped.is_err(), "the table cannot be removed");
    assert!(meta_path.exists(), "meta.json was removed before the tables");
}

/// `meta.json` is the first thing an open of a new store writes: an open that dies afterwards must
/// leave the model recorded, so the tables it made are not adopted by another model's open.
#[cfg(unix)]
#[test]
fn a_new_store_records_its_meta_json_before_it_creates_a_table() {
    let dir = tempfile::tempdir().unwrap();
    if !lock_down(dir.path()) {
        return;
    }

    let opened = LanceStore::open(dir.path(), 4, "test");

    unlock(dir.path());
    let error = format!("{:#}", opened.err().expect("a read-only directory cannot be opened"));
    assert!(
        error.contains(META_FILE),
        "the first write of an open is the meta: {error}"
    );
}

/// A row embedded from an earlier write of a note must not take the place of the row of a later
/// one: the writer embedded from a listing, and the note was saved again since.
#[test]
fn an_upsert_from_an_earlier_write_spares_the_row_of_a_later_one() {
    let dir = tempfile::tempdir().unwrap();
    let store = LanceStore::open(dir.path(), 4, "test").unwrap();
    store.upsert_memory(memory_row("k", "new text", 300)).unwrap();

    store.upsert_memory_rows(&[memory_row("k", "old text", 100)]).unwrap();

    let stamps: Vec<i64> = store
        .search_memory("s", "group", "", vec![0.0; 4], 10, &[])
        .unwrap()
        .into_iter()
        .map(|hit| hit.updated_at)
        .collect();
    assert!(
        stamps.contains(&300),
        "the later write's row is still there: {stamps:?}"
    );

    store.upsert_memory(memory_row("k", "newest text", 400)).unwrap();
    let found = store.memory_stamps("s", "group", "").unwrap();
    assert_eq!(found.stamps["k"], 400);
    assert!(found.duplicated.is_empty(), "a later write removes every earlier row");
}

/// The tags are part of the search, not a filter over its result: a row carrying one of them further
/// from the query than one carrying none is still found within the limit.
#[test]
fn a_memory_search_by_tags_is_filtered_in_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = LanceStore::open(dir.path(), 4, "test").unwrap();
    let mut near = memory_row("near", "", 1);
    near.embedding = vec![0.0; 4];
    let mut far = memory_row("far", "", 2);
    far.embedding = vec![1.0; 4];
    far.tags = vec!["wanted".into(), "it's".into()];
    store.upsert_memory_rows(&[near, far]).unwrap();

    let keys = |tags: &[&str]| -> Vec<String> {
        let tags: Vec<String> = tags.iter().map(|tag| tag.to_string()).collect();
        store
            .search_memory("s", "group", "", vec![0.0; 4], 1, &tags)
            .unwrap()
            .into_iter()
            .map(|hit| hit.key)
            .collect()
    };

    assert_eq!(keys(&[]), ["near"]);
    assert_eq!(keys(&["wanted"]), ["far"]);
    assert_eq!(
        keys(&["absent", "wanted"]),
        ["far"],
        "a row carrying any of the tags is kept"
    );
    assert_eq!(keys(&["it's"]), ["far"], "a quote in a tag is escaped");
    assert!(keys(&["absent"]).is_empty());
    assert!(keys(&["absent", "missing"]).is_empty());
}

/// The temp file `meta.json` is written through is named by pid. Something another process has
/// at the name every process once shared must not fail this write or be renamed into place.
#[test]
fn another_process_temp_file_does_not_get_in_the_way_of_writing_meta() {
    let dir = tempfile::tempdir().unwrap();
    let meta_path = dir.path().join(META_FILE);
    let foreign = dir.path().join("meta.json.tmp");
    std::fs::create_dir(&foreign).unwrap();
    let meta = LanceMeta {
        dim: 4,
        embedding_model: "test".to_string(),
        schema_ver: VECTOR_STORE_VER,
    };

    write_meta(&meta_path, &meta).expect("the write goes through its own temp file");

    let written: LanceMeta = serde_json::from_slice(&std::fs::read(&meta_path).unwrap()).unwrap();
    assert_eq!(written, meta);
    assert!(foreign.exists(), "the other process's file is left alone");
    let own = dir.path().join(format!("meta.json.{}.tmp", std::process::id()));
    assert!(!own.exists(), "the temp file is renamed away");
}

/// Opens of one new store from several threads at once all succeed and leave the tables in place:
/// none removes, as tables without a meta, what another has just created.
#[test]
fn concurrent_opens_of_a_new_store_all_succeed() {
    for _ in 0..4 {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let opens: Vec<_> = (0..6)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || {
                    LanceStore::open(&path, 4, "test").map(|store| store.memory_stamps("s", "group", "").map(|_| ()))
                })
            })
            .collect();
        for open in opens {
            open.join().unwrap().expect("open").expect("the memory table is there");
        }
    }
}

/// A store keeps its id for as long as it holds its rows, and a wiped or removed store comes back
/// under another: that is how a scan tells the store it wrote to from one created since.
#[test]
fn a_store_keeps_its_id_until_it_is_wiped() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(store_id(dir.path()), None, "no store, no id");
    drop(LanceStore::open(dir.path(), 4, "test").unwrap());
    let first = store_id(dir.path()).expect("a new store has an id");

    drop(LanceStore::open(dir.path(), 4, "test").unwrap());
    assert_eq!(
        store_id(dir.path()).as_deref(),
        Some(first.as_str()),
        "a reopen keeps it"
    );

    drop(LanceStore::open(dir.path(), 4, "another-model").unwrap());
    let second = store_id(dir.path()).expect("the rebuilt store has an id");
    assert_ne!(first, second, "a wipe starts a new store");
}

/// An open waits for another open of the store only so long, and the lock it takes is released
/// when the guard drops.
#[test]
fn the_open_lock_is_waited_for_within_a_bound_and_released_on_drop() {
    let dir = tempfile::tempdir().unwrap();
    let held = lock_open(dir.path(), OPEN_LOCK_WAIT)
        .unwrap()
        .expect("a free lock is taken");

    let path = dir.path().to_path_buf();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(lock_open(&path, std::time::Duration::from_millis(100)).is_err());
    });
    let refused = receiver
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("a held lock is waited for within the bound, not forever");
    assert!(refused, "the lock was held throughout, so the wait ends in an error");

    drop(held);
    assert!(
        lock_open(dir.path(), std::time::Duration::ZERO).unwrap().is_some(),
        "free once the guard drops"
    );
}

/// An open that cannot get the lock in time fails rather than open beside the holder, which may be
/// wiping the store; it touches nothing, and the store opens once the lock is free.
#[test]
fn an_open_that_waits_out_a_held_lock_fails_and_touches_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let held = lock_open(dir.path(), OPEN_LOCK_WAIT)
        .unwrap()
        .expect("a free lock is taken");

    let refused = LanceStore::open_waiting(dir.path(), 4, "model", std::time::Duration::from_millis(100));

    assert!(refused.is_err(), "an open beside the holder is refused");
    assert!(
        !dir.path().join(META_FILE).exists(),
        "nothing was created beside the holder"
    );
    drop(held);
    drop(LanceStore::open(dir.path(), 4, "model").expect("opens once the lock is free"));
}

/// A store written by a binary whose store version was the release minor reads as older than the
/// decoupled version and is wiped and rebuilt, never refused: an upgrade does not lock the user
/// out of their vector store, and the memory notes, kept elsewhere, re-embed into it.
#[test]
fn a_store_versioned_by_the_release_minor_is_older_and_wiped() {
    assert!(u32::from(crate::version::RELEASE_MINOR) < VECTOR_STORE_VER);
    let dir = tempfile::tempdir().unwrap();
    let meta = LanceMeta {
        dim: 4,
        embedding_model: "m".to_string(),
        schema_ver: u32::from(crate::version::RELEASE_MINOR),
    };
    write_meta(&dir.path().join(META_FILE), &meta).unwrap();
    std::fs::write(dir.path().join("stale.lance"), b"x").unwrap();
    let store = LanceStore::open(dir.path(), 4, "m").expect("an older store opens");
    assert!(!dir.path().join("stale.lance").exists(), "its tables were wiped");
    let written: LanceMeta = serde_json::from_slice(&std::fs::read(dir.path().join(META_FILE)).unwrap()).unwrap();
    assert_eq!(written.schema_ver, VECTOR_STORE_VER);
    drop(store);
}

fn small_fragments(store: &LanceStore, table: &str) -> usize {
    store.inner.rt().block_on(async {
        let t = store.inner.connection.open_table(table).execute().await.unwrap();
        t.stats().await.unwrap().fragment_stats.num_small_fragments
    })
}

/// Each `basemind memory save` is its own process: one open, one upsert, gone. The per-process
/// commit counter never reaches its period there, so the table's own fragment count bounds it.
#[test]
fn short_lived_memory_writers_keep_the_fragment_count_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let writes = super::batch::AUTO_COMPACT_SMALL_FRAGMENTS + 8;
    for i in 0..writes {
        let store = LanceStore::open(dir.path(), 4, "test").unwrap();
        store
            .upsert_memory(memory_row(&format!("k{i}"), "note", i64::try_from(i).unwrap() + 1))
            .unwrap();
    }

    let store = LanceStore::open(dir.path(), 4, "test").unwrap();
    let fragments = small_fragments(&store, MEMORY_TABLE);
    assert!(
        fragments < super::batch::AUTO_COMPACT_SMALL_FRAGMENTS,
        "{fragments} small fragments after {writes} one-write processes"
    );
    assert_eq!(store.memory_stamps("s", "group", "").unwrap().stamps.len(), writes);
}

fn listing(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// A search from a process without the workspace lock, embedding with another preset, wiped the
/// writer's tables; the writer's next open wiped them back, and the two did so on every search. A
/// read-side open of a mismatched store is refused and leaves it exactly as it was.
#[test]
fn a_read_side_open_of_another_preset_refuses_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("lance");
    drop(LanceStore::open(&store, 8, "balanced").expect("the writer builds the store"));
    let meta = std::fs::read(store.join(META_FILE)).unwrap();
    let store_id = std::fs::read(store.join(STORE_ID_FILE)).unwrap();
    let before = listing(&store);
    let keep = sentinel(&store);

    let refused = LanceStore::open_existing(&store, 16, "fast")
        .err()
        .expect("another preset is refused");
    assert!(refused.to_string().contains("balanced"), "{refused}");

    assert!(keep.exists(), "nothing was wiped");
    assert_eq!(std::fs::read(store.join(META_FILE)).unwrap(), meta);
    assert_eq!(std::fs::read(store.join(STORE_ID_FILE)).unwrap(), store_id);
    let mut after = listing(&store);
    after.retain(|name| name != "sentinel");
    assert_eq!(after, before);
    assert!(
        LanceStore::open_existing(&store, 8, "balanced").is_ok(),
        "the matching preset reads it"
    );
}

/// A read-side open where no store was built creates nothing.
#[test]
fn a_read_side_open_of_a_missing_store_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("lance");
    assert!(LanceStore::open_existing(&store, 8, "balanced").is_err());
    assert!(!store.exists());
}

/// A newer basemind's store is refused by the read side, not wiped.
#[test]
fn a_read_side_open_refuses_a_newer_store() {
    let dir = tempfile::tempdir().unwrap();
    let newer = LanceMeta {
        dim: 8,
        embedding_model: "balanced".to_string(),
        schema_ver: VECTOR_STORE_VER + 1,
    };
    std::fs::write(dir.path().join(META_FILE), serde_json::to_vec(&newer).unwrap()).unwrap();
    let keep = sentinel(dir.path());
    let refused = LanceStore::open_existing(dir.path(), 8, "balanced")
        .err()
        .expect("refused");
    assert!(refused.to_string().contains("newer"), "{refused}");
    assert!(keep.exists());
}

/// Bytes the metadata cache of `session` holds after `writes` memory upserts and searches by a
/// store opened through it.
fn metadata_cached_after_writes(session: Arc<lancedb::Session>, writes: i64) -> usize {
    let dir = tempfile::tempdir().unwrap();
    let tables = [(MEMORY_TABLE, memory_schema(4))];
    let store = LanceStore::open_with(dir.path(), 4, "test", OPEN_LOCK_WAIT, &tables, Arc::clone(&session)).unwrap();
    for i in 0..writes {
        store.upsert_memory(memory_row(&format!("k{}", i % 4), "", i)).unwrap();
        store.search_memory("s", "group", "", vec![0.0; 4], 4, &[]).unwrap();
    }
    store.inner.rt().block_on(session.metadata_cache_stats()).size_bytes
}

/// Every store of the process caches into one session, so the daemon's Lance caches have one
/// bound however many stores it holds open.
#[test]
fn every_store_shares_the_process_session() {
    assert!(Arc::ptr_eq(&session::shared(), &session::shared()));
    const { assert!(session::INDEX_CACHE_BYTES + session::METADATA_CACHE_BYTES <= 128 << 20) };
}

/// Each commit caches its version's manifest and file metadata, and a pruned version's entries
/// stay until the budget evicts them: under Lance's 1 GiB default the cache grows with every write
/// (a daemon soak grew by about 4 MiB/min this way). A bounded session holds at most its budget.
#[test]
fn the_metadata_cache_stays_within_its_budget_under_steady_writes() {
    const BUDGET: usize = 256 << 10;
    const WRITES: i64 = 120;

    let unbounded = metadata_cached_after_writes(session::bounded(64 << 20, 1 << 30), WRITES);
    assert!(
        unbounded > 2 * BUDGET,
        "under the default budget the writes cache more than the bound under test: {unbounded}"
    );

    let bounded = metadata_cached_after_writes(session::bounded(64 << 20, BUDGET), WRITES);
    assert!(bounded > 0, "the store caches into the session it was opened with");
    assert!(bounded <= BUDGET, "{bounded} bytes cached over a {BUDGET}-byte budget");
}

/// The paths this process holds open under `dir`.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn open_files_under(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let dir = dir.canonicalize().unwrap();
    let mut open = Vec::new();
    #[cfg(target_os = "linux")]
    for entry in std::fs::read_dir("/proc/self/fd").unwrap().flatten() {
        if let Ok(target) = std::fs::read_link(entry.path()) {
            open.push(target);
        }
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: `getdtablesize` has no preconditions. ~keep
        let table = unsafe { libc::getdtablesize() };
        for fd in 0..table {
            let mut buf = [0u8; libc::PATH_MAX as usize];
            // SAFETY: `F_GETPATH` writes at most `PATH_MAX` bytes into `buf`; a closed fd fails. ~keep
            if unsafe { libc::fcntl(fd, libc::F_GETPATH, buf.as_mut_ptr()) } == 0 {
                let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
                open.push(std::path::PathBuf::from(
                    String::from_utf8_lossy(&buf[..len]).into_owned(),
                ));
            }
        }
    }
    open.retain(|path| path.starts_with(&dir));
    open
}

/// Dropping a store ends the tasks lance spawned for it, so nothing keeps a handle into its
/// directory once the last clone is gone and the directory can be removed.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn a_dropped_store_leaves_no_open_handle_in_its_directory() {
    let dir = tempfile::tempdir().unwrap();
    let probe = std::fs::File::create(dir.path().join("probe")).unwrap();
    assert_eq!(open_files_under(dir.path()).len(), 1, "an open file is seen");
    drop(probe);
    let store = LanceStore::open(dir.path(), 4, "test").unwrap();
    let rows: Vec<MemoryRow> = (0..64).map(|i| memory_row(&format!("k{i}"), "text", i)).collect();
    store.upsert_memory_rows(&rows).unwrap();
    let hits = store.search_memory("s", "group", "", vec![0.0; 4], 10, &[]).unwrap();
    assert_eq!(hits.len(), 10, "the search read the table");
    let runtime = store.inner.rt().handle().clone();
    assert!(
        runtime.metrics().num_alive_tasks() > 0,
        "lance left tasks of its own on the store's runtime, which the drop has to end"
    );
    let reader = store.clone();
    drop(store);
    assert!(
        runtime.metrics().num_alive_tasks() > 0,
        "a live clone keeps the store's tasks"
    );
    drop(reader);

    assert_eq!(runtime.metrics().num_alive_tasks(), 0, "the drop ended lance's tasks");
    assert_eq!(open_files_under(dir.path()), Vec::<std::path::PathBuf>::new());
    std::fs::remove_dir_all(dir.path()).unwrap();
}
