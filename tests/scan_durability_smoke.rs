//! Durability contract for `scanner::scan`: the code map survives a broken optional lane.
//!
//! The optional post-extraction lanes (resolve / doc batches / code batches / bm25 stats) run
//! *after* the code map is complete. Two real failures took the whole scan down with them: the
//! `stack-graphs` stitcher panicking on a rayon worker, and the embedding-model download hanging
//! forever on a blackholed IPv6 route (killed by the operator). Both left `index.msgpack` unwritten
//! — `file_count: 0` on disk next to gigabytes of committed blobs, and a full re-scan every launch.
//!
//! These tests pin both halves of the fix:
//! - a lane that PANICS is contained: `scan` returns `Ok` and the code map is on disk;
//! - a lane that never returns (stood in for by an `abort` mid-lane — what a SIGKILL does) still
//!   leaves the code map on disk, because the flush happens BEFORE the lanes run.
//!
//! The second half covers the index side of the same barrier: the Fjall index is sealed (generation
//! stamped, journal fsynced) before the file map is written, and an index stamped below its file map
//! is rebuilt from the blobs.
//!
//! The lane fault is injected through the `test-support`-only seam in `basemind::scanner_lanes`; neither
//! the environment variable nor the branch exists in a production build.

use std::fs;
use std::path::Path;
use std::process::Command;

use basemind::config::ConfigV1;
use basemind::scanner::{EmbedMode, ScanSource, scan};
use basemind::scanner_lanes::{LANE_RESOLVE, TEST_FAULT_LANE_ENV};
use basemind::store::{INDEX_FILE, Store, VIEW_WORKING};
use tempfile::TempDir;

/// Write a small multi-file Rust tree so the scan has a real code map to lose. `git init` makes it
/// a project root the allow-list accepts (issue #62); `.git/` is invisible to the scanner, so the
/// `updated == 3` counts below are unchanged.
fn seed_repo(root: &Path) {
    let status = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(root)
        .status()
        .expect("run git init");
    assert!(status.success(), "git init succeeds in {root:?}");
    fs::write(root.join("alpha.rs"), b"pub fn alpha() -> u32 { 1 }\n").unwrap();
    fs::write(root.join("beta.rs"), b"pub fn beta() -> u32 { alpha() }\n").unwrap();
    fs::write(root.join("gamma.rs"), b"pub struct Gamma { pub x: u32 }\n").unwrap();
}

/// Run in a child `basemind scan` so the fault variable reaches that process alone: set on this
/// one, it would inject the panic into every scan the other tests of this binary run meanwhile.
#[test]
fn panicking_optional_lane_leaves_the_code_map_flushed_and_queryable() {
    basemind::store::init_isolated_cache();
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    seed_repo(root);

    let output = Command::new(env!("CARGO_BIN_EXE_basemind"))
        .args(["--root", root.to_str().unwrap(), "scan", "--quiet"])
        .env(TEST_FAULT_LANE_ENV, LANE_RESOLVE)
        .output()
        .expect("run basemind scan");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "a panicking optional lane must not fail the scan; stderr: {stderr}"
    );
    assert!(
        stderr.contains(&format!("injected fault in optional scan lane `{LANE_RESOLVE}`")),
        "the fault must actually fire in the child; stderr: {stderr}"
    );

    let store = Store::open(root, VIEW_WORKING).expect("open store after the degraded scan");
    let index_path = store.view_dir.join(INDEX_FILE);
    assert!(index_path.exists(), "the code map must be flushed to {index_path:?}");
    assert_eq!(store.index.files.len(), 3, "every source file is still extracted");
    let hits = basemind::query::search_symbols(&store, "beta", None).unwrap();
    assert_eq!(hits.len(), 1, "the code map stays queryable after a degraded lane");
}

