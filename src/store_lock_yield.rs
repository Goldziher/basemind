//! The background embedding worker (`basemind embed-worker`) holds the store write lock only on
//! loan: any other writer that finds the worker holding it asks for it back and waits, instead of
//! failing as it would against a `serve` or a `watch`.
//!
//! The ask is a small marker beside the lock, `embed-yield`, naming the waiting process. The worker
//! polls it between files and stops its pass at the next file boundary (completed files stay
//! committed), releases the lock, and takes it again only once no live process asks. A marker whose
//! writer died, or that is older than a waiter ever waits, asks nothing, so a crashed waiter cannot
//! park the worker for good.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::LockHolder;

/// Filename of the request marker inside a workspace cache dir.
const YIELD_FILE: &str = "embed-yield";

/// How long a writer waits for the worker to let go before it reports the lock as held. The worker
/// stops at a file boundary, so this bounds the embedding of the one document it is inside.
pub(crate) const YIELD_WAIT: Duration = Duration::from_secs(120);

/// How often a waiting writer tries the lock again.
const RETRY: Duration = Duration::from_millis(50);

fn marker(basemind_dir: &Path) -> PathBuf {
    basemind_dir.join(YIELD_FILE)
}

/// Whether `command`, a lock holder's recorded command, is the background embedding worker's.
pub(crate) fn held_by_worker(command: &str) -> bool {
    command == LockHolder::EmbedWorker.command()
}

/// A writer's request that the worker let go of the lock, withdrawn when dropped.
pub(crate) struct Request {
    path: PathBuf,
    pid: String,
}

impl Request {
    /// Ask the worker holding the lock of `basemind_dir` to let go. Best-effort: a marker that
    /// cannot be written leaves the writer waiting out [`YIELD_WAIT`] as it would anyway.
    pub(crate) fn file(basemind_dir: &Path) -> Self {
        let request = Self {
            path: marker(basemind_dir),
            pid: std::process::id().to_string(),
        };
        let _ = std::fs::write(&request.path, &request.pid);
        request
    }
}

impl Drop for Request {
    /// Withdraw the request unless a later writer has replaced it with its own.
    fn drop(&mut self) {
        if std::fs::read_to_string(&self.path).is_ok_and(|text| text.trim() == self.pid) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Whether a live writer is asking the worker to let go of the lock of `basemind_dir`.
pub fn requested(basemind_dir: &Path) -> bool {
    let path = marker(basemind_dir);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return false;
    };
    let fresh = std::fs::metadata(&path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age <= YIELD_WAIT);
    fresh && text.trim().parse::<u32>().is_ok_and(requester_is_live)
}

#[cfg(any(unix, windows))]
fn requester_is_live(pid: u32) -> bool {
    crate::boot_identity::pid_is_live(pid)
}

/// A platform that cannot tell a live pid from a dead one trusts the marker's age alone.
#[cfg(not(any(unix, windows)))]
fn requester_is_live(_pid: u32) -> bool {
    true
}

/// Wait up to [`YIELD_WAIT`] for the worker to let go, asking it to, trying `attempt` until it
/// wins the lock. Returns the last attempt's outcome.
pub(crate) fn wait_for_worker<T, E>(basemind_dir: &Path, mut attempt: impl FnMut() -> Result<T, E>) -> Result<T, E> {
    let _request = Request::file(basemind_dir);
    let deadline = std::time::Instant::now() + YIELD_WAIT;
    loop {
        let outcome = attempt();
        if outcome.is_ok() || std::time::Instant::now() >= deadline {
            return outcome;
        }
        std::thread::sleep(RETRY);
    }
}

#[cfg(test)]
mod tests {
    use super::super::{acquire_lock_as, try_acquire_lock_as};
    use super::*;

    #[test]
    fn a_writer_asks_the_worker_for_the_lock_and_gets_it_once_the_worker_lets_go() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().to_path_buf();
        let worker_guard = acquire_lock_as(&dir, LockHolder::EmbedWorker).expect("the worker takes the lock");
        assert!(!requested(&dir), "nothing asks before a writer arrives");

        let worker_dir = dir.clone();
        let worker = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            while !requested(&worker_dir) {
                assert!(std::time::Instant::now() < deadline, "the writer never asked");
                std::thread::sleep(Duration::from_millis(10));
            }
            drop(worker_guard);
        });
        let scan = acquire_lock_as(&dir, LockHolder::Scan).expect("the scan gets the lock the worker gave up");
        worker.join().expect("worker thread");
        assert!(
            !requested(&dir),
            "the request is withdrawn once the writer holds the lock"
        );
        assert!(
            try_acquire_lock_as(&dir, LockHolder::EmbedWorker).is_err(),
            "the scan holds the lock now"
        );
        drop(scan);
    }

    #[test]
    fn a_writer_does_not_wait_for_a_holder_that_is_not_the_worker() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _serve = acquire_lock_as(tmp.path(), LockHolder::Serve).expect("serve takes the lock");
        let started = std::time::Instant::now();
        assert!(acquire_lock_as(tmp.path(), LockHolder::Scan).is_err());
        assert!(
            started.elapsed() < YIELD_WAIT / 4,
            "a serve holder is reported, not waited out"
        );
        assert!(!requested(tmp.path()), "a serve holder is never asked to yield");
    }

    #[test]
    fn an_unreadable_request_asks_nothing_and_a_live_one_asks() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(!requested(tmp.path()), "no marker asks nothing");
        std::fs::write(marker(tmp.path()), "not a pid").expect("marker");
        assert!(!requested(tmp.path()));
        std::fs::write(marker(tmp.path()), std::process::id().to_string()).expect("marker");
        assert!(requested(tmp.path()), "a live requester asks");
    }

    #[test]
    fn a_withdrawn_request_leaves_a_later_writers_request_in_place() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let first = Request::file(tmp.path());
        std::fs::write(marker(tmp.path()), "1").expect("a later writer's request");
        drop(first);
        assert_eq!(std::fs::read_to_string(marker(tmp.path())).expect("kept"), "1");
    }
}
