//! Durability contract for `scanner::scan`: the code map survives a broken optional lane.
//!
//! The optional post-extraction lanes (resolve / doc batches / code batches / bm25 stats) run
//! *after* the code map is complete. Two real failures took the whole scan down with them: the
//! `stack-graphs` stitcher panicking on a rayon worker, and the embedding-model download hanging
//! forever on a blackholed IPv6 route (killed by the operator). Both left `index.msgpack` unwritten
//! — `file_count: 0` on disk next to gigabytes of committed blobs, and a full re-scan every launch.
//!
//! These tests pin both halves of the fix:
//! - a lane that PANICS is contained: `scan` returns `Ok` and the code map is on disk;
//! - a lane that never returns (stood in for by an `abort` mid-lane — what a SIGKILL does) still
//!   leaves the code map on disk, because the flush happens BEFORE the lanes run.
//!
//! The fault is injected through the `test-support`-only seam in `basemind::scanner_lanes`; neither
//! the environment variable nor the branch exists in a production build.

use std::fs;
use std::path::Path;
use std::process::Command;

use basemind::config::ConfigV1;
use basemind::scanner::{EmbedMode, ScanSource, scan};
use basemind::scanner_lanes::{LANE_RESOLVE, TEST_FAULT_LANE_ENV};
use basemind::store::{INDEX_FILE, Store, VIEW_WORKING};
use tempfile::TempDir;

/// Write a small multi-file Rust tree so the scan has a real code map to lose.
fn seed_repo(root: &Path) {
    fs::write(root.join("alpha.rs"), b"pub fn alpha() -> u32 { 1 }\n").unwrap();
    fs::write(root.join("beta.rs"), b"pub fn beta() -> u32 { alpha() }\n").unwrap();
    fs::write(root.join("gamma.rs"), b"pub struct Gamma { pub x: u32 }\n").unwrap();
}

#[test]
fn panicking_optional_lane_leaves_the_code_map_flushed_and_queryable() {
    basemind::store::init_isolated_cache();
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    seed_repo(root);

    // SAFETY: `TEST_FAULT_LANE_ENV` is read only by the scanner's lane guard, and the only scan in
    // this binary is the one below — no other thread reads the variable while it is being set.
    unsafe { std::env::set_var(TEST_FAULT_LANE_ENV, LANE_RESOLVE) };

    let cfg = ConfigV1::with_defaults();
    let mut store = Store::open(root, VIEW_WORKING).expect("open store");
    let report = scan(root, &mut store, &cfg, ScanSource::WorkingTree, EmbedMode::Inline);

    // SAFETY: see above.
    unsafe { std::env::remove_var(TEST_FAULT_LANE_ENV) };

    let report = report.expect("a panicking optional lane must not fail the scan");
    assert_eq!(report.stats.updated, 3, "every source file is still extracted");

    let index_path = store.view_dir.join(INDEX_FILE);
    assert!(index_path.exists(), "the code map must be flushed to {index_path:?}");

    let hits = basemind::query::search_symbols(&store, "beta", None).unwrap();
    assert_eq!(hits.len(), 1, "the code map stays queryable after a degraded lane");

    drop(store);
    let reopened = Store::open(root, VIEW_WORKING).expect("reopen store");
    assert_eq!(
        reopened.index.files.len(),
        3,
        "the persisted code map must not be empty"
    );
}

/// A lane killed mid-flight (hang → SIGKILL; here: `abort` inside the lane) must still leave the
/// code map on disk. This is what pins the *ordering*: only an `index.msgpack` flushed BEFORE the
/// lanes can survive a process that never reaches the end of `scan`.
#[test]
fn code_map_survives_a_lane_that_kills_the_process() {
    basemind::store::init_isolated_cache();
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    seed_repo(root);

    // Inherits `BASEMIND_DATA_HOME` from `init_isolated_cache`, so the child writes into the same
    // isolated cache this test then reads back.
    let output = Command::new(env!("CARGO_BIN_EXE_basemind"))
        .args(["--root", root.to_str().unwrap(), "scan", "--quiet"])
        .env(TEST_FAULT_LANE_ENV, format!("{LANE_RESOLVE}:abort"))
        .output()
        .expect("run basemind scan");
    assert!(
        !output.status.success(),
        "the injected abort must actually kill the scan process (status: {:?})",
        output.status
    );

    let store = Store::open(root, VIEW_WORKING).expect("open store after the killed scan");
    let index_path = store.view_dir.join(INDEX_FILE);
    assert!(
        index_path.exists(),
        "the code map must already be on disk when the lane kills the process; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        store.index.files.len(),
        3,
        "the code map persisted by the killed scan must not be empty"
    );
}