/// A lane killed mid-flight (hang → SIGKILL; here: `abort` inside the lane) must still leave the
/// code map on disk. This is what pins the *ordering*: only an `index.msgpack` flushed BEFORE the
/// lanes can survive a process that never reaches the end of `scan`.
#[test]
fn code_map_survives_a_lane_that_kills_the_process() {
    basemind::store::init_isolated_cache();
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    seed_repo(root);

    let output = Command::new(env!("CARGO_BIN_EXE_basemind"))
        .args(["--root", root.to_str().unwrap(), "scan", "--quiet"])
        .env(TEST_FAULT_LANE_ENV, format!("{LANE_RESOLVE}:abort"))
        .output()
        .expect("run basemind scan");
    assert!(
        !output.status.success(),
        "the injected abort must actually kill the scan process (status: {:?})",
        output.status
    );

    let store = Store::open(root, VIEW_WORKING).expect("open store after the killed scan");
    let index_path = store.view_dir.join(INDEX_FILE);
    assert!(
        index_path.exists(),
        "the code map must already be on disk when the lane kills the process; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        store.index.files.len(),
        3,
        "the code map persisted by the killed scan must not be empty"
    );
}

/// Scan `seed_repo` into a fresh store and return it with the scan complete.
fn scanned_store(root: &Path) -> Store {
    seed_repo(root);
    let mut store = Store::open(root, VIEW_WORKING).expect("open store");
    let report = scan(
        root,
        &mut store,
        &ConfigV1::with_defaults(),
        ScanSource::WorkingTree,
        EmbedMode::Deferred,
        basemind::retrieval_wiring::backend(),
    )
    .expect("scan");
    assert_eq!(report.stats.updated, 3);
    store
}

/// Rewrite the working view's scan-generation stamp in the group index behind basemind's back, as a
/// journal that lost its tail would leave it. Must run with no `Store` of the group open.
fn forge_index_generation(root: &Path, generation: u64) {
    let view_dir = basemind::store::workspace_cache_dir(root)
        .join(basemind::store::VIEWS_DIR)
        .join(VIEW_WORKING);
    let view_id = file_map_on_disk(&view_dir).view_id;
    let db = fjall::Database::builder(basemind::store_layout::group_index_dir(root).join("index.fjall"))
        .open()
        .expect("open raw index");
    let meta = db
        .keyspace("meta", fjall::KeyspaceCreateOptions::default)
        .expect("meta keyspace");
    let mut key = basemind::index::scoped::view_prefix(view_id).to_vec();
    key.extend_from_slice(b"scan_generation");
    meta.insert(key, generation.to_be_bytes()).expect("forge the stamp");
    db.persist(fjall::PersistMode::SyncAll)
        .expect("persist the forged stamp");
}

/// The content key `rel`'s symbol and call rows are written under.
fn content_of(store: &Store, rel: &str) -> basemind::index::keys::ContentKey {
    let entry = store.lookup(rel).expect("indexed");
    basemind::index::keys::content_key(&entry.hash_hex, &entry.language, entry.extract_epoch)
}

/// Names of the symbols the Fjall index (not the blob store) holds for `name`.
fn indexed_symbol_paths(store: &Store, name: &str) -> Vec<String> {
    use basemind::index::view::IndexView;
    store
        .index_view()
        .expect("writer store has an index")
        .symbols_by_name_lookup(name, 16)
        .into_iter()
        .map(|(_, _, path, _)| path.to_string())
        .collect()
}

fn rescan(root: &Path, store: &mut Store) -> basemind::scanner::ScanReport {
    scan(
        root,
        store,
        &ConfigV1::with_defaults(),
        ScanSource::WorkingTree,
        EmbedMode::Deferred,
        basemind::retrieval_wiring::backend(),
    )
    .expect("rescan")
}

