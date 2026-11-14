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

#[test]
fn panicking_optional_lane_leaves_the_code_map_flushed_and_queryable() {
    basemind::store::init_isolated_cache();
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    seed_repo(root);

    // SAFETY: `TEST_FAULT_LANE_ENV` is read only by the scanner's lane guard, and the only scan in
    unsafe { std::env::set_var(TEST_FAULT_LANE_ENV, LANE_RESOLVE) };

    let cfg = ConfigV1::with_defaults();
    let mut store = Store::open(root, VIEW_WORKING).expect("open store");
    let report = scan(root, &mut store, &cfg, ScanSource::WorkingTree, EmbedMode::Inline);

    // SAFETY: see above.
    unsafe { std::env::remove_var(TEST_FAULT_LANE_ENV) };

    let report = report.expect("a panicking optional lane must not fail the scan");
    assert_eq!(report.stats.updated, 3, "every source file is still extracted");

    let index_path = store.view_dir.join(INDEX_FILE);
    assert!(index_path.exists(), "the code map must be flushed to {index_path:?}");

    let hits = basemind::query::search_symbols(&store, "beta", None).unwrap();
    assert_eq!(hits.len(), 1, "the code map stays queryable after a degraded lane");

    drop(store);
    let reopened = Store::open(root, VIEW_WORKING).expect("reopen store");
    assert_eq!(
        reopened.index.files.len(),
        3,
        "the persisted code map must not be empty"
    );
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
    )
    .expect("scan");
    assert_eq!(report.stats.updated, 3);
    store
}

/// Rewrite the index's scan-generation stamp behind basemind's back, as a journal that lost its tail
/// would leave it. Must run with no `Store` open on the view.
fn forge_index_generation(view_dir: &Path, generation: u64) {
    let db = fjall::Database::builder(view_dir.join("index.fjall"))
        .open()
        .expect("open raw index");
    let meta = db
        .keyspace("meta", fjall::KeyspaceCreateOptions::default)
        .expect("meta keyspace");
    meta.insert("scan_generation", generation.to_be_bytes())
        .expect("forge the stamp");
    db.persist(fjall::PersistMode::SyncAll)
        .expect("persist the forged stamp");
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
    let view_dir = store.view_dir.clone();
    drop(store);

    forge_index_generation(&view_dir, sealed - 1);

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
    assert!(reopened.index.generation > sealed);
    assert_eq!(
        reopened.index_db.as_ref().unwrap().scan_generation().unwrap(),
        reopened.index.generation
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
    let view_dir = store.view_dir.clone();
    let drop_beta_rows = |store: &Store| {
        let mut writer = store.index_db.as_ref().unwrap().writer();
        writer.remove_file(&RelPath::from("beta.rs")).expect("stage the loss");
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

    forge_index_generation(&view_dir, sealed - 1);
    let mut behind = Store::open(root, VIEW_WORKING).expect("reopen behind");
    assert!(behind.index_rebuild_needed);
    let report = rescan(root, &mut behind);
    assert_eq!(report.stats.updated, 3);
    assert_eq!(indexed_symbol_paths(&behind, "beta"), vec!["beta.rs".to_string()]);
    assert_eq!(indexed_symbol_paths(&behind, "alpha"), vec!["alpha.rs".to_string()]);
}

/// A file map written before the generation existed decodes to generation 0, which claims nothing
/// about the index: it opens as before, and the next flush starts stamping.
#[test]
fn a_file_map_without_a_generation_opens_as_before() {
    #[derive(serde::Serialize)]
    struct PreGenerationIndex<'a> {
        schema_ver: u16,
        files: &'a ahash::AHashMap<basemind::path::RelPath, basemind::store::FileEntry>,
    }

    basemind::store::init_isolated_cache();
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let store = scanned_store(root);
    let manifest = store.view_dir.join(INDEX_FILE);
    let old = rmp_serde::to_vec_named(&PreGenerationIndex {
        schema_ver: store.index.schema_ver,
        files: &store.index.files,
    })
    .unwrap();
    drop(store);
    fs::write(&manifest, old).unwrap();

    let mut reopened = Store::open(root, VIEW_WORKING).expect("open the older file map");
    assert_eq!(reopened.index.generation, 0);
    assert!(!reopened.index_rebuild_needed, "no generation, no claim, no rebuild");
    assert_eq!(reopened.index.files.len(), 3);
    let report = rescan(root, &mut reopened);
    assert_eq!(report.stats.skipped_unchanged, 3);
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
