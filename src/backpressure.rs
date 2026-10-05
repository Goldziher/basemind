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
//! - after `max_wait` over the ceiling the advisory [`FootprintGate::admit`] admits anyway
//!   ([`AdmitOutcome::WaitedOut`]), trading a memory overshoot for guaranteed forward progress.
//!   That is only safe for a leaf that is already serialised by something else. The document
//!   tier is not: it used [`admit`](FootprintGate::admit) and, with every worker over the ceiling,
//!   every worker was admitted after five seconds, so the ceiling bounded nothing. It uses
//!   [`FootprintGate::admit_exclusive`] instead, which admits at most ONE worker at a time while
//!   over the ceiling (an [`AdmitOutcome::Serialized`] admission holds a process-wide token for
//!   the duration of its work) and admits immediately when nothing else is running, since waiting
//!   on an idle process can never free memory.
//!
//! The gate holds no global state: the scanner constructs one per admit point from the
//! injected [`Config`](crate::config), sampling [`crate::sysres::phys_footprint`]. Tests
//! inject a stub sampler to drive the over-then-under transition deterministically without
//! touching real memory.

use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock, PoisonError, TryLockError};
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
    /// Admitted while over the ceiling as the single overshoot admission: no other worker is
    /// admitted over the ceiling until this one's [`Admission`] drops.
    Serialized,
}

/// Shared bookkeeping behind [`FootprintGate::admit_exclusive`]: the overshoot token and the number
/// of live admissions. Process-wide in production ([`GLOBAL_ADMISSIONS`]); tests build their own so
/// they cannot observe one another.
pub struct AdmissionState {
    overshoot: Mutex<()>,
    in_flight: AtomicUsize,
}

impl AdmissionState {
    const fn new() -> Self {
        Self {
            overshoot: Mutex::new(()),
            in_flight: AtomicUsize::new(0),
        }
    }
}

static GLOBAL_ADMISSIONS: AdmissionState = AdmissionState::new();

thread_local! {
    /// Address of the [`AdmissionState`] whose overshoot token this thread currently holds, or 0.
    /// Lets nested admissions on the same thread pass through instead of deadlocking on the token
    /// their own caller holds.
    static HOLDS_OVERSHOOT: Cell<usize> = const { Cell::new(0) };
}

/// A live admission from [`FootprintGate::admit_exclusive`]. Hold it for the duration of the work it
/// admitted: dropping it releases the overshoot token (if any) and the in-flight count.
#[must_use = "the admission must be held while the admitted work runs"]
pub struct Admission {
    outcome: AdmitOutcome,
    state: &'static AdmissionState,
    token: Option<MutexGuard<'static, ()>>,
}

impl Admission {
    fn new(state: &'static AdmissionState, outcome: AdmitOutcome, token: Option<MutexGuard<'static, ()>>) -> Self {
        state.in_flight.fetch_add(1, Ordering::AcqRel);
        Self { outcome, state, token }
    }

    /// How the gate admitted this worker.
    pub fn outcome(&self) -> AdmitOutcome {
        self.outcome
    }
}

