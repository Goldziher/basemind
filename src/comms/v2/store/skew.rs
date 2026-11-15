//! Noticing a clock that reads behind the store.
//!
//! The store keeps working when it does (see the module's "Time" section), but messages are
//! then stamped ahead of the clock and outlive their retention by the difference, which an
//! operator can only correct by fixing the clock. A clock that is ahead cannot be told from a
//! store nobody posted to for a while, so only the other direction is reported.

/// How far behind the newest timestamp issued the clock may read before it is reported, in
/// microseconds. Far above what a burst of posts within one clock reading adds, which is one
/// microsecond per post.
pub(super) const SKEW_WARN_MICROS: i64 = 60 * 1_000_000;

/// Least time between two reports made while posting, in microseconds of the clock.
const SKEW_WARN_INTERVAL_MICROS: i64 = 10 * 60 * 1_000_000;

/// By how much `clock_micros` is behind `last_ts`, the newest timestamp issued, when that is
/// more than [`SKEW_WARN_MICROS`].
pub(super) fn clock_behind(clock_micros: i64, last_ts: i64) -> Option<i64> {
    let behind = last_ts.saturating_sub(clock_micros);
    (behind > SKEW_WARN_MICROS).then_some(behind)
}

/// When a clock behind the store was last reported, so that a run of posts reports it once.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct SkewLog {
    reported_at: Option<i64>,
}

impl SkewLog {
    /// By how much the clock is behind, when that is to be reported now: it is over the
    /// threshold, and the last report was not within the interval. The interval is measured on
    /// the clock in either direction, since the clock is what is in doubt.
    pub(super) fn due(&mut self, clock_micros: i64, last_ts: i64) -> Option<i64> {
        let behind = clock_behind(clock_micros, last_ts)?;
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

/// Log that the clock reads `behind_micros` behind the newest timestamp issued, noticed `when`.
pub(super) fn warn(behind_micros: i64, when: &str) {
    tracing::warn!(
        behind_secs = behind_micros / 1_000_000,
        when,
        "comms: the clock is behind the newest message of the store; messages are stamped ahead of it and expire \
         that much later than their retention until it catches up"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINUTE: i64 = 60 * 1_000_000;

    #[test]
    fn only_a_clock_more_than_the_threshold_behind_is_reported() {
        assert_eq!(clock_behind(1_000, 1_000), None);
        assert_eq!(clock_behind(1_000, 1_000 + SKEW_WARN_MICROS), None);
        assert_eq!(
            clock_behind(1_000, 1_001 + SKEW_WARN_MICROS),
            Some(SKEW_WARN_MICROS + 1)
        );
        assert_eq!(clock_behind(1_000 + 400 * MINUTE, 1_000), None, "an idle store");
        assert_eq!(clock_behind(1_000, i64::MIN), None, "a store without a post");
        assert_eq!(clock_behind(i64::MIN, i64::MAX), Some(i64::MAX));
    }

    #[test]
    fn a_run_of_posts_reports_the_skew_once_per_interval() {
        let mut log = SkewLog::default();
        let last_ts = 100 * MINUTE;

        assert_eq!(log.due(0, last_ts), Some(100 * MINUTE));
        assert_eq!(log.due(MINUTE, last_ts + 1), None);
        assert_eq!(log.due(-MINUTE, last_ts + 2), None, "the clock went back again");
        assert_eq!(log.due(10 * MINUTE, last_ts + 3), Some(90 * MINUTE + 3));
        assert_eq!(log.due(99 * MINUTE + 10, last_ts + 4), None, "within the threshold");
        assert_eq!(log.due(-9 * MINUTE, last_ts), Some(109 * MINUTE));
    }
}
