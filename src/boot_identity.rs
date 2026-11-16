//! Which boot of the machine this is, and which process holds a pid, so a file left by a process
//! can be told from one left by a process of the same pid before a restart.
//!
//! Each platform hands out an id per boot: Linux `boot_id`, macOS `kern.bootsessionuuid`, Windows
//! the `BootId` counter. Where none can be read the identity falls back to the time of boot, which
//! moves whenever the clock is stepped, so two readings of it are the same boot when they are within
//! [`BOOT_TIME_SLACK_SECS`] of each other: far less than a machine takes to restart, far more than a
//! clock correction moves it.

use std::time::{Duration, SystemTime};

/// Prefix of an identity that is a boot time in unix seconds.
const TIME_PREFIX: &str = "t:";
/// Prefix of an identity that is an opaque id.
const ID_PREFIX: &str = "id:";
/// How far apart two boot-time readings of one boot may be.
const BOOT_TIME_SLACK_SECS: u64 = 10;

/// The identity of the running boot, or `None` where the platform offers none or it cannot be read.
pub(crate) fn current() -> Option<String> {
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "ios", windows))]
    if let Some(id) = boot_id() {
        return Some(format!("{ID_PREFIX}{id}"));
    }
    let secs = boot_time()?.duration_since(SystemTime::UNIX_EPOCH).ok()?.as_secs();
    Some(format!("{TIME_PREFIX}{secs}"))
}

/// Whether two identities name the same boot; `None` when they are of kinds that cannot be
/// compared (an id against a time).
pub(crate) fn same_boot(a: &str, b: &str) -> Option<bool> {
    let secs = |identity: &str| identity.strip_prefix(TIME_PREFIX)?.parse::<u64>().ok();
    match (secs(a), secs(b)) {
        (Some(a), Some(b)) => Some(a.abs_diff(b) <= BOOT_TIME_SLACK_SECS),
        (None, None) => Some(a == b),
        _ => None,
    }
}

/// When the running boot began by the wall clock, or `None` where it cannot be read. Moves with
/// every clock step: only for evidence that names no boot.
pub(crate) fn boot_time() -> Option<SystemTime> {
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string("/proc/stat").ok()?;
        let secs = stat
            .lines()
            .find_map(|line| line.strip_prefix("btime "))?
            .trim()
            .parse::<u64>()
            .ok()?;
        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
    }
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        let mut boot = libc::timeval { tv_sec: 0, tv_usec: 0 };
        let mut len = std::mem::size_of::<libc::timeval>();
        // SAFETY: `kern.boottime` writes at most `len` bytes of a `timeval` into `boot`, and the
        // name is a NUL-terminated literal.
        let rc = unsafe {
            libc::sysctlbyname(
                c"kern.boottime".as_ptr(),
                std::ptr::addr_of_mut!(boot).cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        let secs = u64::try_from(boot.tv_sec).ok().filter(|secs| rc == 0 && *secs > 0)?;
        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
    }
    #[cfg(windows)]
    {
        // SAFETY: takes no arguments and reads no caller memory.
        let up = Duration::from_millis(unsafe { win::GetTickCount64() });
        SystemTime::now().checked_sub(up)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "ios", windows)))]
    {
        None
    }
}

/// When the process holding `pid` started, as a value that is the same at every reading for one
/// process and differs for the next process given that pid. `None` when there is no such process
/// or it cannot be read. Not comparable across platforms or with wall-clock time.
pub(crate) fn process_start(pid: u32) -> Option<u64> {
    let pid = valid_pid(pid)?;
    #[cfg(target_os = "linux")]
    {
        u64::try_from(linux::start_ticks(pid)?).ok()
    }
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        let (secs, micros) = mac::start(pid)?;
        Some(secs.wrapping_mul(1_000_000).wrapping_add(micros))
    }
    #[cfg(windows)]
    {
        win::creation_time(pid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "ios", windows)))]
    {
        let _ = pid;
        None
    }
}

