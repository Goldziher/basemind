//! The free-space check a full scan runs once its candidates are known, against a faked free figure.

use super::tests::one_file_repo;
use crate::config::ConfigV1;
use crate::disk_space::{self, BLOBS_PER_SOURCE, DEFAULT_HEADROOM_MB, INDEX_PER_SOURCE, INDEX_REWRITE};
use crate::scanner::{EmbedMode, ScanError, ScanReport, ScanSource, scan};
use crate::store::{Store, VIEW_WORKING, init_isolated_cache};

/// What [`one_file_repo`] writes to `alpha.rs`.
const ALPHA: &[u8] = b"pub fn alpha() -> u32 { 1 }\n";
const BETA: &[u8] = b"pub fn beta() -> u32 { 2 }\n";
const GAMMA: &[u8] = b"pub fn gamma() -> u32 { 3 }\n";
/// Room for the index the refused passes wrote before a later pass is let through.
const SLACK: u64 = 64 << 20;

fn scan_with_free(root: &std::path::Path, store: &mut Store, free: u64) -> Result<ScanReport, ScanError> {
    disk_space::fake_free_bytes(Some(free));
    let outcome = scan(
        root,
        store,
        &ConfigV1::with_defaults(),
        ScanSource::WorkingTree,
        EmbedMode::Deferred,
        crate::scan_retrieval::Retrieval::inert(),
    );
    disk_space::fake_free_bytes(None);
    outcome
}

fn refusal(outcome: Result<ScanReport, ScanError>) -> disk_space::InsufficientSpace {
    match outcome {
        Err(ScanError::InsufficientSpace(refusal)) => refusal,
        other => panic!("expected a free-space refusal, got {other:?}"),
    }
}

fn len(bytes: &[u8]) -> u64 {
    u64::try_from(bytes.len()).expect("fixture size")
}

/// A first scan starts from an empty index, so an estimate of the index alone let it through with
/// little more than the headroom free. It is now charged for the source it extracts: the free figure
/// that covers the index and the headroom, and everything but one byte of the rest, is refused
/// before any file is extracted, and a figure that covers it all scans.
#[test]
fn a_first_scan_that_cannot_fit_its_source_is_refused_and_one_that_fits_runs() {
    init_isolated_cache();
    let dir = one_file_repo();
    let root = dir.path();
    std::fs::write(root.join("beta.rs"), BETA).expect("write");
    std::fs::write(root.join("data.bin"), [0u8; 4096]).expect("a file no grammar claims");
    let mut store = Store::open(root, VIEW_WORKING).expect("open");

    let first = refusal(scan_with_free(root, &mut store, 0));
    let source = len(ALPHA) + len(BETA);
    assert_eq!(first.free_bytes, 0);
    assert_eq!(
        first.estimate.source_bytes, source,
        "only the files with a grammar count"
    );
    assert!(!first.estimate.vectors, "the default config embeds no code");
    assert_eq!(first.estimate.headroom_bytes, DEFAULT_HEADROOM_MB << 20);
    assert_eq!(first.estimate.blob_growth(), BLOBS_PER_SOURCE.of(source));
    assert_eq!(
        first.estimate.index_growth(),
        INDEX_REWRITE.of(first.estimate.index_bytes) + INDEX_PER_SOURCE.of(source)
    );
    assert_eq!(first.required_bytes, first.estimate.required_bytes());
    assert!(store.index.files.is_empty(), "a refused scan indexes nothing");

    let short = first.required_bytes - 1;
    let index_only = INDEX_REWRITE.of(first.estimate.index_bytes) + first.estimate.headroom_bytes;
    assert!(
        short >= index_only,
        "the index-only estimate would have let this scan through"
    );
    let second = refusal(scan_with_free(root, &mut store, short));
    assert_eq!(second.free_bytes, short);
    assert_eq!(second.estimate.source_bytes, source);
    assert!(store.index.files.is_empty(), "a refused scan indexes nothing");

    scan_with_free(root, &mut store, second.required_bytes + SLACK).expect("a scan with room runs");
    assert!(store.lookup("alpha.rs").is_some());
    assert!(store.lookup("beta.rs").is_some());
}

/// A rescan is charged for the index it rewrites and for the files no file map lists yet, not for
/// the files it already indexed.
#[test]
fn a_rescan_is_charged_only_for_the_files_the_file_map_does_not_list() {
    init_isolated_cache();
    let dir = one_file_repo();
    let root = dir.path();
    let mut store = Store::open(root, VIEW_WORKING).expect("open");
    scan_with_free(root, &mut store, u64::MAX).expect("first scan");

    let rescan = refusal(scan_with_free(root, &mut store, 0));
    assert_eq!(rescan.estimate.source_bytes, 0);
    assert!(rescan.estimate.index_bytes > 0, "the first scan wrote an index");
    assert_eq!(
        rescan.estimate.index_growth(),
        INDEX_REWRITE.of(rescan.estimate.index_bytes)
    );

    std::fs::write(root.join("gamma.rs"), GAMMA).expect("write");
    let grown = refusal(scan_with_free(root, &mut store, 0));
    assert_eq!(grown.estimate.source_bytes, len(GAMMA));
}
