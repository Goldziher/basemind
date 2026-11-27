//! When the process-wide embedding engine is released.
//!
//! The engine (one ONNX session and its weights, ~275 MiB for the default preset) is loaded by the
//! first embedding and kept by xberg until [`super::release_engines`]. Releasing it after every full
//! scan pass made a daemon serving searches load and drop it every few seconds, so a pass now
//! releases only the engine it loaded itself and that no request used, and anything else is
//! released once the engine has been idle for [`IDLE_RELEASE_AFTER`].

use std::sync::{Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

/// How long the engine stays resident after its last embedding while no pass is running. Requests
/// arriving closer together than this share one load.
pub(crate) const IDLE_RELEASE_AFTER: Duration = Duration::from_secs(120);

/// What an embedding served: a request (a search query or a memory note, one text) or a scan pass
/// (batches of chunks).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Use {
    Request,
    Pass,
}

/// The bookkeeping behind the release decisions. Every method takes the current time, so the rules
/// are tested without a clock.
#[derive(Debug, Default)]
pub(crate) struct Residency {
    /// An embedding succeeded since the engine was last released.
    resident: bool,
    /// Full passes in flight.
    passes: usize,
    /// Embeddings in flight, of either kind.
    embeds: usize,
    /// The engine was resident when the first of the passes in flight began.
    resident_at_pass_start: bool,
    /// A request embedded while a pass was in flight.
    request_during_passes: bool,
    /// When the last embedding ended.
    last_use: Option<Instant>,
}

impl Residency {
    pub(crate) fn begin_pass(&mut self) {
        if self.passes == 0 {
            self.resident_at_pass_start = self.resident;
            self.request_during_passes = false;
        }
        self.passes += 1;
    }

    /// End a pass; `true` when the engine is to be released now: this was the last pass in flight,
    /// the passes loaded the engine (it was not resident when they began), no request used it while
    /// they ran and no embedding is still running.
    pub(crate) fn end_pass(&mut self) -> bool {
        self.passes = self.passes.saturating_sub(1);
        self.passes == 0
            && self.resident
            && !self.resident_at_pass_start
            && !self.request_during_passes
            && self.embeds == 0
    }

    pub(crate) fn begin_use(&mut self) {
        self.embeds += 1;
    }

    /// End an embedding of `kind` at `now`; `loaded` says it ran the model (the engine is resident).
    pub(crate) fn end_use(&mut self, kind: Use, loaded: bool, now: Instant) {
        self.embeds = self.embeds.saturating_sub(1);
        self.last_use = Some(now);
        if loaded {
            self.resident = true;
        }
        if kind == Use::Request && self.passes > 0 {
            self.request_during_passes = true;
        }
    }

    /// When the idle release is due, or `None` while nothing is resident or the engine is in use.
    pub(crate) fn idle_deadline(&self, grace: Duration) -> Option<Instant> {
        if !self.resident || self.passes > 0 || self.embeds > 0 {
            return None;
        }
        Some(self.last_use.map_or_else(Instant::now, |at| at + grace))
    }

    /// Whether the engine has been idle for `grace` at `now`.
    pub(crate) fn idle_release_due(&self, now: Instant, grace: Duration) -> bool {
        self.idle_deadline(grace).is_some_and(|deadline| now >= deadline)
    }

    /// Record that the engines were dropped.
    pub(crate) fn released(&mut self) {
        self.resident = false;
    }

    pub(crate) fn passes(&self) -> usize {
        self.passes
    }

    /// Embeddings and reranks in flight.
    #[cfg(all(test, any(feature = "documents", feature = "code-search")))]
    pub(crate) fn in_use(&self) -> usize {
        self.embeds
    }
}

static STATE: Mutex<Residency> = Mutex::new(Residency {
    resident: false,
    passes: 0,
    embeds: 0,
    resident_at_pass_start: false,
    request_during_passes: false,
    last_use: None,
});

/// Wakes the idle releaser when the state it waits on changes.
static CHANGED: Condvar = Condvar::new();

pub(crate) fn state() -> MutexGuard<'static, Residency> {
    STATE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Note the end of an embedding, and start the idle releaser the first time the engine loads.
pub(crate) fn end_use(kind: Use, loaded: bool) {
    state().end_use(kind, loaded, Instant::now());
    if loaded {
        spawn_idle_releaser();
    }
    CHANGED.notify_all();
}

/// Wake the idle releaser after a pass or embedding ended.
pub(crate) fn notify() {
    CHANGED.notify_all();
}

