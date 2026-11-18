use super::*;
use crate::extract::{Call, FileMapL2, Import, Symbol, SymbolKind};
use crate::index::view::IndexView;
use tempfile::TempDir;

/// The content key the tests write a file's rows under.
fn content() -> keys::ContentKey {
    keys::content_key("hash", "rust", crate::extract::EXTRACT_EPOCH)
}

/// Publish `paths` as the view's live holders of [`content`], as a file map write would.
fn hold(db: &IndexDb, paths: &[&RelPath]) {
    db.publish_live_keys(crate::index::live_keys::LiveKeys::from_entries(
        paths.iter().map(|rel| ((*rel).clone(), content())),
    ));
}

fn fresh_db() -> (TempDir, IndexDb) {
    let dir = tempfile::tempdir().unwrap();
    let db = IndexDb::open(dir.path()).unwrap();
    (dir, db)
}

#[test]
fn open_reports_a_wipe_only_when_the_stamped_schema_version_differs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let memory = dir.path().join("memory.fjall");
    let fresh = IndexDb::open_with_memory(dir.path(), &memory).expect("fresh open");
    assert!(!fresh.wiped_on_open(), "a first open wipes nothing");
    fresh
        .meta
        .insert(
            crate::index::META_SCHEMA_VER,
            (crate::index::INDEX_SCHEMA_VER - 1).to_be_bytes(),
        )
        .expect("stamp an older schema version");
    drop(fresh);

    let wiped = IndexDb::open_with_memory(dir.path(), &memory).expect("reopen");
    assert!(wiped.wiped_on_open(), "an older revision wipes the index");
    assert!(wiped.view().symbols_index_is_empty());
    drop(wiped);

    let again = IndexDb::open_with_memory(dir.path(), &memory).expect("third open");
    assert!(!again.wiped_on_open(), "the rewritten stamp matches, so no second wipe");
}

/// Stamp a schema version one past this binary's and return the directory holding the index.
fn index_stamped_by_a_newer_basemind() -> (TempDir, std::path::PathBuf, u32) {
    let dir = tempfile::tempdir().expect("tempdir");
    let memory = dir.path().join("memory.fjall");
    let newer = crate::index::INDEX_SCHEMA_VER + 1;
    let fresh = IndexDb::open_with_memory(dir.path(), &memory).expect("fresh open");
    fresh
        .meta
        .insert(crate::index::META_SCHEMA_VER, newer.to_be_bytes())
        .expect("stamp a newer schema version");
    drop(fresh);
    (dir, memory, newer)
}

#[test]
fn writer_open_refuses_an_index_stamped_by_a_newer_basemind_and_leaves_it_intact() {
    let (dir, memory, newer) = index_stamped_by_a_newer_basemind();

    let Err(error) = IndexDb::open_with_memory(dir.path(), &memory) else {
        panic!("a newer index must not open for writing");
    };

    assert!(
        matches!(
            error,
            IndexError::NewerSchema { stored, current }
                if stored == newer && current == crate::index::INDEX_SCHEMA_VER
        ),
        "expected NewerSchema, got: {error}"
    );
    let message = error.to_string();
    assert!(
        message.contains("newer basemind") && message.contains("upgrade basemind"),
        "the error must tell the user to upgrade, got: {message}"
    );
    assert_eq!(
        crate::index::peek_index_schema_ver(dir.path()).expect("peek"),
        Some(newer),
        "the newer index keeps its stamp: it was neither wiped nor restamped"
    );
}

#[test]
fn read_only_open_refuses_an_index_stamped_by_a_newer_basemind_and_leaves_it_intact() {
    let (dir, memory, newer) = index_stamped_by_a_newer_basemind();

    let Err(error) = IndexDb::open_tolerating_held_memory(dir.path(), &memory) else {
        panic!("a newer index must not open read-only");
    };

    assert!(
        matches!(error, IndexError::NewerSchema { stored, .. } if stored == newer),
        "expected NewerSchema, got: {error}"
    );
    assert_eq!(
        crate::index::peek_index_schema_ver(dir.path()).expect("peek"),
        Some(newer),
        "a read-only open must not wipe or restamp a newer index"
    );
}

