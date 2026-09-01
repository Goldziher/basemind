//! Best-effort memory backpressure for the allocation-heavy stages of a scan.
//!
//! A [`FootprintGate`] throttles admission to the two stages that dominate a scan's peak
//! memory — document extraction and code-chunk embedding — against the `[resources]`
//! `max_footprint_mb` ceiling. When the process is over the ceiling, the calling worker
//! parks in a bounded backoff loop so in-flight work can complete and release memory before
//! more is admitted.
//!
//! It is deliberately *best-effort* and never fails a scan:
//! - `max_footprint_mb = "off"`, or an auto ceiling on a platform that reports no memory
//!   limit, makes the gate a no-op ([`AdmitOutcome::Disabled`]).
//! - a sampler that cannot read the footprint (an unsupported platform, or a failed syscall)
//!   admits immediately ([`AdmitOutcome::Unavailable`]).
//! - after `max_wait` over the ceiling the gate admits anyway ([`AdmitOutcome::WaitedOut`]),
//!   trading a memory overshoot for guaranteed forward progress — the goal is to shave the
//!   peak, not to enforce an invariant the allocator won't.
//!
//! The gate holds no global state: the scanner constructs one per admit point from the
//! injected [`Config`](crate::config), sampling [`crate::sysres::phys_footprint`]. Tests
//! inject a stub sampler to drive the over-then-under transition deterministically without
//! touching real memory.

use std::sync::{Condvar, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use crate::config::MaxFootprint;

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
    /// The gate has no ceiling to enforce (`max_footprint_mb = "off"`, or auto on a platform
    /// that reports no limit); admitted without sampling.
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
    /// Construct a gate for the `[resources] max_footprint_mb` setting, sampling the real
    /// process footprint via [`crate::sysres::phys_footprint`].
    ///
    /// The setting is resolved here rather than by the caller because auto mode has to consult
    /// the environment — see [`MaxFootprint::resolve_mb`]. That sample is rate-limited inside
    /// `sysres`, so constructing a gate per admit point stays cheap. A resolved ceiling of `0`
    /// (`"off"`, or auto with no detectable limit) yields a disabled gate whose
    /// [`admit`](FootprintGate::admit) is a no-op.
    pub fn new(setting: MaxFootprint) -> Self {
        FootprintGate::with_sampler(setting.resolve_mb(), crate::sysres::phys_footprint)
    }
}

impl<S> FootprintGate<S>
where
    S: Fn() -> Option<u64>,
{
    /// Construct a gate from an already-resolved mebibyte ceiling (`0` = disabled) and an
    /// injected sampler. Used by tests to drive the over-then-under transition
    /// deterministically, and by [`FootprintGate::new`] once auto has been resolved.
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

/// Counting semaphore bounding how many documents are extracted at once, enforcing
/// `[resources] max_concurrent_documents`.
///
/// A `Mutex` + `Condvar` and not an async primitive: the waiters are rayon workers, which are
/// blocking OS threads with no reactor to yield to.
///
/// Distinct from [`FootprintGate`], which reacts to memory *already* allocated. A document
/// extraction's spike (xberg's decoded page buffers, OCR bitmaps, an embedding batch) lands faster
/// than the 50 ms sampler can see it, so a footprint ceiling alone bounds the corpus after the
/// fact. This bounds the number of spikes that can overlap, before the first one starts.
struct DocSemaphore {
    available: Mutex<usize>,
    released: Condvar,
}

/// The process-wide document semaphore, or `None` when this caller's `max_concurrent_documents` is
/// `0` (auto, today's unbounded dispatch).
///
/// The `0` case returns before touching the `OnceLock`, which is the whole subtlety here. A daemon
/// hosts many workspaces in one process and `0` is the *default*, so initialising the cell from the
/// first caller regardless of its value would let one default-configured workspace latch "no
/// semaphore" and silently disable the knob for every workspace opened afterwards — a config that
/// parses, validates, and does nothing. That is precisely the failure mode `max_footprint_mb` had
/// in issue #62, and it is not worth reproducing in the fix for it.
///
/// The first caller that actually asks for a bound still sets the capacity for the life of the
/// process, mirroring [`scanner_pool`](crate::scanner_file::scanner_pool) and `embed_pool`: the
/// workers it bounds are themselves a process-global pool, so a per-scan limit would not be one.
fn doc_semaphore(max_concurrent: usize) -> Option<&'static DocSemaphore> {
    if max_concurrent == 0 {
        return None;
    }
    static SEMAPHORE: OnceLock<DocSemaphore> = OnceLock::new();
    Some(SEMAPHORE.get_or_init(|| DocSemaphore {
        available: Mutex::new(max_concurrent),
        released: Condvar::new(),
    }))
}

