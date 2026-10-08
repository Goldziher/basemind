//! In-process upgrade behaviour, no old binary needed: the epoch-gated re-extraction of blobs an
//! older extractor wrote, and the reclamation of the blobs a tier migration orphans.
//!
//! Plain `#[test]` only — `scanner::scan` is synchronous and opening LanceDB inside a tokio runtime
//! panics. Each test runs against the process-isolated cache, never the developer's.

#![cfg(feature = "documents")]

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime};

use basemind::config::ConfigV1;
use basemind::extract::{EXTRACT_EPOCH, FileMapL1, SCHEMA_VER};
use basemind::scanner::{EmbedMode, ScanReport, ScanSource, scan};
use basemind::store::{FileEntry, Store, VIEW_WORKING};
use tempfile::TempDir;

const PY: &str = "class Svc:\n    def run(self, n):\n        total = 0\n        for i in range(n):\n            total += i\n        return total\n";

fn fixture(salt: &str) -> (TempDir, ConfigV1) {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let _ = Command::new("git").args(["init", "-q"]).current_dir(root).status();
    // Salted so the process-shared content-addressed blob cache never serves another test's blob.
    fs::write(root.join("app.py"), format!("# {salt}\n{PY}")).unwrap();
    fs::write(
        root.join("README.md"),
        format!("# Title\n\nProse about the project ({salt}).\n"),
    )
    .unwrap();
    let mut cfg = ConfigV1::with_defaults();
    cfg.documents.enabled = true;
    cfg.documents.embed = false;
    (dir, cfg)
}

fn full_scan(root: &Path, store: &mut Store, cfg: &ConfigV1) -> ScanReport {
    scan(root, store, cfg, ScanSource::WorkingTree, EmbedMode::Inline).expect("scan")
}

fn blob_exists(store: &Store, hash_hex: &str, suffix: &str) -> bool {
    store.blobs_dir.join(format!("{hash_hex}.{suffix}.msgpack")).exists()
}

/// Backdate a file's mtime past the sweep's young-blob grace window.
fn age_past_grace(path: &Path) {
    let old = SystemTime::now() - Duration::from_secs(7 * 60 * 60);
    fs::File::options()
        .write(true)
        .open(path)
        .and_then(|f| f.set_modified(old))
        .expect("backdate blob");
}

/// Rewrite `app.py`'s blob and entry the way an extractor from before `EXTRACT_EPOCH` left them: the
/// signature embeds the whole body and the epoch is `0`.
fn age_to_pre_epoch(store: &mut Store) -> String {
    let entry = store.lookup("app.py").expect("app.py indexed").clone();
    let mut l1: FileMapL1 = store.read_l1_by_hex(&entry.hash_hex).unwrap().expect("l1 blob");
    let class = l1.symbols.iter_mut().find(|s| s.name == "Svc").expect("class symbol");
    class.signature = Some(PY.to_string());
    l1.extract_epoch = 0;
    store.overwrite_filemap_hex(&entry.hash_hex, &l1, None).unwrap();
    store.upsert(
        "app.py",
        FileEntry {
            extract_epoch: 0,
            ..entry.clone()
        },
    );
    store.flush().unwrap();
    entry.hash_hex
}

fn svc_signature(store: &Store, hash_hex: &str) -> String {
    let l1 = store.read_l1_by_hex(hash_hex).unwrap().expect("l1 blob");
    l1.symbols
        .iter()
        .find(|s| s.name == "Svc")
        .and_then(|s| s.signature.clone())
        .expect("class signature")
}

#[test]
fn a_pre_epoch_blob_is_re_extracted_once_without_a_wipe() {
    let (dir, cfg) = fixture("epoch");
    let root = dir.path();
    let mut store = Store::open(root, VIEW_WORKING).unwrap();
    let first = full_scan(root, &mut store, &cfg);
    assert_eq!(first.stats.refreshed_extraction, 0, "a fresh index starts current");

    let hash = age_to_pre_epoch(&mut store);
    assert!(
        svc_signature(&store, &hash).contains("total += i"),
        "premise: the stale blob embeds the body"
    );

    // Same file, same mtime, same content: only the epoch says it is stale.
    let upgraded = full_scan(root, &mut store, &cfg);
    assert_eq!(
        upgraded.stats.refreshed_extraction, 1,
        "exactly the pre-epoch file is refreshed"
    );
    assert_eq!(upgraded.stats.updated, 1);
    assert!(
        upgraded.stats.displaced_artifacts(),
        "a refresh asks for a cleanup sweep"
    );
    let signature = svc_signature(&store, &hash);
    assert!(
        !signature.contains("total += i"),
        "signature is header-only now: {signature:?}"
    );
    assert_eq!(store.lookup("app.py").unwrap().extract_epoch, EXTRACT_EPOCH);
    assert_eq!(
        store.read_l1_by_hex(&hash).unwrap().unwrap().schema_ver,
        SCHEMA_VER,
        "no schema bump involved"
    );

    let settled = full_scan(root, &mut store, &cfg);
    assert_eq!(settled.stats.updated, 0, "the next scan is a no-op");
    assert_eq!(settled.stats.refreshed_extraction, 0);
    assert!(!settled.stats.displaced_artifacts());
}