/// The file map is fsynced; index batches are not. After a power loss the file map can therefore
/// say "indexed, unchanged" about files whose rows never reached the disk. The two sides carry the
/// same generation, so an index stamped below its file map is rebuilt from the blobs.
#[test]
fn an_index_older_than_its_file_map_is_rebuilt_from_blobs() {
    basemind::store::init_isolated_cache();
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let store = scanned_store(root);
    let sealed = store.index.generation;
    assert!(sealed > 0, "a flushed file map records the generation it sealed");
    assert_eq!(
        store.index_db.as_ref().unwrap().scan_generation().unwrap(),
        sealed,
        "the index carries the same generation as the file map"
    );
    drop(store);

    forge_index_generation(root, sealed - 1);

    let mut reopened = Store::open(root, VIEW_WORKING).expect("reopen");
    assert!(
        reopened.index_rebuild_needed,
        "an index stamped below its file map must be scheduled for a rebuild"
    );
    let report = rescan(root, &mut reopened);
    assert_eq!(
        report.stats.updated, 3,
        "every file is re-indexed, none skipped as unchanged"
    );
    assert_eq!(report.stats.reused_extraction, 3, "from the blobs, without re-parsing");
    assert_eq!(indexed_symbol_paths(&reopened, "beta"), vec!["beta.rs".to_string()]);
    assert_eq!(
        basemind::query::file_outline(&reopened, "gamma.rs")
            .unwrap()
            .symbols
            .len(),
        1
    );
    assert_eq!(
        reopened.index_db.as_ref().unwrap().scan_generation().unwrap(),
        reopened.index.generation,
        "the rebuilt index and the file map are sealed together again"
    );

    drop(reopened);
    let settled = Store::open(root, VIEW_WORKING).expect("open after the rebuild");
    assert!(!settled.index_rebuild_needed, "a repaired index is not rebuilt again");
}

/// Partial loss: one file's rows are gone while the file map still vouches for the file.
///
/// Detected only through the generation. Rows that vanish while the two generations still agree are
/// NOT detectable: the stamp is the last write of a scan, so it proves the journal reached that point,
/// not that every row is still there. The second half of the test pins that limit.
#[test]
fn lost_rows_are_repaired_when_the_generation_fell_behind_and_only_then() {
    use basemind::path::RelPath;

    basemind::store::init_isolated_cache();
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let store = scanned_store(root);
    let drop_beta_rows = |store: &Store| {
        let mut writer = store.index_db.as_ref().unwrap().writer();
        writer.remove_file(&RelPath::from("beta.rs")).expect("stage the loss");
        writer
            .remove_content(&content_of(store, "beta.rs"))
            .expect("stage the loss");
        writer.commit().expect("commit the loss");
    };

    drop_beta_rows(&store);
    drop(store);
    let mut same_generation = Store::open(root, VIEW_WORKING).expect("reopen");
    assert!(!same_generation.index_rebuild_needed);
    let report = rescan(root, &mut same_generation);
    assert_eq!(
        report.stats.skipped_unchanged, 3,
        "the file map still vouches for beta.rs"
    );
    assert!(
        indexed_symbol_paths(&same_generation, "beta").is_empty(),
        "rows lost under an intact generation stay lost: this is the undetectable case"
    );
    let sealed = same_generation.index.generation;
    drop(same_generation);

    forge_index_generation(root, sealed - 1);
    let mut behind = Store::open(root, VIEW_WORKING).expect("reopen behind");
    assert!(behind.index_rebuild_needed);
    let report = rescan(root, &mut behind);
    assert_eq!(report.stats.updated, 3);
    assert_eq!(indexed_symbol_paths(&behind, "beta"), vec!["beta.rs".to_string()]);
    assert_eq!(indexed_symbol_paths(&behind, "alpha"), vec!["alpha.rs".to_string()]);
}

/// A file map from before the group index (no incarnation, no view id, no generation) vouches for
/// rows the group index never held: the writer open empties it and the next scan rebuilds the view.
#[test]
fn a_file_map_from_before_the_group_index_is_rebuilt() {
    #[derive(serde::Serialize)]
    struct PreGroupIndex<'a> {
        schema_ver: u16,
        files: &'a ahash::AHashMap<basemind::path::RelPath, basemind::store::FileEntry>,
        blob_layout: u8,
    }

    basemind::store::init_isolated_cache();
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let store = scanned_store(root);
    let manifest = store.view_dir.join(INDEX_FILE);
    let old = rmp_serde::to_vec_named(&PreGroupIndex {
        schema_ver: store.index.schema_ver,
        files: &store.index.files,
        blob_layout: basemind::store_layout::BLOB_LAYOUT,
    })
    .unwrap();
    drop(store);
    fs::write(&manifest, old).unwrap();

    let mut reopened = Store::open(root, VIEW_WORKING).expect("open the older file map");
    assert!(
        reopened.index_rebuild_needed,
        "the map names no incarnation of this group index"
    );
    assert!(reopened.index.files.is_empty());
    let report = rescan(root, &mut reopened);
    assert_eq!(report.stats.updated, 3);
    assert_eq!(indexed_symbol_paths(&reopened, "beta"), vec!["beta.rs".to_string()]);
    assert!(reopened.index.generation > 0, "the next flush stamps both sides");
}

