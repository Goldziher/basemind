//! Which boot of the machine this is, so a file left by a process can be told from one left by a
//! process of the same pid before a restart.
//!
//! Linux hands out a random id per boot. macOS and Windows offer only the time of boot, which
//! moves a little whenever the clock is corrected, so two readings of it are the same boot when
//! they are within [`BOOT_TIME_SLACK_SECS`] of each other: far less than a machine takes to
//! restart, far more than a clock correction moves it.

/// Prefix of an identity that is a boot time in unix seconds.
const TIME_PREFIX: &str = "t:";
/// Prefix of an identity that is an opaque id.
#[cfg(target_os = "linux")]
const ID_PREFIX: &str = "id:";
/// How far apart two boot-time readings of one boot may be.
const BOOT_TIME_SLACK_SECS: u64 = 10;

/// The identity of the running boot, or `None` where the platform offers none or it cannot be read.
pub(crate) fn current() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
        let id = id.trim();
        (!id.is_empty()).then(|| format!("{ID_PREFIX}{id}"))
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
        (rc == 0 && boot.tv_sec > 0).then(|| format!("{TIME_PREFIX}{}", boot.tv_sec))
    }
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            /// Milliseconds since the system started.
            fn GetTickCount64() -> u64;
        }
        // SAFETY: takes no arguments and reads no caller memory.
        let up = unsafe { GetTickCount64() } / 1000;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs();
        Some(format!("{TIME_PREFIX}{}", now.saturating_sub(up)))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "ios", windows)))]
    {
        None
    }
}

/// Whether two identities name the same boot.
pub(crate) fn same_boot(a: &str, b: &str) -> bool {
    let secs = |identity: &str| identity.strip_prefix(TIME_PREFIX)?.parse::<u64>().ok();
    match (secs(a), secs(b)) {
        (Some(a), Some(b)) => a.abs_diff(b) <= BOOT_TIME_SLACK_SECS,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn the_running_boot_has_an_identity_that_does_not_change() {
        let first = current().expect("this platform names its boot");
        let second = current().expect("still");
        assert!(same_boot(&first, &second), "{first} and {second} are one boot");
    }

    #[test]
    fn boot_times_a_clock_correction_apart_are_one_boot_and_a_restart_apart_are_two() {
        assert!(same_boot("t:1000", "t:1000"));
        assert!(same_boot("t:1000", "t:1010"), "a clock correction moves the boot time");
        assert!(same_boot("t:1010", "t:1000"));
        assert!(!same_boot("t:1000", "t:1011"), "a restart takes longer than the slack");
    }

    #[test]
    fn opaque_ids_are_one_boot_only_when_equal() {
        assert!(same_boot("id:aa", "id:aa"));
        assert!(!same_boot("id:aa", "id:ab"));
        assert!(!same_boot("id:1000", "t:1000"), "an id is never compared as a time");
    }
}
