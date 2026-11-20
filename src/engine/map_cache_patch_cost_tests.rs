//! What a one-file delta over a 100k-file map costs next to rebuilding the map: the delta rebuilds
//! one page of the file view and one of the term index, allocates one entry in each and shares the
//! document paths, so its cost does not grow with the map.

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::MapCache;
use crate::extract::{FileMapL1, Symbol, SymbolKind};
use crate::path::RelPath;
use crate::store::{FileEntry, Store, VIEW_WORKING};

const FILES: usize = 100_000;

/// The saved file's new content hash. The blob store is shared by every test in the process, so it
/// names content no other test writes.
const SAVED_HASH: &str = "279e279e279e279e279e279e279e279e279e279e279e279e279e279e279e279e";

fn outline(name: &str) -> FileMapL1 {
    FileMapL1 {
        schema_ver: crate::extract::SCHEMA_VER,
        language: "rust".into(),
        size_bytes: 0,
        had_errors: false,
        error_count: 0,
        symbols: vec![Symbol {
            name: name.to_string(),
            kind: SymbolKind::Function,
            start_byte: 0,
            end_byte: 0,
            start_row: 0,
            start_col: 0,
            signature: None,
            decorators: Vec::new(),
        }],
        imports: Vec::new(),
        implementations: Vec::new(),
        rationale: Vec::new(),
        extract_epoch: crate::extract::EXTRACT_EPOCH,
    }
}

fn path_of(n: usize) -> RelPath {
    RelPath::from(format!("src/m{:03}/f{n:06}.rs", n % 100).as_str())
}

fn corpus() -> Vec<(RelPath, FileMapL1)> {
    (0..FILES)
        .map(|n| (path_of(n), outline(&format!("patch_cost_{n}"))))
        .collect()
}

/// A warm map over the corpus, and a store whose index names one file with new content, as the
/// watcher batch of one saved file leaves it.
struct Saved {
    store: Store,
    cache: MapCache,
    saved: RelPath,
    /// Dropped after the store that lives in it.
    _dir: tempfile::TempDir,
}

fn one_saved_file() -> Saved {
    crate::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let mut store = Store::open(dir.path(), VIEW_WORKING).expect("store");
    let cache = MapCache::from_synthetic(corpus());
    cache.terms();
    let saved = path_of(FILES / 2);
    let content = outline("patch_cost_saved");
    store
        .write_filemap_hex(SAVED_HASH, &content, None)
        .expect("write the saved file's blob");
    // The synthetic map reads no blobs: admit the saved outline as its decode would.
    cache.l1_cache().seed(SAVED_HASH, Arc::new(content));
    store.index.files.insert(
        saved.clone(),
        FileEntry {
            hash_hex: SAVED_HASH.to_string(),
            language: "rust".to_string(),
            size_bytes: 0,
            mtime: 0,
            extract_epoch: crate::extract::EXTRACT_EPOCH,
            content_key: None,
            lines: 0,
        },
    );
    Saved {
        store,
        cache,
        saved,
        _dir: dir,
    }
}

#[test]
fn a_one_file_delta_over_100k_files_rebuilds_one_page_of_each_structure() {
    let fixture = one_saved_file();
    let next = fixture
        .cache
        .with_delta(&fixture.store, std::slice::from_ref(&fixture.saved), &[]);

    assert_eq!(next.len(), FILES);
    assert_eq!(
        next.get(&fixture.saved).map(|l1| l1.symbols[0].name.clone()),
        Some("patch_cost_saved".to_string())
    );
    assert_eq!(
        next.files.unshared_with(&fixture.cache.files),
        (1, 1),
        "file view: pages and entries the delta allocated"
    );
    assert_eq!(
        next.terms.get().map(|terms| terms.unshared_with(fixture.cache.terms())),
        Some((1, 1)),
        "term index: patched by the delta, with the pages and entries it allocated"
    );
    assert!(
        Arc::ptr_eq(&next.doc_paths, &fixture.cache.doc_paths),
        "a delta naming no document shares the document paths"
    );
}

/// A one-file delta against a rebuild of the file view and term index from already-decoded
/// outlines, a lower bound of what [`MapCache::build`] plus its term warm cost. Run with
/// `cargo test --release --lib map_cache_patch_cost -- --ignored --nocapture`.
#[test]
#[ignore = "benchmark: prints a one-file delta's time against a rebuild's"]
fn bench_one_file_delta_against_a_rebuild() {
    const DELTA_ROUNDS: u32 = 200;
    const REBUILD_ROUNDS: u32 = 3;
    let fixture = one_saved_file();
    let updated = std::slice::from_ref(&fixture.saved);

    let started = Instant::now();
    for _ in 0..DELTA_ROUNDS {
        std::hint::black_box(fixture.cache.with_delta(&fixture.store, updated, &[]));
    }
    let delta = started.elapsed() / DELTA_ROUNDS;

    let mut rebuild = Duration::ZERO;
    for _ in 0..REBUILD_ROUNDS {
        let files = corpus();
        let started = Instant::now();
        let rebuilt = MapCache::from_synthetic(files);
        rebuilt.terms();
        rebuild += started.elapsed();
        std::hint::black_box(rebuilt);
    }
    let rebuild = rebuild / REBUILD_ROUNDS;

    println!("files={FILES} one_file_delta={delta:?} rebuild={rebuild:?}");
    assert!(
        delta < Duration::from_millis(1),
        "a one-file delta over {FILES} files must run well under 1 ms, took {delta:?}"
    );
    assert!(delta < rebuild, "the delta {delta:?} must beat the rebuild {rebuild:?}");
}