/// One thread for the life of the process that releases the engine once it has been idle for
/// [`IDLE_RELEASE_AFTER`]. It sleeps on [`CHANGED`] while nothing is resident or the engine is in
/// use, and otherwise until the idle deadline, so an idle process takes no periodic wake-ups.
fn spawn_idle_releaser() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        let spawned = std::thread::Builder::new().name("bm-embed-idle".into()).spawn(|| {
            let mut st = state();
            loop {
                st = match st.idle_deadline(IDLE_RELEASE_AFTER) {
                    None => CHANGED.wait(st).unwrap_or_else(PoisonError::into_inner),
                    Some(_) if st.idle_release_due(Instant::now(), IDLE_RELEASE_AFTER) => {
                        let dropped = super::clear_engines();
                        st.released();
                        drop(st);
                        super::release_memory_if(dropped);
                        tracing::info!(dropped, "embedding engine idle; released resident embedding engines");
                        state()
                    }
                    Some(deadline) => {
                        let wait = deadline.saturating_duration_since(Instant::now());
                        CHANGED.wait_timeout(st, wait).unwrap_or_else(PoisonError::into_inner).0
                    }
                };
            }
        });
        if let Err(error) = spawned {
            tracing::warn!(
                ?error,
                "could not start the embedding idle releaser; the engine stays resident"
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const GRACE: Duration = Duration::from_secs(120);

    fn embed(state: &mut Residency, kind: Use, now: Instant) {
        state.begin_use();
        state.end_use(kind, true, now);
    }

    #[test]
    fn a_pass_that_loaded_the_engine_and_served_no_request_releases_it() {
        let (mut state, t0) = (Residency::default(), Instant::now());
        state.begin_pass();
        embed(&mut state, Use::Pass, t0);
        assert!(state.end_pass());
    }

    #[test]
    fn a_pass_that_embedded_nothing_releases_nothing() {
        let mut state = Residency::default();
        state.begin_pass();
        assert!(!state.end_pass(), "nothing was loaded");
    }

    #[test]
    fn a_pass_never_releases_an_engine_that_was_resident_when_it_began() {
        let (mut state, t0) = (Residency::default(), Instant::now());
        embed(&mut state, Use::Request, t0);
        for pass in 0..3 {
            state.begin_pass();
            embed(&mut state, Use::Pass, t0 + Duration::from_secs(pass));
            assert!(!state.end_pass(), "pass {pass} released the engine a request loaded");
        }
    }

    #[test]
    fn a_request_during_the_pass_keeps_the_engine_the_pass_loaded() {
        let (mut state, t0) = (Residency::default(), Instant::now());
        state.begin_pass();
        embed(&mut state, Use::Pass, t0);
        embed(&mut state, Use::Request, t0);
        assert!(!state.end_pass());
    }

    #[test]
    fn an_embedding_in_flight_keeps_the_engine_past_the_pass() {
        let (mut state, t0) = (Residency::default(), Instant::now());
        state.begin_pass();
        embed(&mut state, Use::Pass, t0);
        state.begin_use();
        assert!(!state.end_pass());
    }

    #[test]
    fn only_the_last_of_concurrent_passes_releases() {
        let (mut state, t0) = (Residency::default(), Instant::now());
        state.begin_pass();
        state.begin_pass();
        embed(&mut state, Use::Pass, t0);
        assert!(!state.end_pass(), "a pass is still running");
        assert!(state.end_pass());
    }

    #[test]
    fn the_idle_release_waits_for_the_grace_after_the_last_use() {
        let (mut state, t0) = (Residency::default(), Instant::now());
        assert_eq!(state.idle_deadline(GRACE), None, "nothing resident");
        embed(&mut state, Use::Request, t0);
        assert!(!state.idle_release_due(t0 + GRACE - Duration::from_secs(1), GRACE));
        embed(&mut state, Use::Request, t0 + Duration::from_secs(60));
        assert!(
            !state.idle_release_due(t0 + GRACE, GRACE),
            "a later use restarts the grace"
        );
        assert!(state.idle_release_due(t0 + Duration::from_secs(60) + GRACE, GRACE));
        state.released();
        assert_eq!(state.idle_deadline(GRACE), None);
    }

    #[test]
    fn the_idle_release_waits_for_passes_and_embeddings_in_flight() {
        let (mut state, t0) = (Residency::default(), Instant::now());
        embed(&mut state, Use::Request, t0);
        let late = t0 + 10 * GRACE;
        state.begin_pass();
        assert!(!state.idle_release_due(late, GRACE));
        assert!(!state.end_pass());
        state.begin_use();
        assert!(!state.idle_release_due(late, GRACE));
        state.end_use(Use::Request, true, t0);
        assert!(state.idle_release_due(late, GRACE));
    }

    #[test]
    fn a_failed_embedding_does_not_mark_the_engine_resident() {
        let (mut state, t0) = (Residency::default(), Instant::now());
        state.begin_pass();
        state.begin_use();
        state.end_use(Use::Pass, false, t0);
        assert!(!state.end_pass());
        assert_eq!(state.idle_deadline(GRACE), None);
    }
}