fn synthetic_l1(syms: &[(&str, SymbolKind, u32)]) -> FileMapL1 {
    FileMapL1 {
        schema_ver: crate::extract::SCHEMA_VER,
        language: "rust".to_string(),
        size_bytes: 0,
        had_errors: false,
        error_count: 0,
        symbols: syms
            .iter()
            .map(|(name, kind, start)| Symbol {
                name: name.to_string(),
                kind: *kind,
                start_byte: *start,
                end_byte: *start + 1,
                start_row: 0,
                start_col: 0,
                signature: None,
                decorators: Vec::new(),
            })
            .collect(),
        imports: Vec::new(),
        implementations: Vec::new(),
        rationale: Vec::new(),
        extract_epoch: crate::extract::EXTRACT_EPOCH,
    }
}

#[test]
fn upsert_and_query_symbols_by_name() {
    let (_d, db) = fresh_db();
    let mut w = db.writer();
    let rel = RelPath::from("src/a.rs");
    let l1 = synthetic_l1(&[("alpha", SymbolKind::Function, 0)]);
    w.upsert_file(&rel, &content(), &l1, None).unwrap();
    w.commit().unwrap();
    assert!(
        db.view().symbols_by_name_lookup("alpha", 10).is_empty(),
        "rows no live key names are not read"
    );

    hold(&db, &[&rel]);
    let hits = db.view().symbols_by_name_lookup("alpha", 10);
    assert_eq!(hits, vec![("alpha".to_string(), SymbolKind::Function, rel, 0)]);
}

/// Content rows are shared: removing a path leaves them for other holders, and the view stops
/// reading them once its live keys drop the path.
#[test]
fn removing_a_path_leaves_its_content_rows_and_the_live_keys_decide() {
    let (_d, db) = fresh_db();
    let mut w = db.writer();
    let (a, b) = (RelPath::from("src/a.rs"), RelPath::from("src/b.rs"));
    let l1 = synthetic_l1(&[("alpha", SymbolKind::Function, 0)]);
    w.upsert_file(&a, &content(), &l1, None).unwrap();
    w.upsert_file(&b, &content(), &l1, None).unwrap();
    w.commit().unwrap();
    assert_eq!(db.symbols_by_path.iter().count(), 1, "one content, one row");
    hold(&db, &[&a, &b]);
    assert_eq!(
        db.view().symbols_by_name_lookup("alpha", 10).len(),
        2,
        "one hit per holder"
    );

    let mut w = db.writer();
    w.remove_file(&a).unwrap();
    w.commit().unwrap();
    hold(&db, &[&b]);

    assert_eq!(db.symbols_by_name.iter().count(), 1, "b still holds the content");
    let hits = db.view().symbols_by_name_lookup("alpha", 10);
    assert_eq!(hits, vec![("alpha".to_string(), SymbolKind::Function, b, 0)]);
    hold(&db, &[]);
    assert!(db.view().symbols_by_name_lookup("alpha", 10).is_empty());
    assert!(db.view().symbols_index_is_empty());
}

fn calls(sites: &[(&str, u32)]) -> FileMapL2 {
    FileMapL2 {
        schema_ver: crate::extract::SCHEMA_VER,
        language: "rust".to_string(),
        calls: sites
            .iter()
            .map(|(callee, start)| Call {
                callee: callee.to_string(),
                start_byte: *start,
                end_byte: *start + 1,
                start_row: 0,
                start_col: 0,
            })
            .collect(),
        docs: Vec::new(),
    }
}

