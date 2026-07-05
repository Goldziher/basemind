//! Coverage for the TSLP `tags.scm` fallback: scans a fixture in a language with no basemind
//! override but with an upstream vendored tags.scm (kotlin, csharp), and asserts that the
//! `adapt_tslp_tags` adapter produces queries that the extractor consumes — symbols and call
//! sites land in the index instead of dropping silently.

use std::fs;

use basemind::config::ConfigV1;
use basemind::scanner::scan;
use basemind::store::Store;
use tempfile::TempDir;

fn fresh_repo() -> (TempDir, ConfigV1) {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = ConfigV1::with_defaults();
    (dir, cfg)
}

fn scan_fixture(name: &str) -> (TempDir, Store) {
    let (dir, cfg) = fresh_repo();
    let root = dir.path();
    let bytes = fs::read(format!("tests/fixtures/lang_fallback/{name}")).expect("fixture exists");
    fs::write(root.join(name), bytes).expect("write fixture");

    let mut store = Store::open(root, basemind::store::VIEW_WORKING).expect("open store");
    let report = scan(
        root,
        &mut store,
        &cfg,
        basemind::scanner::ScanSource::WorkingTree,
        basemind::scanner::EmbedMode::Inline,
    )
    .expect("scan");
    assert_eq!(report.stats.updated, 1, "fixture should be processed");
    assert_eq!(report.stats.skipped_no_lang, 0, "fixture lang must resolve");
    (dir, store)
}

/// Materialize the L2 outline (calls) for `rel` via the live escalation path. Returns the
/// number of call sites whose callee identifier matches `needle` (substring).
fn count_calls(store: &Store, root: &std::path::Path, rel: &str, needle: &str) -> usize {
    let l2 = basemind::query::file_outline_l2(store, rel.as_bytes(), root).expect("file_outline_l2");
    l2.calls.iter().filter(|c| c.callee.contains(needle)).count()
}

#[test]
fn kotlin_fallback_yields_symbols() {
    let (_dir, store) = scan_fixture("sample.kt");
    let entry = store.lookup("sample.kt").expect("indexed");
    assert_eq!(entry.language, "kotlin");

    let hits = basemind::query::search_symbols(&store, "Greeter", None).expect("search");
    assert!(
        !hits.is_empty(),
        "kotlin fallback: expected `Greeter` class symbol from TSLP tags.scm"
    );

    let hello_hits = basemind::query::search_symbols(&store, "hello", None).expect("search");
    assert!(
        !hello_hits.is_empty(),
        "kotlin fallback: expected `hello` function symbol"
    );
}

#[test]
fn kotlin_fallback_yields_calls() {
    let (dir, store) = scan_fixture("sample.kt");
    let matches = count_calls(&store, dir.path(), "sample.kt", "greet");
    assert!(
        matches >= 1,
        "kotlin fallback: expected ≥ 1 call to `greet`, found {matches}"
    );
}

#[test]
fn csharp_fallback_yields_symbols() {
    let (_dir, store) = scan_fixture("sample.cs");
    let entry = store.lookup("sample.cs").expect("indexed");
    assert_eq!(entry.language, "csharp");

    let hits = basemind::query::search_symbols(&store, "Greeter", None).expect("search");
    assert!(
        !hits.is_empty(),
        "csharp fallback: expected `Greeter` class symbol from TSLP tags.scm"
    );

    let hello_hits = basemind::query::search_symbols(&store, "Hello", None).expect("search");
    assert!(
        !hello_hits.is_empty(),
        "csharp fallback: expected `Hello` method symbol"
    );
}
