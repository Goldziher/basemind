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
/// because that differs from the current `MEMORY_SCHEMA_VER`, force a wipe — never a
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
        schema_ver: MEMORY_SCHEMA_VER,
    };
    assert_ne!(MEMORY_SCHEMA_VER, 0, "current schema ver must differ from the legacy 0");
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
        schema_ver: MEMORY_SCHEMA_VER,
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
        schema_ver: MEMORY_SCHEMA_VER,
    };
    std::fs::write(&meta_path, serde_json::to_vec(&on_disk).unwrap()).unwrap();
    let keep = sentinel(dir.path());

    let expected = LanceMeta {
        schema_ver: MEMORY_SCHEMA_VER + 1,
        ..on_disk
    };
    wipe_on_mismatch(dir.path(), &meta_path, &expected).unwrap();
    assert!(!keep.exists(), "a schema_ver bump should wipe the store");
}

/// A store written before the document-link table was dropped still holds `doc_links.lance`. The
/// table is no longer declared, which must not read as a mismatch: the store opens, keeps its
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
    assert!(tables.iter().any(|name| name == "documents"), "{tables:?}");
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
        schema_ver: MEMORY_SCHEMA_VER,
    };
    let on_disk = LanceMeta {
        schema_ver: MEMORY_SCHEMA_VER + 1,
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

    let hits = store.search_memory("s", "group", "", vec![0.0; 4], 10, None).unwrap();

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

    let hits = store.search_memory("s", "group", "", vec![0.0; 4], 10, None).unwrap();
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
        schema_ver: MEMORY_SCHEMA_VER,
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
    let newer = format!(r#"{{"schema_ver":{},"vectors":{{"dim":384}}}}"#, MEMORY_SCHEMA_VER + 1);
    std::fs::write(&meta_path, &newer).unwrap();
    let keep = sentinel(dir.path());
    let expected = LanceMeta {
        dim: 384,
        embedding_model: "balanced".to_string(),
        schema_ver: MEMORY_SCHEMA_VER,
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
        schema_ver: MEMORY_SCHEMA_VER,
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
        schema_ver: MEMORY_SCHEMA_VER,
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
        .search_memory("s", "group", "", vec![0.0; 4], 10, None)
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

/// The tag is part of the search, not a filter over its result: a tagged row further from the
/// query than an untagged one is still found within the limit.
#[test]
fn a_memory_search_by_tag_is_filtered_in_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = LanceStore::open(dir.path(), 4, "test").unwrap();
    let mut near = memory_row("near", "", 1);
    near.embedding = vec![0.0; 4];
    let mut far = memory_row("far", "", 2);
    far.embedding = vec![1.0; 4];
    far.tags = vec!["wanted".into(), "it's".into()];
    store.upsert_memory_rows(&[near, far]).unwrap();

    let keys = |tag: Option<&str>| -> Vec<String> {
        store
            .search_memory("s", "group", "", vec![0.0; 4], 1, tag)
            .unwrap()
            .into_iter()
            .map(|hit| hit.key)
            .collect()
    };

    assert_eq!(keys(None), ["near"]);
    assert_eq!(keys(Some("wanted")), ["far"]);
    assert_eq!(keys(Some("it's")), ["far"], "a quote in a tag is escaped");
    assert!(keys(Some("absent")).is_empty());
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
        schema_ver: MEMORY_SCHEMA_VER,
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
