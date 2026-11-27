//! The write buffer a long-lived process keeps across rescans.

use super::tests::{full_scan, one_file_repo};
use crate::store::{Store, VIEW_WORKING, init_isolated_cache};

/// Unflushed bytes the index may hold once a pass has ended and fjall's flushes have run.
const SETTLED: u64 = 8 * 1_024 * 1_024;

/// A module whose every function, call and term changes with `version`.
fn module(version: u32) -> String {
    (0..400)
        .map(|i| format!("pub fn f{version}_{i}(x: u32) -> u32 {{ g{version}_{i}(x) + h{version}_{i}(x) }}\n"))
        .collect()
}

/// Waits for the memtables a pass handed to fjall's flush worker, and returns what stays unflushed.
fn settled_bytes(store: &Store) -> u64 {
    let db = store.index_db.as_ref().expect("index");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while db.unflushed_bytes() > SETTLED && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    db.unflushed_bytes()
}

/// Rescans that keep rewriting a small repository's rows fill no memtable to its ceiling, so fjall
/// never flushes them on its own; the end of each pass keeps the unflushed rows bounded instead of
/// letting every keyspace's memtable grow for as long as the process lives.
#[test]
fn repeated_rescans_keep_the_index_write_buffer_bounded() {
    init_isolated_cache();
    let dir = one_file_repo();
    let root = dir.path();
    let mut store = Store::open(root, VIEW_WORKING).expect("open");
    for version in 0..40 {
        std::fs::write(root.join("churn.rs"), module(version)).expect("write");
        full_scan(root, &mut store);
        let left = settled_bytes(&store);
        assert!(
            left <= SETTLED,
            "pass {version} left {left} bytes unflushed, more than {SETTLED}"
        );
    }
}
