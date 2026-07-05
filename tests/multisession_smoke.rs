//! Multi-SESSION contention repro: several `basemind serve` processes against the
//! same repo. Unlike `concurrency_smoke.rs` (many concurrent calls to ONE server),
//! these tests answer the load-bearing question behind the "blocked / not
//! responsive" reports: can more than one holder open the Fjall index at once?

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use basemind::config::ConfigV1;
use basemind::index::IndexDb;
use basemind::scanner::{ScanSource, scan};
use basemind::store::{Store, VIEW_WORKING};
use tempfile::TempDir;

/// Scan a tiny two-file repo so the Fjall `calls_by_callee` keyspace (which backs
/// `find_references`) is populated, then drop the writer to release every lock.
fn scanned_repo() -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::write(root.join("a.rs"), b"pub fn alpha() {}\n").expect("write a.rs");
    std::fs::write(root.join("b.rs"), b"fn beta() { alpha(); alpha(); }\n").expect("write b.rs");
    {
        // Embeddings off: these tests resolve references/impls from blobs, not vectors, and run the
        // scan on a `#[tokio::test]` thread. With the ONNX model cached, an embedding scan would open
        // LanceDB (`block_on` inside the live runtime) and panic — a test-harness fragility unrelated
        // to what's under test. Production wraps `scan` in `spawn_blocking`, so it's unaffected.
        let mut cfg = ConfigV1::with_defaults();
        cfg.documents.embed = false;
        cfg.code_search.embed = false;
        let mut store = Store::open(root, VIEW_WORKING).expect("open store");
        scan(root, &mut store, &cfg, ScanSource::WorkingTree).expect("scan");
    } // drop → release the fs2 advisory lock AND Fjall's directory lock
    dir
}

/// THE decisive question: can two holders open the SAME Fjall index at once?
///
/// If this asserts `is_err`, Fjall is single-process-exclusive and a second
/// `serve` can never read the index a first one holds — which is the root of the
/// multi-session breakage and the input to any "switch backends" decision.
#[test]
fn fjall_index_rejects_a_second_concurrent_opener() {
    let dir = scanned_repo();
    let view_dir = dir.path().join(".basemind/views/working");

    let first = IndexDb::open(&view_dir).expect("first open succeeds");
    let second = IndexDb::open(&view_dir);

    assert!(
        second.is_err(),
        "fjall ALLOWED a second concurrent open — multi-reader works, look elsewhere"
    );
    drop(first);
}

/// End-to-end symptom at the `Store` layer: while session #1 holds the store
/// (like a live `serve`), session #2 falls back to read-only exactly as
/// `cmd_serve` does — and silently loses the Fjall index, so
/// `find_references`/`find_callers` return empty while blob-backed reads still work.
#[test]
fn second_session_loses_the_fjall_index_but_keeps_blob_reads() {
    let dir = scanned_repo();
    let root = dir.path();

    let serve1 = Store::open(root, VIEW_WORKING).expect("serve #1");
    assert!(serve1.index_db.is_some(), "serve #1 owns the Fjall index");

    let serve2 = Store::open_read_only(root, VIEW_WORKING).expect("serve #2 read-only fallback");
    assert!(
        serve2.index_db.is_none(),
        "2nd concurrent session has no Fjall index (single-holder lock). \
         find_references/find_callers are served from the in-RAM call index instead — \
         see concurrency_smoke::second_session_resolves_find_references_from_blobs."
    );

    // Reads that go through the blobs already work on the 2nd session regardless of
    // the Fjall lock: symbols come straight from the msgpack blobs.
    let hits = basemind::query::search_symbols(&serve2, "alpha", None).expect("search");
    assert_eq!(hits.len(), 1, "blob-backed search still works on the 2nd session");
}

/// Regression guard for the writer→read-only DOWNGRADE race (peer B reproduced it 4/4 under load
/// on 0.14.0). A single rightful writer, opened repeatedly while a storm of readers hammers
/// `open_read_only`, must ALWAYS come up WITH the Fjall index — never silently downgraded to the
/// read-only (blob) fallback, which would leave the repo with zero writers (no auto-scan / watcher
/// / rescan → stale index).
///
/// Pre-fix this failed reliably: every `open_read_only` unconditionally attempted the single-holder
/// fjall open, so a reader transiently holding the lock forced the writer's `IndexDb::open` to
/// `fjall::Error::Locked`, which `cmd_serve` misread as contention and downgraded. The fix is two
/// cooperating parts exercised here together: the writer retries a transient `Locked`
/// (`open_index_with_retry`, F1), and readers skip the fjall open entirely while a writer holds
/// `.basemind/.lock` (`writer_lock_is_held`, F3), draining the storm so the writer's retry wins.
#[test]
fn writer_never_downgrades_under_a_reader_storm() {
    let dir = scanned_repo();
    let root = Arc::new(dir.path().to_path_buf());
    let stop = Arc::new(AtomicBool::new(false));

    // Reader storm: several threads continuously open the store read-only, exactly as concurrent
    // CLI `query`/`outline` calls or lock-losing read-only serves do.
    let readers: Vec<_> = (0..8)
        .map(|_| {
            let root = Arc::clone(&root);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if let Ok(store) = Store::open_read_only(&root, VIEW_WORKING) {
                        drop(store);
                    }
                }
            })
        })
        .collect();

    // The sole rightful writer, opened repeatedly mid-storm, must never fail and never downgrade.
    for i in 0..20 {
        let store = Store::open(&root, VIEW_WORKING)
            .unwrap_or_else(|error| panic!("writer open #{i} failed under reader storm: {error}"));
        assert!(
            store.index_db.is_some(),
            "writer #{i} lost the Fjall index (downgraded to read-only) under the reader storm — \
             the multi-session writer-downgrade race is back"
        );
        drop(store);
    }

    stop.store(true, Ordering::Relaxed);
    for reader in readers {
        reader.join().expect("reader thread panicked");
    }
}
