use std::fs;

use gitmind::config::ConfigV1;
use gitmind::extract::SymbolKind;
use gitmind::scanner::{FileStatus, scan, scan_paths};
use gitmind::store::Store;
use tempfile::TempDir;

fn fresh_repo() -> (TempDir, ConfigV1) {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = ConfigV1::with_defaults();
    (dir, cfg)
}

#[test]
fn scan_extracts_rust_symbols() {
    let (dir, cfg) = fresh_repo();
    let root = dir.path();

    fs::write(
        root.join("a.rs"),
        b"pub fn alpha() {}\npub struct Beta { x: i32 }\n",
    )
    .unwrap();

    let mut store = Store::open(root, gitmind::store::VIEW_WORKING).unwrap();
    let report = scan(
        root,
        &mut store,
        &cfg,
        gitmind::scanner::ScanSource::WorkingTree,
    )
    .unwrap();
    assert_eq!(report.stats.updated, 1);
    assert_eq!(report.stats.skipped_unchanged, 0);

    let entry = store.lookup("a.rs").expect("a.rs indexed");
    assert_eq!(entry.language, "rust");

    let hits = gitmind::query::search_symbols(&store, "alpha", None).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].symbol.kind, SymbolKind::Function);
    assert_eq!(hits[0].path, "a.rs");

    let hits = gitmind::query::search_symbols(&store, "Beta", Some(SymbolKind::Struct)).unwrap();
    assert_eq!(hits.len(), 1);
}

#[test]
fn rescan_is_idempotent_and_uses_cache() {
    let (dir, cfg) = fresh_repo();
    let root = dir.path();

    fs::write(root.join("a.rs"), b"pub fn alpha() {}\n").unwrap();

    let mut store = Store::open(root, gitmind::store::VIEW_WORKING).unwrap();
    let first = scan(
        root,
        &mut store,
        &cfg,
        gitmind::scanner::ScanSource::WorkingTree,
    )
    .unwrap();
    assert_eq!(first.stats.updated, 1);
    drop(store);

    let mut store = Store::open(root, gitmind::store::VIEW_WORKING).unwrap();
    let second = scan(
        root,
        &mut store,
        &cfg,
        gitmind::scanner::ScanSource::WorkingTree,
    )
    .unwrap();
    assert_eq!(second.stats.updated, 0);
    assert_eq!(second.stats.skipped_unchanged, 1);
}

#[test]
fn modifying_a_file_triggers_reextract() {
    let (dir, cfg) = fresh_repo();
    let root = dir.path();

    fs::write(root.join("a.rs"), b"pub fn alpha() {}\n").unwrap();
    {
        let mut store = Store::open(root, gitmind::store::VIEW_WORKING).unwrap();
        scan(
            root,
            &mut store,
            &cfg,
            gitmind::scanner::ScanSource::WorkingTree,
        )
        .unwrap();
    }
    fs::write(root.join("a.rs"), b"pub fn gamma() {}\n").unwrap();
    {
        let mut store = Store::open(root, gitmind::store::VIEW_WORKING).unwrap();
        let s = scan(
            root,
            &mut store,
            &cfg,
            gitmind::scanner::ScanSource::WorkingTree,
        )
        .unwrap();
        assert_eq!(s.stats.updated, 1);
        let hits = gitmind::query::search_symbols(&store, "gamma", None).unwrap();
        assert_eq!(hits.len(), 1);
        let hits = gitmind::query::search_symbols(&store, "alpha", None).unwrap();
        assert!(hits.is_empty(), "old symbol should be gone");
    }
}

#[test]
fn removed_files_get_purged_from_index() {
    let (dir, cfg) = fresh_repo();
    let root = dir.path();

    fs::write(root.join("a.rs"), b"pub fn alpha() {}\n").unwrap();
    fs::write(root.join("b.rs"), b"pub fn beta() {}\n").unwrap();
    {
        let mut store = Store::open(root, gitmind::store::VIEW_WORKING).unwrap();
        scan(
            root,
            &mut store,
            &cfg,
            gitmind::scanner::ScanSource::WorkingTree,
        )
        .unwrap();
    }
    fs::remove_file(root.join("b.rs")).unwrap();
    {
        let mut store = Store::open(root, gitmind::store::VIEW_WORKING).unwrap();
        let s = scan(
            root,
            &mut store,
            &cfg,
            gitmind::scanner::ScanSource::WorkingTree,
        )
        .unwrap();
        assert_eq!(s.stats.removed, 1);
        assert!(store.lookup("b.rs").is_none());
        assert!(store.lookup("a.rs").is_some());
    }
}

#[test]
fn skips_large_files() {
    let (dir, mut cfg) = fresh_repo();
    cfg.scan.max_file_bytes = 1024;
    let root = dir.path();

    let big = vec![b'x'; 4096];
    fs::write(root.join("big.rs"), &big).unwrap();

    let mut store = Store::open(root, gitmind::store::VIEW_WORKING).unwrap();
    let s = scan(
        root,
        &mut store,
        &cfg,
        gitmind::scanner::ScanSource::WorkingTree,
    )
    .unwrap();
    assert_eq!(s.stats.skipped_too_large, 1);
    assert!(store.lookup("big.rs").is_none());
}

#[test]
fn ignores_unknown_languages() {
    let (dir, cfg) = fresh_repo();
    let root = dir.path();
    fs::write(root.join("weird.xyz"), b"data").unwrap();

    let mut store = Store::open(root, gitmind::store::VIEW_WORKING).unwrap();
    let s = scan(
        root,
        &mut store,
        &cfg,
        gitmind::scanner::ScanSource::WorkingTree,
    )
    .unwrap();
    // Globset default doesn't include *.xyz so it isn't even a candidate.
    assert_eq!(s.stats.scanned, 0);
}

