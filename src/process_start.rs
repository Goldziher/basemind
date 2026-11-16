//! When a process started, for telling a daemon from a later process that was given its pid.
//!
//! A pidfile outlives the daemon that wrote it, and the system hands the number out again. A
//! record names the daemon's own process start, and the process at its pid is the daemon exactly
//! when that start is the one it has now. A record an older release wrote names none; the daemon
//! was running when it wrote it, so a process that started after the record was written cannot be
//! it. The starts are read by [`crate::boot_identity`].

use crate::daemon_lock::DaemonRecord;

/// Clock steps and the one-second grain of the start times read below.
const SLACK_SECS: i64 = 2;

/// Whether the process now running under `record.pid` can be the daemon that wrote `record`. By
/// the recorded process start when there is one: the wall-clock rule below breaks once the clock
/// is stepped past the slack, because on Linux a start in wall-clock time is the boot time plus the
/// ticks since, and the boot time moves with the clock. Otherwise by wall-clock time: the process
/// started no later than the record says the daemon did, and `true` where the platform gives no
/// start time, so the check narrows what may be signalled and never widens it.
pub fn can_have_written(record: &DaemonRecord) -> bool {
    let this_boot = crate::boot_identity::current();
    crate::boot_identity::is_recorded_process(
        record.pid,
        record.process_start,
        record.boot.as_deref(),
        this_boot.as_deref(),
    )
    .unwrap_or_else(|| {
        crate::boot_identity::process_started_unix(record.pid)
            .is_none_or(|started| started <= record.started_unix + SLACK_SECS)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon_lock::DaemonKind;

    const AN_HOUR: i64 = 3600;

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
            process_start: None,
            boot: None,
        }
    }

    /// A record older than the process under its pid was written by an earlier process: the
    /// daemon died and its number was given out again.
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    #[test]
    fn a_process_that_started_after_the_record_did_not_write_it() {
        let me = std::process::id();
        let started = crate::boot_identity::process_started_unix(me).expect("this process has a start time");
        assert!(can_have_written(&record(me, now())), "a record written just now");
        assert!(can_have_written(&record(me, started)), "the record of its own start");
        assert!(!can_have_written(&record(me, started - AN_HOUR)));
    }

    /// The wall clock was stepped after the daemon started: by wall-clock time the live daemon
    /// now seems to have started an hour after its record. Its recorded start still names it, and
    /// names no other process given the pid.
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    #[test]
    fn a_recorded_start_names_the_daemon_whatever_the_clock_did() {
        let me = std::process::id();
        let own = crate::boot_identity::process_start(me).expect("this process has a start");
        let started = crate::boot_identity::process_started_unix(me).expect("and a start time");
        let stepped = DaemonRecord {
            process_start: Some(own),
            boot: crate::boot_identity::current(),
            ..record(me, started - AN_HOUR)
        };
        assert!(can_have_written(&stepped), "the daemon itself");
        let other = DaemonRecord {
            process_start: Some(own.wrapping_add(1)),
            ..record(me, now())
        };
        assert!(!can_have_written(&other), "another process given the pid");
        let earlier_boot = DaemonRecord {
            process_start: Some(own),
            boot: Some("id:an-earlier-boot".to_string()),
            ..record(me, now())
        };
        // A boot named only by its time is never told apart by it. ~keep
        if crate::boot_identity::current().is_some_and(|boot| boot.starts_with("id:")) {
            assert!(
                !can_have_written(&earlier_boot),
                "a start of another boot names no process of this one"
            );
        }
    }

    #[test]
    fn a_pid_nothing_runs_under_has_no_start_time_and_is_not_ruled_out_by_it() {
        const UNUSED_PID: u32 = 0x7FFF_FFFE;
        assert_eq!(crate::boot_identity::process_started_unix(UNUSED_PID), None);
        assert!(can_have_written(&record(UNUSED_PID, 0)));
    }
}
