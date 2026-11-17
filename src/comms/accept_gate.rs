//! What every daemon accept loop shares: a back-off after a failed accept, a rate-limited log of
//! it, a cap on the connections one front-end serves at once, and a first-byte deadline.
//!
//! An accept that fails at once (`EMFILE` / `ENFILE` when the process is out of descriptors, or a
//! transient network error) used to be retried immediately: one core at 100% and a `warn` line per
//! iteration for as long as the descriptors stayed exhausted. Nothing capped how many connections a
//! client could hold open, which is how the descriptors run out in the first place.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Most connections one front-end serves at once. A further client waits in the listen backlog
/// until one closes. Far above what the sessions of one user hold (one relay per session, short
/// CLI links), and far below the descriptor budget [`crate::comms::fd_budget`] provides.
pub const MAX_CONNECTIONS: usize = 256;

/// First pause after a failed accept; it doubles on each further failure.
const FIRST_BACKOFF: Duration = Duration::from_millis(10);
/// Longest pause between two accepts that fail.
const MAX_BACKOFF: Duration = Duration::from_secs(1);
/// At most one log line per this long while accepts keep failing.
const LOG_EVERY: Duration = Duration::from_secs(10);

/// How long a connection may stay silent before its first byte. A client writes at once; one that
/// never does is dropped rather than held, because a held link keeps the daemon from idling out.
pub const FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(30);

/// The semaphore of one front-end's connection slots.
pub fn connection_slots() -> Arc<Semaphore> {
    Arc::new(Semaphore::new(MAX_CONNECTIONS))
}

/// Wait for a free connection slot. The permit lives as long as the connection's task.
pub async fn next_slot(slots: &Arc<Semaphore>) -> OwnedSemaphorePermit {
    // The semaphore is never closed, so `acquire_owned` cannot fail. ~keep
    Arc::clone(slots)
        .acquire_owned()
        .await
        .unwrap_or_else(|_| unreachable!("connection slots are never closed"))
}

/// Back-off and log state of one accept loop.
pub struct AcceptBackoff {
    what: &'static str,
    next: Duration,
    last_log: Option<Instant>,
    suppressed: u64,
}

impl AcceptBackoff {
    /// State for the accept loop named `what` in the log.
    pub fn new(what: &'static str) -> Self {
        Self {
            what,
            next: FIRST_BACKOFF,
            last_log: None,
            suppressed: 0,
        }
    }

    /// An accept succeeded: the next failure pauses for the shortest time again.
    pub fn succeeded(&mut self) {
        self.next = FIRST_BACKOFF;
    }

    /// An accept failed: log it unless one was logged within [`LOG_EVERY`], then pause before the
    /// next attempt, longer each time up to [`MAX_BACKOFF`].
    pub async fn failed(&mut self, error: &std::io::Error) {
        let now = Instant::now();
        if self.last_log.is_none_or(|last| now.duration_since(last) >= LOG_EVERY) {
            tracing::warn!(
                front_end = self.what,
                %error,
                suppressed = self.suppressed,
                retry_in_ms = self.next.as_millis() as u64,
                "comms: accept failed; backing off"
            );
            self.last_log = Some(now);
            self.suppressed = 0;
        } else {
            self.suppressed += 1;
        }
        tokio::time::sleep(self.next).await;
        self.next = (self.next * 2).min(MAX_BACKOFF);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An accept that fails at once, as on `EMFILE`, must not be retried in a hot loop. Half a
    /// second of failures is a handful of attempts with the back-off (10+20+40+80+160 ms), and
    /// millions without it.
    #[tokio::test]
    async fn failing_accepts_are_retried_a_bounded_number_of_times() {
        let mut backoff = AcceptBackoff::new("test");
        let emfile = std::io::Error::from_raw_os_error(24);
        let started = Instant::now();
        let mut attempts = 0u32;
        while started.elapsed() < Duration::from_millis(500) {
            attempts += 1;
            backoff.failed(&emfile).await;
        }
        assert!(attempts <= 7, "{attempts} accept attempts in 500 ms");
        assert!(backoff.suppressed >= 1, "only the first failure is logged");
    }

    #[tokio::test(start_paused = true)]
    async fn the_backoff_is_capped_and_resets_after_a_success() {
        let mut backoff = AcceptBackoff::new("test");
        let error = std::io::Error::other("transient");
        for _ in 0..20 {
            backoff.failed(&error).await;
        }
        assert_eq!(backoff.next, MAX_BACKOFF);
        backoff.succeeded();
        assert_eq!(backoff.next, FIRST_BACKOFF);
    }

    #[tokio::test]
    async fn a_front_end_holds_at_most_its_connection_cap() {
        let slots = connection_slots();
        let mut held = Vec::new();
        for _ in 0..MAX_CONNECTIONS {
            held.push(next_slot(&slots).await);
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(50), next_slot(&slots))
                .await
                .is_err(),
            "one connection past the cap must wait"
        );
        held.pop();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), next_slot(&slots))
                .await
                .is_ok(),
            "a closed connection frees its slot"
        );
    }
}