/// Unix seconds at which the process now running as `pid` started, by the wall clock. `None` when
/// there is no such process, it may not be inspected, or the platform gives no start time. On
/// Linux it is derived from the boot time, so it moves with every clock step: prefer
/// [`process_start`] wherever a recorded value can be compared.
pub(crate) fn process_started_unix(pid: u32) -> Option<i64> {
    let pid = valid_pid(pid)?;
    #[cfg(target_os = "linux")]
    {
        let ticks = linux::start_ticks(pid)?;
        let boot = boot_time()?.duration_since(SystemTime::UNIX_EPOCH).ok()?.as_secs();
        // SAFETY: `sysconf` only reads a constant of the running system.
        let per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        if per_second <= 0 {
            return None;
        }
        Some(i64::try_from(boot).ok()? + ticks / i64::from(per_second))
    }
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        i64::try_from(mac::start(pid)?.0).ok()
    }
    #[cfg(windows)]
    {
        /// `FILETIME` intervals (100 ns) per second.
        const INTERVALS_PER_SECOND: u64 = 10_000_000;
        /// Seconds from the `FILETIME` epoch (1601-01-01) to the unix epoch.
        const SECONDS_FROM_1601_TO_1970: i64 = 11_644_473_600;
        let intervals = win::creation_time(pid)?;
        Some(i64::try_from(intervals / INTERVALS_PER_SECOND).ok()? - SECONDS_FROM_1601_TO_1970)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "ios", windows)))]
    {
        let _ = pid;
        None
    }
}

/// `pid` as the platform's signed pid, or `None` for one no process can have: `0` names the caller's
/// process group to `kill`, and a value past `i32::MAX` turns negative, naming a group or every
/// process.
fn valid_pid(pid: u32) -> Option<i32> {
    i32::try_from(pid).ok().filter(|pid| *pid > 0)
}

/// Whether a process id is still live. Unix: `kill(pid, 0)`, where `0` (signalable) and `EPERM`
/// (exists, not ours) both mean alive and `ESRCH` means gone. Windows: `OpenProcess` succeeds for a
/// live pid. A pid no process can have (see [`valid_pid`]) is never live.
#[cfg(any(unix, windows))]
pub fn pid_is_live(pid: u32) -> bool {
    let Some(pid) = valid_pid(pid) else {
        return false;
    };
    #[cfg(unix)]
    {
        // SAFETY: `kill` with signal 0 performs only an existence/permission check and sends no
        // signal; `pid` is positive, so it names one process, never a group.
        let rc = unsafe { libc::kill(pid, 0) };
        rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(windows)]
    {
        win::is_live(pid.unsigned_abs())
    }
}

#[cfg(target_os = "linux")]
mod linux {
    /// Field 22 of `/proc/<pid>/stat`, `starttime`: clock ticks from the boot to the start.
    pub(super) fn start_ticks(pid: i32) -> Option<i64> {
        start_ticks_in(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
    }

    /// [`start_ticks`] of a `stat` line. The command in field 2 may hold spaces and parentheses, so
    /// the fields are counted from its closing one.
    pub(super) fn start_ticks_in(stat: &str) -> Option<i64> {
        const FIELDS_AFTER_COMMAND_BEFORE_START: usize = 19;
        let (_, after_command) = stat.rsplit_once(')')?;
        after_command
            .split_ascii_whitespace()
            .nth(FIELDS_AFTER_COMMAND_BEFORE_START)?
            .parse()
            .ok()
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
mod mac {
    /// The start of the process `pid`: unix seconds and the microseconds past them.
    pub(super) fn start(pid: i32) -> Option<(u64, u64)> {
        // SAFETY: `proc_bsdinfo` is plain old data, so all zeroes is a valid value.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = libc::c_int::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
        // SAFETY: `info` is a live `proc_bsdinfo` and `size` its size, which is all
        // `PROC_PIDTBSDINFO` writes into it.
        let written = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                std::ptr::addr_of_mut!(info).cast(),
                size,
            )
        };
        (written == size).then_some((info.pbi_start_tvsec, info.pbi_start_tvusec))
    }
}

/// The platform's per-boot id, without its prefix.
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "ios", windows))]
fn boot_id() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
        let id = id.trim();
        (!id.is_empty()).then(|| id.to_string())
    }
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        let mut buf = [0_u8; 64];
        let mut len = buf.len();
        // SAFETY: `kern.bootsessionuuid` writes at most `len` bytes into `buf`, and the name is a
        // NUL-terminated literal.
        let rc = unsafe {
            libc::sysctlbyname(
                c"kern.bootsessionuuid".as_ptr(),
                buf.as_mut_ptr().cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            return None;
        }
        let bytes = buf.get(..len)?;
        let id = std::str::from_utf8(bytes).ok()?.trim_end_matches('\0').trim();
        (!id.is_empty()).then(|| id.to_string())
    }
    #[cfg(windows)]
    {
        /// `HKEY_LOCAL_MACHINE`.
        const HKLM: isize = 0x8000_0002_u32 as i32 as isize;
        /// `RRF_RT_REG_DWORD`: accept only a `REG_DWORD`.
        const RRF_RT_REG_DWORD: u32 = 0x0000_0010;
        let wide = |text: &str| text.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
        let key = wide(r"SYSTEM\CurrentControlSet\Control\Session Manager\Memory Management\PrefetchParameters");
        let value = wide("BootId");
        let mut id: u32 = 0;
        let mut size = u32::try_from(std::mem::size_of::<u32>()).ok()?;
        // SAFETY: both names are NUL-terminated UTF-16 buffers that outlive the call, and with
        // `RRF_RT_REG_DWORD` the call writes at most `size` (4) bytes into `id`.
        let status = unsafe {
            win::RegGetValueW(
                HKLM,
                key.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_DWORD,
                std::ptr::null_mut(),
                std::ptr::addr_of_mut!(id).cast(),
                &mut size,
            )
        };
        (status == 0).then(|| id.to_string())
    }
}

