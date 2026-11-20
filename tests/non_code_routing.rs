//! Tier routing: only code is code-mapped. Prose, data and config files (markdown, json, yaml,
//! toml, ...) belong to the document tier, and a path that changes tier (an index built before
//! this rule, or a file that stops being code) is moved on the next scan, full or incremental.
//!
//! Plain `#[test]` only — `scanner::scan` is synchronous and opening LanceDB inside a tokio
//! runtime panics.

#![cfg(feature = "documents")]

use std::fs;
use std::path::Path;

use basemind::config::ConfigV1;
use basemind::scanner::{EmbedMode, ScanSource, scan, scan_paths};
use basemind::store::{DocEntry, FileEntry, Store};
use tempfile::TempDir;

const NON_CODE: &[&str] = &[
    "README.md",
    "cfg.json",
    "settings.yaml",
    "Cargo.toml",
    "data.csv",
    "layout.xml",
    "app.log",
];

/// Valid content per format, salted with `salt` (the fixture root) so the process-shared
/// content-addressed blob cache never serves another test's blob.
fn body(name: &str, salt: &str) -> String {
    match name {
        "README.md" => format!("# Title\n\nSome prose about the project and how it works ({salt}).\n"),
        "cfg.json" => format!("{{\"name\": \"demo\", \"salt\": \"{salt}\", \"flags\": [\"a\", \"b\"]}}\n"),
        "settings.yaml" => format!("name: demo\nsalt: \"{salt}\"\nitems:\n  - alpha\n  - beta\n"),
        "Cargo.toml" => format!("[package]\nname = \"demo\"\nsalt = \"{salt}\"\nversion = \"0.1.0\"\n"),
        "data.csv" => format!("id,name\n1,alpha\n2,{salt}\n"),
        "layout.xml" => format!("<root><item>alpha</item><item>{salt}</item></root>\n"),
        "app.log" => format!("2026-11-17 10:00:00 INFO started\n2026-11-17 10:00:01 WARN {salt}\n"),
        other => unreachable!("{other}"),
    }
}

fn fixture() -> (TempDir, ConfigV1) {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let _ = basemind::testing::git_command()
        .args(["init", "-q"])
        .current_dir(root)
        .status();
    fs::write(root.join("a.rs"), "pub fn alpha() -> u32 {\n    1\n}\n").unwrap();
    let salt = root.display().to_string();
    for name in NON_CODE {
        fs::write(root.join(name), body(name, &salt)).unwrap();
    }
    let mut cfg = ConfigV1::with_defaults();
    cfg.documents.enabled = true;
    cfg.documents.embed = false;
    (dir, cfg)
}

fn full_scan(root: &Path, store: &mut Store, cfg: &ConfigV1) {
    scan(
        root,
        store,
        cfg,
        ScanSource::WorkingTree,
        EmbedMode::Inline,
        basemind::retrieval_wiring::backend(),
    )
    .expect("scan");
}

fn code_entry(language: &str) -> FileEntry {
    FileEntry {
        hash_hex: "0".repeat(64),
        language: language.to_string(),
        size_bytes: 1,
        mtime: 0,
        extract_epoch: basemind::extract::EXTRACT_EPOCH,
        content_key: None,
        lines: 0,
    }
}

#[test]
fn prose_data_and_config_files_land_in_the_document_tier() {
    let (dir, cfg) = fixture();
    let root = dir.path();
    let mut store = Store::open(root, basemind::store::VIEW_WORKING).unwrap();
    full_scan(root, &mut store, &cfg);

    assert_eq!(store.lookup("a.rs").map(|e| e.language.as_str()), Some("rust"));
    assert!(store.lookup_doc("a.rs").is_none(), "code is not a document");
    for name in NON_CODE {
        assert!(store.lookup(name).is_none(), "{name} must not be code-mapped");
        assert!(store.lookup_doc(name).is_some(), "{name} must be in doc_files");
    }
}

/// Config and data files are not only tracked: their rows are in the group's document store, every
/// chunk cites its lines, and the lexical lane finds them (`docs_search` with embeddings off).
#[test]
fn data_and_config_files_are_searchable_documents_that_cite_their_lines() {
    let (dir, cfg) = fixture();
    let root = dir.path();
    let mut store = Store::open(root, basemind::store::VIEW_WORKING).unwrap();
    full_scan(root, &mut store, &cfg);

    // The `balanced` preset's dimension.
    let dim = 768;
    let docs = basemind::lance::docs::DocStore::shared_existing(
        &basemind::lance::docs::group_store_dir(root, &cfg.documents.embedding_preset, dim),
        dim,
        &cfg.documents.embedding_preset,
    )
    .expect("open")
    .expect("the scan wrote the store");
    for name in ["cfg.json", "settings.yaml", "Cargo.toml", "data.csv", "app.log"] {
        let entry = store.lookup_doc(name).expect(name);
        let key = basemind::store::doc_key(&entry.hash_hex, &entry.config_digest);
        let chunks = docs.chunks(&[(key, vec![0])]).expect("read");
        assert_eq!(chunks.len(), 1, "{name} has rows");
        assert_eq!(chunks[0].line_start, Some(1), "{name} cites its lines");
        assert!(chunks[0].line_end.is_some(), "{name}");
    }
    let hits = docs
        .lexical("WARN", &basemind::lance::docs::DocPredicate::default(), 10, false)
        .expect("lexical lane");
    assert!(hits.iter().any(|hit| hit.text.contains("WARN")), "the log is found");
}

