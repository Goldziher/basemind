//! When a process started, for telling a daemon from a later process that was given its pid.
//!
//! A pidfile outlives the daemon that wrote it, and the system hands the number out again. The
//! daemon was running when it wrote the record, so a process that started after the record was
//! written cannot be it. The start is read by [`crate::boot_identity`].

use crate::daemon_lock::DaemonRecord;

/// Clock steps and the one-second grain of the start times read below.
const SLACK_SECS: i64 = 2;

/// Whether the process now running under `record.pid` can be the daemon that wrote `record`:
/// it started no later than the record says the daemon did. `true` as well where the platform
/// gives no start time, so the check narrows what may be signalled and never widens it.
pub fn can_have_written(record: &DaemonRecord) -> bool {
    crate::boot_identity::process_started_unix(record.pid)
        .is_none_or(|started| started <= record.started_unix + SLACK_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon_lock::DaemonKind;

    fn now() -> i64 {
        let since_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a clock after 1970");
        i64::try_from(since_epoch.as_secs()).expect("seconds fit")
    }

    fn record(pid: u32, started_unix: i64) -> DaemonRecord {
        DaemonRecord {
            pid,
            kind: DaemonKind::Comms,
            dir: "comms".into(),
            version: "0.33.0".to_string(),
            started_unix,
        }
    }

    /// A record older than the process under its pid was written by an earlier process: the
    /// daemon died and its number was given out again.
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    #[test]
    fn a_process_that_started_after_the_record_did_not_write_it() {
        const AN_HOUR: i64 = 3600;
        let me = std::process::id();
        let started = crate::boot_identity::process_started_unix(me).expect("this process has a start time");
        assert!(can_have_written(&record(me, now())), "a record written just now");
        assert!(can_have_written(&record(me, started)), "the record of its own start");
        assert!(!can_have_written(&record(me, started - AN_HOUR)));
    }

    #[test]
    fn a_pid_nothing_runs_under_has_no_start_time_and_is_not_ruled_out_by_it() {
        const UNUSED_PID: u32 = 0x7FFF_FFFE;
        assert_eq!(crate::boot_identity::process_started_unix(UNUSED_PID), None);
        assert!(can_have_written(&record(UNUSED_PID, 0)));
    }
}
