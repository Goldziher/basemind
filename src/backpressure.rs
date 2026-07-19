//! Best-effort memory backpressure for the allocation-heavy stages of a scan.
//!
//! A [`FootprintGate`] throttles admission to the two stages that dominate a scan's peak
//! memory — document extraction and code-chunk embedding — against the `[resources]`
//! `max_footprint_mb` ceiling. When the process is over the ceiling, the calling worker
//! parks in a bounded backoff loop so in-flight work can complete and release memory before
//! more is admitted.
//!
//! It is deliberately *best-effort* and never fails a scan:
//! - `max_footprint_mb == 0` (the default) makes the gate a no-op ([`AdmitOutcome::Disabled`]).
//! - a sampler that cannot read the footprint (non-macOS, or a failed syscall) admits
//!   immediately ([`AdmitOutcome::Unavailable`]).
//! - after `max_wait` over the ceiling the gate admits anyway ([`AdmitOutcome::WaitedOut`]),
//!   trading a memory overshoot for guaranteed forward progress — the goal is to shave the
//!   peak, not to enforce an invariant the allocator won't.
//!
//! The gate holds no global state: the scanner constructs one per admit point from the
//! injected [`Config`](crate::config), sampling [`crate::sysres::phys_footprint`]. Tests
//! inject a stub sampler to drive the over-then-under transition deterministically without
//! touching real memory.

use std::time::{Duration, Instant};

/// Bytes in one mebibyte — the unit `max_footprint_mb` is expressed in.
const BYTES_PER_MB: u64 = 1024 * 1024;

/// How long a throttled worker sleeps between footprint re-samples.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Upper bound on how long a single [`FootprintGate::admit`] call parks before giving up and
/// admitting anyway. Caps the worst-case stall a misconfigured ceiling can impose on a scan.
const DEFAULT_MAX_WAIT: Duration = Duration::from_secs(5);

/// Outcome of a [`FootprintGate::admit`] call. Returned for observability and to let tests
/// assert whether throttling actually happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmitOutcome {
    /// The gate is disabled (`max_footprint_mb == 0`); admitted without sampling.
    Disabled,
    /// The sampler could not read the footprint; admitted without throttling.
    Unavailable,
    /// The footprint was already under the ceiling; admitted without waiting.
    Clear,
    /// The worker parked while over the ceiling and was admitted once it dropped under.
    Throttled,
    /// The worker parked for the full `max_wait` while still over the ceiling and was admitted
    /// anyway to guarantee forward progress.
    WaitedOut,
}

/// A best-effort admission gate keyed on the process physical footprint. Cheap to construct (a
/// couple of scalar fields plus the sampler), so the scanner builds one per admit point rather
/// than threading a shared instance through the scan.
///
/// Generic over the sampler so the production path uses a zero-cost `fn` pointer while tests
/// inject a stateful closure. The default type parameter lets call sites write
/// `FootprintGate::new(mb)` without naming the sampler.
pub struct FootprintGate<S = fn() -> Option<u64>>
where
    S: Fn() -> Option<u64>,
{
    limit_bytes: u64,
    sampler: S,
    poll_interval: Duration,
    max_wait: Duration,
}

impl FootprintGate {
    /// Construct a gate for a `max_footprint_mb` ceiling, sampling the real process footprint via
    /// [`crate::sysres::phys_footprint`]. `max_footprint_mb == 0` yields a disabled gate whose
    /// [`admit`](FootprintGate::admit) is a no-op.
    pub fn new(max_footprint_mb: usize) -> Self {
        FootprintGate::with_sampler(max_footprint_mb, crate::sysres::phys_footprint)
    }
}

