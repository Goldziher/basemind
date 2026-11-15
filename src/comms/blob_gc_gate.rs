//! The gate between scans that write blobs and the sweep that deletes them.
//!
//! A scan writes a file's blob before the file map that references it, and the sweep counts
//! references from the file maps: a sweep that lands in between reaps a blob a scan is about to
//! vouch for. Every scan the daemon runs holds the shared side for its whole duration, whichever
//! way it was started (a client's rescan, a hosted read stack's, the rebuild a freshly opened
//! workspace owes); the sweep takes the exclusive side.
//!
//! A plain lock, not an async one: scans run on blocking threads. The sweep's wait is bounded and
//! never queues, so a scan that never ends starves a sweep cycle instead of wedging the maintenance
//! loop, and a waiting sweep never holds up a scan.

use std::sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant};

/// How often a waiting sweep looks again.
const SWEEP_POLL: Duration = Duration::from_millis(10);

#[derive(Debug, Default)]
pub(crate) struct BlobGcGate {
    lock: RwLock<()>,
}

impl BlobGcGate {
    /// Hold off the sweep while a scan runs. Blocks while a sweep is running.
    pub(crate) fn scanning(&self) -> RwLockReadGuard<'_, ()> {
        self.lock.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// The right to sweep, once no scan is running, or `None` when scans kept it for all of
    /// `bound`.
    pub(crate) fn try_sweep(&self, bound: Duration) -> Option<RwLockWriteGuard<'_, ()>> {
        let deadline = Instant::now() + bound;
        loop {
            match self.lock.try_write() {
                Ok(guard) => return Some(guard),
                Err(std::sync::TryLockError::Poisoned(poisoned)) => return Some(poisoned.into_inner()),
                Err(std::sync::TryLockError::WouldBlock) if Instant::now() >= deadline => return None,
                Err(std::sync::TryLockError::WouldBlock) => std::thread::sleep(SWEEP_POLL),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sweep_waits_out_a_scan_and_gives_up_at_its_bound() {
        let gate = BlobGcGate::default();
        let scanning = gate.scanning();
        let also_scanning = gate.scanning();

        assert!(gate.try_sweep(Duration::from_millis(30)).is_none(), "a scan is running");

        drop((scanning, also_scanning));
        assert!(gate.try_sweep(Duration::ZERO).is_some(), "no scan is running");
    }

    /// Every way into a scan holds the gate, the hosted read stack's rescan included: while a
    /// sweep has it, that scan does not start.
    #[test]
    fn a_hosted_rescan_waits_for_a_running_sweep() {
        use crate::mcp::HostBackend;

        crate::store::init_isolated_cache();
        let root = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(root.path())
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::write(root.path().join("a.rs"), "pub fn a() {}\n").unwrap();
        let pool = std::sync::Arc::new(crate::comms::workspace_pool::WorkspacePool::new(2));
        let gate = pool.blob_gc_gate();
        let sweeping = gate.try_sweep(Duration::ZERO).expect("nothing is scanning");

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let scan = {
            let (pool, root) = (std::sync::Arc::clone(&pool), root.path().to_path_buf());
            std::thread::spawn(move || {
                let scanned = pool.host_rescan(&root, None, true, false);
                let _ = done_tx.send(());
                scanned
            })
        };
        assert!(
            done_rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "the scan ran while the sweep held the gate"
        );

        drop(sweeping);
        let stats = scan.join().unwrap().expect("the scan runs once the sweep is done");
        assert_eq!(stats.updated, 1);
    }

    /// A scan holds the gate for as long as it runs, whichever way it was started: while a hosted
    /// rescan is in progress a sweep must find the gate taken. The check watches the running scan
    /// instead of timing a sweep against it, so a scan that never takes the gate fails the test
    /// whatever the machine's speed.
    #[test]
    fn a_hosted_rescan_holds_the_gate_while_it_runs() {
        use crate::mcp::HostBackend;

        crate::store::init_isolated_cache();
        let root = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(root.path())
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::write(root.path().join("a.rs"), "pub fn a() {}\n").unwrap();
        let pool = std::sync::Arc::new(crate::comms::workspace_pool::WorkspacePool::new(2));
        let gate = pool.blob_gc_gate();

        let scan = {
            let (pool, root) = (std::sync::Arc::clone(&pool), root.path().to_path_buf());
            std::thread::spawn(move || pool.host_rescan(&root, None, true, false))
        };
        let mut gate_taken = false;
        while !gate_taken && !scan.is_finished() {
            gate_taken = gate.try_sweep(Duration::ZERO).is_none();
            std::thread::yield_now();
        }
        let stats = scan.join().unwrap().expect("the scan runs");

        assert_eq!(stats.updated, 1);
        assert!(gate_taken, "the scan ran without holding the blob-GC gate");
    }
}
