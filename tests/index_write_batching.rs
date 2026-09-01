//! The byte accounting the scanner's write batching depends on.
//!
//! `IndexWriter` holds every staged key and value in RAM until the batch commits, so
//! `staged_bytes()` — not the number of files staged — is what bounds a scanner worker's live
//! memory (see `scanner_index_batch`). These assertions pin the accounting itself: what charges
//! the counter, and that a re-upsert's delete tombstones charge it too.

use basemind::extract::{FileMapL1, SCHEMA_VER, Symbol, SymbolKind};
use basemind::index::IndexDb;
use basemind::path::RelPath;

fn synthetic_l1(names: &[&str]) -> FileMapL1 {
    FileMapL1 {
        schema_ver: SCHEMA_VER,
        language: "rust".to_string(),
        size_bytes: 0,
        had_errors: false,
        error_count: 0,
        symbols: names
            .iter()
            .enumerate()
            .map(|(i, name)| Symbol {
                name: (*name).to_string(),
                kind: SymbolKind::Function,
                start_byte: i as u32 * 8,
                end_byte: i as u32 * 8 + 4,
                start_row: 0,
                start_col: 0,
                signature: Some("fn(x: u32) -> u32".to_string()),
                decorators: Vec::new(),
            })
            .collect(),
        imports: Vec::new(),
        implementations: Vec::new(),
        rationale: Vec::new(),
    }
}

#[test]
fn staged_bytes_counts_inserts_and_deletes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = IndexDb::open(dir.path()).expect("open index");
    let rel = RelPath::from("src/a.rs");
    let l1 = synthetic_l1(&["alpha", "beta"]);

    let mut writer = db.writer();
    assert_eq!(writer.staged_bytes(), 0);
    writer.upsert_file(&rel, &l1, None).unwrap();
    let inserts_only = writer.staged_bytes();
    assert!(inserts_only > 0, "staging symbols must charge the byte counter");
    writer.commit().unwrap();

    let mut writer = db.writer();
    writer.remove_file(&rel).unwrap();
    assert!(
        writer.staged_bytes() > 0,
        "delete tombstones hold their keys in the batch too"
    );

    let mut writer = db.writer();
    writer.upsert_file(&rel, &l1, None).unwrap();
    assert!(
        writer.staged_bytes() > inserts_only,
        "a re-upsert stages the previous entries' deletes on top of the fresh inserts"
    );
}

/// A bigger file stages proportionally more bytes — the property the byte budget rests on, and
/// the one a file count cannot see.
#[test]
fn staged_bytes_scales_with_the_file_s_symbol_count() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = IndexDb::open(dir.path()).expect("open index");

    let names: Vec<String> = (0..64).map(|i| format!("sym_{i}")).collect();
    let all: Vec<&str> = names.iter().map(String::as_str).collect();

    let mut writer = db.writer();
    writer
        .upsert_file(&RelPath::from("src/small.rs"), &synthetic_l1(&all[..4]), None)
        .unwrap();
    let small_bytes = writer.staged_bytes();

    let mut writer = db.writer();
    writer
        .upsert_file(&RelPath::from("src/large.rs"), &synthetic_l1(&all), None)
        .unwrap();
    let large_bytes = writer.staged_bytes();

    assert!(
        large_bytes > small_bytes * 8,
        "16x the symbols must stage far more bytes ({small_bytes} -> {large_bytes})"
    );
}
