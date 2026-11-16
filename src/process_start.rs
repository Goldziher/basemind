//! When a process started, for telling a daemon from a later process that was given its pid.
//!
//! A pidfile outlives the daemon that wrote it, and the system hands the number out again. The
//! daemon was running when it wrote the record, so a process that started after the record was
//! written cannot be it.

use crate::daemon_lock::DaemonRecord;

/// Clock steps and the one-second grain of the start times read below.
const SLACK_SECS: i64 = 2;

/// Whether the process now running under `record.pid` can be the daemon that wrote `record`:
/// it started no later than the record says the daemon did. `true` as well where the platform
/// gives no start time (see [`started_unix`]), so the check narrows what may be signalled and
/// never widens it.
pub fn can_have_written(record: &DaemonRecord) -> bool {
    started_unix(record.pid).is_none_or(|started| started <= record.started_unix + SLACK_SECS)
}

/// Unix seconds at which the process now running as `pid` started. `None` when there is no such
/// process, it may not be inspected, or the platform is not macOS, Linux or Windows.
pub fn started_unix(pid: u32) -> Option<i64> {
    imp::started_unix(pid)
}

#[cfg(target_os = "macos")]
mod imp {
    pub(super) fn started_unix(pid: u32) -> Option<i64> {
        let pid = libc::c_int::try_from(pid).ok()?;
        let size = libc::c_int::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
        // SAFETY: `proc_bsdinfo` is plain data for which all-zero bytes are a valid value.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        // SAFETY: the buffer is a live `proc_bsdinfo` and `size` is its size, which is all
        // `proc_pidinfo` writes for `PROC_PIDTBSDINFO`.
        let written = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
        (written == size).then(|| i64::try_from(info.pbi_start_tvsec).ok())?
    }
}

#[cfg(target_os = "linux")]
mod imp {
    pub(super) fn started_unix(pid: u32) -> Option<i64> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let ticks = start_ticks(&stat)?;
        let boot = std::fs::read_to_string("/proc/stat")
            .ok()?
            .lines()
            .find_map(|line| line.strip_prefix("btime "))?
            .trim()
            .parse::<i64>()
            .ok()?;
        // SAFETY: `sysconf` reads a constant of the running system.
        let per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        (per_second > 0).then(|| boot + ticks / i64::from(per_second))
    }

    /// Field 22 of `/proc/<pid>/stat`: the start time in clock ticks since boot. The command in
    /// field 2 may hold spaces and parentheses, so the fields are counted from its closing one.
    pub(super) fn start_ticks(stat: &str) -> Option<i64> {
        const FIELDS_AFTER_COMMAND_BEFORE_START: usize = 19;
        let (_, after_command) = stat.rsplit_once(')')?;
        after_command
            .split_ascii_whitespace()
            .nth(FIELDS_AFTER_COMMAND_BEFORE_START)?
            .parse()
            .ok()
    }
}

#[cfg(windows)]
mod imp {
    /// A Win32 `FILETIME`: 100 ns intervals since 1601-01-01, in two halves.
    #[repr(C)]
    #[derive(Default)]
    struct FileTime {
        low: u32,
        high: u32,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
        fn CloseHandle(handle: isize) -> i32;
        fn GetProcessTimes(
            process: isize,
            creation: *mut FileTime,
            exit: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
    }

    pub(super) fn started_unix(pid: u32) -> Option<i64> {
        const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
        const INTERVALS_PER_SECOND: u64 = 10_000_000;
        const SECONDS_FROM_1601_TO_1970: i64 = 11_644_473_600;

        let (mut creation, mut exit, mut kernel, mut user) = Default::default();
        // SAFETY: `OpenProcess` and `CloseHandle` take only values; `GetProcessTimes` writes the
        // four `FileTime`s it is given, which live until it returns.
        let read = unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if process == 0 {
                return None;
            }
            let read = GetProcessTimes(
                process,
                &raw mut creation,
                &raw mut exit,
                &raw mut kernel,
                &raw mut user,
            );
            CloseHandle(process);
            read
        };
        if read == 0 {
            return None;
        }
        let intervals = (u64::from(creation.high) << 32) | u64::from(creation.low);
        Some(i64::try_from(intervals / INTERVALS_PER_SECOND).ok()? - SECONDS_FROM_1601_TO_1970)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
mod imp {
    pub(super) fn started_unix(_pid: u32) -> Option<i64> {
        None
    }
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

    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    #[test]
    fn this_process_started_before_now_and_not_long_ago() {
        const A_DAY: i64 = 24 * 3600;
        let started = started_unix(std::process::id()).expect("this process has a start time");
        assert!(
            started <= now() && started > now() - A_DAY,
            "{started} against {}",
            now()
        );
    }

    /// A record older than the process under its pid was written by an earlier process: the
    /// daemon died and its number was given out again.
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    #[test]
    fn a_process_that_started_after_the_record_did_not_write_it() {
        const AN_HOUR: i64 = 3600;
        let me = std::process::id();
        let started = started_unix(me).expect("this process has a start time");
        assert!(can_have_written(&record(me, now())), "a record written just now");
        assert!(can_have_written(&record(me, started)), "the record of its own start");
        assert!(!can_have_written(&record(me, started - AN_HOUR)));
    }

    #[test]
    fn a_pid_nothing_runs_under_has_no_start_time_and_is_not_ruled_out_by_it() {
        const UNUSED_PID: u32 = 0x7FFF_FFFE;
        assert_eq!(started_unix(UNUSED_PID), None);
        assert!(can_have_written(&record(UNUSED_PID, 0)));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_start_field_is_found_behind_a_command_with_spaces_and_parentheses() {
        let stat = "4242 (a (odd) name) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 987654 20 21";
        assert_eq!(imp::start_ticks(stat), Some(987_654));
        assert_eq!(imp::start_ticks("4242 (short) S 1 2"), None);
    }
}