impl<S> FootprintGate<S>
where
    S: Fn() -> Option<u64>,
{
    /// Construct a gate with an injected sampler. Used by tests to drive the over-then-under
    /// transition deterministically.
    pub fn with_sampler(max_footprint_mb: usize, sampler: S) -> Self {
        Self {
            limit_bytes: (max_footprint_mb as u64).saturating_mul(BYTES_PER_MB),
            sampler,
            poll_interval: DEFAULT_POLL_INTERVAL,
            max_wait: DEFAULT_MAX_WAIT,
        }
    }

    /// Override the poll interval and max wait. Test-only: production always uses the defaults
    /// ([`DEFAULT_POLL_INTERVAL`] / [`DEFAULT_MAX_WAIT`]), which suit a real scan.
    #[cfg(test)]
    pub fn with_timing(mut self, poll_interval: Duration, max_wait: Duration) -> Self {
        self.poll_interval = poll_interval;
        self.max_wait = max_wait;
        self
    }

    /// Park the calling thread while the process footprint exceeds the ceiling, re-sampling every
    /// `poll_interval`, up to `max_wait`. Returns the [`AdmitOutcome`]. Returns immediately when
    /// the gate is disabled or the sampler yields `None`.
    pub fn admit(&self) -> AdmitOutcome {
        if self.limit_bytes == 0 {
            return AdmitOutcome::Disabled;
        }
        let start = Instant::now();
        let mut parked = false;
        loop {
            match (self.sampler)() {
                None => return AdmitOutcome::Unavailable,
                Some(footprint) if footprint <= self.limit_bytes => {
                    return if parked {
                        AdmitOutcome::Throttled
                    } else {
                        AdmitOutcome::Clear
                    };
                }
                Some(_) => {
                    let elapsed = start.elapsed();
                    if elapsed >= self.max_wait {
                        tracing::warn!(
                            limit_mb = self.limit_bytes / BYTES_PER_MB,
                            waited_ms = elapsed.as_millis() as u64,
                            "footprint gate over ceiling for max_wait; admitting to guarantee progress"
                        );
                        return AdmitOutcome::WaitedOut;
                    }
                    parked = true;
                    std::thread::sleep(self.poll_interval);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    const MB: u64 = 1024 * 1024;

    #[test]
    fn disabled_gate_admits_without_sampling() {
        let polled = AtomicUsize::new(0);
        let gate = FootprintGate::with_sampler(0, || {
            polled.fetch_add(1, Ordering::SeqCst);
            Some(u64::MAX)
        });
        assert_eq!(gate.admit(), AdmitOutcome::Disabled);
        assert_eq!(polled.load(Ordering::SeqCst), 0, "disabled gate must not sample");
    }

    #[test]
    fn unavailable_sample_admits_without_throttling() {
        let gate = FootprintGate::with_sampler(100, || None);
        assert_eq!(gate.admit(), AdmitOutcome::Unavailable);
    }

    #[test]
    fn under_ceiling_admits_without_waiting() {
        let gate = FootprintGate::with_sampler(100, || Some(10 * MB));
        assert_eq!(gate.admit(), AdmitOutcome::Clear);
    }

    #[test]
    fn over_then_under_parks_until_clear() {
        // 200 MB ceiling: first two samples are over it, then it drops under.
        let calls = AtomicUsize::new(0);
        let gate = FootprintGate::with_sampler(200, || {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            if n < 2 { Some(500 * MB) } else { Some(50 * MB) }
        })
        .with_timing(Duration::from_millis(1), Duration::from_secs(5));
        assert_eq!(gate.admit(), AdmitOutcome::Throttled);
        assert!(
            calls.load(Ordering::SeqCst) >= 3,
            "gate must re-sample until the footprint falls under the ceiling"
        );
    }

    #[test]
    fn persistent_over_waits_out_then_admits() {
        let gate = FootprintGate::with_sampler(100, || Some(u64::MAX))
            .with_timing(Duration::from_millis(1), Duration::from_millis(20));
        let start = Instant::now();
        assert_eq!(gate.admit(), AdmitOutcome::WaitedOut);
        assert!(
            start.elapsed() >= Duration::from_millis(20),
            "gate must park the full max_wait before giving up"
        );
    }
}
