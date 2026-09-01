//! Regression cover for the two scan-footprint fixes behind issue #62 (a scan that reached
//! 43.8 GiB RSS and got the daemon OOM-killed): directory-level pruning of excluded trees, and the
//! hard `[scan] max_candidates` ceiling.

use std::fs;
use std::path::Path;

use basemind::config::ConfigV1;
use basemind::scanner::{EmbedMode, ScanError, ScanSource, scan};
use basemind::store::Store;
use tempfile::TempDir;

fn fresh_repo() -> (TempDir, ConfigV1) {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    (dir, ConfigV1::with_defaults())
}

fn indexed_paths(store: &Store) -> Vec<String> {
    let mut paths: Vec<String> = store
        .index
        .files
        .keys()
        .map(|rel| rel.to_str_lossy().into_owned())
        .collect();
    paths.sort();
    paths
}

/// A `node_modules` tree that no `.gitignore` covers used to be walked and stat'd in full, with
/// every one of its files thrown away by the per-file exclude globs afterwards. It is now pruned at
/// the directory — and the indexed set must be **exactly** what it was before, which is the
/// property that makes the optimisation safe to ship.
#[test]
fn non_gitignored_node_modules_is_pruned_without_changing_the_indexed_set() {
    let (dir, cfg) = fresh_repo();
    let root = dir.path();

    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), b"pub fn kept() {}\n").unwrap();
    for pkg in 0..20 {
        let pkg_dir = root.join(format!("node_modules/pkg{pkg}/lib"));
        fs::create_dir_all(&pkg_dir).unwrap();
        for file in 0..10 {
            fs::write(pkg_dir.join(format!("m{file}.js")), b"export function noise() {}\n").unwrap();
        }
    }

    let mut store = Store::open(root, basemind::store::VIEW_WORKING).unwrap();
    let report = scan(root, &mut store, &cfg, ScanSource::WorkingTree, EmbedMode::Inline).unwrap();

    assert_eq!(
        indexed_paths(&store),
        vec!["src/a.rs".to_string()],
        "only the real source file is indexed"
    );
    assert!(
        report.results.iter().all(|r| !r.path.starts_with("node_modules/")),
        "no node_modules file reached the per-file pipeline"
    );
}

fn write_many(dir: &Path, count: usize) {
    fs::create_dir_all(dir).unwrap();
    for i in 0..count {
        fs::write(dir.join(format!("f{i}.rs")), b"pub fn f() {}\n").unwrap();
    }
}

#[test]
fn candidate_cap_aborts_the_scan_before_any_write() {
    let (dir, mut cfg) = fresh_repo();
    let root = dir.path();
    write_many(&root.join("big"), 40);
    cfg.scan.max_candidates = 20;

    let mut store = Store::open(root, basemind::store::VIEW_WORKING).unwrap();
    let err = scan(root, &mut store, &cfg, ScanSource::WorkingTree, EmbedMode::Inline)
        .expect_err("the cap must abort the scan");

    match &err {
        ScanError::TooManyCandidates {
            count, cap, top_dirs, ..
        } => {
            assert_eq!(*cap, 20);
            assert_eq!(*count, 20, "the walk stops at the cap, it does not run to completion");
            assert_eq!(top_dirs.first().map(|(name, _)| name.as_str()), Some("big"));
        }
        other => panic!("unexpected error: {other}"),
    }
    let msg = err.to_string();
    assert!(msg.contains("big (20)"), "message names the top contributor: {msg}");
    assert!(msg.contains("max_candidates"), "message names the knob: {msg}");
    assert!(store.index.files.is_empty(), "the abort happens before any index write");
}

#[test]
fn candidate_cap_of_zero_is_unlimited() {
    let (dir, mut cfg) = fresh_repo();
    let root = dir.path();
    write_many(&root.join("big"), 40);
    cfg.scan.max_candidates = 0;

    let mut store = Store::open(root, basemind::store::VIEW_WORKING).unwrap();
    scan(root, &mut store, &cfg, ScanSource::WorkingTree, EmbedMode::Inline).expect("0 disables the ceiling");
    assert_eq!(store.index.files.len(), 40);
}