impl Drop for Admission {
    fn drop(&mut self) {
        self.state.in_flight.fetch_sub(1, Ordering::AcqRel);
        if self.token.take().is_some() {
            HOLDS_OVERSHOOT.with(|held| held.set(0));
        }
    }
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
    admissions: &'static AdmissionState,
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
            admissions: &GLOBAL_ADMISSIONS,
        }
    }

    /// Use `state` instead of the process-wide admission bookkeeping. Test-only.
    #[cfg(test)]
    pub fn with_admissions(mut self, state: &'static AdmissionState) -> Self {
        self.admissions = state;
        self
    }

    /// True when a single piece of work estimated at `estimated_bytes` cannot fit under the ceiling
    /// even on an otherwise idle process. Such work must be skipped, not waited for: no amount of
    /// parking makes it fit, and admitting it anyway is exactly how a ceiling stops meaning anything.
    /// Always false for a disabled gate.
    pub fn exceeds_budget(&self, estimated_bytes: u64) -> bool {
        self.limit_bytes != 0 && estimated_bytes > self.limit_bytes
    }

    /// The ceiling in mebibytes (0 when disabled). For log fields.
    pub fn limit_mb(&self) -> u64 {
        self.limit_bytes / BYTES_PER_MB
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

impl<S> FootprintGate<S>
where
    S: Fn() -> Option<u64>,
{
    /// Admit one unit of heavy work, serialising it while the process is over the ceiling.
    ///
    /// Under the ceiling this is [`admit`](Self::admit) (returns at once, any number of workers).
    /// Over it, only the holder of the process-wide overshoot token proceeds:
    /// - with nothing else in flight it proceeds immediately, because parking an idle process
    ///   cannot release anything (the resident baseline alone can exceed a small ceiling; waiting
    ///   there made every item stall the full `max_wait` for no benefit);
    /// - otherwise it parks up to `max_wait` for in-flight work to finish and free memory, then
    ///   proceeds anyway — but alone, so the overshoot is one unit of work, not one per worker.
    ///
    /// Workers that do not hold the token keep re-sampling and are admitted the moment the
    /// footprint drops under the ceiling, or inherit the token when the holder's [`Admission`]
    /// drops. A thread that already holds the token passes straight through.
    pub fn admit_exclusive(&self) -> Admission {
        let state = self.admissions;
        if self.limit_bytes == 0 {
            return Admission::new(state, AdmitOutcome::Disabled, None);
        }
        let identity = std::ptr::from_ref(state) as usize;
        if HOLDS_OVERSHOOT.with(Cell::get) == identity {
            return Admission::new(state, AdmitOutcome::Serialized, None);
        }
        let start = Instant::now();
        let mut parked = false;
        let mut token: Option<MutexGuard<'static, ()>> = None;
        loop {
            match (self.sampler)() {
                None => return Admission::new(state, AdmitOutcome::Unavailable, None),
                Some(footprint) if footprint <= self.limit_bytes => {
                    let outcome = if parked {
                        AdmitOutcome::Throttled
                    } else {
                        AdmitOutcome::Clear
                    };
                    return Admission::new(state, outcome, None);
                }
                Some(_) => {
                    if token.is_none() {
                        token = match state.overshoot.try_lock() {
                            Ok(guard) => Some(guard),
                            Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
                            Err(TryLockError::WouldBlock) => None,
                        };
                    }
                    if token.is_some() {
                        let idle = state.in_flight.load(Ordering::Acquire) == 0;
                        let elapsed = start.elapsed();
                        if idle || elapsed >= self.max_wait {
                            if !idle {
                                tracing::warn!(
                                    limit_mb = self.limit_bytes / BYTES_PER_MB,
                                    waited_ms = elapsed.as_millis() as u64,
                                    "footprint gate over ceiling for max_wait; admitting one unit of work alone"
                                );
                            }
                            HOLDS_OVERSHOOT.with(|held| held.set(identity));
                            return Admission::new(state, AdmitOutcome::Serialized, token);
                        }
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

    /// A fresh bookkeeping state per test: the production one is process-wide, so tests sharing it
    /// would see each other's admissions.
    fn isolated_state() -> &'static AdmissionState {
        Box::leak(Box::new(AdmissionState::new()))
    }

    fn over_gate(state: &'static AdmissionState, max_wait: Duration) -> FootprintGate<impl Fn() -> Option<u64>> {
        FootprintGate::with_sampler(100, || Some(500 * MB))
            .with_timing(Duration::from_millis(1), max_wait)
            .with_admissions(state)
    }

    /// The bug: every worker over the ceiling was admitted after `max_wait`, so N workers meant N
    /// overshooting extractions and the ceiling bounded nothing. Over the ceiling exactly one
    /// worker may be inside at a time, however many arrive.
    #[test]
    fn over_the_ceiling_the_gate_serialises_instead_of_opening_the_floodgates() {
        let state = isolated_state();
        let live = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let admitted = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    let gate = over_gate(state, Duration::from_millis(5));
                    let admission = gate.admit_exclusive();
                    assert_eq!(admission.outcome(), AdmitOutcome::Serialized);
                    peak.fetch_max(live.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(15));
                    live.fetch_sub(1, Ordering::SeqCst);
                    admitted.fetch_add(1, Ordering::SeqCst);
                    drop(admission);
                });
            }
        });
        assert_eq!(admitted.load(Ordering::SeqCst), 8, "serialising must not starve anyone");
        assert_eq!(
            peak.load(Ordering::SeqCst),
            1,
            "more than one worker was inside while the process was over the ceiling"
        );
        assert_eq!(
            state.in_flight.load(Ordering::SeqCst),
            0,
            "every admission must release"
        );
    }

    /// The advisory `admit` keeps its documented behaviour (it is what the drive governor reads),
    /// which is exactly why the document tier must not use it: all callers pass after `max_wait`.
    #[test]
    fn the_advisory_admit_still_admits_every_waiter_which_is_why_documents_do_not_use_it() {
        let peak = AtomicUsize::new(0);
        let live = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    let gate = FootprintGate::with_sampler(100, || Some(500 * MB))
                        .with_timing(Duration::from_millis(1), Duration::from_millis(5));
                    assert_eq!(gate.admit(), AdmitOutcome::WaitedOut);
                    peak.fetch_max(live.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(30));
                    live.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        assert!(peak.load(Ordering::SeqCst) > 1, "advisory admission overlaps by design");
    }

    /// Parking an idle process frees nothing: the resident baseline alone can exceed a small ceiling,
    /// and waiting `max_wait` per item there made scans crawl without lowering memory.
    #[test]
    fn an_idle_process_over_the_ceiling_is_admitted_without_waiting() {
        let gate = over_gate(isolated_state(), Duration::from_secs(30));
        let start = Instant::now();
        let admission = gate.admit_exclusive();
        assert_eq!(admission.outcome(), AdmitOutcome::Serialized);
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "nothing is running, so there is nothing to wait for (took {:?})",
            start.elapsed()
        );
    }

    /// With other work in flight, waiting can help: the gate parks the full `max_wait` for it to
    /// finish, then proceeds alone.
    #[test]
    fn with_other_work_in_flight_the_gate_waits_max_wait_then_proceeds_alone() {
        let state = isolated_state();
        let under = FootprintGate::with_sampler(100, || Some(10 * MB)).with_admissions(state);
        let running = under.admit_exclusive();
        assert_eq!(running.outcome(), AdmitOutcome::Clear);

        let gate = over_gate(state, Duration::from_millis(40));
        let start = Instant::now();
        let admission = gate.admit_exclusive();
        assert_eq!(admission.outcome(), AdmitOutcome::Serialized);
        assert!(
            start.elapsed() >= Duration::from_millis(40),
            "must give in-flight work its max_wait"
        );
        drop((admission, running));
        assert_eq!(state.in_flight.load(Ordering::SeqCst), 0);
    }

    /// Workers that lost the token are admitted in parallel again the moment memory recovers; the
    /// serialisation lasts only as long as the overshoot does.
    #[test]
    fn waiters_resume_in_parallel_once_the_footprint_drops() {
        let state = isolated_state();
        let over = std::sync::atomic::AtomicBool::new(true);
        let gate = || {
            FootprintGate::with_sampler(100, || {
                Some(if over.load(Ordering::SeqCst) { 500 * MB } else { 10 * MB })
            })
            .with_timing(Duration::from_millis(1), Duration::from_secs(30))
            .with_admissions(state)
        };
        let holder = gate().admit_exclusive();
        assert_eq!(holder.outcome(), AdmitOutcome::Serialized);
        std::thread::scope(|scope| {
            let waiter = scope.spawn(|| gate().admit_exclusive().outcome());
            std::thread::sleep(Duration::from_millis(20));
            assert!(
                !waiter.is_finished(),
                "a second worker must wait while over the ceiling"
            );
            over.store(false, Ordering::SeqCst);
            let outcome = waiter.join().expect("waiter");
            assert_eq!(outcome, AdmitOutcome::Throttled, "admitted without needing the token");
        });
        drop(holder);
    }

    /// A thread that already holds the overshoot token (nested extraction on the same worker) must
    /// pass straight through; blocking on its own token would deadlock the scan.
    #[test]
    fn a_nested_admission_on_the_token_holding_thread_does_not_deadlock() {
        let state = isolated_state();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let outer = over_gate(state, Duration::from_millis(5)).admit_exclusive();
            let inner = over_gate(state, Duration::from_millis(5)).admit_exclusive();
            tx.send((outer.outcome(), inner.outcome())).ok();
        });
        let outcomes = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("nested admission deadlocked on its own token");
        assert_eq!(outcomes, (AdmitOutcome::Serialized, AdmitOutcome::Serialized));
    }

    #[test]
    fn a_unit_of_work_larger_than_the_whole_ceiling_is_over_budget() {
        let gate = FootprintGate::with_sampler(100, || Some(0));
        assert!(!gate.exceeds_budget(100 * MB), "exactly the ceiling still fits");
        assert!(gate.exceeds_budget(100 * MB + 1));
        let disabled = FootprintGate::with_sampler(0, || Some(0));
        assert!(!disabled.exceeds_budget(u64::MAX), "no ceiling, no verdict");
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
