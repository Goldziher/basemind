//! The registry of shared outline caches: one cache per blob directory, one budget for all of its
//! holders together, and nothing kept once the last holder lets go.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::{Outlines, SharedOutlines};
use crate::engine::MapCache;
use crate::engine::l1_cache::{FileIndexView, L1Cache};
use crate::store::{Store, VIEW_WORKING};

/// One mebibyte, the smallest budget `[resources] max_map_cache_mb` can express.
const ONE_MIB: u64 = 1024 * 1024;

/// Scan forty generated sources whose names start with `tag` into a store over `root`. Their
/// decoded outlines total well over [`ONE_MIB`]; two roots scanned with one tag hold the same
/// content, so the same hashes.
fn scan_corpus(root: &Path, tag: &str) -> Store {
    crate::store::init_isolated_cache();
    for file in 0..40u32 {
        let source: String = (0..150u32)
            .map(|i| {
                format!(
                    "pub fn {tag}_{file:03}_operation_{i:03}_with_a_deliberately_long_name(a: u64) -> u64 {{ a }}\n"
                )
            })
            .collect();
        std::fs::write(root.join(format!("{tag}_{file:03}.rs")), source).expect("write source");
    }
    let config = crate::config::ConfigV1::with_defaults();
    let mut store = Store::open(root, VIEW_WORKING).expect("open store");
    crate::scanner::scan(
        root,
        &mut store,
        &config,
        crate::scanner::ScanSource::WorkingTree,
        crate::scanner::EmbedMode::Inline,
        crate::retrieval_wiring::backend(),
    )
    .expect("scan");
    store
}

/// An outline with nothing in it, to charge a cache with.
fn empty_outline() -> Arc<crate::extract::FileMapL1> {
    Arc::new(crate::extract::FileMapL1 {
        schema_ver: crate::extract::SCHEMA_VER,
        language: "rust".to_string(),
        size_bytes: 1,
        had_errors: false,
        error_count: 0,
        symbols: Vec::new(),
        imports: Vec::new(),
        implementations: Vec::new(),
        rationale: Vec::new(),
        extract_epoch: crate::extract::EXTRACT_EPOCH,
    })
}

/// Two views of identical content read one cache, and the second decodes nothing the first did.
#[test]
fn views_of_identical_content_share_one_cache_and_decode_each_outline_once() {
    let (left, right) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (left_store, right_store) = (scan_corpus(left.path(), "same"), scan_corpus(right.path(), "same"));
    let shared = SharedOutlines::default();

    let left_map = MapCache::build(&left_store, Outlines::shared(&shared, 0));
    let right_map = MapCache::build(&right_store, Outlines::shared(&shared, 0));
    assert!(
        std::ptr::eq(left_map.l1_cache(), right_map.l1_cache()),
        "one cache serves both views"
    );

    left_map.for_each(|_, _| {});
    let decoded = left_map.l1_cache().misses();
    assert_eq!(decoded, left_map.len() as u64, "each outline is decoded once");
    right_map.for_each(|_, _| {});
    assert_eq!(
        right_map.l1_cache().misses(),
        decoded,
        "the second view decodes nothing"
    );
}

/// The budget binds the holders together: two corpora that each exceed it alone stay under it
/// streamed through one shared cache.
#[test]
fn the_shared_budget_bounds_every_holder_together() {
    let (left, right) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (left_store, right_store) = (scan_corpus(left.path(), "left"), scan_corpus(right.path(), "right"));
    for store in [&left_store, &right_store] {
        let alone = MapCache::build(store, 0);
        alone.for_each(|_, _| {});
        assert!(
            alone.l1_cache().charged_bytes() > ONE_MIB,
            "each corpus alone exceeds the budget (charged={})",
            alone.l1_cache().charged_bytes()
        );
    }
    let shared = SharedOutlines::default();

    let left_map = MapCache::build(&left_store, Outlines::shared(&shared, ONE_MIB));
    let right_map = MapCache::build(&right_store, Outlines::shared(&shared, ONE_MIB));
    left_map.for_each(|_, _| {});
    right_map.for_each(|_, _| {});

    let charged = left_map.l1_cache().charged_bytes();
    assert!(charged <= ONE_MIB, "one budget for both holders (charged={charged})");
}

/// A holder configuring a larger budget widens the shared one, `0` (unbounded) widest; a smaller
/// one never shrinks it.
#[test]
fn a_larger_budget_widens_the_shared_cache_and_a_smaller_one_does_not_shrink_it() {
    let shared = SharedOutlines::default();
    let dir = Path::new("blobs");
    let first = shared.cache_for(dir, ONE_MIB);

    for (asked, expected) in [(2 * ONE_MIB, 2 * ONE_MIB), (ONE_MIB, 2 * ONE_MIB), (0, 0), (ONE_MIB, 0)] {
        let cache = shared.cache_for(dir, asked);
        assert!(Arc::ptr_eq(&cache, &first), "one cache over one directory");
        assert_eq!(cache.budget_bytes(), expected, "after asking for {asked}");
    }
}

/// Outlines are only shared over the blob directory they were read from.
#[test]
fn another_blob_directory_gets_a_cache_of_its_own() {
    let shared = SharedOutlines::default();
    let first = shared.cache_for(Path::new("blobs"), 0);
    let other = shared.cache_for(Path::new("other-blobs"), 0);

    assert!(!Arc::ptr_eq(&first, &other));
    assert_eq!(shared.live(), 2);
}

/// The cache outlives a holder that leaves while another still reads it, and goes with the last.
#[test]
fn the_shared_cache_goes_with_its_last_holder() {
    let shared = SharedOutlines::default();
    let dir = Path::new("blobs");
    let first = shared.cache_for(dir, 0);
    let second = shared.cache_for(dir, 0);
    first.seed(&"a".repeat(64), empty_outline());

    drop(first);
    assert_eq!(shared.live(), 1, "a holder is left");
    assert!(
        second.charged_bytes() > 0,
        "the leaver's outlines stay for the holder left"
    );
    drop(second);
    assert_eq!(shared.live(), 0, "the last holder took the cache with it");
    assert_eq!(
        shared.cache_for(dir, 0).charged_bytes(),
        0,
        "a later holder starts cold"
    );
}

/// A rescan drops content its own view stopped naming from a cache of its own, never from a shared
/// one, where a sibling worktree usually still names it.
#[test]
fn only_a_private_cache_forgets_superseded_content() {
    let hash = "b".repeat(64);
    let cases: [(fn(PathBuf, u64) -> L1Cache, bool); 2] = [(L1Cache::new, false), (L1Cache::shared, true)];
    for (open, kept) in cases {
        let cache = open(PathBuf::new(), 0);
        cache.seed(&hash, empty_outline());

        cache.forget_superseded([hash.as_str()].into_iter().collect(), &FileIndexView::default());

        assert_eq!(cache.holds(&hash), kept);
    }
}
