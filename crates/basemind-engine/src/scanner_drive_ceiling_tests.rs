use std::time::Duration;

use super::{CollectObserver, DriveGovernor, FOOTPRINT_CEILING_REASON, ScanDrive, drive_scan_governed};
use crate::backpressure::FootprintGate;
use crate::config::{Config, ConfigV1, MaxFootprint};
use crate::scanner::{EmbedMode, ScanCancel, ScanSource, ScanStats, scan_with_cancel};
use crate::scanner_file::scanner_pool;
use crate::scanner_filter::Filters;
use crate::store::{Store, VIEW_WORKING, init_isolated_cache, read_status_sidecar};

const MB: u64 = 1024 * 1024;

fn config() -> Config {
    let mut config = ConfigV1::with_defaults();
    config.documents.exclude = Vec::new();
    config.documents.embed = false;
    config.code_search.embed = false;
    config
}

fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("alpha.rs"), b"pub fn alpha() -> u32 { 1 }\n").expect("write");
    dir
}

/// What one `drive_scan_governed` call over `alpha.rs` left behind, with the footprint source faked.
struct Driven {
    cancel: ScanCancel,
    stats: ScanStats,
    seen: Vec<String>,
}

fn drive_with_footprint(footprint_mb: u64, hard_percent: usize) -> Driven {
    init_isolated_cache();
    let dir = repo();
    let root = dir.path();
    let config = config();
    let mut store = Store::open(root, VIEW_WORKING).expect("open");
    let filters = Filters::build(&config, Vec::new(), crate::scan_retrieval::Retrieval::inert()).expect("filters");
    let source = ScanSource::WorkingTree;
    let scope = crate::scanner_flush::derive_scope(root, &source);
    let cancel = ScanCancel::new();
    let drive = ScanDrive {
        root,
        filters: &filters,
        source: &source,
        config: &config,
        scope: &scope,
        embed: EmbedMode::Deferred,
        cancel: &cancel,
    };
    let pool = scanner_pool(config.resources.scan_threads).expect("pool");
    let gate = FootprintGate::with_sampler(100, move || Some(footprint_mb * MB))
        .with_hard_percent(hard_percent)
        .with_timing(Duration::from_millis(1), Duration::from_millis(2));
    let governor = DriveGovernor::with_gate(gate, 1024, pool.current_num_threads());
    let mut stats = ScanStats::default();
    let outcome = drive_scan_governed(
        &drive,
        &["alpha.rs".to_string()],
        &mut store,
        &mut stats,
        &mut CollectObserver::new(),
        governor,
        pool,
    )
    .expect("drive");
    let mut seen: Vec<String> = outcome.seen.into_iter().collect();
    seen.sort();
    Driven { cancel, stats, seen }
}

#[test]
fn a_footprint_past_the_hard_ceiling_cancels_the_drive_before_any_file_is_processed() {
    let driven = drive_with_footprint(151, 150);
    assert!(driven.cancel.is_cancelled());
    assert!(driven.stats.footprint_ceiling_hit);
    assert_eq!(driven.stats.scanned, 0);
    assert!(driven.seen.is_empty());
}

/// `(footprint MB, hard percent)` that must leave the scan running: under the hard ceiling, over
/// only the soft one, and the hard ceiling left off.
#[test]
fn a_footprint_the_hard_ceiling_allows_leaves_the_drive_running() {
    for (footprint_mb, hard_percent) in [(150, 150), (500, 0)] {
        let driven = drive_with_footprint(footprint_mb, hard_percent);
        assert!(!driven.cancel.is_cancelled(), "{footprint_mb} MB at {hard_percent}%");
        assert!(!driven.stats.footprint_ceiling_hit);
        assert_eq!(driven.seen, ["alpha.rs"]);
    }
}

#[test]
fn a_child_token_trips_alone_and_follows_its_parent() {
    let parent = ScanCancel::new();
    let child = parent.child();
    child.cancel();
    assert!(child.is_cancelled());
    assert!(!parent.is_cancelled(), "a child's cancel must not reach its parent");

    let other = parent.child();
    assert!(!other.is_cancelled());
    parent.cancel();
    assert!(other.is_cancelled(), "a child follows its parent");
}

/// The whole scan against the real footprint reading, whose ceiling (1 MiB, hard at 100 %) every
/// process is past: the pass stops itself, leaves the caller's token alone and marks the index
/// partial; the next scan without the ceiling finishes the job and clears the mark.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[test]
fn a_scan_past_the_hard_ceiling_cancels_itself_and_leaves_the_index_partial() {
    init_isolated_cache();
    let dir = repo();
    let root = dir.path();
    let mut store = Store::open(root, VIEW_WORKING).expect("open");
    let mut capped = config();
    capped.resources.max_footprint_mb = MaxFootprint::Mebibytes(1);
    capped.resources.hard_footprint_percent = 100;
    let caller = ScanCancel::new();

    let report = scan_with_cancel(
        root,
        &mut store,
        &capped,
        ScanSource::WorkingTree,
        EmbedMode::Deferred,
        &caller,
        crate::scan_retrieval::Retrieval::inert(),
    )
    .expect("a ceiling stop is a partial pass, not an error");

    assert!(report.cancelled);
    assert!(report.stats.footprint_ceiling_hit);
    assert_eq!(report.stats.scanned, 0);
    assert!(
        !caller.is_cancelled(),
        "the pass must not trip a token other scans share"
    );
    let partial = read_status_sidecar(&store.basemind_dir)
        .expect("sidecar")
        .health()
        .partial
        .expect("the index is marked partial");
    assert_eq!(partial.reason, FOOTPRINT_CEILING_REASON);

    let report = scan_with_cancel(
        root,
        &mut store,
        &config(),
        ScanSource::WorkingTree,
        EmbedMode::Deferred,
        &caller,
        crate::scan_retrieval::Retrieval::inert(),
    )
    .expect("rescan");
    assert!(!report.cancelled);
    assert_eq!(report.stats.updated, 1);
    assert!(
        read_status_sidecar(&store.basemind_dir)
            .expect("sidecar")
            .health()
            .partial
            .is_none()
    );
}
