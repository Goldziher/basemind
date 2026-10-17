//! A scan on a volume without room for the index is refused up front rather than started and left
//! to fail on `StorageFull` halfway (issue #79).
//!
//! Alone in its binary because it sets the headroom through the process environment, which would
//! race any other scan reading it.

use std::fs;

use basemind::config::ConfigV1;
use basemind::scanner::{EmbedMode, ScanError, ScanSource, scan};
use basemind::store::{Store, VIEW_WORKING};
use tempfile::TempDir;

#[test]
fn a_scan_is_refused_when_the_volume_is_below_the_headroom() {
    basemind::store::init_isolated_cache();
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    assert!(
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(root)
            .status()
            .expect("git init")
            .success()
    );
    fs::write(root.join("alpha.rs"), b"pub fn alpha() {}\n").unwrap();
    // SAFETY: the only test in this binary, and set before the scan reads it.
    unsafe { std::env::set_var(basemind::disk_space::MIN_FREE_ENV, "999999999999") };

    let mut store = Store::open(root, VIEW_WORKING).expect("open store");
    let outcome = scan(root, &mut store, &ConfigV1::with_defaults(), ScanSource::WorkingTree, EmbedMode::Deferred);

    match outcome {
        Err(ScanError::InsufficientSpace(refusal)) => {
            assert!(refusal.required_bytes > refusal.free_bytes);
            assert!(refusal.to_string().contains("BASEMIND_MIN_FREE_MB"));
        }
        other => panic!("expected a free-space refusal, got {other:?}"),
    }
    assert!(store.index.files.is_empty(), "a refused scan indexes nothing");
}
