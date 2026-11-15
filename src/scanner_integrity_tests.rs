use super::*;
use crate::config::ConfigV1;
use crate::index::view::IndexView;
use crate::scanner::{EmbedMode, ScanReport, ScanSource, scan};
use crate::store::{VIEW_WORKING, init_isolated_cache};

fn one_file_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let status = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(dir.path())
        .status()
        .expect("run git init");
    assert!(status.success());
    std::fs::write(dir.path().join("alpha.rs"), b"pub fn alpha() -> u32 { 1 }\n").expect("write");
    dir
}

fn full_scan(root: &std::path::Path, store: &mut Store) -> ScanReport {
    scan(
        root,
        store,
        &ConfigV1::with_defaults(),
        ScanSource::WorkingTree,
        EmbedMode::Deferred,
    )
    .expect("a non-fatal commit failure degrades the scan, it does not fail it")
}

/// A commit that fails without a crash loses its batch just as a power cut does. The file map must
/// not record the batch's files, or every later scan would skip them as unchanged over an index that
/// never received their rows.
#[test]
fn a_file_whose_index_commit_failed_is_left_out_of_the_file_map_and_indexed_by_the_next_scan() {
    init_isolated_cache();
    let dir = one_file_repo();
    let root = dir.path();
    let mut store = Store::open(root, VIEW_WORKING).expect("open");
    store.index_db.as_ref().expect("index").fault.inject_commit_failures(1);

    let report = full_scan(root, &mut store);
    assert_eq!(report.stats.updated, 0);
    assert_eq!(
        report.stats.extract_failed, 1,
        "the lost file is reported, not counted as indexed"
    );
    assert!(store.lookup("alpha.rs").is_none());
    assert!(store.index_view().expect("index").symbols_index_is_empty());

    drop(store);
    let mut store = Store::open(root, VIEW_WORKING).expect("reopen");
    assert!(
        store.index.files.is_empty(),
        "the flushed file map does not claim the file"
    );

    let report = full_scan(root, &mut store);
    assert_eq!(report.stats.updated, 1, "the next scan indexes the file");
    assert!(store.lookup("alpha.rs").is_some());
    assert_eq!(
        store
            .index_view()
            .expect("index")
            .symbols_by_name_lookup("alpha", 4)
            .len(),
        1
    );
}

/// The resolve pass writes its rows after the code map was sealed, and the file map already calls
/// their files unchanged, so nothing would rewrite a row a power cut took. Both scan entry points
/// must leave the journal synced past them.
#[test]
fn rows_written_after_the_seal_are_synced_before_a_scan_returns() {
    init_isolated_cache();
    let dir = one_file_repo();
    let root = dir.path();
    let outer = root.join("outer.py");
    std::fs::write(&outer, b"def outer(a):\n    count = 1\n    return count + a\n").expect("write");
    let mut store = Store::open(root, VIEW_WORKING).expect("open");

    full_scan(root, &mut store);
    let db = store.index_db.clone().expect("index");
    let resolved = db.resolution_rows().expect("rows");
    assert!(
        !resolved.is_empty(),
        "the resolve pass wrote rows after the seal, or this test proves nothing"
    );
    assert!(
        !db.has_unsynced_tail(),
        "a full scan syncs what it wrote after the seal"
    );

    std::fs::write(&outer, b"def outer(a):\n    total = 2\n    return total + total + a\n").expect("rewrite");
    let report = crate::scanner::scan_paths(
        root,
        &mut store,
        &ConfigV1::with_defaults(),
        &[outer],
        EmbedMode::Deferred,
    )
    .expect("incremental scan");
    assert_eq!(report.stats.updated, 1);
    assert_ne!(
        db.resolution_rows().expect("rows"),
        resolved,
        "the incremental resolve pass rewrote rows after the seal"
    );
    assert!(
        !db.has_unsynced_tail(),
        "an incremental scan syncs what it wrote after the seal"
    );
}

