//! An unchanged document is not read again: the document lane skips it on its recorded size and
//! mtime, as the code lane does, instead of reading and hashing every document on every scan.

#![cfg(all(feature = "documents", unix))]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, SystemTime};

use basemind::config::ConfigV1;
use basemind::scanner::{CollectObserver, EmbedMode, FileStatus, ScanCancel, ScanSource, scan_with_observer};
use basemind::store::Store;

fn scan_status(root: &Path, cfg: &ConfigV1, rel: &str) -> FileStatus {
    let mut store = Store::open(root, basemind::store::VIEW_WORKING).expect("open store");
    let mut observer = CollectObserver::new();
    scan_with_observer(
        root,
        &mut store,
        cfg,
        ScanSource::WorkingTree,
        EmbedMode::Inline,
        &ScanCancel::new(),
        &mut observer,
        basemind::retrieval_wiring::backend(),
    )
    .expect("scan");
    observer
        .results()
        .iter()
        .find(|r| r.path == rel)
        .map(|r| r.status.clone())
        .unwrap_or_else(|| panic!("{rel} not reported"))
}

#[test]
fn an_unchanged_document_is_not_read_again() {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let mut cfg = ConfigV1::with_defaults();
    cfg.documents.embed = false;
    let doc = root.join("notes.md");
    fs::write(&doc, "# Notes\n\nThe quick brown fox jumps over the lazy dog.\n").expect("write");
    // Older than the racy window, so the recorded stamp is trusted.
    let old = SystemTime::now() - Duration::from_secs(120);
    fs::File::options()
        .write(true)
        .open(&doc)
        .expect("open")
        .set_modified(old)
        .expect("set mtime");

    let first = scan_status(root, &cfg, "notes.md");
    assert!(
        matches!(first, FileStatus::DocIndexed { .. }),
        "control: indexed first, got {first:?}"
    );

    // Unreadable but with the same size and mtime: only a scan that reads the file notices.
    fs::set_permissions(&doc, fs::Permissions::from_mode(0o000)).expect("chmod");
    let second = scan_status(root, &cfg, "notes.md");
    fs::set_permissions(&doc, fs::Permissions::from_mode(0o644)).expect("chmod back");
    assert!(matches!(second, FileStatus::DocUnchanged), "got {second:?}");
}