/// The index is sealed (generation stamped and journal fsynced in one batch) BEFORE the file map is
/// written. Observed by making the file-map write fail: the index already carries the next
/// generation while the file map on disk still carries the previous one.
#[test]
fn the_index_is_sealed_before_the_file_map_is_written() {
    basemind::store::init_isolated_cache();
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let mut store = scanned_store(root);
    let sealed = store.index.generation;

    fs::create_dir(store.view_dir.join(format!("{INDEX_FILE}.tmp"))).unwrap();
    store
        .flush()
        .expect_err("the file map cannot be written over a directory");

    assert_eq!(
        store.index_db.as_ref().unwrap().scan_generation().unwrap(),
        sealed + 1,
        "the index was sealed before the file-map write was attempted"
    );
    let on_disk: basemind::store::Index =
        rmp_serde::from_slice(&fs::read(store.view_dir.join(INDEX_FILE)).unwrap()).unwrap();
    assert_eq!(on_disk.generation, sealed, "the previous file map is still in place");

    drop(store);
    let reopened = Store::open(root, VIEW_WORKING).expect("reopen");
    assert!(
        !reopened.index_rebuild_needed,
        "an index ahead of its file map is the safe direction and is left alone"
    );
}

/// Lose `beta.rs`'s index rows and leave the index one generation behind its file map, as a journal
/// that lost its tail would. Returns the view directory and the generation the file map carries
/// (three scans, so at least 3: a rebuilt index, sealed once for its emptied file map and once by the
/// rebuild scan, is distinguishable from it).
fn scanned_store_with_an_index_behind(root: &Path) -> (std::path::PathBuf, u64) {
    let mut store = scanned_store(root);
    rescan(root, &mut store);
    rescan(root, &mut store);
    let mut writer = store.index_db.as_ref().unwrap().writer();
    writer
        .remove_file(&basemind::path::RelPath::from("beta.rs"))
        .expect("stage the loss");
    writer
        .remove_content(&content_of(&store, "beta.rs"))
        .expect("stage the loss");
    writer.commit().expect("commit the loss");
    let (view_dir, sealed) = (store.view_dir.clone(), store.index.generation);
    drop(store);
    forge_index_generation(root, sealed - 1);
    (view_dir, sealed)
}

/// A store opened read-only never ran the writer open's generation check, yet a scan can run into it
/// in place (`Workspace::rescan`, behind `index_rescan`). Skipping the file map's files as unchanged and then
/// sealing past the file map would make the two sides agree over the lost rows for good.
#[test]
fn a_scan_into_a_read_only_opened_store_rebuilds_an_index_behind_its_file_map() {
    basemind::store::init_isolated_cache();
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let (_, sealed) = scanned_store_with_an_index_behind(root);

    let mut in_place = Store::open_read_only(root, VIEW_WORKING).expect("open read-only");
    assert!(in_place.index_db.is_none(), "the read-only open holds no group handle");
    assert_eq!(in_place.index.files.len(), 3, "and never ran the writer open's check");
    in_place.ensure_dirs().unwrap();

    let report = rescan(root, &mut in_place);
    assert_eq!(report.stats.skipped_unchanged, 0, "no file is skipped as unchanged");
    assert_eq!(report.stats.updated, 3);
    assert_eq!(report.stats.reused_extraction, 3, "from the blobs, without re-parsing");
    assert_eq!(indexed_symbol_paths(&in_place, "beta"), vec!["beta.rs".to_string()]);
    assert_eq!(indexed_symbol_paths(&in_place, "alpha"), vec!["alpha.rs".to_string()]);
    assert_eq!(
        in_place.index_db.as_ref().unwrap().scan_generation().unwrap(),
        in_place.index.generation
    );
    assert!(
        in_place.index.generation < sealed,
        "the rebuilt index starts a new count"
    );

    drop(in_place);
    let settled = Store::open(root, VIEW_WORKING).expect("writer open after the rebuild");
    assert!(!settled.index_rebuild_needed);
    assert_eq!(settled.index.files.len(), 3);
}

