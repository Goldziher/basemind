//! Per-phase peak memory of one scan.
//!
//! The `memory` log lines show the footprint over time but leave the phase attribution to whoever
//! reads them next to the breadcrumb. [`PhasePeaks`] folds the same samples into the phase the scan
//! was in when each was taken, so the scan report can state "extract peaked at 500 MB, resolve at
//! 5.5 GB" without a log trawl. The phases are the `PHASE_*` vocabulary of [`crate::scan_evidence`].
//!
//! Peaks are the highest *footprint* ([`crate::sysres::MemoryReading::used_bytes`], the same figure
//! the `memory` line prints as `rss_mb`) seen by the sampler thread plus one sample at each phase
//! boundary, so a transient shorter than the sampling interval can still be missed.

use std::sync::{Arc, Mutex, PoisonError};

/// Peak footprint observed while the scan was in `phase`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhasePeak {
    /// One of the `PHASE_*` names in [`crate::scan_evidence`].
    pub phase: &'static str,
    /// Highest footprint sampled during the phase, in bytes.
    pub peak_bytes: u64,
}

#[derive(Debug)]
struct Inner {
    current: &'static str,
    peaks: Vec<PhasePeak>,
}

/// Shared handle: the scan advances the phase, the sampler thread feeds readings. Cloning shares
/// the same tally.
#[derive(Debug, Clone)]
pub struct PhasePeaks {
    inner: Arc<Mutex<Inner>>,
}

impl Default for PhasePeaks {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                current: crate::scan_evidence::PHASE_STARTING,
                peaks: Vec::new(),
            })),
        }
    }
}

impl PhasePeaks {
    /// Switch to `phase` and take a boundary sample so a phase shorter than the sampling interval
    /// still records a value.
    pub fn enter(&self, phase: &'static str) {
        self.lock().current = phase;
        self.sample_now();
    }

    /// Fold one footprint reading into the current phase.
    pub fn observe(&self, bytes: u64) {
        let mut inner = self.lock();
        let phase = inner.current;
        match inner.peaks.iter_mut().find(|p| p.phase == phase) {
            Some(peak) => peak.peak_bytes = peak.peak_bytes.max(bytes),
            None => inner.peaks.push(PhasePeak {
                phase,
                peak_bytes: bytes,
            }),
        }
    }

    /// Read the footprint now and fold it in. A platform with no reading contributes nothing.
    pub fn sample_now(&self) {
        if let Some(reading) = crate::sysres::memory_reading() {
            self.observe(reading.used_bytes);
        }
    }

    /// The peaks so far, in the order the phases were first observed.
    pub fn snapshot(&self) -> Vec<PhasePeak> {
        self.lock().peaks.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `enter` also takes a real boundary sample, so injected values are chosen far from any real
    /// footprint: `u64::MAX` marks the phase that received it.
    #[test]
    fn readings_fold_into_the_phase_that_was_current() {
        let peaks = PhasePeaks::default();
        peaks.enter("extract");
        peaks.observe(u64::MAX);
        peaks.observe(1);
        peaks.enter("resolve");
        peaks.observe(1);

        let snap = peaks.snapshot();
        let of = |phase: &str| snap.iter().find(|p| p.phase == phase).map(|p| p.peak_bytes);
        assert_eq!(of("extract"), Some(u64::MAX), "extract keeps its own maximum");
        assert!(
            of("resolve") < Some(u64::MAX),
            "extract readings do not leak into resolve"
        );
    }

    #[test]
    fn a_clone_shares_the_tally_with_the_sampler() {
        let peaks = PhasePeaks::default();
        let sampler = peaks.clone();
        peaks.enter("flush");
        sampler.observe(u64::MAX);
        let snap = peaks.snapshot();
        assert_eq!(
            snap.iter().find(|p| p.phase == "flush").map(|p| p.peak_bytes),
            Some(u64::MAX)
        );
    }
}