/// The in-place rebuild restarts the generation count, so the file map that listed every file is
/// replaced on disk before the scan walks anything: a scan that dies leaves no map to skip by.
#[test]
fn preflight_persists_the_emptied_file_map_of_an_index_it_rebuilds_in_place() {
    init_isolated_cache();
    let dir = one_file_repo();
    let root = dir.path();
    let mut store = Store::open(root, VIEW_WORKING).expect("open");
    full_scan(root, &mut store);
    let view_dir = store.view_dir.clone();
    let mut ahead = store.index.clone();
    drop(store);
    ahead.generation += 1;
    let manifest = view_dir.join(crate::store::INDEX_FILE);
    std::fs::write(&manifest, rmp_serde::to_vec_named(&ahead).expect("encode")).expect("forge the file map");

    let mut in_place = Store::open_read_only(root, VIEW_WORKING).expect("open read-only");
    assert_eq!(in_place.index.files.len(), 1);
    preflight(&mut in_place).expect("preflight");

    let on_disk = crate::store::read_index(&view_dir).expect("read").expect("a file map");
    assert!(on_disk.files.is_empty(), "no file map vouches for the emptied index");
    let db = in_place.index_db.as_ref().expect("index");
    assert!(db.view().symbols_index_is_empty());
    assert_eq!(on_disk.generation, db.scan_generation().expect("read"));
    assert_eq!(on_disk.generation, 1, "the emptied index starts a new count");
}

fn rotations(db: &IndexDb) -> u32 {
    db.fault.rotations.load(std::sync::atomic::Ordering::Acquire)
}

/// Neither a full scan nor an incremental batch of a small index flushes its memtables: there is no
/// sealed journal to release, and the flush would only delay the scan. What they wrote survives a
/// reopen.
#[test]
fn a_scan_that_sealed_no_journal_does_not_flush_the_memtables() {
    init_isolated_cache();
    let dir = one_file_repo();
    let root = dir.path();
    let mut store = Store::open(root, VIEW_WORKING).expect("open");

    full_scan(root, &mut store);
    let db = store.index_db.clone().expect("index");
    assert_eq!(rotations(&db), 0);
    assert!(!db.has_unsynced_tail());

    let alpha = root.join("alpha.rs");
    std::fs::write(&alpha, b"pub fn alpha() -> u32 { 2 }\n").expect("rewrite");
    let report = crate::scanner::scan_paths(
        root,
        &mut store,
        &ConfigV1::with_defaults(),
        &[alpha],
        EmbedMode::Deferred,
    )
    .expect("incremental scan");
    assert_eq!(report.stats.updated, 1);
    assert_eq!(rotations(&db), 0);

    drop(db);
    drop(store);
    let store = Store::open(root, VIEW_WORKING).expect("reopen");
    assert_eq!(
        store
            .index_view()
            .expect("index")
            .symbols_by_name_lookup("alpha", 4)
            .len(),
        1,
        "the rows survive a reopen"
    );
}

/// Two installed basemind minors share one cache. In a flat blob directory each overwrote the
/// other's blobs as "not current" while the other side's file map kept calling the files unchanged.
/// A scan writes only inside its own schema's namespace.
#[test]
fn a_scan_never_overwrites_a_blob_another_basemind_version_wrote() {
    use crate::extract::SCHEMA_VER;

    init_isolated_cache();
    let repo = one_file_repo();
    let hash_hex = crate::hashing::hex(&crate::hashing::hash_bytes(b"pub fn alpha() -> u32 { 1 }\n"));
    let blobs_root = crate::store_layout::global_blobs_root();
    let name = format!("{hash_hex}.fm.msgpack");
    let foreign = [
        blobs_root.join(&name),
        blobs_root.join(format!("v{}", SCHEMA_VER + 1)).join(&name),
    ];
    for path in &foreign {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, b"another version's blob").expect("plant");
    }

    let mut store = Store::open(repo.path(), VIEW_WORKING).expect("open");
    let report = full_scan(repo.path(), &mut store);

    assert_eq!(report.stats.updated, 1);
    for path in &foreign {
        assert_eq!(
            std::fs::read(path).expect("read"),
            b"another version's blob",
            "{} was overwritten",
            path.display()
        );
    }
    let own = store.blob_path_fm_hex(&hash_hex);
    assert!(
        own.starts_with(blobs_root.join(format!("v{SCHEMA_VER}"))),
        "{}",
        own.display()
    );
    assert!(store.read_l1_by_hex(&hash_hex).expect("read own blob").is_some());
}

