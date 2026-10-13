//! Bounds on document extraction that outlive one file (issue #72): timed-out documents are
//! tombstoned by content hash and extraction limits, and the document phase of a scan has a total
//! budget. The abandoned-thread cap has its own binary (`doc_abandoned_cap.rs`) because it counts a
//! process-wide resource.
//!
//! The slow document is a 4 MB log forced through xberg's markdown splitter (cutover raised to
//! `u64::MAX`): about 1.4 s per MB in a debug build, so a 1 s budget is exceeded by a wide margin.

#![cfg(feature = "documents")]

use std::fs;
use std::path::Path;
use std::sync::Mutex;

use basemind::config::ConfigV1;
use basemind::scanner::{EmbedMode, ScanReport, ScanSource, scan};
use basemind::store::{Store, VIEW_WORKING};

/// Scans share the process-wide abandoned-thread ledger; run them one at a time so a neighbour's
/// still-burning thread cannot change what a scan sees.
static SERIAL: Mutex<()> = Mutex::new(());

fn slow_log(seed: u32) -> String {
    let mut text = String::with_capacity(4 * 1024 * 1024 + 100);
    let mut i = 0u64;
    while text.len() < 4 * 1024 * 1024 {
        text.push_str(&format!(
            "run{seed} 2021-01-27 14:{:02}:{:02} host{} svc[{i}]: event id={} status=ok\n",
            i % 60,
            i % 59,
            i % 7,
            i * 31
        ));
        i += 1;
    }
    text
}

fn guide() -> String {
    (0..40)
        .map(|i| format!("## Section {i}\n\nSome *text* with `code` and a [link](http://x/{i}).\n\n"))
        .collect()
}

/// Offline config whose 1 s per-file budget the slow log always overruns.
fn slow_config() -> ConfigV1 {
    basemind::store::init_isolated_cache();
    let mut cfg = ConfigV1::with_defaults();
    cfg.documents.embed = false;
    cfg.documents.extraction_timeout_secs = 1;
    cfg.documents.plain_text_chunk_max_bytes = u64::MAX;
    cfg.documents.max_abandoned_extractions = 64;
    // One worker, so documents run one after another. The scan pool is process-wide and sized by the
    // first scan, so every test in this binary must ask for the same size.
    cfg.resources.scan_threads = 1;
    cfg
}

fn scan_once(root: &Path, cfg: &ConfigV1) -> ScanReport {
    let mut store = Store::open(root, VIEW_WORKING).expect("open store");
    scan(root, &mut store, cfg, ScanSource::WorkingTree, EmbedMode::Inline).expect("scan")
}

fn open(root: &Path) -> Store {
    Store::open(root, VIEW_WORKING).expect("open store")
}

