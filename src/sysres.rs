//! System-resource sampling for the *reporting* process — current and peak resident set size
//! (RSS), in bytes.
//!
//! basemind historically reported on-disk footprint only; the `cache_stats` surface now also
//! answers "how much RAM does it consume". The number is the RSS of whatever process calls
//! [`sample`]: inside `basemind serve` that is the long-lived MCP server (the value the user
//! cares about); from the one-shot `basemind cache stats` CLI it is that short-lived process.
//! Both are honest self-measurements, so the field is labelled "this process".
//!
//! Platform sources:
//! - **macOS**: mach `task_info(MACH_TASK_BASIC_INFO)` for current RSS; `getrusage` `ru_maxrss`
//!   (bytes on Darwin) for peak.
//! - **Linux**: `/proc/self/statm` (resident pages × page size) for current; `getrusage`
//!   `ru_maxrss` (kilobytes) for peak.
//! - **Other platforms**: `None` (best-effort; callers render "unavailable").

/// Resident-set-size sample for the current process, in bytes. Each field is `None` when the
/// value cannot be read on this platform or the syscall failed — callers treat `None` as
/// "unavailable" rather than zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RssSample {
    /// Current resident set size (physical RAM backing the process), in bytes.
    pub current_bytes: Option<u64>,
    /// Peak resident set size observed over the process lifetime, in bytes.
    pub peak_bytes: Option<u64>,
}

/// Sample the current process's RSS. Cheap (one syscall per field); safe to call per request.
pub fn sample() -> RssSample {
    RssSample {
        current_bytes: current_rss(),
        peak_bytes: peak_rss(),
    }
}

#[cfg(target_os = "macos")]
fn current_rss() -> Option<u64> {
    use std::mem;

    use mach2::kern_return::KERN_SUCCESS;
    use mach2::message::mach_msg_type_number_t;
    use mach2::task::task_info;
    use mach2::task_info::{MACH_TASK_BASIC_INFO, mach_task_basic_info, task_info_t};
    use mach2::traps::mach_task_self;

    // SAFETY: `task_info` with `MACH_TASK_BASIC_INFO` fills a `mach_task_basic_info` struct. We
    // pass a zeroed buffer of exactly that type and its element count in 32-bit words, and read
    // `resident_size` only when the kernel returns `KERN_SUCCESS`.
    unsafe {
        let mut info = mem::zeroed::<mach_task_basic_info>();
        let mut count = (mem::size_of::<mach_task_basic_info>() / mem::size_of::<u32>())
            as mach_msg_type_number_t;
        let kr = task_info(
            mach_task_self(),
            MACH_TASK_BASIC_INFO,
            (&mut info as *mut mach_task_basic_info).cast::<i32>() as task_info_t,
            &mut count,
        );
        if kr == KERN_SUCCESS {
            Some(info.resident_size)
        } else {
            None
        }
    }
}

#[cfg(target_os = "linux")]
fn current_rss() -> Option<u64> {
    // `/proc/self/statm` field 2 (0-indexed 1) is the resident page count.
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let resident_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    // SAFETY: `sysconf` is a pure query with no memory effects; a non-positive result means the
    // limit is indeterminate, so we fall back to the near-universal 4 KiB page.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let page = if page > 0 { page as u64 } else { 4096 };
    Some(resident_pages.saturating_mul(page))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn current_rss() -> Option<u64> {
    None
}

#[cfg(unix)]
fn peak_rss() -> Option<u64> {
    use std::mem;
    // SAFETY: `getrusage` writes a full `rusage` into the zeroed buffer; we read `ru_maxrss` only
    // when it returns 0 (success).
    unsafe {
        let mut usage: libc::rusage = mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut usage) != 0 {
            return None;
        }
        let maxrss = usage.ru_maxrss.max(0) as u64;
        // `ru_maxrss` is bytes on Darwin, kilobytes on Linux/BSD.
        #[cfg(target_os = "macos")]
        let bytes = maxrss;
        #[cfg(not(target_os = "macos"))]
        let bytes = maxrss.saturating_mul(1024);
        Some(bytes)
    }
}

#[cfg(not(unix))]
fn peak_rss() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn sample_reports_nonzero_rss_on_unix() {
        let s = sample();
        // A live process always has some resident memory; both readers should succeed on the
        // Unix CI platforms (macOS + Linux).
        let current = s
            .current_bytes
            .expect("current RSS should be readable on unix");
        assert!(current > 0, "current RSS must be positive, got {current}");
        let peak = s.peak_bytes.expect("peak RSS should be readable on unix");
        assert!(
            peak >= current,
            "peak RSS ({peak}) should be >= current RSS ({current})"
        );
    }
}
