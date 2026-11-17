//! Coalesces refusal records, so a launched agent looping on a refused call can neither grow
//! the audit log without end nor keep the disk under it busy.
//!
//! Per caller and outcome, the first [`BURST`] refusals in a [`WINDOW`] are written; later ones
//! are counted, and the next one written carries the count. What is still counted when the
//! supervisor is dropped is written then.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Refusals written per caller and outcome in one window.
pub(crate) const BURST: u32 = 8;
/// The window refusals are counted over.
pub(crate) const WINDOW: Duration = Duration::from_secs(60);
/// The most callers tracked at once; past it, callers share one count per outcome.
const MAX_KEYS: usize = 256;

#[derive(Debug)]
struct Window {
    start: Instant,
    written: u32,
    suppressed: u64,
}

/// The refusal counts of one supervisor.
#[derive(Debug, Default)]
pub(crate) struct Refusals {
    windows: Mutex<HashMap<(String, &'static str), Window>>,
}

impl Refusals {
    /// Whether a refusal of `caller` with `outcome` is written now: `Some` with how many like it
    /// were suppressed since the last one written, or `None` when this one is suppressed.
    pub fn admit(&self, caller: &str, outcome: &'static str) -> Option<u64> {
        let now = Instant::now();
        let mut windows = self.windows.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut key = (caller.to_owned(), outcome);
        if windows.len() >= MAX_KEYS && !windows.contains_key(&key) {
            windows.retain(|_, window| now.duration_since(window.start) < WINDOW || window.suppressed > 0);
            if windows.len() >= MAX_KEYS {
                key = ("*".to_owned(), outcome);
            }
        }
        let window = windows.entry(key).or_insert(Window {
            start: now,
            written: 0,
            suppressed: 0,
        });
        if now.duration_since(window.start) >= WINDOW {
            window.start = now;
            window.written = 0;
        }
        if window.written < BURST {
            window.written += 1;
            Some(std::mem::take(&mut window.suppressed))
        } else {
            window.suppressed += 1;
            None
        }
    }

    /// The counts still suppressed, by caller and outcome, emptied.
    pub fn drain(&self) -> Vec<(String, &'static str, u64)> {
        let mut windows = self.windows.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        windows
            .drain()
            .filter(|(_, window)| window.suppressed > 0)
            .map(|((caller, outcome), window)| (caller, outcome, window.suppressed))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_burst_is_written_and_the_rest_counted_per_caller_and_outcome() {
        let refusals = Refusals::default();
        let written = (0..20)
            .filter(|_| refusals.admit("agent", "fan_out_exceeded").is_some())
            .count();
        assert_eq!(written, BURST as usize);
        // Another outcome, and another caller, have counts of their own.
        assert_eq!(refusals.admit("agent", "depth_exceeded"), Some(0));
        assert_eq!(refusals.admit("other", "fan_out_exceeded"), Some(0));
        assert_eq!(refusals.drain(), [("agent".to_owned(), "fan_out_exceeded", 12)]);
        assert!(refusals.drain().is_empty());
    }
}