#[test]
fn a_timed_out_document_is_not_retried_until_the_file_or_the_limits_change() {
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("slow.txt"), slow_log(1)).unwrap();
    fs::write(root.join("guide.md"), guide()).unwrap();
    let cfg = slow_config();

    // First scan: the log overruns its budget and is tombstoned; the guide is unaffected.
    let first = scan_once(root, &cfg);
    assert_eq!(first.stats.doc_timeouts, 1);
    assert_eq!(first.stats.docs_tombstoned, 0);
    assert_eq!(first.stats.docs_indexed, 1);
    let tombstone = open(root)
        .lookup_doc_tombstone("slow.txt")
        .cloned()
        .expect("tombstone recorded");
    assert_eq!(tombstone.timeout_secs, 1);
    assert!(
        open(root).lookup_doc("slow.txt").is_none(),
        "a timeout leaves no doc entry"
    );

    // Unchanged file, unchanged limits: skipped without a retry, and not counted as a failure.
    let second = scan_once(root, &cfg);
    assert_eq!(second.stats.doc_timeouts, 0, "no second timeout");
    assert_eq!(second.stats.docs_tombstoned, 1);
    assert_eq!(second.stats.extract_failed, 0);
    assert_eq!(second.stats.docs_indexed, 0);

    // A changed limit retries it: here the timeout is raised, but the log still overruns 2 s.
    let mut raised = cfg.clone();
    raised.documents.extraction_timeout_secs = 2;
    let third = scan_once(root, &raised);
    assert_eq!(
        third.stats.docs_tombstoned, 0,
        "raising the timeout retries the document"
    );
    assert_eq!(third.stats.doc_timeouts, 1);
    assert_eq!(open(root).lookup_doc_tombstone("slow.txt").unwrap().timeout_secs, 2);

    // Editing the file retries it too (same limits as the previous scan).
    fs::write(root.join("slow.txt"), slow_log(2)).unwrap();
    let fourth = scan_once(root, &raised);
    assert_eq!(fourth.stats.docs_tombstoned, 0, "an edited file is retried");
    assert_eq!(fourth.stats.doc_timeouts, 1);
    let edited = open(root).lookup_doc_tombstone("slow.txt").cloned().unwrap();
    assert_ne!(
        edited.hash_hex, tombstone.hash_hex,
        "the tombstone follows the new content"
    );

    // Putting the guard back (linear chunking) indexes it and clears the tombstone.
    let mut guarded = raised.clone();
    // The earlier scans leave their quadratic splitters burning CPU in abandoned threads; a small runner
    // can starve the linear chunker past 2 s, so this scan gets a budget that only a real hang would hit.
    guarded.documents.extraction_timeout_secs = 60;
    guarded.documents.plain_text_chunk_max_bytes = ConfigV1::with_defaults().documents.plain_text_chunk_max_bytes;
    let fifth = scan_once(root, &guarded);
    assert_eq!(fifth.stats.doc_timeouts, 0);
    // The cutover is part of the document digest, so the guide is re-processed as well.
    assert_eq!(fifth.stats.docs_indexed, 2);
    assert_eq!(fifth.stats.docs_degraded, 1, "only the log takes the linear chunker");
    let store = open(root);
    assert!(store.lookup_doc("slow.txt").is_some());
    assert!(!store.has_doc_tombstone("slow.txt"), "success clears the tombstone");
}

#[test]
fn a_deleted_document_takes_its_tombstone_with_it() {
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("slow.txt"), slow_log(3)).unwrap();
    let cfg = slow_config();

    assert_eq!(scan_once(root, &cfg).stats.doc_timeouts, 1);
    assert!(open(root).has_doc_tombstone("slow.txt"));

    fs::remove_file(root.join("slow.txt")).unwrap();
    scan_once(root, &cfg);
    assert!(!open(root).has_doc_tombstone("slow.txt"));
}

#[test]
fn the_document_phase_budget_defers_the_rest_and_the_next_scan_picks_them_up() {
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for (i, name) in ["a.txt", "b.txt", "c.txt"].into_iter().enumerate() {
        fs::write(root.join(name), slow_log(10 + u32::try_from(i).unwrap())).unwrap();
    }
    fs::write(root.join("guide.md"), guide()).unwrap();
    let mut cfg = slow_config();
    // The phase budget equals the per-file one, so the first overrunning log spends the whole phase.
    cfg.documents.phase_budget_secs = 1;

    let first = scan_once(root, &cfg);
    let stats = first.stats;
    assert_eq!(
        stats.doc_timeouts, 1,
        "only the first log is allowed to overrun: {stats:?}"
    );
    assert!(stats.docs_deferred >= 2, "the other logs are deferred: {stats:?}");
    assert_eq!(stats.docs_deferred + stats.docs_indexed, 3, "{stats:?}");
    assert_eq!(
        stats.extract_failed, 1,
        "a deferral is not a failure, only the timeout is: {stats:?}"
    );

    // Next scan with a generous phase budget: everything left is attempted; the document that already
    // timed out is tombstoned, the deferred logs time out on their own per-file budget.
    cfg.documents.phase_budget_secs = 1800;
    let second = scan_once(root, &cfg);
    let stats = second.stats;
    assert_eq!(stats.docs_deferred, 0, "{stats:?}");
    assert_eq!(stats.docs_tombstoned, 1, "{stats:?}");
    assert_eq!(stats.doc_timeouts, 2, "{stats:?}");
    let store = open(root);
    assert!(
        store.lookup_doc("guide.md").is_some(),
        "the normal document was picked up"
    );
    assert_eq!(
        ["a.txt", "b.txt", "c.txt"]
            .iter()
            .filter(|p| store.has_doc_tombstone(**p))
            .count(),
        3
    );
}
