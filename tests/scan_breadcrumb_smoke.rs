//! The scan breadcrumb, producer end (issue #62).
//!
//! `src/scan_evidence.rs` unit-tests the record in isolation; these tests assert the wiring that
//! makes it mean anything: that a real full-tree scan writes one at the real path, that the path is
//! the one `stale_inflight_scans` enumerates, that the pass retracts it on the way out, and that the
//! watcher's incremental entry point leaves it alone. Every one of those is a way the feature could
//! be silently inert while still compiling.

use std::fs;
use std::path::{Path, PathBuf};

use basemind::config::ConfigV1;
use basemind::scan_evidence::{self, PHASE_STARTING, ScanInflight};
use basemind::scanner::{
    EmbedMode, FileResult, ScanCancel, ScanObserver, ScanSource, scan, scan_paths, scan_with_observer,
};
use basemind::store::{Store, VIEW_WORKING};
use basemind::store_layout::{CACHE_DIR, WORKSPACES_DIR, cache_root, workspace_cache_dir};

/// A repo-shaped tempdir with one source file. Returned whole so the `TempDir` outlives the scan.
fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("app.rs"), "fn main() {}\n").expect("write source");
    dir
}

fn open(root: &Path) -> Store {
    Store::open(root, VIEW_WORKING).expect("open store")
}

/// The directory `stale_inflight_scans` enumerates. Derived here from the same layout constants the
/// consumer uses, so a future split between producer and consumer roots fails these tests.
fn workspaces_dir() -> PathBuf {
    cache_root().join(CACHE_DIR).join(WORKSPACES_DIR)
}

/// Snapshots the breadcrumb from the real cache dir on the first file the drive absorbs — the only
/// deterministic vantage point *inside* a pass, since the record is gone by the time `scan` returns.
struct Peek {
    cache_dir: PathBuf,
    seen: Option<ScanInflight>,
    files: usize,
}

impl ScanObserver for Peek {
    fn on_file(&mut self, _result: FileResult) {
        self.files += 1;
        if self.seen.is_none() {
            self.seen = scan_evidence::read(&self.cache_dir);
        }
    }
}

#[test]
fn a_full_scan_writes_a_breadcrumb_at_the_path_the_doctor_reads() {
    basemind::store::init_isolated_cache();
    let dir = repo();
    let root = dir.path();
    let cache_dir = workspace_cache_dir(root);

    assert!(
        cache_dir.starts_with(workspaces_dir()),
        "the breadcrumb must land under the dir stale_inflight_scans enumerates: \
         {} is not under {}",
        cache_dir.display(),
        workspaces_dir().display()
    );

    let mut store = open(root);
    let mut peek = Peek {
        cache_dir: cache_dir.clone(),
        seen: None,
        files: 0,
    };
    scan_with_observer(
        root,
        &mut store,
        &ConfigV1::with_defaults(),
        ScanSource::WorkingTree,
        EmbedMode::Deferred,
        &ScanCancel::new(),
        &mut peek,
    )
    .expect("scan");

    assert!(peek.files > 0, "the fixture must produce at least one candidate");
    let record = peek.seen.expect("a live full-tree scan leaves a record on disk");
    assert_eq!(record.pid, std::process::id());
    assert_eq!(record.root, root, "the record names the root being scanned");
    assert_eq!(record.version, env!("CARGO_PKG_VERSION"));
    assert_ne!(
        record.phase, PHASE_STARTING,
        "by the time files are being absorbed the pass has advanced past `starting`"
    );
    assert!(
        record.candidates.is_some_and(|count| count > 0),
        "candidate enumeration has produced a count by then: {record:?}"
    );
}

#[test]
fn a_scan_that_returns_leaves_no_breadcrumb_behind() {
    basemind::store::init_isolated_cache();
    let dir = repo();
    let root = dir.path();
    let cache_dir = workspace_cache_dir(root);

    let mut store = open(root);
    scan(
        root,
        &mut store,
        &ConfigV1::with_defaults(),
        ScanSource::WorkingTree,
        EmbedMode::Deferred,
    )
    .expect("scan");

    assert_eq!(
        scan_evidence::read(&cache_dir),
        None,
        "a pass that returned normally must retract its claim"
    );
    assert!(
        !scan_evidence::inflight_path(&cache_dir).exists(),
        "and the file itself must be gone, not merely unreadable"
    );
}

#[test]
fn the_incremental_entry_point_writes_no_breadcrumb() {
    basemind::store::init_isolated_cache();
    let dir = repo();
    let root = dir.path();
    let cache_dir = workspace_cache_dir(root);
    let touched = vec![root.join("app.rs")];

    let mut store = open(root);
    let mut peek = Peek {
        cache_dir: cache_dir.clone(),
        seen: None,
        files: 0,
    };
    basemind::scanner::scan_paths_with_observer(
        root,
        &mut store,
        &ConfigV1::with_defaults(),
        &touched,
        EmbedMode::Deferred,
        &ScanCancel::new(),
        &mut peek,
    )
    .expect("scan_paths");

    assert!(peek.files > 0, "the watcher path must have absorbed the touched file");
    assert_eq!(
        peek.seen, None,
        "scan_paths runs per watcher batch; a breadcrumb there would be rewritten continuously \
         for no evidentiary gain"
    );
    assert_eq!(
        scan_evidence::read(&cache_dir),
        None,
        "and nothing is left behind either"
    );

    // Guard the plain wrapper too: it is the one the watcher actually calls.
    scan_paths(
        root,
        &mut store,
        &ConfigV1::with_defaults(),
        &touched,
        EmbedMode::Deferred,
    )
    .expect("scan_paths");
    assert_eq!(scan_evidence::read(&cache_dir), None);
}

/// Producer to consumer, end to end: take the record a real scan wrote, put it back under the pid of
/// a process that no longer exists — the OOM kill this whole mechanism exists to catch — and require
/// `comms doctor`'s enumerator to find it.
#[cfg(any(unix, windows))]
#[test]
fn a_leaked_breadcrumb_is_found_by_the_stale_scan_enumerator() {
    basemind::store::init_isolated_cache();
    let dir = repo();
    let root = dir.path();
    let cache_dir = workspace_cache_dir(root);

    let mut store = open(root);
    let mut peek = Peek {
        cache_dir: cache_dir.clone(),
        seen: None,
        files: 0,
    };
    scan_with_observer(
        root,
        &mut store,
        &ConfigV1::with_defaults(),
        ScanSource::WorkingTree,
        EmbedMode::Deferred,
        &ScanCancel::new(),
        &mut peek,
    )
    .expect("scan");
    let mut record = peek.seen.expect("a live scan leaves a record");

    // Past every platform's pid ceiling, so the enumerator can never mistake it for a live scan.
    record.pid = 0x7FFF_FFFE;
    fs::write(
        scan_evidence::inflight_path(&cache_dir),
        serde_json::to_vec(&record).expect("encode"),
    )
    .expect("replant the record");

    let stale = scan_evidence::stale_inflight_scans_in(&workspaces_dir());
    let found = stale
        .iter()
        .find(|scan| scan.cache_dir == cache_dir)
        .unwrap_or_else(|| panic!("the enumerator must report the leaked record; saw {stale:?}"));
    assert_eq!(found.record.root, root);
    assert_eq!(found.record.candidates, record.candidates);
    assert!(scan_evidence::clear(&cache_dir), "and it can be acknowledged");
}