#[cfg(windows)]
mod win {
    /// `OpenProcess` access mask: query basic info only.
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

    /// Win32 `FILETIME`: 100 ns intervals since 1601, split in two halves.
    #[repr(C)]
    #[derive(Default)]
    struct FileTime {
        low: u32,
        high: u32,
    }

    /// An open process handle, closed on drop.
    pub(super) struct Process(isize);

    impl Drop for Process {
        fn drop(&mut self) {
            // SAFETY: `self.0` is a handle `OpenProcess` returned and nothing else closes.
            unsafe { CloseHandle(self.0) };
        }
    }

    /// A handle to the live process `pid`, or `None` when there is none or it may not be queried.
    pub(super) fn open_process(pid: u32) -> Option<Process> {
        // SAFETY: `OpenProcess` takes only values and reads no caller memory.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        (handle != 0).then_some(Process(handle))
    }

    /// Whether `pid` names a process that has not exited. One `OpenProcess` refuses with
    /// `ERROR_ACCESS_DENIED` exists and is not ours (the `EPERM` of `kill`), so it is live; one it
    /// opens is live only until it exits, because a handle anyone still holds keeps an exited
    /// process openable.
    pub(super) fn is_live(pid: u32) -> bool {
        const ERROR_ACCESS_DENIED: i32 = 5;
        /// The exit code `GetExitCodeProcess` reports for a process that has not exited.
        const STILL_ACTIVE: u32 = 259;
        let Some(process) = open_process(pid) else {
            return std::io::Error::last_os_error().raw_os_error() == Some(ERROR_ACCESS_DENIED);
        };
        let mut code = 0;
        // SAFETY: `process` holds an open handle, and `GetExitCodeProcess` writes the `u32` it is
        // given, which lives until it returns.
        let read = unsafe { GetExitCodeProcess(process.0, &raw mut code) };
        read == 0 || code == STILL_ACTIVE
    }