/// A content extracted again with a different result and no epoch bump (a grammar or query change,
/// a dev build) is rewritten whole: the earlier extraction's rows are not served beside the new.
#[test]
fn writing_a_content_again_replaces_the_rows_of_its_earlier_extraction() {
    let (_d, db) = fresh_db();
    let rel = RelPath::from("src/a.rs");
    let first = synthetic_l1(&[("alpha", SymbolKind::Function, 0), ("beta", SymbolKind::Function, 10)]);
    let mut w = db.writer();
    w.upsert_file(
        &rel,
        &content(),
        &first,
        Some(&calls(&[("old_callee", 3), ("kept", 7)])),
    )
    .unwrap();
    w.commit().unwrap();

    let again = synthetic_l1(&[("gamma", SymbolKind::Function, 0)]);
    let mut w = db.writer();
    w.upsert_file(&rel, &content(), &again, Some(&calls(&[("kept", 7)])))
        .unwrap();
    w.commit().unwrap();
    hold(&db, &[&rel]);

    let view = db.view();
    for gone in ["alpha", "beta"] {
        assert!(view.symbols_by_name_lookup(gone, 10).is_empty(), "{gone} is gone");
    }
    assert_eq!(view.symbols_by_name_lookup("gamma", 10).len(), 1);
    assert_eq!(db.symbols_by_path.iter().count(), 1);
    assert_eq!(db.symbols_by_name.iter().count(), 1);
    assert_eq!(db.calls_by_path.iter().count(), 1);
    assert_eq!(db.calls_by_callee.iter().count(), 1, "old_callee's row is gone");
    assert!(view.call_at(&content(), 3).unwrap().is_none());
    assert!(view.call_at(&content(), 7).unwrap().is_some());

    // The same extraction again writes nothing; one without calls leaves the calls as they are. ~keep
    let mut w = db.writer();
    w.upsert_file(&rel, &content(), &again, Some(&calls(&[("kept", 7)])))
        .unwrap();
    assert_eq!(w.staged_bytes(), 0, "an identical extraction stages nothing");
    w.upsert_file(&rel, &content(), &again, None).unwrap();
    w.commit().unwrap();
    assert_eq!(db.calls_by_callee.iter().count(), 1);
}

#[test]
fn calls_index_round_trip() {
    let (_d, db) = fresh_db();
    let mut w = db.writer();
    let rel = RelPath::from("src/main.rs");
    let l1 = synthetic_l1(&[("main", SymbolKind::Function, 0)]);
    let l2 = FileMapL2 {
        schema_ver: crate::extract::SCHEMA_VER,
        language: "rust".to_string(),
        calls: vec![
            Call {
                callee: "spawn".to_string(),
                start_byte: 10,
                end_byte: 15,
                start_row: 0,
                start_col: 0,
            },
            Call {
                callee: "spawn".to_string(),
                start_byte: 30,
                end_byte: 35,
                start_row: 0,
                start_col: 0,
            },
            Call {
                callee: "spawn_blocking".to_string(),
                start_byte: 50,
                end_byte: 64,
                start_row: 0,
                start_col: 0,
            },
        ],
        docs: Vec::new(),
    };
    w.upsert_file(&rel, &content(), &l1, Some(&l2)).unwrap();
    w.commit().unwrap();

    let prefix = keys::calls_by_callee_prefix("spawn");
    let spawn_row_len = keys::call_by_callee("spawn", &content(), 0).unwrap().len();
    let mut spawn_hits = 0;
    for guard in db.calls_by_callee.raw().prefix(prefix) {
        let (k, _) = guard.into_inner().unwrap();
        assert_eq!(k.len(), spawn_row_len, "prefix scan must not bleed into spawn_blocking");
        assert_eq!(keys::call_by_callee_content(&k), Some(content()));
        spawn_hits += 1;
    }
    assert_eq!(spawn_hits, 2);

    hold(&db, &[&rel]);
    let view = db.view();
    let call = view.call_at(&content(), 30).unwrap().expect("the call at 30");
    assert_eq!(rmp_serde::from_slice::<Call>(&call).unwrap().start_byte, 30);
    assert_eq!(view.calls_in_content(&content()).count(), 3);
    let other = keys::content_key("other", "rust", crate::extract::EXTRACT_EPOCH);
    assert_eq!(view.calls_in_content(&other).count(), 0);
}

