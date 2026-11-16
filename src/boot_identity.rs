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
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "ios", windows))]
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
    #[cfg(target_os = "linux")]
    {
        // Field 22, `starttime`, in clock ticks since boot; the command name before it may hold
        // spaces and parentheses, so fields are counted from the last `)`.
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let after_name = &stat[stat.rfind(')')? + 1..];
        after_name.split_whitespace().nth(19)?.parse().ok()
    }
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        let pid = libc::c_int::try_from(pid).ok()?;
        // SAFETY: `proc_bsdinfo` is plain old data, so all zeroes is a valid value.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = libc::c_int::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
        // SAFETY: `PROC_PIDTBSDINFO` writes at most `size` bytes of a `proc_bsdinfo` into `info`.
        let written = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                std::ptr::addr_of_mut!(info).cast(),
                size,
            )
        };
        (written == size).then(|| {
            info.pbi_start_tvsec
                .wrapping_mul(1_000_000)
                .wrapping_add(info.pbi_start_tvusec)
        })
    }
    #[cfg(windows)]
    {
        /// `OpenProcess` access mask: query basic info only.
        const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
        let mut created = win::FileTime::default();
        let mut exited = win::FileTime::default();
        let mut kernel = win::FileTime::default();
        let mut user = win::FileTime::default();
        // SAFETY: `OpenProcess`/`CloseHandle` take only primitive arguments; `GetProcessTimes`
        // writes one `FILETIME` into each of the four out-pointers, which outlive the call.
        let ok = unsafe {
            let handle = win::OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle == 0 {
                return None;
            }
            let ok = win::GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user);
            win::CloseHandle(handle);
            ok
        };
        (ok != 0).then(|| (u64::from(created.high) << 32) | u64::from(created.low))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "ios", windows)))]
    {
        let _ = pid;
        None
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
    /// Win32 `FILETIME`: 100 ns intervals since 1601, split in two halves.
    #[repr(C)]
    #[derive(Default)]
    pub(super) struct FileTime {
        pub(super) low: u32,
        pub(super) high: u32,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        /// Milliseconds since the system started.
        pub(super) fn GetTickCount64() -> u64;
        /// Returns a process handle (0 on failure).
        pub(super) fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
        /// Nonzero on success.
        pub(super) fn GetProcessTimes(
            handle: isize,
            creation: *mut FileTime,
            exit: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
        pub(super) fn CloseHandle(handle: isize) -> i32;
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

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn the_running_boot_has_an_id_that_does_not_change() {
        let first = current().expect("this platform names its boot");
        let second = current().expect("still");
        assert!(
            first.starts_with(ID_PREFIX),
            "{first} is a per-boot id, not a time the clock moves"
        );
        assert_eq!(
            same_boot(&first, &second),
            Some(true),
            "{first} and {second} are one boot"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_process_start_names_one_process() {
        let own = process_start(std::process::id()).expect("this process has a start");
        assert_eq!(process_start(std::process::id()), Some(own), "stable across readings");
        assert_eq!(process_start(u32::MAX - 11), None, "no process has that pid");
        let mut child = std::process::Command::new("sleep").arg("30").spawn().expect("spawn");
        let other = process_start(child.id());
        let _ = child.kill();
        let _ = child.wait();
        assert!(other.is_some_and(|other| other != own), "{other:?} vs {own}");
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