/// Blobs are written without an fsync and the file map with one, so after a power loss the map can
/// name a blob whose bytes never reached the disk. The breadcrumb a scan leaves when it never
/// finished makes the next full pass validate blobs instead of trusting them for existing.
#[test]
fn the_pass_after_an_unclean_shutdown_rebuilds_a_torn_blob() {
    init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::write(root.join("torn.rs"), b"pub fn torn_blob_fixture() -> u32 { 7 }\n").expect("write");
    let mut store = Store::open(root, VIEW_WORKING).expect("open");
    assert_eq!(full_scan(root, &mut store).stats.updated, 1);
    let hash_hex = store.lookup("torn.rs").expect("entry").hash_hex.clone();
    let blob = store.blob_path_fm_hex(&hash_hex);

    std::fs::write(&blob, b"").expect("tear the blob");
    assert_eq!(
        full_scan(root, &mut store).stats.skipped_unchanged,
        1,
        "an ordinary pass trusts the blob for existing: the hot path reads no blob"
    );

    // A scan that died without running a destructor leaves its breadcrumb behind.
    std::mem::forget(crate::scan_evidence::ScanBreadcrumb::begin_in(&store.basemind_dir, root).expect("crumb"));
    let report = full_scan(root, &mut store);

    assert_eq!(report.stats.updated, 1, "the torn blob is rebuilt");
    assert!(store.read_l1_by_hex(&hash_hex).expect("read").is_some());
    assert!(!crate::scan_evidence::blobs_need_verifying(&store.basemind_dir));
    assert_eq!(full_scan(root, &mut store).stats.skipped_unchanged, 1);
}

/// The rebuild empties `files` only. A process that died after writing that emptied file map left a
/// store with no code files but its documents, which read as scanned, so an incremental scan
/// indexed the given paths and nothing else. The pending rebuild is in the file map itself, and the
/// incremental entry point honours it.
#[test]
fn a_rebuild_interrupted_after_the_emptied_file_map_is_finished_by_an_incremental_scan() {
    init_isolated_cache();
    let dir = one_file_repo();
    let root = dir.path();
    {
        let mut store = Store::open(root, VIEW_WORKING).expect("open");
        full_scan(root, &mut store);
        // What the rebuild leaves on disk before its scan runs, in a store that also holds documents.
        store.remove("alpha.rs");
        store.index.rebuild_pending = true;
        store.upsert_doc(
            "notes.pdf",
            crate::store::DocEntry {
                hash_hex: "h".to_string(),
                embedding_preset: String::new(),
                size_bytes: 1,
                mtime: 0,
                embedded: false,
                embed_attempted: false,
                config_digest: String::new(),
            },
        );
        store.flush().expect("flush");
    }

    let mut store = Store::open(root, VIEW_WORKING).expect("reopen");
    assert!(
        store.needs_full_scan(),
        "documents in the map must not hide a pending rebuild"
    );

    crate::scanner::scan_paths(root, &mut store, &ConfigV1::with_defaults(), &[], EmbedMode::Deferred)
        .expect("scan_paths");

    assert!(store.lookup("alpha.rs").is_some(), "the whole tree is indexed again");
    assert!(!store.index.rebuild_pending);
    let on_disk = crate::store::read_index(&store.view_dir)
        .expect("read")
        .expect("file map");
    assert!(!on_disk.rebuild_pending, "the finished rebuild is recorded on disk");
}

/// The purge of a deleted file used to drop its file-map entry and discard the result of the index
/// commit, so a failed commit left rows no later scan would ever revisit.
#[test]
fn a_deleted_file_stays_in_the_file_map_when_its_index_rows_could_not_be_removed() {
    init_isolated_cache();
    let dir = one_file_repo();
    let root = dir.path();
    let mut store = Store::open(root, VIEW_WORKING).expect("open");
    full_scan(root, &mut store);
    std::fs::remove_file(root.join("alpha.rs")).expect("delete");
    store.index_db.as_ref().expect("index").fault.inject_commit_failures(1);

    let failed = scan(
        root,
        &mut store,
        &ConfigV1::with_defaults(),
        ScanSource::WorkingTree,
        EmbedMode::Deferred,
    );

    assert!(failed.is_err(), "the failed purge commit must fail the scan");
    assert!(store.lookup("alpha.rs").is_some(), "the entry stays while its rows do");

    let report = full_scan(root, &mut store);
    assert_eq!(report.stats.removed, 1, "the next scan purges it");
    assert!(store.lookup("alpha.rs").is_none());
    assert!(store.index_view().expect("index").symbols_index_is_empty());
}