#[test]
fn imports_by_module_round_trip() {
    let (_d, db) = fresh_db();
    let mut w = db.writer();
    let rel = RelPath::from("src/foo.py");
    let mut l1 = synthetic_l1(&[]);
    l1.imports = vec![
        Import {
            module: Some("os".to_string()),
            raw: "import os".to_string(),
            start_byte: 0,
            end_byte: 9,
        },
        Import {
            module: Some("os.path".to_string()),
            raw: "import os.path".to_string(),
            start_byte: 10,
            end_byte: 24,
        },
    ];
    w.upsert_file(&rel, &content(), &l1, None).unwrap();
    w.commit().unwrap();

    let prefix = keys::imports_by_module_prefix("os");
    let mut os_hits = 0;
    for guard in db.imports_by_module.raw().prefix(prefix) {
        let (k, _) = guard.into_inner().unwrap();
        let (module, _, _) = keys::parse_import_by_module(&k).unwrap();
        assert_eq!(module, "os");
        os_hits += 1;
    }
    assert_eq!(os_hits, 1, "prefix scan must isolate `os` from `os.path`");
}

fn synthetic_l1_with_impls(impls: &[(&str, &str, u32)]) -> FileMapL1 {
    let mut l1 = synthetic_l1(&[]);
    l1.implementations = impls
        .iter()
        .map(|(t, i, sb)| crate::extract::Implementation {
            trait_name: t.to_string(),
            impl_type: i.to_string(),
            start_byte: *sb,
            start_row: 0,
            start_col: 0,
        })
        .collect();
    l1
}

/// Iteration-3 dual-partition test for implementations. Mirrors
/// `imports_by_path_roundtrip_and_dual_partition_consistency`: upsert two rows, verify
/// both partitions have 2 entries; re-upsert with one row dropped, verify both
/// partitions have 1 entry; remove the file, verify both partitions empty.
#[test]
fn implementations_dual_partition_consistency() {
    let (_d, db) = fresh_db();
    let rel = RelPath::from("src/foo.rs");

    let mut w = db.writer();
    w.upsert_file(
        &rel,
        &content(),
        &synthetic_l1_with_impls(&[("Display", "Foo", 0), ("Debug", "Foo", 10)]),
        None,
    )
    .unwrap();
    w.commit().unwrap();

    assert_eq!(db.implementations_by_trait.iter().count(), 2);
    assert_eq!(db.implementations_by_path.iter().count(), 2);

    let prefix = keys::impls_by_trait_prefix("Display");
    let mut display_hits = 0;
    for guard in db.implementations_by_trait.raw().prefix(prefix) {
        let (k, _) = guard.into_inner().unwrap();
        let (trait_name, impl_type, back_rel, _) = keys::parse_impl_by_trait(&k).unwrap();
        assert_eq!(trait_name, "Display");
        assert_eq!(impl_type, "Foo");
        assert_eq!(back_rel, rel);
        display_hits += 1;
    }
    assert_eq!(display_hits, 1);

    let mut w = db.writer();
    w.upsert_file(
        &rel,
        &content(),
        &synthetic_l1_with_impls(&[("Display", "Foo", 0)]),
        None,
    )
    .unwrap();
    w.commit().unwrap();

    assert_eq!(db.implementations_by_trait.iter().count(), 1);
    assert_eq!(db.implementations_by_path.iter().count(), 1);

    let mut w = db.writer();
    w.remove_file(&rel).unwrap();
    w.commit().unwrap();

    assert!(db.implementations_by_trait.iter().next().is_none());
    assert!(db.implementations_by_path.iter().next().is_none());
}

