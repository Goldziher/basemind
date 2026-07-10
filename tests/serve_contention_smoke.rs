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
use std::process::Command;
use std::time::{Duration, Instant};

use basemind::config::ConfigV1;
use basemind::scanner::{ScanSource, scan};
use basemind::store::{LockHolder, Store, VIEW_WORKING};

/// Build a temp repo with one indexed file, then release the lock used for the initial scan so the
/// tests below start from a clean, unlocked, already-scanned `.basemind/`.
fn scanned_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fs::write(root.join("a.rs"), b"pub fn alpha() {}\npub fn beta() { alpha(); }\n").expect("write source");
    let cfg = ConfigV1::with_defaults();
    let mut store = Store::open(root, VIEW_WORKING).expect("initial open");
    scan(
        root,
        &mut store,
        &cfg,
        ScanSource::WorkingTree,
        basemind::scanner::EmbedMode::Inline,
    )
    .expect("initial scan");
    drop(store);
    dir
}

#[test]
fn second_writer_fails_fast_without_spinning() {
    let dir = scanned_repo();
    let root = dir.path();
    let _writer = Store::open_with_holder(root, VIEW_WORKING, LockHolder::Serve).expect("first writer opens");

    let started = Instant::now();
    let result = Store::open_with_holder(root, VIEW_WORKING, LockHolder::Serve);
    let elapsed = started.elapsed();

    let error = match result {
        Ok(_) => panic!("a second writer must not acquire the held lock"),
        Err(error) => error,
    };
    assert!(
        error.is_lock_contention(),
        "expected a lock-contention error, got: {error}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "second writer took {elapsed:?} — not failing fast (possible busy-spin regression)"
    );
}

#[test]
fn read_only_serve_coexists_with_live_writer() {
    let dir = scanned_repo();
    let root = dir.path();
    let _writer = Store::open_with_holder(root, VIEW_WORKING, LockHolder::Serve).expect("writer holds lock");

    let reader = Store::open_read_only(root, VIEW_WORKING).expect("read-only open alongside live writer");
    let hits = basemind::query::search_symbols(&reader, "alpha", None).expect("search");
    assert_eq!(
        hits.len(),
        1,
        "read-only serve must resolve symbols from the shared index"
    );
    assert_eq!(hits[0].path.as_str(), Some("a.rs"));
}

#[test]
fn cli_scan_exits_cleanly_when_a_writer_holds_the_lock() {
    let dir = scanned_repo();
    let root = dir.path();
    let _writer = Store::open_with_holder(root, VIEW_WORKING, LockHolder::Serve).expect("writer holds lock");

    let started = Instant::now();
    let output = Command::new(env!("CARGO_BIN_EXE_basemind"))
        .args(["scan"])
        .current_dir(root)
        .output()
        .expect("run basemind scan");
    let elapsed = started.elapsed();

    assert!(
        output.status.success(),
        "scan against a held lock must exit cleanly (0), got {:?}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("basemind serve") && combined.contains("rescan"),
        "notice must name the `basemind serve` holder and point at its `rescan` tool, got:\n{combined}"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "pre-flight scan took {elapsed:?} — should short-circuit, not block on lock retries"
    );
}
