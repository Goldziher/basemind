//! Integrity contract for `scanner::scan` (issues #79 and #84): a scan that could not persist its
//! work, or was cut short, must never leave a status that reads complete.
//!
//! A real full disk cannot be summoned in a test, so the fatal commit failure is injected through
//! the same latch the writers feed (`IndexDb::record_commit_error`), from a `ScanObserver` so it
//! lands mid-scan, after files were absorbed and before the code map is flushed.

use std::fs;
use std::path::Path;
use std::sync::Once;

use basemind::config::ConfigV1;
use basemind::index::IndexDb;
use basemind::index::IndexError;
use basemind::scanner::{EmbedMode, FileResult, ScanCancel, ScanError, ScanObserver, ScanSource, scan, scan_with_cancel, scan_with_observer};
use basemind::store::{Store, VIEW_WORKING, read_status_sidecar};
use tempfile::TempDir;

/// The free-space pre-check would otherwise demand its default headroom from the test machine's disk.
fn no_headroom() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        basemind::store::init_isolated_cache();
        // SAFETY: set once, before any scan in this binary reads it.
        unsafe { std::env::set_var(basemind::disk_space::MIN_FREE_ENV, "0") };
    });
}

fn seed_repo(root: &Path) {
    let status = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(root)
        .status()
        .expect("run git init");
    assert!(status.success());
    fs::write(root.join("alpha.rs"), b"pub fn alpha() -> u32 { 1 }\n").unwrap();
    fs::write(root.join("beta.rs"), b"pub fn beta() -> u32 { alpha() }\n").unwrap();
}

/// Latches `error` on the index handle when the first file is absorbed.
struct FaultOnFirstFile {
    db: IndexDb,
    error: Option<IndexError>,
}

impl ScanObserver for FaultOnFirstFile {
    fn on_file(&mut self, _result: FileResult) {
        if let Some(error) = self.error.take() {
            self.db.record_commit_error(&error);
        }
    }
}

fn storage_full() -> IndexError {
    IndexError::Fjall(fjall::Error::Io(std::io::Error::from(std::io::ErrorKind::StorageFull)))
}

fn scan_with_injected(store: &mut Store, root: &Path, error: IndexError) -> Result<(), ScanError> {
    let db = store.index_db.clone().expect("writable store has an index");
    let mut observer = FaultOnFirstFile { db, error: Some(error) };
    let cfg = ConfigV1::with_defaults();
    scan_with_observer(
        root,
        store,
        &cfg,
        ScanSource::WorkingTree,
        EmbedMode::Deferred,
        &ScanCancel::new(),
        &mut observer,
    )
    .map(|_| ())
}

#[test]
fn a_clean_scan_records_a_complete_resolved_index() {
    no_headroom();
    let dir = TempDir::new().unwrap();
    seed_repo(dir.path());
    let mut store = Store::open(dir.path(), VIEW_WORKING).expect("open store");
    scan(dir.path(), &mut store, &ConfigV1::with_defaults(), ScanSource::WorkingTree, EmbedMode::Deferred)
        .expect("scan");
    let status = read_status_sidecar(&store.basemind_dir).expect("sidecar");
    assert!(!status.is_degraded(), "a finished scan is healthy: {status:?}");
    assert!(status.resolve_complete);
}

#[test]
fn a_full_disk_mid_scan_aborts_and_the_status_reads_not_complete() {
    no_headroom();
    let dir = TempDir::new().unwrap();
    seed_repo(dir.path());
    let mut store = Store::open(dir.path(), VIEW_WORKING).expect("open store");

    let outcome = scan_with_injected(&mut store, dir.path(), storage_full());

    assert!(matches!(outcome, Err(ScanError::IndexFault(_))), "got {outcome:?}");
    let status = read_status_sidecar(&store.basemind_dir).expect("an aborted scan still records its status");
    assert!(status.partial, "{status:?}");
    assert!(status.is_degraded());
    assert!(
        status.degraded_summary().expect("summary").contains("disk full"),
        "the operator is told why: {status:?}"
    );

    // A full disk is cured by freeing space: the next scan on the same handle clears the latch.
    scan(dir.path(), &mut store, &ConfigV1::with_defaults(), ScanSource::WorkingTree, EmbedMode::Deferred)
        .expect("rescan after space is freed");
    let status = read_status_sidecar(&store.basemind_dir).expect("sidecar");
    assert!(!status.is_degraded(), "a completed rescan clears the degradation: {status:?}");
}

#[test]
fn a_poisoned_handle_keeps_refusing_scans_until_it_is_reopened() {
    no_headroom();
    let dir = TempDir::new().unwrap();
    seed_repo(dir.path());
    let mut store = Store::open(dir.path(), VIEW_WORKING).expect("open store");

    let outcome = scan_with_injected(&mut store, dir.path(), IndexError::Fjall(fjall::Error::Poisoned));
    assert!(matches!(outcome, Err(ScanError::IndexFault(_))), "got {outcome:?}");
    assert!(store.index_db.as_ref().expect("index").is_poisoned());

    let again = scan(dir.path(), &mut store, &ConfigV1::with_defaults(), ScanSource::WorkingTree, EmbedMode::Deferred);
    assert!(matches!(again, Err(ScanError::IndexFault(_))), "poison is not cleared by retrying: {again:?}");

    store.reopen_index_db().expect("reopen");
    assert!(!store.index_db.as_ref().expect("index").is_poisoned());
    scan(dir.path(), &mut store, &ConfigV1::with_defaults(), ScanSource::WorkingTree, EmbedMode::Deferred)
        .expect("a reopened handle scans again");
    assert!(!read_status_sidecar(&store.basemind_dir).expect("sidecar").is_degraded());
}

#[test]
fn a_cancelled_scan_is_recorded_as_partial_until_a_full_scan_completes() {
    no_headroom();
    let dir = TempDir::new().unwrap();
    seed_repo(dir.path());
    let mut store = Store::open(dir.path(), VIEW_WORKING).expect("open store");
    let cfg = ConfigV1::with_defaults();

    let cancel = ScanCancel::new();
    cancel.cancel();
    let report = scan_with_cancel(dir.path(), &mut store, &cfg, ScanSource::WorkingTree, EmbedMode::Deferred, &cancel)
        .expect("a cancelled scan still returns a report");
    assert!(report.cancelled);

    let status = read_status_sidecar(&store.basemind_dir).expect("sidecar");
    assert!(status.partial, "{status:?}");
    assert_eq!(status.partial_reason.as_deref(), Some("cancelled"));
    assert!(status.progress_total.is_some() && status.progress_done.is_some());
    assert!(status.degraded_summary().expect("summary").contains("partial scan"));

    scan(dir.path(), &mut store, &cfg, ScanSource::WorkingTree, EmbedMode::Deferred).expect("full scan");
    let status = read_status_sidecar(&store.basemind_dir).expect("sidecar");
    assert!(!status.partial && !status.is_degraded(), "{status:?}");
}
