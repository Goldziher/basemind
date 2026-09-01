//! Paired unit tests for the bounded read stack: that the byte budget is actually enforced, and
//! that enforcing it does not change what a whole-corpus stream yields.
//!
//! `tests/map_cache_equivalence.rs` proves the tool surface is byte-identical bounded vs unbounded.
//! That proof is only meaningful if the bounded arm really evicted — otherwise both arms are
//! all-hits and the comparison is vacuous. These tests pin the missing half: eviction happens, and
//! the stream still visits every file with the same content.

use std::collections::BTreeMap;

use super::MapCache;
use crate::path::RelPath;
use crate::store::{Store, VIEW_WORKING};

/// One mebibyte — the smallest budget `[resources] max_map_cache_mb` can express, and the one the
/// equivalence test's bounded arm runs under.
const ONE_MIB: u64 = 1024 * 1024;

/// Scan a generated corpus whose decoded outlines total well over [`ONE_MIB`].
fn scan_oversized_corpus(root: &std::path::Path) -> Store {
    for f in 0..40u32 {
        let mut src = String::new();
        for i in 0..150u32 {
            src.push_str(&format!(
                "pub fn module_{f:03}_operation_{i:03}_with_a_deliberately_long_name(a: u64, b: &str) -> u64 {{ let _ = b; a }}\n"
            ));
        }
        // A trait + impl per file so the read-only `implementations` projection has something to
        // charge; without them that projection is empty and its cap is untestable.
        src.push_str(&format!(
            "pub trait Module{f:03}Drawable {{ fn draw(&self); }}\n\
             pub struct Module{f:03}Widget;\n\
             impl Module{f:03}Drawable for Module{f:03}Widget {{ fn draw(&self) {{}} }}\n"
        ));
        std::fs::write(root.join(format!("module_{f:03}.rs")), src).unwrap();
    }
    let cfg = crate::config::ConfigV1::with_defaults();
    let mut store = Store::open(root, VIEW_WORKING).expect("open store");
    crate::scanner::scan(
        root,
        &mut store,
        &cfg,
        crate::scanner::ScanSource::WorkingTree,
        crate::scanner::EmbedMode::Inline,
    )
    .expect("scan");
    store
}

/// Collect the whole corpus the way a tool does — stream and project — into a comparable shape.
fn stream_symbol_names(cache: &MapCache) -> BTreeMap<RelPath, Vec<String>> {
    let mut out: BTreeMap<RelPath, Vec<String>> = BTreeMap::new();
    cache.for_each(|path, l1| {
        out.insert(path.clone(), l1.symbols.iter().map(|s| s.name.clone()).collect());
    });
    out
}

/// The budget must bind (charge stays under it, so the LRU really evicted) AND the stream must
/// still yield every file with byte-identical symbol content. Those two together are what make the
/// LRU a latency trade rather than a correctness one.
#[test]
fn map_cache_budget_is_enforced_and_streams_the_whole_corpus() {
    let tmp = tempfile::tempdir().unwrap();
    let store = scan_oversized_corpus(tmp.path());

    let unbounded = MapCache::build(&store, 0);
    let bounded = MapCache::build(&store, ONE_MIB);

    assert_eq!(unbounded.len(), bounded.len(), "same file view either way");

    let from_unbounded = stream_symbol_names(&unbounded);
    let from_bounded = stream_symbol_names(&bounded);
    assert_eq!(
        from_unbounded, from_bounded,
        "a bounded cache must stream the same corpus content as an unbounded one"
    );

    assert!(
        unbounded.l1_cache().charged_bytes() > ONE_MIB,
        "the fixture must exceed the budget, or the bounded arm never evicts (charged={})",
        unbounded.l1_cache().charged_bytes()
    );
    assert!(
        bounded.l1_cache().charged_bytes() <= ONE_MIB,
        "the byte budget must bind (charged={})",
        bounded.l1_cache().charged_bytes()
    );
    // A second pass is where the two diverge: the unbounded cache is fully warm and reads nothing,
    // the bounded one must fault every evicted outline back in.
    let (unbounded_before, bounded_before) = (unbounded.l1_cache().misses(), bounded.l1_cache().misses());
    stream_symbol_names(&unbounded);
    stream_symbol_names(&bounded);
    assert_eq!(
        unbounded.l1_cache().misses(),
        unbounded_before,
        "a warm unbounded cache re-reads nothing"
    );
    assert!(
        bounded.l1_cache().misses() > bounded_before,
        "a bounded cache re-reads the outlines it evicted"
    );
}

/// A point lookup must resolve identically whether the entry is resident or has to be faulted in —
/// including for a file the streaming pass already evicted.
#[test]
fn point_lookup_after_eviction_returns_the_same_outline() {
    let tmp = tempfile::tempdir().unwrap();
    let store = scan_oversized_corpus(tmp.path());
    let cache = MapCache::build(&store, ONE_MIB);

    let first = RelPath::from("module_000.rs");
    let before = cache.get(&first).expect("first file resolves");
    // Stream the whole corpus, which under a 1 MiB budget evicts the early files.
    cache.for_each(|_, _| {});
    let after = cache.get(&first).expect("first file still resolves after eviction");

    assert_eq!(*before, *after, "an evicted outline re-reads to the same value");
    assert!(cache.contains(&first), "the file view is unaffected by eviction");
}

/// The unbounded sentinel must genuinely disable eviction, so `max_map_cache_mb = 0` really is the
/// pre-split behaviour the equivalence test compares against.
#[test]
fn zero_budget_keeps_every_outline_resident() {
    let tmp = tempfile::tempdir().unwrap();
    let store = scan_oversized_corpus(tmp.path());
    let cache = MapCache::build(&store, 0);

    cache.for_each(|_, _| {});
    let after_first_pass = cache.l1_cache().misses();
    cache.for_each(|_, _| {});
    assert_eq!(
        cache.l1_cache().misses(),
        after_first_pass,
        "with no budget a second whole-corpus pass reads no blob at all"
    );
}

/// A session with no Fjall index (the normal `daemon_writer` front-end topology) answers reference
/// and implementation queries from in-RAM projections of the blobs. Those projections are O(corpus),
/// so they are charged against the same budget — and when the budget truncates them the cache says
/// so, rather than letting a partial answer read as "no matches".
#[test]
fn read_only_projections_are_capped_and_report_it() {
    let tmp = tempfile::tempdir().unwrap();
    drop(scan_oversized_corpus(tmp.path()));

    let store = Store::open_read_only_no_index(tmp.path(), VIEW_WORKING).expect("read-only store");
    assert!(store.index_db.is_none(), "the read-only opener must not open an index");

    let tight = MapCache::build(&store, 2048);
    assert!(tight.calls.is_some() && tight.impls.is_some(), "projections are built");
    assert!(
        tight.projections_capped,
        "a 4 KiB budget cannot hold the projections for this corpus"
    );

    let roomy = MapCache::build(&store, 0);
    assert!(
        !roomy.projections_capped,
        "an unbounded budget must never report truncation"
    );
}