    /// When the process `pid` was created, in `FILETIME` intervals.
    pub(super) fn creation_time(pid: i32) -> Option<u64> {
        let process = open_process(pid.unsigned_abs())?;
        let mut created = FileTime::default();
        let mut exited = FileTime::default();
        let mut kernel = FileTime::default();
        let mut user = FileTime::default();
        // SAFETY: `process` holds an open handle, and `GetProcessTimes` writes one `FILETIME` into
        // each of the four out-pointers, which outlive the call.
        let ok = unsafe { GetProcessTimes(process.0, &mut created, &mut exited, &mut kernel, &mut user) };
        (ok != 0).then(|| (u64::from(created.high) << 32) | u64::from(created.low))
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        /// Milliseconds since the system started.
        pub(super) fn GetTickCount64() -> u64;
        /// Returns a process handle (0 on failure).
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
        /// Nonzero on success.
        fn GetProcessTimes(
            handle: isize,
            creation: *mut FileTime,
            exit: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
        fn CloseHandle(handle: isize) -> i32;
        /// `STILL_ACTIVE` until the process exits (0 on failure).
        fn GetExitCodeProcess(process: isize, code: *mut u32) -> i32;
    }

    #[link(name = "advapi32")]
    unsafe extern "system" {
        /// Returns `ERROR_SUCCESS` (0) on success.
        pub(super) fn RegGetValueW(
            key: isize,
            sub_key: *const u16,
            value: *const u16,
            flags: u32,
            kind: *mut u32,
            data: *mut core::ffi::c_void,
            size: *mut u32,
        ) -> i32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pid no process has, and that every platform's pid type can carry.
    const UNUSED_PID: u32 = 0x7FFF_FFFE;

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn the_running_boot_has_an_identity_that_does_not_change() {
        let first = current().expect("this platform names its boot");
        let second = current().expect("still");
        // Windows keeps its boot id in the registry, where a server edition may not have it; the
        // identity is then the boot time. Elsewhere the id is always there. ~keep
        let id_expected = !cfg!(windows) || boot_id().is_some();
        if id_expected {
            assert!(
                first.starts_with(ID_PREFIX),
                "{first} is a per-boot id, not a time the clock moves"
            );
        } else {
            assert!(first.starts_with(TIME_PREFIX), "{first} falls back to the boot time");
        }
        assert_eq!(
            same_boot(&first, &second),
            Some(true),
            "{first} and {second} are one boot"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn a_process_start_is_stable_and_absent_for_a_pid_no_process_has() {
        let own = process_start(std::process::id()).expect("this process has a start");
        assert_eq!(process_start(std::process::id()), Some(own), "stable across readings");
        assert_eq!(process_start(UNUSED_PID), None, "no process has that pid");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_process_start_names_one_process() {
        let own = process_start(std::process::id()).expect("this process has a start");
        let mut child = std::process::Command::new("sleep").arg("30").spawn().expect("spawn");
        let other = process_start(child.id());
        let _ = child.kill();
        let _ = child.wait();
        assert!(other.is_some_and(|other| other != own), "{other:?} vs {own}");
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn this_process_started_before_now_and_not_long_ago() {
        const A_DAY: i64 = 24 * 3600;
        let now = i64::try_from(
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("a clock after 1970")
                .as_secs(),
        )
        .expect("seconds fit");
        let started = process_started_unix(std::process::id()).expect("this process has a start time");
        assert!(started <= now && started > now - A_DAY, "{started} against {now}");
        assert_eq!(process_started_unix(UNUSED_PID), None);
    }

    /// `kill(0, 0)` reaches the caller's process group and `kill(-1, 0)` every process it may
    /// signal: either reads as live, and a marker or record named by such a pid would never go.
    #[cfg(any(unix, windows))]
    #[test]
    fn a_pid_no_process_can_have_is_never_live() {
        assert!(pid_is_live(std::process::id()), "our own pid is live");
        assert!(!pid_is_live(UNUSED_PID), "an absent pid is not live");
        assert!(!pid_is_live(0), "pid 0 is the caller's process group");
        assert!(!pid_is_live(u32::MAX), "-1 is every process");
        assert!(!pid_is_live(u32::MAX - 11), "a negative pid is a process group");
        assert!(!pid_is_live(0x8000_0000), "past i32::MAX");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_start_field_is_found_behind_a_command_with_spaces_and_parentheses() {
        let stat = "4242 (a (odd) name) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 987654 20 21";
        assert_eq!(linux::start_ticks_in(stat), Some(987_654));
        assert_eq!(linux::start_ticks_in("4242 (short) S 1 2"), None);
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn the_boot_began_before_now() {
        let began = boot_time().expect("this platform dates its boot");
        assert!(began < SystemTime::now());
    }

    #[test]
    fn boot_times_a_clock_correction_apart_are_one_boot_and_a_restart_apart_are_two() {
        assert_eq!(same_boot("t:1000", "t:1000"), Some(true));
        assert_eq!(
            same_boot("t:1000", "t:1010"),
            Some(true),
            "a clock correction moves the boot time"
        );
        assert_eq!(same_boot("t:1010", "t:1000"), Some(true));
        assert_eq!(
            same_boot("t:1000", "t:1011"),
            Some(false),
            "a restart takes longer than the slack"
        );
    }

    #[test]
    fn opaque_ids_are_one_boot_only_when_equal_and_never_compared_with_a_time() {
        assert_eq!(same_boot("id:aa", "id:aa"), Some(true));
        assert_eq!(same_boot("id:aa", "id:ab"), Some(false));
        assert_eq!(same_boot("id:1000", "t:1000"), None);
    }
}
