//! The byte accounting the scanner's write batching depends on.
//!
//! `IndexWriter` holds every staged key and value in RAM until the batch commits, so
//! `staged_bytes()` — not the number of files staged — is what bounds a scanner worker's live
//! memory (see `scanner_index_batch`). These assertions pin the accounting itself: what charges
//! the counter, and that a re-upsert's delete tombstones charge it too.

use basemind::extract::{FileMapL1, Implementation, SCHEMA_VER, Symbol, SymbolKind};
use basemind::index::IndexDb;
use basemind::index::keys::{ContentKey, content_key};
use basemind::path::RelPath;

fn content() -> ContentKey {
    content_key("hash", "rust", basemind::extract::EXTRACT_EPOCH)
}

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
        extract_epoch: basemind::extract::EXTRACT_EPOCH,
    }
}

/// The path-keyed rows (implementations) are replaced on a re-upsert; the content rows (symbols) are
/// replaced when the extraction differs, and left alone when it is the same.
#[test]
fn staged_bytes_counts_inserts_and_deletes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = IndexDb::open(dir.path()).expect("open index");
    let rel = RelPath::from("src/a.rs");
    let mut l1 = synthetic_l1(&["alpha", "beta"]);
    l1.implementations = vec![Implementation {
        trait_name: "Display".to_string(),
        impl_type: "Alpha".to_string(),
        start_byte: 0,
        start_row: 0,
        start_col: 0,
    }];

    let mut writer = db.writer();
    assert_eq!(writer.staged_bytes(), 0);
    writer.upsert_file(&rel, &content(), &l1, None).unwrap();
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
    writer.upsert_file(&rel, &content(), &l1, None).unwrap();
    assert!(
        writer.staged_bytes() < inserts_only,
        "the same extraction again stages only the path-keyed rows"
    );

    let mut changed = synthetic_l1(&["gamma", "delta"]);
    changed.implementations = l1.implementations.clone();
    let mut writer = db.writer();
    writer.upsert_file(&rel, &content(), &changed, None).unwrap();
    assert!(
        writer.staged_bytes() > inserts_only,
        "a changed extraction stages the previous entries' deletes on top of the fresh inserts"
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
        .upsert_file(
            &RelPath::from("src/small.rs"),
            &content(),
            &synthetic_l1(&all[..4]),
            None,
        )
        .unwrap();
    let small_bytes = writer.staged_bytes();

    let mut writer = db.writer();
    writer
        .upsert_file(&RelPath::from("src/large.rs"), &content(), &synthetic_l1(&all), None)
        .unwrap();
    let large_bytes = writer.staged_bytes();

    assert!(
        large_bytes > small_bytes * 8,
        "16x the symbols must stage far more bytes ({small_bytes} -> {large_bytes})"
    );
}

/// The accounting has to model fjall's *per-entry* cost, not just the payload. Each staged entry is
/// a 64-byte `batch::Item` in the write batch's `Vec` before one byte of key or value is charged,
/// and that fixed cost dominates the entries the scanner stages most of — the key-only secondary
/// indexes and the 8-byte BM25 posting values. Counting payload alone made the whole byte budget
/// mean 2-3x more resident memory than its name said.
///
/// Each symbol stages exactly two entries (`symbols_by_path` + `symbols_by_name`), so with short
/// names and no signature — payloads far below the two `Item` slots — the counter must still be at
/// least `2 x 64` per symbol. A payload-only counter is well under that.
#[test]
fn staged_bytes_charges_fjalls_per_entry_overhead_not_only_the_payload() {
    const ITEM_BYTES: u64 = 64;
    const SYMBOLS: u64 = 64;

    let dir = tempfile::tempdir().expect("tempdir");
    let db = IndexDb::open(dir.path()).expect("open index");

    let mut l1 = synthetic_l1(&[]);
    l1.symbols = (0..SYMBOLS)
        .map(|i| Symbol {
            name: format!("s{i}"),
            kind: SymbolKind::Function,
            start_byte: i as u32 * 8,
            end_byte: i as u32 * 8 + 4,
            start_row: 0,
            start_col: 0,
            signature: None,
            decorators: Vec::new(),
        })
        .collect();

    let mut writer = db.writer();
    writer
        .upsert_file(&RelPath::from("s.rs"), &content(), &l1, None)
        .unwrap();

    let floor = 2 * ITEM_BYTES * SYMBOLS;
    assert!(
        writer.staged_bytes() >= floor,
        "{SYMBOLS} symbols stage {SYMBOLS} x 2 entries, so the counter cannot be below their \
         {floor} bytes of `Item` slots alone (got {})",
        writer.staged_bytes()
    );
}
