use std::io::ErrorKind;

use super::keeps_prior_entry;
use crate::config::{Config, ConfigV1};
use crate::index::view::IndexView;
use crate::scanner::{EmbedMode, FileStatus, ScanReport, ScanSource, scan};
use crate::store::{Store, VIEW_WORKING, init_isolated_cache};

fn read_failed(kind: ErrorKind) -> FileStatus {
    FileStatus::ReadFailed {
        kind,
        msg: String::new(),
    }
}

#[test]
fn only_a_verdict_on_the_file_lets_the_stale_purge_drop_its_entry() {
    assert!(keeps_prior_entry(&read_failed(ErrorKind::PermissionDenied)));
    assert!(keeps_prior_entry(&read_failed(ErrorKind::Other)));
    assert!(keeps_prior_entry(&FileStatus::ParseTimedOut));
    assert!(keeps_prior_entry(&FileStatus::ExtractFailed { msg: String::new() }));
    assert!(!keeps_prior_entry(&read_failed(ErrorKind::NotFound)));
    assert!(!keeps_prior_entry(&read_failed(ErrorKind::InvalidInput)));
    assert!(!keeps_prior_entry(&FileStatus::SkippedTooLarge { size: 1 }));
    assert!(!keeps_prior_entry(&FileStatus::SkippedBinary));
    assert!(!keeps_prior_entry(&FileStatus::SkippedNonUtf8));
    assert!(!keeps_prior_entry(&FileStatus::SkippedNoLang));
    assert!(!keeps_prior_entry(&FileStatus::Removed));
    #[cfg(feature = "documents")]
    {
        assert!(keeps_prior_entry(&FileStatus::DocTimedOut));
        assert!(keeps_prior_entry(&FileStatus::DocTombstoned));
    }
}

fn config() -> Config {
    let mut config = ConfigV1::with_defaults();
    config.documents.exclude = Vec::new();
    config.documents.embed = false;
    config.code_search.embed = false;
    config
}

fn full_scan(root: &std::path::Path, store: &mut Store, config: &Config) -> ScanReport {
    scan(
        root,
        store,
        config,
        ScanSource::WorkingTree,
        EmbedMode::Inline,
        crate::scan_retrieval::Retrieval::inert(),
    )
    .expect("scan")
}

/// Make `path` unreadable; `false` when the process can read it anyway (running as root), where
/// the read failure these tests need cannot be produced.
#[cfg(unix)]
fn lock(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000)).expect("chmod 000");
    std::fs::read(path).is_err()
}

#[cfg(unix)]
fn unlock(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).expect("chmod 644");
}

fn alpha_symbols(store: &Store) -> usize {
    store
        .index_view()
        .expect("index")
        .symbols_by_name_lookup("alpha", 4)
        .len()
}

/// A read refused for a moment (an editor's save window, a permission race) is not a deletion: the
/// file's rows survive both the full scan's stale purge and the incremental path's.
#[cfg(unix)]
#[test]
fn a_file_that_cannot_be_read_keeps_its_indexed_rows() {
    init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let file = root.join("alpha.rs");
    std::fs::write(&file, b"pub fn alpha() -> u32 { 1 }\n").expect("write");
    let config = config();
    let mut store = Store::open(root, VIEW_WORKING).expect("open");
    assert_eq!(full_scan(root, &mut store, &config).stats.updated, 1);
    assert_eq!(alpha_symbols(&store), 1);

    // A new mtime and size, so the unchanged shortcut cannot answer without reading the file.
    std::fs::write(&file, b"pub fn alpha() -> u32 { 2 }\n\n").expect("rewrite");
    if !lock(&file) {
        return;
    }
    let report = full_scan(root, &mut store, &config);
    assert_eq!(report.stats.read_failed, 1, "the failure is reported");
    assert_eq!(report.stats.removed, 0, "and is not taken for a deletion");
    assert!(store.lookup("alpha.rs").is_some());
    assert_eq!(alpha_symbols(&store), 1);

    let report = crate::scanner::scan_paths(
        root,
        &mut store,
        &config,
        std::slice::from_ref(&file),
        EmbedMode::Inline,
        crate::scan_retrieval::Retrieval::inert(),
    )
    .expect("rescan");
    assert_eq!(report.stats.read_failed, 1);
    assert_eq!(report.stats.removed, 0);
    assert!(store.lookup("alpha.rs").is_some());
    assert_eq!(alpha_symbols(&store), 1);

    unlock(&file);
    let report = full_scan(root, &mut store, &config);
    assert_eq!(
        report.stats.updated, 1,
        "the next readable scan indexes the new content"
    );
}

/// One file whose extraction panics fails alone: the scan finishes, reports it, and indexes the
/// rest.
#[test]
fn a_file_whose_extraction_panics_is_reported_and_the_scan_completes() {
    init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::write(root.join("alpha.rs"), b"pub fn alpha() -> u32 { 1 }\n").expect("write");
    std::fs::write(
        root.join(crate::scanner_file::TEST_PANIC_FILE),
        b"pub fn beta() -> u32 { 2 }\n",
    )
    .expect("write");
    let mut store = Store::open(root, VIEW_WORKING).expect("open");
    let report = full_scan(root, &mut store, &config());
    assert_eq!(report.stats.updated, 1);
    assert_eq!(report.stats.extract_failed, 1);
    assert_eq!(alpha_symbols(&store), 1);
}
