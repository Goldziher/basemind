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

/// Directory pruning must not change *what gets indexed* — the half of the property that would be
/// a data-loss bug if it broke, and the only half a scan's output can observe. Whether the walker
/// still descends into `node_modules` is invisible here (pre-pruning, `walk_candidates` dropped
/// those paths through `Filters::allows` before they became candidates, so this set was already
/// exactly `["src/a.rs"]`); that half is covered by
/// `scanner_filter::tests::the_dir_pruner_stops_the_walk_descending_into_a_non_gitignored_node_modules`,
/// which compares walk work with and without the gate.
#[test]
fn pruning_a_non_gitignored_node_modules_leaves_the_indexed_set_unchanged() {
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
            candidates,
            cap,
            top_dirs,
            ..
        } => {
            assert_eq!(*cap, 20);
            assert_eq!(
                *candidates, 40,
                "the count is what the walk actually saw, not the cap parroted back"
            );
            assert_eq!(top_dirs.first().map(|(name, _)| name.as_str()), Some("big"));
        }
        other => panic!("unexpected error: {other}"),
    }
    let msg = err.to_string();
    assert!(msg.contains("big (40)"), "message names the top contributor: {msg}");
    assert!(msg.contains("max_candidates"), "message names the knob: {msg}");
    assert!(store.index.files.is_empty(), "the abort happens before any index write");
}

/// A repository holding *exactly* `max_candidates` candidates must scan. The cap used to be
/// evaluated at the top of the walk loop against every entry — so once the candidate list reached
/// the cap, the next directory node or non-candidate file aborted a scan that never exceeded
/// anything. Whether it fired depended on readdir order, making it an intermittent spurious
/// failure for any repo sitting on the boundary; the empty directories here are what used to trip
/// it.
#[test]
fn exactly_max_candidates_is_not_a_breach() {
    let (dir, mut cfg) = fresh_repo();
    let root = dir.path();
    write_many(&root.join("big"), 20);
    for i in 0..40 {
        fs::create_dir_all(root.join(format!("empty{i}"))).unwrap();
    }
    cfg.scan.max_candidates = 20;

    let mut store = Store::open(root, basemind::store::VIEW_WORKING).unwrap();
    scan(root, &mut store, &cfg, ScanSource::WorkingTree, EmbedMode::Inline)
        .expect("exactly max_candidates candidates is within the ceiling");
    assert_eq!(store.index.files.len(), 20);
}

/// The contributor list must name the tree that actually caused the breach. `ignore::Walk` is
/// depth-first in readdir order, so a tally truncated at the breach names whichever directory
/// happened to be enumerated first — here `aaa`, which is 166x smaller than the real offender.
#[test]
fn the_contributor_list_names_the_heaviest_tree_not_the_first_one() {
    let (dir, mut cfg) = fresh_repo();
    let root = dir.path();
    write_many(&root.join("aaa"), 30);
    write_many(&root.join("zzz"), 5000);
    cfg.scan.max_candidates = 20;

    let mut store = Store::open(root, basemind::store::VIEW_WORKING).unwrap();
    let err = scan(root, &mut store, &cfg, ScanSource::WorkingTree, EmbedMode::Inline)
        .expect_err("the cap must abort the scan");

    match &err {
        ScanError::TooManyCandidates { top_dirs, .. } => {
            assert_eq!(
                top_dirs.first().map(|(name, count)| (name.as_str(), *count)),
                Some(("zzz", 5000)),
                "the survey past the breach is what makes the message actionable: {top_dirs:?}"
            );
        }
        other => panic!("unexpected error: {other}"),
    }
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
