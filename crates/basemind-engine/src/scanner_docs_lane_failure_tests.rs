//! The document lane under a read failure. A retrieval-layer test: it scans with this build's
//! document lane, which the core scan tests do not reach.

use crate::config::{Config, ConfigV1};
use crate::scanner::{EmbedMode, ScanReport, ScanSource, scan};
use crate::store::{Store, VIEW_WORKING, init_isolated_cache};

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
        crate::scanner_vectors::test_retrieval(),
    )
    .expect("scan")
}

/// Make `path` unreadable; `false` when the process can read it anyway (running as root), where
/// the read failure this test needs cannot be produced.
fn lock(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000)).expect("chmod 000");
    std::fs::read(path).is_err()
}

fn unlock(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).expect("chmod 644");
}

/// The document lane keeps its entry on the same terms as a code file
/// (`scanner_drive::failure_tests`).
#[test]
fn a_document_that_cannot_be_read_keeps_its_entry() {
    init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let file = root.join("notes.md");
    std::fs::write(&file, b"# Notes\n\nPhotosynthesis turns light into sugar.\n").expect("write");
    let config = config();
    let mut store = Store::open(root, VIEW_WORKING).expect("open");
    assert_eq!(full_scan(root, &mut store, &config).stats.docs_indexed, 1);
    assert!(store.lookup_doc("notes.md").is_some());

    std::fs::write(&file, b"# Notes\n\nChlorophyll absorbs red and blue light.\n\n").expect("rewrite");
    if !lock(&file) {
        return;
    }
    let report = full_scan(root, &mut store, &config);
    assert_eq!(report.stats.read_failed, 1);
    assert_eq!(report.stats.removed, 0);
    assert!(store.lookup_doc("notes.md").is_some());

    let report = crate::scanner::scan_paths(
        root,
        &mut store,
        &config,
        std::slice::from_ref(&file),
        EmbedMode::Inline,
        crate::scanner_vectors::test_retrieval(),
    )
    .expect("rescan");
    assert_eq!(report.stats.removed, 0);
    assert!(store.lookup_doc("notes.md").is_some());
    unlock(&file);
}
