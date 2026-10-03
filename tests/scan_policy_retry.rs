//! A purge that did not complete must not be recorded as applied: the next scan has to retry it.
//! Lives in its own test binary because the fault-injection env var is process-wide.

use basemind::config::ConfigV1;
use basemind::scanner::{EmbedMode, ScanSource, scan};
use basemind::scanner_lanes::{LANE_EMBED_POLICY, TEST_FAULT_LANE_ENV};
use basemind::store::{Store, VIEW_WORKING};

#[test]
fn a_failed_policy_purge_is_not_recorded_and_is_retried() {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "pub fn a() {}\n").unwrap();
    let mut cfg = ConfigV1::with_defaults();
    cfg.documents.embed = false;
    cfg.code_search.embed = false;
    let mut store = Store::open(dir.path(), VIEW_WORKING).unwrap();

    // SAFETY: this binary holds exactly one test, so nothing else reads the environment concurrently.
    unsafe { std::env::set_var(TEST_FAULT_LANE_ENV, LANE_EMBED_POLICY) };
    scan(dir.path(), &mut store, &cfg, ScanSource::WorkingTree, EmbedMode::Inline).unwrap();
    assert!(
        store.index.embed_policy.is_empty(),
        "the purge lane died, so the policy must stay unrecorded"
    );

    unsafe { std::env::remove_var(TEST_FAULT_LANE_ENV) };
    scan(dir.path(), &mut store, &cfg, ScanSource::WorkingTree, EmbedMode::Inline).unwrap();
    assert!(
        !store.index.embed_policy.is_empty(),
        "the retry completes and records the policy"
    );
}