#[test]
fn imports_by_path_roundtrip_and_dual_partition_consistency() {
    let (_d, db) = fresh_db();
    let mut w = db.writer();
    let rel = RelPath::from("src/foo.py");
    let mut l1 = synthetic_l1(&[]);
    l1.imports = vec![
        Import {
            module: Some("os".to_string()),
            raw: "import os".to_string(),
            start_byte: 0,
            end_byte: 9,
        },
        Import {
            module: Some("os.path".to_string()),
            raw: "import os.path".to_string(),
            start_byte: 10,
            end_byte: 24,
        },
    ];
    w.upsert_file(&rel, &content(), &l1, None).unwrap();
    w.commit().unwrap();

    assert_eq!(db.imports_by_module.iter().count(), 2);
    assert_eq!(db.imports_by_path.iter().count(), 2);

    let prefix = keys::imports_by_path_prefix(&rel);
    let mut path_hits = 0;
    for guard in db.imports_by_path.raw().prefix(prefix) {
        let (k, _) = guard.into_inner().unwrap();
        let (back_rel, _, _) = keys::parse_import_by_path(&k).unwrap();
        assert_eq!(back_rel, rel);
        path_hits += 1;
    }
    assert_eq!(path_hits, 2);

    let mut l1 = synthetic_l1(&[]);
    l1.imports = vec![Import {
        module: Some("os".to_string()),
        raw: "import os".to_string(),
        start_byte: 0,
        end_byte: 9,
    }];
    let mut w = db.writer();
    w.upsert_file(&rel, &content(), &l1, None).unwrap();
    w.commit().unwrap();

    assert_eq!(db.imports_by_module.iter().count(), 1);
    assert_eq!(db.imports_by_path.iter().count(), 1);

    let mut w = db.writer();
    w.remove_file(&rel).unwrap();
    w.commit().unwrap();

    assert!(db.imports_by_module.iter().next().is_none());
    assert!(db.imports_by_path.iter().next().is_none());
}

/// Mixed oversized/normal upsert: the normal symbol must land in both partitions, the
/// oversized symbol must land only in `symbols_by_path` (outline stays complete). No
/// panic, no error propagated.
#[test]
fn oversized_identifier_skipped_gracefully() {
    let (_d, db) = fresh_db();
    let rel = RelPath::from("src/big.rs");
    let huge_name = "x".repeat(65536);
    let l1 = synthetic_l1(&[
        ("normal_fn", SymbolKind::Function, 0),
        (&huge_name, SymbolKind::Function, 100),
    ]);
    let mut w = db.writer();
    w.upsert_file(&rel, &content(), &l1, None).unwrap();
    w.commit().unwrap();

    assert_eq!(
        db.symbols_by_path.iter().count(),
        2,
        "both symbols must be in symbols_by_path"
    );
    assert_eq!(
        db.symbols_by_name.iter().count(),
        1,
        "only the normal symbol must be in symbols_by_name"
    );
    let prefix = keys::symbols_by_name_prefix("normal_fn");
    let hits: Vec<_> = db
        .symbols_by_name
        .raw()
        .prefix(prefix)
        .map(|g| g.into_inner().unwrap())
        .collect();
    assert_eq!(hits.len(), 1);
}

