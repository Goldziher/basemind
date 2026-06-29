//! Multi-SESSION contention repro: several `basemind serve` processes against the
//! same repo. Unlike `concurrency_smoke.rs` (many concurrent calls to ONE server),
//! these tests answer the load-bearing question behind the "blocked / not
//! responsive" reports: can more than one holder open the Fjall index at once?

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
        let mut store = Store::open(root, VIEW_WORKING).expect("open store");
        scan(
            root,
            &mut store,
            &ConfigV1::with_defaults(),
            ScanSource::WorkingTree,
        )
        .expect("scan");
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
    assert_eq!(
        hits.len(),
        1,
        "blob-backed search still works on the 2nd session"
    );
}
