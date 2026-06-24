//! Concurrent-serve lock contention — regressions for issues #26 and #27.
//!
//! The editor plugin spawns one `basemind serve` per session against the same repo, but the store
//! write lock is single-holder. Two behaviors are pinned here:
//!
//! * **#26** — a second writer must *fail fast* with a lock-contention error, never busy-spin to
//!   multi-GB RSS. The test bounds the second open with a wall-clock budget so a re-introduced spin
//!   surfaces as a CI timeout instead of a silent pass.
//! * **#27** — a contending serve falls back to a *read-only* open of the same shared index and
//!   still answers reads, instead of exiting and handing the MCP client an opaque `-32000`.

use std::fs;
use std::time::{Duration, Instant};

use basemind::config::ConfigV1;
use basemind::scanner::{ScanSource, scan};
use basemind::store::{LockHolder, Store, VIEW_WORKING};

/// Build a temp repo with one indexed file, then release the lock used for the initial scan so the
/// tests below start from a clean, unlocked, already-scanned `.basemind/`.
fn scanned_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fs::write(
        root.join("a.rs"),
        b"pub fn alpha() {}\npub fn beta() { alpha(); }\n",
    )
    .expect("write source");
    let cfg = ConfigV1::with_defaults();
    let mut store = Store::open(root, VIEW_WORKING).expect("initial open");
    scan(root, &mut store, &cfg, ScanSource::WorkingTree).expect("initial scan");
    drop(store); // release the scan lock
    dir
}

#[test]
fn second_writer_fails_fast_without_spinning() {
    let dir = scanned_repo();
    let root = dir.path();
    let _writer =
        Store::open_with_holder(root, VIEW_WORKING, LockHolder::Serve).expect("first writer opens");

    let started = Instant::now();
    let result = Store::open_with_holder(root, VIEW_WORKING, LockHolder::Serve);
    let elapsed = started.elapsed();

    // `Store` is not `Debug`, so unwrap the error arm by hand rather than via `expect_err`.
    let error = match result {
        Ok(_) => panic!("a second writer must not acquire the held lock"),
        Err(error) => error,
    };
    assert!(
        error.is_lock_contention(),
        "expected a lock-contention error, got: {error}"
    );
    // `acquire_lock_as` budgets 25 × 20 ms = 500 ms then errors. Allow generous slack but assert it
    // stays bounded — a busy-spin (issue #26) would blow far past this or never return.
    assert!(
        elapsed < Duration::from_secs(5),
        "second writer took {elapsed:?} — not failing fast (possible busy-spin regression)"
    );
}

#[test]
fn read_only_serve_coexists_with_live_writer() {
    let dir = scanned_repo();
    let root = dir.path();
    let _writer =
        Store::open_with_holder(root, VIEW_WORKING, LockHolder::Serve).expect("writer holds lock");

    // The read-only fallback takes no write lock and opens alongside the live writer — the same
    // concurrent-reader path the CLI `query` uses. This is what a contending serve does instead of
    // dying with `-32000`.
    let reader =
        Store::open_read_only(root, VIEW_WORKING).expect("read-only open alongside live writer");
    let hits = basemind::query::search_symbols(&reader, "alpha", None).expect("search");
    assert_eq!(
        hits.len(),
        1,
        "read-only serve must resolve symbols from the shared index"
    );
    assert_eq!(hits[0].path.as_str(), Some("a.rs"));
}