#[test]
fn resolved_edges_dual_partition_consistency() {
    use crate::intel::model::{FileResolvedRefs, ResolvedEdge};
    let (_d, db) = fresh_db();
    let rel = RelPath::from("src/app.ts");

    let mut refs = FileResolvedRefs::new("typescript");
    refs.intra = vec![
        ResolvedEdge {
            use_start: 100,
            use_end: 103,
            def_start: 4,
            def_end: 7,
        },
        ResolvedEdge {
            use_start: 200,
            use_end: 203,
            def_start: 4,
            def_end: 7,
        },
    ];
    let mut w = db.writer();
    w.upsert_resolved_file(&rel, &refs).unwrap();
    w.commit().unwrap();

    assert_eq!(db.refs_by_def.raw().iter().count(), 2);
    assert_eq!(db.refs_by_path.raw().iter().count(), 2);

    let mut uses: Vec<u32> = db
        .refs_by_def
        .raw()
        .prefix(keys::refs_by_def_prefix(&rel, 4))
        .map(|g| {
            let (k, _) = g.into_inner().unwrap();
            let (_dp, dstart, _up, ustart) = keys::parse_ref_by_def(&k).unwrap();
            assert_eq!(dstart, 4);
            ustart
        })
        .collect();
    uses.sort_unstable();
    assert_eq!(uses, vec![100, 200], "both uses must resolve to def@4");

    let defs: Vec<u32> = db
        .refs_by_path
        .raw()
        .prefix(keys::refs_by_use_prefix(&rel, 100))
        .map(|g| {
            let (k, _) = g.into_inner().unwrap();
            let (_up, ustart, _dp, dstart) = keys::parse_ref_by_path(&k).unwrap();
            assert_eq!(ustart, 100);
            dstart
        })
        .collect();
    assert_eq!(defs, vec![4], "use@100 must resolve to def@4");

    refs.intra.truncate(1);
    let mut w = db.writer();
    w.upsert_resolved_file(&rel, &refs).unwrap();
    w.commit().unwrap();
    assert_eq!(db.refs_by_def.raw().iter().count(), 1);
    assert_eq!(db.refs_by_path.raw().iter().count(), 1);

    let mut w = db.writer();
    w.remove_resolved_file(&rel).unwrap();
    w.commit().unwrap();
    assert!(db.refs_by_def.raw().iter().next().is_none());
    assert!(db.refs_by_path.raw().iter().next().is_none());
}

/// A full rescan re-resolves every file. Facts that did not change must stage nothing, and facts
/// that did must leave exactly the new edge set, as the delete-then-insert they replace did.
#[test]
fn an_unchanged_resolved_file_is_not_rewritten() {
    use crate::intel::model::{FileResolvedRefs, ResolvedEdge};
    let edge = |use_start, def_start| ResolvedEdge {
        use_start,
        use_end: use_start + 1,
        def_start,
        def_end: def_start + 1,
    };
    let (_d, db) = fresh_db();
    let rel = RelPath::from("src/app.rs");
    let mut refs = FileResolvedRefs::new("rust");
    refs.intra = vec![edge(100, 4), edge(200, 4), edge(300, 8)];
    let mut w = db.writer();
    w.upsert_resolved_file(&rel, &refs).unwrap();
    w.commit().unwrap();

    let mut w = db.writer();
    w.upsert_resolved_file(&rel, &refs).unwrap();
    assert_eq!(w.staged_bytes(), 0, "re-resolving unchanged facts must stage nothing");
    w.commit().unwrap();
    assert_eq!(db.refs_by_path.raw().iter().count(), 3);

    refs.intra = vec![edge(100, 4), edge(300, 9), edge(400, 8)];
    let mut w = db.writer();
    w.upsert_resolved_file(&rel, &refs).unwrap();
    w.commit().unwrap();
    let mut by_path: Vec<(u32, u32)> = db
        .refs_by_path
        .raw()
        .iter()
        .map(|g| {
            let (k, _) = g.into_inner().unwrap();
            let (_up, ustart, _dp, dstart) = keys::parse_ref_by_path(&k).unwrap();
            (ustart, dstart)
        })
        .collect();
    by_path.sort_unstable();
    assert_eq!(by_path, vec![(100, 4), (300, 9), (400, 8)]);
    let mut by_def: Vec<(u32, u32)> = db
        .refs_by_def
        .raw()
        .iter()
        .map(|g| {
            let (k, _) = g.into_inner().unwrap();
            let (_dp, dstart, _up, ustart) = keys::parse_ref_by_def(&k).unwrap();
            (ustart, dstart)
        })
        .collect();
    by_def.sort_unstable();
    assert_eq!(by_def, by_path, "both partitions hold the same edges");
}

