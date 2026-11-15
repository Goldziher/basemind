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

/// The row predicate appends the key clause to the namespace predicate, and a
/// single-quote in any segment is escaped so the literal cannot break out.
#[test]
fn row_predicate_pins_key_and_escapes_quotes() {
    let p = memory_row_predicate("s", "individual", "a", "o'brien");
    assert_eq!(
        p,
        "scope = 's' AND visibility = 'individual' AND agent_id = 'a' AND key = 'o''brien'"
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

fn memory_row(key: &str, value: &str, updated_at: i64) -> MemoryRow {
    MemoryRow {
        scope: "s".into(),
        key: key.into(),
        value: value.into(),
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