/// One held document-extraction slot; releases it on drop.
pub struct DocSlot {
    semaphore: &'static DocSemaphore,
}

impl Drop for DocSlot {
    fn drop(&mut self) {
        let mut available = self.semaphore.available.lock().unwrap_or_else(PoisonError::into_inner);
        *available += 1;
        self.semaphore.released.notify_one();
    }
}

/// Block until a document-extraction slot is free, returning the guard that holds it. `None` — the
/// unbounded default — is returned immediately and costs nothing.
///
/// Never acquired while holding the store lock or an open index batch: a bounded wait behind a lock
/// the releasing worker also needs would deadlock rather than throttle.
pub fn acquire_doc_slot(max_concurrent: usize) -> Option<DocSlot> {
    let semaphore = doc_semaphore(max_concurrent)?;
    let mut available = semaphore.available.lock().unwrap_or_else(PoisonError::into_inner);
    while *available == 0 {
        available = semaphore
            .released
            .wait(available)
            .unwrap_or_else(PoisonError::into_inner);
    }
    *available -= 1;
    drop(available);
    Some(DocSlot { semaphore })
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

    /// F6: `max_concurrent_documents` was parsed and never consumed. The semaphore is the consumer,
    /// so the property to pin is the one an operator sets it for — no more than `LIMIT` extractions
    /// are ever in flight, however many rayon workers arrive at once.
    ///
    /// The semaphore is process-global and first-*bounded*-caller-wins, so this is the only test in
    /// the crate that may call [`acquire_doc_slot`] with a non-zero limit. Calling it with `0` is
    /// always safe and never latches anything, which is the property
    /// [`unbounded_callers_do_not_latch_the_semaphore`] pins.
    #[test]
    fn the_document_semaphore_bounds_concurrent_extractions() {
        const LIMIT: usize = 2;
        let live = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    let _slot = acquire_doc_slot(LIMIT).expect("a positive limit must yield a real slot");
                    peak.fetch_max(live.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(5));
                    live.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        let peak = peak.load(Ordering::SeqCst);
        assert!(peak >= 1, "every worker must eventually get a slot");
        assert!(peak <= LIMIT, "at most {LIMIT} extractions may overlap, saw {peak}");
        assert_eq!(live.load(Ordering::SeqCst), 0, "every slot must be released on drop");
    }

    /// A `0` (auto) caller must not decide for the process. `0` is the default, and a daemon hosts
    /// many workspaces in one process: if the first one to extract a document happened to be
    /// default-configured, latching "no semaphore" would silently disable
    /// `max_concurrent_documents` for every workspace opened after it.
    ///
    /// Ordering is the assertion. This runs `0` first and then a bounded limit, and the bounded
    /// limit must still bind.
    #[test]
    fn unbounded_callers_do_not_latch_the_semaphore() {
        assert!(
            acquire_doc_slot(0).is_none(),
            "an auto limit yields no slot and must cost nothing"
        );
        assert!(
            acquire_doc_slot(0).is_none(),
            "repeating it must not latch a decision either"
        );
        assert!(
            acquire_doc_slot(1).is_some(),
            "a later bounded caller must still get a real semaphore"
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
