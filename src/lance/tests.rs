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