/// After a power loss the fsynced file map can vouch for a generation the index journal lost. A
/// read-only open in a process that holds the group index (here through another view's handle)
/// reports the rescan owed, and does not serve the view's rows that the file map no longer matches.
#[test]
fn a_read_only_open_of_an_index_behind_its_file_map_reports_a_rescan_owed() {
    basemind::store::init_isolated_cache();
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    scanned_store_with_an_index_behind(root);
    // Another view's handle keeps the group open in this process once its store, and with it the
    // workspace lock, is gone. ~keep
    let holder = Store::open(root, basemind::store::VIEW_STAGED).expect("another view opens the group");
    let _held = holder.index_db.clone().expect("a handle on the group");
    drop(holder);

    let store = Store::open_read_only(root, VIEW_WORKING).expect("open read-only");
    assert!(
        store.index_db.is_none(),
        "rows the file map does not match are not served"
    );
    assert!(store.rescan_owed(), "with the rescan it owes reported");
    assert!(store.needs_full_scan());
}

/// An incremental scan cannot rebuild the index from the paths it was handed, so it runs the full
/// pass instead of re-indexing one file and sealing over the rest.
#[test]
fn an_incremental_scan_over_an_index_behind_its_file_map_runs_the_full_rebuild() {
    basemind::store::init_isolated_cache();
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    scanned_store_with_an_index_behind(root);

    let mut in_place = Store::open_read_only(root, VIEW_WORKING).expect("open read-only");
    in_place.ensure_dirs().unwrap();
    let report = basemind::scanner::scan_paths(
        root,
        &mut in_place,
        &ConfigV1::with_defaults(),
        &[root.join("alpha.rs")],
        EmbedMode::Deferred,
        basemind::retrieval_wiring::backend(),
    )
    .expect("incremental scan");

    assert_eq!(report.stats.updated, 3, "every file, not only the one named");
    assert_eq!(indexed_symbol_paths(&in_place, "beta"), vec!["beta.rs".to_string()]);
    assert_eq!(in_place.index.files.len(), 3);
}

/// The choke point behind both scans: a file map is never written over an index it is ahead of.
#[test]
fn flush_refuses_to_seal_an_index_behind_its_file_map() {
    basemind::store::init_isolated_cache();
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let mut store = scanned_store(root);
    let sealed = store.index.generation;
    store.index.generation = sealed + 5;

    let refused = store.flush().expect_err("the index is behind the file map");
    assert!(
        refused.to_string().contains("behind its file map"),
        "unexpected error: {refused}"
    );
    assert_eq!(
        store.index_db.as_ref().unwrap().scan_generation().unwrap(),
        sealed,
        "nothing was stamped"
    );
    assert_eq!(file_map_on_disk(&store.view_dir).generation, sealed, "nor written");
}

/// A crash between setting the index aside and deleting it leaves no `index.fjall` at all. The next
/// writer open must start a fresh index, rebuild it and remove what was set aside.
#[test]
fn a_half_discarded_index_opens_and_rebuilds() {
    basemind::store::init_isolated_cache();
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    drop(scanned_store(root));
    let group_dir = basemind::store_layout::group_index_dir(root);
    let aside = group_dir.join("index.fjall.discard-4242-0");
    fs::rename(group_dir.join("index.fjall"), &aside).unwrap();

    let mut reopened = Store::open(root, VIEW_WORKING).expect("open over a half-finished discard");
    assert!(reopened.index_rebuild_needed);
    assert!(!aside.exists(), "the index set aside is deleted on open");
    let report = rescan(root, &mut reopened);
    assert_eq!(report.stats.updated, 3);
    assert_eq!(indexed_symbol_paths(&reopened, "beta"), vec!["beta.rs".to_string()]);
}

fn file_map_on_disk(view_dir: &Path) -> basemind::store::Index {
    rmp_serde::from_slice(&fs::read(view_dir.join(INDEX_FILE)).unwrap()).unwrap()
}

