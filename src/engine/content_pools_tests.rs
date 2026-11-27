//! The content pools share what two workspaces derive from the same content, and only that.

use std::cell::Cell;
use std::path::Path;
use std::sync::Arc;

use super::{GraphKey, GraphPool, KeyedFile, StrPool, TermPool, graph_key};
use crate::engine::MapCache;
use crate::engine::import_graph::ImportGraph;
use crate::engine::l1_shared::{Outlines, SharedOutlines};
use crate::engine::term_index::FileTerms;
use crate::extract::FileMapL1;
use crate::path::RelPath;
use crate::store::{Store, VIEW_WORKING};

/// Source files per fixture tree.
const FILES: u32 = 3;

/// An outline with nothing in it, to project terms from.
fn empty_outline() -> FileMapL1 {
    FileMapL1 {
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
    }
}

fn key(hash: &str, epoch: u16) -> (Arc<str>, u16) {
    (Arc::from(hash), epoch)
}

fn terms() -> FileTerms {
    FileTerms::from_l1(&empty_outline())
}

/// Scan [`FILES`] sources named `src_<n>.rs` into a store over `root`. Trees scanned with one
/// `token` hold identical content at identical paths; another token changes every file.
fn scan_tree(root: &Path, token: &str) -> Store {
    crate::store::init_isolated_cache();
    for file in 0..FILES {
        let source = format!("pub fn {token}_{file}_entry(a: u64) -> u64 {{ a }}\n");
        std::fs::write(root.join(format!("src_{file}.rs")), source).expect("write source");
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

fn rel(file: u32) -> RelPath {
    RelPath::from(format!("src_{file}.rs").as_str())
}

#[test]
fn equal_strings_intern_to_one_allocation_and_unequal_ones_do_not() {
    let pool = StrPool::default();
    let first = pool.intern("aa11");

    assert!(Arc::ptr_eq(&first, &pool.intern("aa11")));
    assert!(!Arc::ptr_eq(&first, &pool.intern("bb22")));
    assert_eq!(&*pool.intern("bb22"), "bb22");
}

#[test]
fn the_string_pool_drops_strings_no_file_map_names_at_its_next_sweep() {
    let pool = StrPool::default();
    let kept = pool.intern("kept");
    for at in 0..super::SWEEP_FLOOR {
        drop(pool.intern(&format!("transient-{at}")));
    }
    pool.intern("trigger the sweep");

    assert!(
        pool.len() < super::SWEEP_FLOOR,
        "dead strings are swept (len={})",
        pool.len()
    );
    assert!(Arc::ptr_eq(&kept, &pool.intern("kept")), "a held string survives");
}

#[test]
fn one_content_key_is_one_projection_and_every_other_key_is_its_own() {
    let pool = TermPool::default();
    let built = Cell::new(0);
    let make = || {
        built.set(built.get() + 1);
        terms()
    };

    let first = pool.intern(key("aa", 3), make);
    let again = pool.intern(key("aa", 3), make);
    let other_hash = pool.intern(key("bb", 3), make);
    let other_epoch = pool.intern(key("aa", 4), make);

    assert!(Arc::ptr_eq(&first, &again), "equal content shares one projection");
    assert!(!Arc::ptr_eq(&first, &other_hash), "another hash is another projection");
    assert!(
        !Arc::ptr_eq(&first, &other_epoch),
        "another epoch is another projection"
    );
    assert_eq!(built.get(), 3, "the shared key projected once");
}

#[test]
fn a_projection_lives_only_while_an_index_holds_it() {
    let pool = TermPool::default();
    let held = pool.intern(key("aa", 3), terms);
    assert_eq!(pool.live_stats().0, 1);

    drop(held);

    assert_eq!(pool.live_stats(), (0, 0), "the pool keeps nothing alive");
}

#[test]
fn two_term_indexes_over_one_content_hold_one_projection_per_file() {
    let (left, right) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (left_store, right_store) = (scan_tree(left.path(), "shared"), scan_tree(right.path(), "shared"));
    let host = SharedOutlines::default();

    let left_map = MapCache::build(&left_store, Outlines::shared(&host, 0));
    let right_map = MapCache::build(&right_store, Outlines::shared(&host, 0));

    let left_terms: Vec<_> = left_map.terms().iter().collect();
    let right_terms: Vec<_> = right_map.terms().iter().collect();
    assert_eq!(left_terms.len(), FILES as usize);
    assert_eq!(left_terms.len(), right_terms.len());
    for ((left_path, left_file), (right_path, right_file)) in left_terms.iter().zip(&right_terms) {
        assert_eq!(left_path, right_path);
        assert!(std::ptr::eq(*left_file, *right_file), "{left_path:?} is one projection");
    }
}

#[test]
fn content_that_differs_is_not_shared_by_the_term_index_or_the_file_map() {
    let (left, right) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (left_store, right_store) = (scan_tree(left.path(), "alpha"), scan_tree(right.path(), "beta"));
    let host = SharedOutlines::default();

    let left_map = MapCache::build(&left_store, Outlines::shared(&host, 0));
    let right_map = MapCache::build(&right_store, Outlines::shared(&host, 0));

    let (left_terms, right_terms) = (left_map.terms(), right_map.terms());
    for ((_, left_file), (_, right_file)) in left_terms.iter().zip(right_terms.iter()) {
        assert!(
            !std::ptr::eq(left_file, right_file),
            "different content, different terms"
        );
    }
    for file in 0..FILES {
        let (left_meta, right_meta) = (left_map.meta(&rel(file)).unwrap(), right_map.meta(&rel(file)).unwrap());
        assert!(!Arc::ptr_eq(&left_meta.hash_hex, &right_meta.hash_hex));
        assert!(
            Arc::ptr_eq(&left_meta.language, &right_meta.language),
            "the language is one string wherever it appears"
        );
    }
}

#[test]
fn two_file_maps_over_one_content_hold_one_allocation_per_hash_and_language() {
    let (left, right) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (left_store, right_store) = (scan_tree(left.path(), "mapped"), scan_tree(right.path(), "mapped"));
    let host = SharedOutlines::default();

    let left_map = MapCache::build(&left_store, Outlines::shared(&host, 0));
    let right_map = MapCache::build(&right_store, Outlines::shared(&host, 0));

    for file in 0..FILES {
        let (left_meta, right_meta) = (left_map.meta(&rel(file)).unwrap(), right_map.meta(&rel(file)).unwrap());
        assert!(Arc::ptr_eq(&left_meta.hash_hex, &right_meta.hash_hex), "src_{file}.rs");
        assert!(Arc::ptr_eq(&left_meta.language, &right_meta.language), "src_{file}.rs");
    }
}

#[test]
fn a_map_with_no_host_shares_nothing_with_another() {
    let (left, right) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (left_store, right_store) = (scan_tree(left.path(), "private"), scan_tree(right.path(), "private"));

    let left_map = MapCache::build(&left_store, 0);
    let right_map = MapCache::build(&right_store, 0);

    let left_meta = left_map.meta(&rel(0)).unwrap();
    let right_meta = right_map.meta(&rel(0)).unwrap();
    assert!(!Arc::ptr_eq(&left_meta.hash_hex, &right_meta.hash_hex));
}

fn digest(files: &[(&str, &str, u16)], documents: &[(&str, &str)], probes_complete: bool) -> GraphKey {
    let paths: Vec<RelPath> = files.iter().map(|(path, ..)| RelPath::from(*path)).collect();
    let doc_paths: Vec<RelPath> = documents.iter().map(|(path, _)| RelPath::from(*path)).collect();
    graph_key(
        files
            .iter()
            .zip(&paths)
            .map(|((_, hash_hex, extract_epoch), path)| KeyedFile {
                path,
                hash_hex,
                extract_epoch: *extract_epoch,
            }),
        documents.iter().zip(&doc_paths).map(|((_, hash), path)| (path, *hash)),
        probes_complete,
    )
}

#[test]
fn the_graph_key_is_the_content_and_not_the_order_it_was_listed_in() {
    let files = [("a.rs", "h1", 1), ("b.rs", "h2", 1)];
    let reversed = [("b.rs", "h2", 1), ("a.rs", "h1", 1)];

    assert_eq!(
        digest(&files, &[("tsconfig.json", "d1")], true),
        digest(&reversed, &[("tsconfig.json", "d1")], true)
    );
}

#[test]
fn any_difference_in_the_content_changes_the_graph_key() {
    let files = [("a.rs", "h1", 1), ("b.rs", "h2", 1)];
    let docs = [("tsconfig.json", "d1")];
    let base = digest(&files, &docs, true);

    let variants = [
        (
            "a file's content",
            digest(&[("a.rs", "h9", 1), ("b.rs", "h2", 1)], &docs, true),
        ),
        (
            "a file's epoch",
            digest(&[("a.rs", "h1", 2), ("b.rs", "h2", 1)], &docs, true),
        ),
        (
            "a file's path",
            digest(&[("c.rs", "h1", 1), ("b.rs", "h2", 1)], &docs, true),
        ),
        ("a missing file", digest(&files[..1], &docs, true)),
        ("a document's content", digest(&files, &[("tsconfig.json", "d2")], true)),
        ("a missing document", digest(&files, &[], true)),
        ("incomplete probes", digest(&files, &docs, false)),
        (
            "a path moved between fields",
            digest(&[("a.rsh1", "", 1), ("b.rs", "h2", 1)], &docs, true),
        ),
    ];
    for (what, variant) in variants {
        assert_ne!(base, variant, "{what} is another graph");
    }
}

#[test]
fn workspaces_of_one_key_share_one_graph_built_once() {
    let pool = GraphPool::default();
    let key = digest(&[("a.rs", "h1", 1)], &[], true);
    let built = Cell::new(0);
    let build = || -> Result<ImportGraph, ()> {
        built.set(built.get() + 1);
        Ok(ImportGraph::default())
    };

    let first = pool.get_or_build(key, build).unwrap();
    let second = pool.get_or_build(key, build).unwrap();

    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(built.get(), 1);
}

#[test]
fn a_graph_of_another_key_is_not_shared() {
    let pool = GraphPool::default();
    let build = || -> Result<ImportGraph, ()> { Ok(ImportGraph::default()) };

    let first = pool
        .get_or_build(digest(&[("a.rs", "h1", 1)], &[], true), build)
        .unwrap();
    let second = pool
        .get_or_build(digest(&[("a.rs", "h2", 1)], &[], true), build)
        .unwrap();

    assert!(!Arc::ptr_eq(&first, &second));
    assert_eq!(pool.live_stats().0, 2);
}

#[test]
fn a_graph_lives_only_while_a_workspace_holds_it_and_a_failed_build_is_not_kept() {
    let pool = GraphPool::default();
    let key = digest(&[("a.rs", "h1", 1)], &[], true);
    let held = pool.get_or_build(key, || Ok::<_, ()>(ImportGraph::default())).unwrap();
    assert_eq!(pool.live_stats().0, 1);

    drop(held);
    assert_eq!(pool.live_stats(), (0, 0), "the pool keeps no graph alive");

    let failed = pool.get_or_build(key, || Err::<ImportGraph, _>("no index"));
    assert_eq!(failed.unwrap_err(), "no index");
    assert_eq!(pool.live_stats().0, 0);
}