#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
#[test]
fn definition_of_prefers_cross_file_target_over_import_binding() {
    use crate::intel::model::{FileResolvedRefs, ResolvedEdge};
    let (_d, db) = fresh_db();
    let importer = RelPath::from("src/app.py");
    let target = RelPath::from("src/module.py");

    let mut refs = FileResolvedRefs::new("python");
    refs.intra.push(ResolvedEdge {
        use_start: 100,
        use_end: 101,
        def_start: 4,
        def_end: 5,
    });
    let mut writer = db.writer();
    writer.upsert_resolved_file(&importer, &refs).unwrap();
    writer.upsert_cross_file_edge(&target, 8, &importer, 100).unwrap();
    writer.commit().unwrap();

    assert_eq!(db.view().definition_of(&importer, 100), Some((target, 8)));
}

#[cfg(feature = "code-search")]
#[test]
fn bm25_dual_partition_consistency() {
    use crate::search::bm25::ChunkPosting;
    let (_d, db) = fresh_db();
    let rel = RelPath::from("src/foo.rs");
    let content = keys::bm25_content_key("h", "rust", "digest");

    let postings = vec![
        ChunkPosting {
            chunk_id: "h:0".to_string(),
            doclen: 3,
            terms: vec![("spawn".to_string(), 2), ("task".to_string(), 1)],
        },
        ChunkPosting {
            chunk_id: "h:1".to_string(),
            doclen: 1,
            terms: vec![("spawn".to_string(), 1)],
        },
    ];
    let mut w = db.writer();
    w.upsert_bm25_file(&rel, &content, &postings).unwrap();
    w.commit().unwrap();

    assert_eq!(db.code_bm25_postings.iter().count(), 3);
    assert_eq!(db.code_bm25_by_path.iter().count(), 2);
    let live = db.bm25_live().unwrap();
    assert_eq!((live.len(), live.hash_of(&content)), (1, Some("h")));

    let mut spawn_docs: Vec<(keys::Bm25Key, u32, u32, u32)> = db
        .code_bm25_postings
        .raw()
        .prefix(keys::code_bm25_postings_prefix("spawn"))
        .map(|g| {
            let (k, v) = g.into_inner().unwrap();
            let (key, ordinal) = keys::parse_code_bm25_posting_chunk(&k).unwrap();
            let (tf, doclen) = keys::parse_code_bm25_posting_value(&v).unwrap();
            (key, ordinal, tf, doclen)
        })
        .collect();
    spawn_docs.sort();
    assert_eq!(spawn_docs, vec![(content, 0, 2, 3), (content, 1, 1, 1)]);

    let sibling = RelPath::from("src/sibling.rs");
    let mut w = db.writer();
    w.upsert_bm25_file(&sibling, &content, &postings).unwrap();
    w.commit().unwrap();
    db.recompute_bm25_stats().unwrap();
    assert_eq!(db.view().bm25_stats(), Some((4, 8)));
    assert_eq!(
        db.code_bm25_postings.iter().count(),
        3,
        "one content, one set of postings"
    );

    let candidates = db.bm25_posting_candidates(&["src/foo.rs".to_string()]).unwrap();
    let mut w = db.writer();
    w.remove_bm25_file(&rel).unwrap();
    w.commit().unwrap();
    db.retire_bm25_postings(candidates).unwrap();
    db.recompute_bm25_stats().unwrap();
    assert_eq!(db.view().bm25_stats(), Some((2, 4)));
    assert_eq!(
        db.code_bm25_postings.iter().count(),
        3,
        "the sibling path still holds the content, so its postings stay"
    );
    assert_eq!(db.code_bm25_by_path.iter().count(), 2);

    let candidates = db.bm25_posting_candidates(&["src/sibling.rs".to_string()]).unwrap();
    let mut w = db.writer();
    w.remove_bm25_file(&sibling).unwrap();
    w.commit().unwrap();
    assert_eq!(
        db.bm25_live().unwrap().len(),
        0,
        "the commit invalidated the cached live keys"
    );
    db.retire_bm25_postings(candidates).unwrap();
    assert!(db.code_bm25_postings.iter().next().is_none());
    assert!(db.code_bm25_by_path.iter().next().is_none());
}