/// A rebuilt index counts its generations from 0 again. The file map that listed every file must not
/// stay on disk beside it: the open writes the emptied one before any scan runs.
#[test]
fn a_writer_open_persists_the_emptied_file_map_before_any_scan() {
    basemind::store::init_isolated_cache();
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let (view_dir, sealed) = scanned_store_with_an_index_behind(root);
    assert_eq!(file_map_on_disk(&view_dir).files.len(), 3);

    let reopened = Store::open(root, VIEW_WORKING).expect("reopen");
    assert!(reopened.index_rebuild_needed, "the following scan is still a rebuild");
    let on_disk = file_map_on_disk(&view_dir);
    assert!(on_disk.files.is_empty(), "no file map vouches for the discarded index");
    let index_generation = reopened.index_db.as_ref().unwrap().scan_generation().unwrap();
    assert_eq!(on_disk.generation, index_generation);
    assert!(index_generation < sealed, "the rebuilt index starts a new count");
}

/// A process killed between the open that wrote the emptied file map and the rebuild scan leaves an
/// empty map over an empty index. The emptied map records the pending rebuild itself, so the next
/// open still owes it, and the store says an incremental scan would index only the paths it is
/// handed.
#[test]
fn a_rebuild_that_never_ran_still_asks_for_a_full_scan() {
    basemind::store::init_isolated_cache();
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let (view_dir, _) = scanned_store_with_an_index_behind(root);

    drop(Store::open(root, VIEW_WORKING).expect("the open that empties the file map"));
    assert!(
        file_map_on_disk(&view_dir).rebuild_pending,
        "the emptied file map on disk records the rebuild it owes"
    );

    let mut killed_before_the_scan = Store::open(root, VIEW_WORKING).expect("reopen");
    assert!(
        killed_before_the_scan.index_rebuild_needed,
        "the pending rebuild outlives the process that never ran it"
    );
    assert!(killed_before_the_scan.needs_full_scan(), "nothing is indexed");

    let report = rescan(root, &mut killed_before_the_scan);
    assert_eq!(report.stats.updated, 3);
    assert!(!killed_before_the_scan.needs_full_scan(), "the tree is indexed again");
    assert!(
        !file_map_on_disk(&view_dir).rebuild_pending,
        "the full pass that rebuilt the index clears the record"
    );
}

/// The restarted count can reach the old file map's generation while the rebuild is still partial:
/// each flush seals the index first, so a flush whose file-map write fails advances it anyway. The
/// old file map would then match an index missing rows and skip their files for good.
///
/// The rebuilt index is left empty by flushing alone: any scan of a store owing a rebuild runs the
/// full pass, which indexes every file before its flush can fail.
#[test]
fn an_interrupted_rebuild_is_not_adopted_by_the_file_map_it_replaces() {
    basemind::store::init_isolated_cache();
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let (view_dir, sealed) = scanned_store_with_an_index_behind(root);

    let mut rebuilding = Store::open(root, VIEW_WORKING).expect("reopen");
    let blocker = view_dir.join(format!("{INDEX_FILE}.tmp"));
    fs::create_dir(&blocker).unwrap();
    while rebuilding.index_db.as_ref().unwrap().scan_generation().unwrap() < sealed {
        rebuilding
            .flush()
            .expect_err("the file map cannot be written over a directory");
    }
    assert!(indexed_symbol_paths(&rebuilding, "alpha").is_empty(), "a partial index");
    assert!(indexed_symbol_paths(&rebuilding, "beta").is_empty(), "a partial index");
    drop(rebuilding);
    fs::remove_dir(&blocker).unwrap();

    let mut reopened = Store::open(root, VIEW_WORKING).expect("reopen after the interrupted rebuild");
    let report = rescan(root, &mut reopened);
    assert_eq!(
        report.stats.skipped_unchanged, 0,
        "no file is skipped over missing rows"
    );
    assert_eq!(report.stats.updated, 3);
    assert_eq!(indexed_symbol_paths(&reopened, "beta"), vec!["beta.rs".to_string()]);
    assert_eq!(indexed_symbol_paths(&reopened, "alpha"), vec!["alpha.rs".to_string()]);
}