#[test]
fn the_epoch_refresh_keeps_the_calls_tier_of_the_old_blob() {
    let (dir, mut cfg) = fixture("epoch-l2");
    cfg.scan.eager_l2 = false;
    let root = dir.path();
    let mut store = Store::open(root, VIEW_WORKING).unwrap();
    full_scan(root, &mut store, &cfg);
    let entry = store.lookup("app.py").unwrap().clone();

    // An old blob that carries calls although this scan would not extract them.
    let (mut l1, l2) = basemind::extract::extract_l1_l2("python", PY.as_bytes(), true).expect("extract");
    l1.extract_epoch = 0;
    store.overwrite_filemap_hex(&entry.hash_hex, &l1, l2.as_ref()).unwrap();
    assert!(
        store.read_l2_by_hex(&entry.hash_hex).unwrap().is_some(),
        "premise: the old blob has calls"
    );
    store.upsert(
        "app.py",
        FileEntry {
            extract_epoch: 0,
            ..entry.clone()
        },
    );

    let report = full_scan(root, &mut store, &cfg);

    assert_eq!(report.stats.refreshed_extraction, 1);
    assert!(
        store.read_l2_by_hex(&entry.hash_hex).unwrap().is_some(),
        "the refresh rewrites L1 without dropping the calls the epoch did not touch"
    );
}

#[test]
fn a_tier_migration_orphans_the_code_lane_blobs_and_the_cleanup_reclaims_only_those() {
    let (dir, cfg) = fixture("migrate");
    let root = dir.path();
    let mut store = Store::open(root, VIEW_WORKING).unwrap();

    // The index an older release left: README.md code-mapped, with its code-lane blobs on disk.
    let bytes = fs::read(root.join("README.md")).unwrap();
    let hash = basemind::hashing::hash_bytes(&bytes);
    let hex = basemind::hashing::hex_buf(&hash);
    let hash_hex = basemind::hashing::hex_str(&hex).to_string();
    let l1 = FileMapL1 {
        schema_ver: SCHEMA_VER,
        language: "markdown".to_string(),
        size_bytes: bytes.len() as u64,
        had_errors: false,
        error_count: 0,
        symbols: Vec::new(),
        imports: Vec::new(),
        implementations: Vec::new(),
        rationale: Vec::new(),
        extract_epoch: 0,
    };
    store.write_filemap_hex(&hash_hex, &l1, None).unwrap();
    fs::write(store.blob_path_chunk_hex(&hash_hex), b"legacy chunk sidecar").unwrap();
    store.upsert(
        "README.md",
        FileEntry {
            hash_hex: hash_hex.clone(),
            language: "markdown".to_string(),
            size_bytes: bytes.len() as u64,
            mtime: 0,
            extract_epoch: 0,
        },
    );

    let migrated = full_scan(root, &mut store, &cfg);

    assert_eq!(
        migrated.stats.tier_migrated, 1,
        "README.md moved from the code map to documents"
    );
    assert!(migrated.stats.displaced_artifacts());
    assert!(store.lookup("README.md").is_none());
    assert!(store.lookup_doc("README.md").is_some());
    assert!(
        blob_exists(&store, &hash_hex, "doc"),
        "the document tier owns the same content hash"
    );
    assert!(
        blob_exists(&store, &hash_hex, "fm"),
        "premise: the orphaned code-lane blob is still on disk"
    );

    // Young blobs are protected: nothing is reclaimed inside the grace window.
    let young = basemind::store_gc::reclaim_displaced_blobs().expect("sweep");
    assert_eq!(young.removed, 0, "a blob younger than the grace window is kept");
    assert!(blob_exists(&store, &hash_hex, "fm"));

    age_past_grace(&store.blob_path_fm_hex(&hash_hex));
    age_past_grace(&store.blob_path_chunk_hex(&hash_hex));
    let report = basemind::store_gc::reclaim_displaced_blobs().expect("sweep");

    assert!(report.removed >= 2, "fm + chunk reclaimed: {report:?}");
    assert!(!blob_exists(&store, &hash_hex, "fm"), "stale code-lane blob reclaimed");
    assert!(
        !blob_exists(&store, &hash_hex, "chunk"),
        "stale chunk sidecar reclaimed"
    );
    assert!(
        blob_exists(&store, &hash_hex, "doc"),
        "the live document blob survives although the stem is shared"
    );
    let py = store.lookup("app.py").expect("code file still indexed");
    assert!(blob_exists(&store, &py.hash_hex, "fm"), "referenced code blobs survive");

    let settled = full_scan(root, &mut store, &cfg);
    assert_eq!(settled.stats.tier_migrated, 0);
    assert_eq!(settled.stats.updated, 0);
    assert!(!settled.stats.displaced_artifacts(), "a second scan is a no-op");
}