/// The probe rows are written per importer, found by the probed path or by one of its ancestor
/// directories, and removed together with the importer's resolved edges (the call a re-resolve or a
/// removal makes) -- with no row left in either keyspace and none touched for a sibling importer.
#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
#[test]
fn import_probe_rows_roundtrip_and_leave_with_the_importer() {
    let (_d, db) = fresh_db();
    let app = RelPath::from("src/app.ts");
    let other = RelPath::from("src/other.ts");
    let (impl_ts, util_ts) = (RelPath::from("src/impl.ts"), RelPath::from("src/util.ts"));
    let probes_app = [keys::probe_hash("src/impl.ts"), keys::probe_hash("src/util")];
    let probes_other = [keys::probe_hash("src/impl.ts")];

    let mut w = db.writer();
    w.upsert_import_probes(&app, &[(7, impl_ts.clone(), 40), (31, util_ts, 2)], &probes_app);
    w.upsert_import_probes(&other, &[(3, impl_ts.clone(), 40)], &probes_other);
    w.commit().unwrap();

    let mut targets = db.view().import_targets_of(&app);
    targets.sort();
    assert_eq!(
        targets.len(),
        2,
        "the manifest row must not read as a binding: {targets:?}"
    );
    assert_eq!(targets[0], (7, impl_ts, 40));

    let mut probing = db.view().importers_probing("src/impl.ts");
    probing.sort();
    assert_eq!(probing, vec![app.clone(), other.clone()]);
    assert_eq!(
        db.view().importers_probing("src/util/index.ts"),
        vec![app.clone()],
        "adding a file under a directory the importer saw missing must reach it through the directory"
    );
    assert!(db.view().is_probed("src/util/index.ts"));
    assert!(!db.view().is_probed("docs/readme.md"));

    let mut w = db.writer();
    w.remove_resolved_file(&app).unwrap();
    w.commit().unwrap();

    assert!(db.view().import_targets_of(&app).is_empty());
    assert_eq!(db.view().importers_probing("src/impl.ts"), vec![other.clone()]);
    assert!(!db.view().is_probed("src/util"));
    assert_eq!(
        db.view().import_targets_of(&other).len(),
        1,
        "a sibling importer's rows must survive"
    );
    assert_eq!(db.importers_by_probe.iter().count(), 1);
}

#[test]
fn import_probes_are_incomplete_until_stamped() {
    let (_d, db) = fresh_db();
    assert!(
        !db.view().import_probes_complete(),
        "a fresh index has no probe rows to trust"
    );
    db.set_import_probes_complete(true).unwrap();
    assert!(db.view().import_probes_complete());
    db.set_import_probes_complete(false).unwrap();
    assert!(!db.view().import_probes_complete());
}

#[cfg(feature = "code-search")]
#[test]
fn full_scan_restores_missing_legacy_bm25_postings() {
    use crate::scanner::{EmbedMode, ScanSource, scan};
    use crate::store::{Store, VIEW_WORKING};

    crate::store::init_isolated_cache();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("lib.rs"), "pub fn legacy_recovery() {}\n").unwrap();
    let mut config = crate::config::default_for_root(root);
    config.code_search.enabled = true;
    config.code_search.embed = false;
    config.documents.enabled = false;
    let mut store = Store::open(root, VIEW_WORKING).unwrap();
    scan(root, &mut store, &config, ScanSource::WorkingTree, EmbedMode::Inline).unwrap();

    let db = store.index_db.as_ref().unwrap();
    let (key, value) = db.code_bm25_postings.iter().next().unwrap().unwrap();
    db.code_bm25_postings.remove(&key).unwrap();
    assert!(db.code_bm25_postings.get(&key).unwrap().is_none());
    store.index.embed_policy = "legacy-embed-policy".into();

    scan(root, &mut store, &config, ScanSource::WorkingTree, EmbedMode::Inline).unwrap();
    let db = store.index_db.as_ref().unwrap();
    assert_eq!(db.code_bm25_postings.get(&key).unwrap(), Some(value));
}
