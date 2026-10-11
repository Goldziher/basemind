//! The cap on abandoned (timed-out, still running) extraction threads (issue #72). Its own binary:
//! the live count is process-wide, so it must not share a process with other timeouts.
//!
//! The slow document is a 4 MB log forced through xberg's markdown splitter: about 1.4 s per MB in a
//! debug build, so it overruns a 1 s budget and keeps its thread busy for several seconds after.

#![cfg(feature = "documents")]

use std::fs;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use basemind::config::ConfigV1;
use basemind::extract::ExtractError;
use basemind::extract::doc::{DocConfig, extract_doc};
use basemind::extract::doc_guard::{ChunkCutovers, abandoned_extractions};
use basemind::scanner::{EmbedMode, ScanSource, scan};
use basemind::store::{Store, VIEW_WORKING};

/// Both tests count the same process-wide ledger, so they run one after the other.
static SERIAL: Mutex<()> = Mutex::new(());

fn slow_log(seed: u32) -> String {
    let mut text = String::with_capacity(4 * 1024 * 1024 + 100);
    let mut i = 0u64;
    while text.len() < 4 * 1024 * 1024 {
        text.push_str(&format!(
            "run{seed} 2021-01-27 host{} svc[{i}]: event id={} status=ok\n",
            i % 7,
            i * 31
        ));
        i += 1;
    }
    text
}

fn drain() {
    let started = Instant::now();
    while abandoned_extractions() > 0 {
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "abandoned threads never finished"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn at_the_cap_further_documents_are_deferred_not_spawned() {
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = tempfile::tempdir().unwrap();
    let slow = dir.path().join("slow.txt");
    let tiny = dir.path().join("tiny.txt");
    fs::write(&slow, slow_log(1)).unwrap();
    fs::write(&tiny, "a short note\n").unwrap();
    let config = DocConfig {
        embed: false,
        embedding_preset: None,
        extraction_timeout_secs: 1,
        max_abandoned_extractions: 1,
        chunk_cutovers: ChunkCutovers {
            markdown_bytes: u64::MAX,
            plain_text_bytes: u64::MAX,
        },
        ..DocConfig::default()
    };

    // The first overrun is abandoned and counted.
    let error = extract_doc(&slow, Some("text/plain"), &config).expect_err("must time out");
    assert!(matches!(error, ExtractError::DocTimeout(_)), "got {error:?}");
    assert_eq!(abandoned_extractions(), 1, "the overrunning thread is still running");

    // At the cap, even a trivial document is refused at once, with a clear reason, and nothing runs.
    let started = Instant::now();
    let refused = extract_doc(&tiny, Some("text/plain"), &config).expect_err("must be deferred");
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "fail fast, no thread spawned"
    );
    let ExtractError::DocDeferred(reason) = &refused else {
        panic!("expected a deferral, got {refused:?}");
    };
    assert!(reason.contains("max_abandoned_extractions"), "{reason}");
    assert!(reason.contains("cap 1"), "{reason}");
    assert_eq!(abandoned_extractions(), 1, "a refusal adds no thread");

    // Once the stuck thread finishes, capacity returns.
    drain();
    let doc = extract_doc(&tiny, Some("text/plain"), &config).expect("capacity is back");
    assert!(!doc.chunks.is_empty());
}

#[test]
fn a_scan_at_the_cap_defers_the_remaining_documents_and_reports_the_live_count() {
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    drain();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for (i, name) in ["a.txt", "b.txt", "c.txt"].into_iter().enumerate() {
        fs::write(root.join(name), slow_log(20 + u32::try_from(i).unwrap())).unwrap();
    }
    basemind::store::init_isolated_cache();
    let mut cfg = ConfigV1::with_defaults();
    cfg.documents.embed = false;
    cfg.documents.extraction_timeout_secs = 1;
    cfg.documents.plain_text_chunk_max_bytes = u64::MAX;
    cfg.documents.max_abandoned_extractions = 1;
    cfg.resources.scan_threads = 1;

    let mut store = Store::open(root, VIEW_WORKING).unwrap();
    let stats = scan(root, &mut store, &cfg, ScanSource::WorkingTree, EmbedMode::Inline)
        .unwrap()
        .stats;

    assert_eq!(stats.doc_timeouts, 1, "{stats:?}");
    assert_eq!(
        stats.docs_deferred, 2,
        "the other two never started a thread: {stats:?}"
    );
    assert_eq!(stats.abandoned_extractions, 1, "{stats:?}");
    assert_eq!(stats.extract_failed, 1, "{stats:?}");
    drain();
}
