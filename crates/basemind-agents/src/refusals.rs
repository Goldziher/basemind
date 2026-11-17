//! Coalesces refusal records, so a launched agent looping on a refused call can neither grow
//! the audit log without end nor keep the disk under it busy.
//!
//! Per caller, the first [`BURST`] records in a [`WINDOW`] are written, whatever their outcome,
//! so a caller cannot multiply its share by varying what it gets refused; later ones are
//! counted, and the next one written carries the count. What is still counted when the
//! supervisor is dropped is written then, under the last suppressed record's event and outcome.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::audit::AuditEvent;

/// Records written per caller in one window.
pub(crate) const BURST: u32 = 8;
/// The window refusals are counted over.
pub(crate) const WINDOW: Duration = Duration::from_secs(60);
/// The most callers tracked at once; past it, callers share one count.
const MAX_KEYS: usize = 256;

#[derive(Debug)]
struct Window {
    start: Instant,
    written: u32,
    suppressed: u64,
    /// The event and outcome of the last record suppressed.
    last: (AuditEvent, &'static str),
}

/// A count still suppressed when the supervisor is dropped.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Pending {
    pub caller: String,
    pub event: AuditEvent,
    pub outcome: &'static str,
    pub suppressed: u64,
}

/// The refusal counts of one supervisor.
#[derive(Debug, Default)]
pub(crate) struct Refusals {
    windows: Mutex<HashMap<String, Window>>,
}

impl Refusals {
    /// Whether a record of `caller` (of `event` with `outcome`) is written now: `Some` with how
    /// many of the caller's were suppressed since the last one written, or `None` when this one
    /// is suppressed.
    pub fn admit(&self, caller: &str, event: AuditEvent, outcome: &'static str) -> Option<u64> {
        let now = Instant::now();
        let mut windows = self.windows.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut key = caller.to_owned();
        if windows.len() >= MAX_KEYS && !windows.contains_key(&key) {
            windows.retain(|_, window| now.duration_since(window.start) < WINDOW || window.suppressed > 0);
            if windows.len() >= MAX_KEYS {
                key = "*".to_owned();
            }
        }
        let window = windows.entry(key).or_insert(Window {
            start: now,
            written: 0,
            suppressed: 0,
            last: (event, outcome),
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
            window.last = (event, outcome);
            None
        }
    }

    /// The counts still suppressed, by caller, emptied.
    pub fn drain(&self) -> Vec<Pending> {
        let mut windows = self.windows.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        windows
            .drain()
            .filter(|(_, window)| window.suppressed > 0)
            .map(|(caller, window)| Pending {
                caller,
                event: window.last.0,
                outcome: window.last.1,
                suppressed: window.suppressed,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_burst_is_written_and_the_rest_counted_per_caller_across_outcomes() {
        let refusals = Refusals::default();
        let outcomes = ["fan_out_exceeded", "depth_exceeded", "unknown_session", "not_owner"];
        let written = (0..20)
            .filter(|at| {
                refusals
                    .admit("agent", AuditEvent::Refusal, outcomes[at % outcomes.len()])
                    .is_some()
            })
            .count();
        assert_eq!(written, BURST as usize);
        // Another outcome has no count of its own; another caller does.
        assert_eq!(refusals.admit("agent", AuditEvent::Stop, "already_stopping"), None);
        assert_eq!(
            refusals.admit("other", AuditEvent::Refusal, "fan_out_exceeded"),
            Some(0)
        );
        assert_eq!(
            refusals.drain(),
            [Pending {
                caller: "agent".to_owned(),
                event: AuditEvent::Stop,
                outcome: "already_stopping",
                suppressed: 13,
            }]
        );
        assert!(refusals.drain().is_empty());
    }
}