#[test]
fn full_scan_moves_legacy_code_mapped_entries_to_documents() {
    let (dir, cfg) = fixture();
    let root = dir.path();
    let mut store = Store::open(root, basemind::store::VIEW_WORKING).unwrap();
    store.upsert("README.md", code_entry("markdown"));
    store.upsert("cfg.json", code_entry("json"));

    full_scan(root, &mut store, &cfg);

    for name in ["README.md", "cfg.json"] {
        assert!(store.lookup(name).is_none(), "{name} purged from the code map");
        assert!(store.lookup_doc(name).is_some(), "{name} re-indexed as a document");
    }
    assert!(store.lookup("a.rs").is_some());
}

#[test]
fn incremental_scan_moves_legacy_code_mapped_entries_to_documents() {
    let (dir, cfg) = fixture();
    let root = dir.path();
    let mut store = Store::open(root, basemind::store::VIEW_WORKING).unwrap();
    store.upsert("README.md", code_entry("markdown"));

    scan_paths(
        root,
        &mut store,
        &cfg,
        &[root.join("README.md")],
        EmbedMode::Inline,
        basemind::retrieval_wiring::backend(),
    )
    .unwrap();

    assert!(store.lookup("README.md").is_none(), "purged from the code map");
    assert!(store.lookup_doc("README.md").is_some(), "indexed as a document");
}

#[test]
fn a_document_that_becomes_code_leaves_the_document_tier() {
    let (dir, cfg) = fixture();
    let root = dir.path();
    let mut store = Store::open(root, basemind::store::VIEW_WORKING).unwrap();
    store.upsert_doc(
        "a.rs",
        DocEntry {
            hash_hex: "1".repeat(64),
            embedding_preset: cfg.documents.embedding_preset.clone(),
            size_bytes: 1,
            mtime: 0,
            embedded: false,
            embed_attempted: false,
            config_digest: String::new(),
        },
    );

    scan_paths(
        root,
        &mut store,
        &cfg,
        &[root.join("a.rs")],
        EmbedMode::Inline,
        basemind::retrieval_wiring::backend(),
    )
    .unwrap();

    assert!(store.lookup_doc("a.rs").is_none(), "no longer a document");
    assert!(store.lookup("a.rs").is_some(), "code-mapped");
}

#[test]
fn documents_exclude_still_gates_non_code_files() {
    let (dir, mut cfg) = fixture();
    let root = dir.path();
    cfg.documents.exclude = vec!["*.json".to_string()];
    let mut store = Store::open(root, basemind::store::VIEW_WORKING).unwrap();
    full_scan(root, &mut store, &cfg);

    assert!(
        store.lookup_doc("cfg.json").is_none(),
        "excluded by [documents] exclude"
    );
    assert!(
        store.lookup("cfg.json").is_none(),
        "and never falls back to the code map"
    );
    assert!(store.lookup_doc("README.md").is_some());
}

#[test]
fn a_current_document_does_not_keep_a_stale_code_entry_alive() {
    let (dir, cfg) = fixture();
    let root = dir.path();
    let mut store = Store::open(root, basemind::store::VIEW_WORKING).unwrap();
    full_scan(root, &mut store, &cfg);
    assert!(store.lookup_doc("README.md").is_some());

    // Document is now up to date (`Unchanged` on rescan); a leftover code entry must still go.
    store.upsert("README.md", code_entry("markdown"));
    full_scan(root, &mut store, &cfg);
    assert!(store.lookup("README.md").is_none(), "full scan purges it");

    store.upsert("README.md", code_entry("markdown"));
    scan_paths(
        root,
        &mut store,
        &cfg,
        &[root.join("README.md")],
        EmbedMode::Inline,
        basemind::retrieval_wiring::backend(),
    )
    .unwrap();
    assert!(store.lookup("README.md").is_none(), "incremental scan purges it");
    assert!(store.lookup_doc("README.md").is_some(), "document survives");
}
