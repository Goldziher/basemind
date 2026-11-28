//! The blob a scan reuses and the blob sweep's grace.

use super::tests::{full_scan, one_file_repo};
use crate::config::ConfigV1;
use crate::scanner::{EmbedMode, ScanError, ScanSource, scan};
use crate::store::{Store, VIEW_WORKING, init_isolated_cache, read_status_sidecar};

fn age(blob: &std::path::Path) -> std::time::SystemTime {
    let aged = std::time::SystemTime::now() - std::time::Duration::from_secs(7 * 3600);
    std::fs::File::options()
        .write(true)
        .open(blob)
        .and_then(|file| file.set_modified(aged))
        .expect("age the blob");
    aged
}

fn mtime(blob: &std::path::Path) -> std::time::SystemTime {
    std::fs::metadata(blob).and_then(|meta| meta.modified()).expect("mtime")
}

/// The blob sweep keeps an unreferenced blob only within its grace, judged by mtime. A scan that
/// reused an old blob no file map referenced left it to be swept before the map named it; the
/// reuse now refreshes the blob's mtime.
#[test]
fn a_blob_reused_for_a_new_path_is_kept_young_for_the_sweep() {
    init_isolated_cache();
    let dir = one_file_repo();
    let root = dir.path();
    // Content of its own: blobs are shared by every test, and another one ages or touches its own. ~keep
    std::fs::write(root.join("alpha.rs"), b"pub fn reused_for_a_new_path() -> u32 { 1 }\n").expect("write");
    let mut store = Store::open(root, VIEW_WORKING).expect("open");
    full_scan(root, &mut store);
    let hash_hex = store.lookup("alpha.rs").expect("entry").hash_hex.clone();
    let blob = store.blob_path_fm_hex(&hash_hex);
    let content = std::fs::read(root.join("alpha.rs")).expect("read");
    std::fs::remove_file(root.join("alpha.rs")).expect("delete");
    full_scan(root, &mut store);
    assert!(!store.hash_is_held(&hash_hex), "no file map references the blob now");
    let aged = age(&blob);
    std::fs::write(root.join("beta.rs"), content).expect("the same content again");

    let report = full_scan(root, &mut store);

    assert_eq!(report.stats.reused_extraction, 1);
    assert!(
        mtime(&blob) > aged + std::time::Duration::from_secs(3600),
        "the reused blob is young again"
    );
}

/// A blob the file map already references is safe from the sweep, so reusing it costs no write:
/// that reuse runs for every file of a rebuild and of the embedding pass.
#[test]
fn a_blob_the_file_map_already_references_is_not_touched_on_reuse() {
    init_isolated_cache();
    let dir = one_file_repo();
    let root = dir.path();
    std::fs::write(root.join("alpha.rs"), b"pub fn referenced_on_reuse() -> u32 { 1 }\n").expect("write");
    let mut store = Store::open(root, VIEW_WORKING).expect("open");
    full_scan(root, &mut store);
    let hash_hex = store.lookup("alpha.rs").expect("entry").hash_hex.clone();
    let blob = store.blob_path_fm_hex(&hash_hex);
    let aged = age(&blob);
    std::fs::copy(root.join("alpha.rs"), root.join("beta.rs")).expect("copy");

    let report = full_scan(root, &mut store);

    assert_eq!(report.stats.reused_extraction, 1, "the copy reused the blob");
    assert!(
        mtime(&blob) < aged + std::time::Duration::from_secs(3600),
        "a referenced blob is left as it was"
    );
}

/// A blob write that finds the disk full stops the scan as an index commit that finds it full does:
/// the scan fails with the fault and its status reads partial. It used to count the file as one
/// failed extraction and finish a scan that read complete over blobs it never wrote.
#[test]
fn a_full_disk_under_a_blob_write_aborts_the_scan_and_its_status_reads_partial() {
    init_isolated_cache();
    let dir = one_file_repo();
    let root = dir.path();
    // Content of its own: a blob another test already wrote would not be written again. ~keep
    std::fs::write(
        root.join("alpha.rs"),
        b"pub fn blob_write_finds_the_disk_full() -> u32 { 1 }\n",
    )
    .expect("write");
    let mut store = Store::open(root, VIEW_WORKING).expect("open");
    store.index_db.as_ref().expect("index").fault.inject_blob_write_full(1);
    crate::disk_space::fake_free_bytes(Some(u64::MAX));

    let outcome = scan(
        root,
        &mut store,
        &ConfigV1::with_defaults(),
        ScanSource::WorkingTree,
        EmbedMode::Deferred,
        crate::scan_retrieval::Retrieval::inert(),
    );
    crate::disk_space::fake_free_bytes(None);

    match outcome {
        Err(ScanError::IndexFault(reason)) => assert!(reason.starts_with("disk full: "), "{reason}"),
        other => panic!("expected the full disk to abort the scan, got {other:?}"),
    }
    let status = read_status_sidecar(&store.basemind_dir).expect("an aborted scan records its status");
    assert!(status.partial, "{status:?}");
    assert!(
        store.lookup("alpha.rs").is_none(),
        "a file whose blob was not written is not indexed"
    );
}