#[test]
fn extracts_python() {
    let (dir, cfg) = fresh_repo();
    let root = dir.path();
    fs::write(
        root.join("m.py"),
        b"import os\n\ndef foo(x):\n    return x\n\nclass Bar:\n    pass\n",
    )
    .unwrap();

    let mut store = Store::open(root, gitmind::store::VIEW_WORKING).unwrap();
    scan(
        root,
        &mut store,
        &cfg,
        gitmind::scanner::ScanSource::WorkingTree,
    )
    .unwrap();

    let outline = gitmind::query::file_outline(&store, "m.py").unwrap();
    assert_eq!(outline.language, "python");
    let names: Vec<&str> = outline.symbols.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"foo"));
    assert!(names.contains(&"Bar"));
    assert!(!outline.imports.is_empty());
}

#[test]
fn store_lock_prevents_concurrent_open() {
    let (dir, _cfg) = fresh_repo();
    let root = dir.path();
    let first = Store::open(root, gitmind::store::VIEW_WORKING).unwrap();
    let err = Store::open(root, gitmind::store::VIEW_WORKING)
        .err()
        .expect("second open must fail");
    assert!(matches!(err, gitmind::store::StoreError::Locked(_)));
    drop(first);
    // After dropping, open succeeds again.
    Store::open(root, gitmind::store::VIEW_WORKING).unwrap();
}

#[test]
fn scan_flags_files_with_syntax_errors() {
    let (dir, cfg) = fresh_repo();
    let root = dir.path();
    // Broken `fn x( {` plus a well-formed neighbor.
    fs::write(
        root.join("broken.rs"),
        b"pub fn ok_one() {}\n\npub fn broken( {\n    let x = ;\n}\n",
    )
    .unwrap();

    let mut store = Store::open(root, gitmind::store::VIEW_WORKING).unwrap();
    let report = scan(
        root,
        &mut store,
        &cfg,
        gitmind::scanner::ScanSource::WorkingTree,
    )
    .unwrap();
    assert_eq!(report.stats.updated, 1);
    assert_eq!(
        report.stats.updated_with_warnings, 1,
        "should flag the file as having parse errors"
    );

    let row = report
        .results
        .iter()
        .find(|r| r.path == "broken.rs")
        .expect("broken.rs in report");
    match &row.status {
        FileStatus::Updated {
            had_errors,
            error_count,
        } => {
            assert!(had_errors, "had_errors should be true");
            assert!(*error_count > 0, "error_count should be > 0");
        }
        other => panic!("expected Updated, got {other:?}"),
    }

    // Recovered symbols are still queryable.
    let outline = gitmind::query::file_outline(&store, "broken.rs").unwrap();
    assert!(outline.had_errors);
    let names: Vec<&str> = outline.symbols.iter().map(|s| s.name.as_str()).collect();
    assert!(
        names.contains(&"ok_one"),
        "well-formed sibling should still be extracted; got {names:?}"
    );
}

#[test]
fn scan_paths_only_touches_listed_files() {
    let (dir, cfg) = fresh_repo();
    let root = dir.path();
    fs::write(root.join("a.rs"), b"pub fn a() {}\n").unwrap();
    fs::write(root.join("b.rs"), b"pub fn b() {}\n").unwrap();
    fs::write(root.join("c.rs"), b"pub fn c() {}\n").unwrap();

    let mut store = Store::open(root, gitmind::store::VIEW_WORKING).unwrap();
    scan(
        root,
        &mut store,
        &cfg,
        gitmind::scanner::ScanSource::WorkingTree,
    )
    .unwrap();

    let hash_b_before = store.lookup("b.rs").unwrap().hash_hex.clone();
    let hash_c_before = store.lookup("c.rs").unwrap().hash_hex.clone();

    // Mutate a.rs only.
    fs::write(root.join("a.rs"), b"pub fn a_changed() {}\n").unwrap();

    let report = scan_paths(root, &mut store, &cfg, &[root.join("a.rs")]).unwrap();
    assert_eq!(report.stats.scanned, 1, "scan_paths visited only one file");
    assert_eq!(report.stats.updated, 1);

    // The unchanged files keep their original hashes.
    assert_eq!(store.lookup("b.rs").unwrap().hash_hex, hash_b_before);
    assert_eq!(store.lookup("c.rs").unwrap().hash_hex, hash_c_before);

    // The mutated file's symbol has changed.
    let hits = gitmind::query::search_symbols(&store, "a_changed", None).unwrap();
    assert_eq!(hits.len(), 1);
}

#[test]
fn scan_paths_purges_removed_files() {
    let (dir, cfg) = fresh_repo();
    let root = dir.path();
    fs::write(root.join("a.rs"), b"pub fn a() {}\n").unwrap();

    let mut store = Store::open(root, gitmind::store::VIEW_WORKING).unwrap();
    scan(
        root,
        &mut store,
        &cfg,
        gitmind::scanner::ScanSource::WorkingTree,
    )
    .unwrap();
    assert!(store.lookup("a.rs").is_some());

    fs::remove_file(root.join("a.rs")).unwrap();
    let report = scan_paths(root, &mut store, &cfg, &[root.join("a.rs")]).unwrap();
    assert_eq!(report.stats.removed, 1);
    assert!(store.lookup("a.rs").is_none());
}
