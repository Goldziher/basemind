//! Reporting a clock that reads behind the store.
//!
//! The store recovers by itself (see the module's "Time" section): the next post is stamped from
//! the clock again. What was stamped while the clock read ahead stays stamped ahead, though, and
//! that is worth a line in the log, because it explains messages that outlive their retention
//! and threads that hold a place under the cap. [`rules::clock_behind`] decides when the clock
//! is behind; this module only keeps a run of posts from reporting it every time.

use crate::comms::v2::rules;

/// Least time between two reports made while posting, in microseconds of the clock.
const SKEW_WARN_INTERVAL_MICROS: i64 = 10 * 60 * 1_000_000;

/// When a clock behind the store was last reported, so that a run of posts reports it once.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct SkewLog {
    reported_at: Option<i64>,
}

impl SkewLog {
    /// By how much the clock is behind `last_ts`, the previous stamp, when that is to be
    /// reported now: it is over the tolerance, and the last report was not within the interval.
    /// The interval is measured on the clock in either direction, since the clock is what is in
    /// doubt.
    pub(super) fn due(&mut self, clock_micros: i64, last_ts: i64) -> Option<i64> {
        let behind = rules::clock_behind(clock_micros, last_ts)?;
        let recent = self
            .reported_at
            .is_some_and(|reported| clock_micros.abs_diff(reported) < SKEW_WARN_INTERVAL_MICROS.unsigned_abs());
        if recent {
            return None;
        }
        self.reported_at = Some(clock_micros);
        Some(behind)
    }
}

/// Log that the clock reads `behind_micros` behind the previous stamp, noticed `when`.
pub(super) fn warn(behind_micros: i64, when: &str) {
    tracing::warn!(
        behind_secs = behind_micros / 1_000_000,
        when,
        "comms: the clock reads behind the previous message's timestamp, so it was ahead when that message was \
         posted or has been set back since; posts are stamped from the clock as it reads now. If it was ahead, \
         messages stamped then expire late by the difference, and a thread created or posted to then is listed \
         first and counts against its repository's thread cap until the clock passes that time or the thread \
         is archived. If it is behind now, messages stamped now expire early by the difference, without \
         their threads"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINUTE: i64 = 60 * 1_000_000;

    #[test]
    fn a_run_of_posts_reports_the_skew_once_per_interval() {
        let mut log = SkewLog::default();
        let last_ts = 100 * MINUTE;

        assert_eq!(log.due(0, last_ts), Some(100 * MINUTE));
        assert_eq!(log.due(MINUTE, last_ts + 1), None);
        assert_eq!(log.due(-MINUTE, last_ts + 2), None, "the clock went back again");
        assert_eq!(log.due(10 * MINUTE, last_ts + 3), Some(90 * MINUTE + 3));
        assert_eq!(log.due(99 * MINUTE + 10, last_ts + 4), None, "within the tolerance");
        assert_eq!(log.due(-9 * MINUTE, last_ts), Some(109 * MINUTE));
    }
}
