//! Terminal-store-failure handling for the [`Broker`].
//!
//! Split out of `daemon.rs` as its own concern: everything here exists for the state where the
//! store will never serve again, which is not request handling but the end of the daemon's life.
//! See [`store_health`](super::store_health) for what counts as terminal and where the evidence of
//! it is written.

use std::sync::atomic::Ordering;

use super::daemon::Broker;
use super::store::CommsStoreError;

impl Broker {
    /// Tell the broker which comms dir backs its store, so a terminal store failure can be written
    /// there before the daemon exits. Called once by the daemon entry point; idempotent.
    pub fn install_comms_dir(&self, dir: std::path::PathBuf) {
        let _ = self.comms_dir.set(dir);
    }

    /// Whether a terminal store failure has been latched. The daemon entry point consults this after
    /// the serve loop returns so the process exits non-zero — a wedged store is not a clean stop.
    pub fn hit_fatal_store_failure(&self) -> bool {
        self.fatal_store.load(Ordering::Relaxed)
    }

    /// Inspect a store error from the request path and, if it proves the store will never serve
    /// again, persist it, log it, and drain. Called on every store error, so this is the single
    /// place the transient/terminal distinction is made.
    ///
    /// A terminal error must not be answered-and-forgotten: the same failure will meet every
    /// subsequent request for this process's lifetime. Holding the singleton past that point is the
    /// whole defect — the process keeps the flock and the socket, so `pid_is_live` stays true and no
    /// replacement can take over, while every request fails identically. Giving the lock up lets the
    /// next `comms start` get a clean process. Latches, so a burst of poisoned requests drains once
    /// and records the FIRST error, which is the one that explains the wedge.
    pub(super) async fn note_store_error(&self, method: &str, error: &CommsStoreError) {
        if !super::store_health::is_fatal(error) {
            return;
        }
        if self.fatal_store.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(dir) = self.comms_dir.get() {
            super::store_health::persist(dir, method, error);
        }
        tracing::error!(
            %error,
            method,
            "comms: store is permanently unusable; releasing the daemon lock and exiting so a clean daemon can take over"
        );
        self.begin_drain().await;
    }
}
