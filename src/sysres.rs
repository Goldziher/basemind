//! System-resource sampling for the *reporting* process: how much memory it is using, and —
//! just as importantly — how much it is allowed to use.
//!
//! Two audiences share this module.
//!
//! **Reporting.** [`sample`] answers "how much RAM does basemind consume" for the
//! `cache_stats` surface. The number is the RSS of whatever process calls it: inside
//! `basemind serve` that is the long-lived MCP server (the value the user cares about); from
//! the one-shot `basemind cache stats` CLI it is that short-lived process. Both are honest
//! self-measurements, so the field is labelled "this process".
//!
//! **Governance.** [`memory_reading`] backs the memory ceiling
//! ([`crate::backpressure::FootprintGate`]). It returns a [`MemoryReading`] — usage *and* the
//! ceiling the platform imposes — because the ceiling is the half that was missing when
//! issue #62 was filed: the process was running under a cgroup `MemoryMax`, was killed for
//! exceeding it, and had no way to know the limit existed. `limit_bytes` is what lets the
//! default footprint budget be derived from the environment instead of guessed.
//!
//! Platform sources, in the order they are consulted:
//!
//! | Platform | Usage | Limit |
//! |---|---|---|
//! | Linux, cgroup v2 | `memory.current` − `inactive_file` | `memory.max`, min over the hierarchy |
//! | Linux, cgroup v1 | `memory.usage_in_bytes` − `total_inactive_file` | `memory.limit_in_bytes` |
//! | Linux, neither | `/proc/self/statm` resident pages | `MemTotal` from `/proc/meminfo` |
//! | macOS | mach `TASK_VM_INFO` `phys_footprint` | `hw.memsize` |
//! | Windows | `K32GetProcessMemoryInfo` working set | `GlobalMemoryStatusEx` total physical |
//! | anything else | unavailable | unavailable |
//!
//! Callers treat "unavailable" as *skip the ceiling*, never as zero: a gate that cannot
//! measure must not block progress.

#[cfg(any(target_os = "linux", test))]
#[path = "sysres_cgroup.rs"]
mod cgroup;

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::Instant;

/// How long a [`memory_reading`] result is reused before the platform is asked again.
///
/// Every admit point in the scanner calls this, several times per file, from every rayon
/// worker at once. Uncached, the Linux path is three `open`/`read`/`close` triples per call
/// and the ceiling meant to fix a memory problem becomes a syscall storm instead. 50 ms is
/// short relative to how fast a scan's footprint can actually move (memtable flushes and ONNX
/// batches are tens of milliseconds at the very least) and long enough that the sampling cost
/// disappears: at most 20 reads per second no matter how many workers ask.
const SAMPLE_CACHE_INTERVAL_MICROS: u64 = 50_000;

/// Which mechanism produced a [`MemoryReading`]'s usage figure. For logs and diagnostics;
/// what the *limit* means is [`MemoryReading::limit_is_enforced`], which is a separate
/// question — a process can read its usage from a cgroup and still be bounded only by the
/// size of the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemorySource {
    /// Linux unified hierarchy (`/sys/fs/cgroup/…/memory.current`).
    CgroupV2,
    /// Linux legacy hierarchy (`/sys/fs/cgroup/memory/…/memory.usage_in_bytes`).
    CgroupV1,
    /// Linux with no memory cgroup: `/proc/self/statm` plus `/proc/meminfo`.
    ProcSelf,
    /// macOS mach `TASK_VM_INFO` plus `hw.memsize`.
    MachTaskInfo,
    /// Windows process working set plus total physical memory.
    WindowsProcess,
}

impl MemorySource {
    /// Stable identifier for logs and diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            MemorySource::CgroupV2 => "cgroup_v2",
            MemorySource::CgroupV1 => "cgroup_v1",
            MemorySource::ProcSelf => "proc_self",
            MemorySource::MachTaskInfo => "mach_task_info",
            MemorySource::WindowsProcess => "windows_process",
        }
    }

    /// Encode for the atomic cache. `0` is reserved for "no reading".
    fn to_code(self) -> u8 {
        match self {
            MemorySource::CgroupV2 => 1,
            MemorySource::CgroupV1 => 2,
            MemorySource::ProcSelf => 3,
            MemorySource::MachTaskInfo => 4,
            MemorySource::WindowsProcess => 5,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(MemorySource::CgroupV2),
            2 => Some(MemorySource::CgroupV1),
            3 => Some(MemorySource::ProcSelf),
            4 => Some(MemorySource::MachTaskInfo),
            5 => Some(MemorySource::WindowsProcess),
            _ => None,
        }
    }
}

/// A memory reading for the current process: what it is using, and the ceiling it is using it
/// against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryReading {
    /// Memory attributable to this process (or its container) right now, in bytes. On Linux
    /// this is a *working set* — reclaimable page cache has already been subtracted, so
    /// merely having read a large repository does not inflate it.
    pub used_bytes: u64,
    /// The ceiling `used_bytes` is measured against, in bytes, or `None` when nothing on this
    /// platform imposes or reports one.
    pub limit_bytes: Option<u64>,
    /// Whether `limit_bytes` is a per-process or per-container ceiling the kernel actually
    /// enforces (a cgroup limit — crossing it is an OOM kill) rather than the size of the
    /// machine (shared with everything else the user is running, and merely impolite to
    /// approach). The two deserve very different budgets, which is why this is a field and
    /// not a property of [`MemoryReading::source`]: a container with a cgroup but no
    /// `memory.max` reads its usage from cgroup v2 and is nevertheless bounded only by RAM.
    pub limit_is_enforced: bool,
    /// Which mechanism produced `used_bytes`.
    pub source: MemorySource,
}

impl MemoryReading {
    /// Fraction of the limit currently in use, or `None` without a limit. Saturates at 1.0
    /// only in the sense that it can exceed it — a process over a soft limit reports > 1.0.
    pub fn utilisation(&self) -> Option<f64> {
        let limit = self.limit_bytes.filter(|limit| *limit > 0)?;
        Some(self.used_bytes as f64 / limit as f64)
    }
}

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

// ---------------------------------------------------------------------------------------
// Rate-limited reading
// ---------------------------------------------------------------------------------------

/// Cached fields. Split across four atomics rather than guarded by a lock because the admit
/// points call this from every rayon worker and an uncontended-in-theory mutex on a hot path
/// is still a shared cache line. The fields can therefore tear relative to each other — a
/// `used_bytes` from one sample paired with a `limit_bytes` from the next. That is benign
/// here: both are advisory inputs to a best-effort ceiling, consecutive samples are at most
/// [`SAMPLE_CACHE_INTERVAL_MICROS`] apart, and the limit in particular is effectively
/// constant for the life of the process.
static CACHE_USED: AtomicU64 = AtomicU64::new(0);
/// Limit in bytes, with `u64::MAX` standing in for `None` (no real limit can reach it).
static CACHE_LIMIT: AtomicU64 = AtomicU64::new(u64::MAX);
/// [`MemorySource::to_code`] with [`ENFORCED_LIMIT_BIT`] set when the limit is enforced, or
/// `0` for "no reading available".
static CACHE_SOURCE: AtomicU8 = AtomicU8::new(0);
/// Microseconds since [`process_start`] at which the cache was last written; `0` = never.
static CACHE_AT_MICROS: AtomicU64 = AtomicU64::new(0);

/// High bit of the cached source byte, carrying [`MemoryReading::limit_is_enforced`]. Packed
/// into the same atomic so the flag can never be read out of step with the source it belongs to.
const ENFORCED_LIMIT_BIT: u8 = 0b1000_0000;

/// Monotonic origin for the cache timestamp. `Instant` cannot be stored in an atomic, so
/// elapsed microseconds from a fixed origin are stored instead.
fn process_start() -> Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    *START.get_or_init(Instant::now)
}

/// A memory reading for the current process, reused for up to
/// [`SAMPLE_CACHE_INTERVAL_MICROS`]. This is the call the admission gates make.
pub fn memory_reading() -> Option<MemoryReading> {
    // Never zero, so `0` stays available as the "never sampled" marker.
    let now = process_start().elapsed().as_micros().max(1) as u64;
    let last = CACHE_AT_MICROS.load(Ordering::Relaxed);
    if last != 0 && now.saturating_sub(last) < SAMPLE_CACHE_INTERVAL_MICROS {
        return load_cached();
    }
    let fresh = memory_reading_uncached();
    store_cached(fresh, now);
    fresh
}

/// Read the platform directly, bypassing the cache. For diagnostics and tests; the hot path
/// wants [`memory_reading`].
pub fn memory_reading_uncached() -> Option<MemoryReading> {
    read_memory()
}

/// The ceiling this process is running under, in bytes, and whether the kernel enforces it.
/// `None` when the platform imposes or reports none.
pub fn detected_memory_limit() -> Option<(u64, bool)> {
    let reading = memory_reading()?;
    Some((reading.limit_bytes?, reading.limit_is_enforced))
}

/// Flatten a reading into the three cache words. Pure, so the encoding — in particular the
/// `u64::MAX`-means-`None` limit and the reserved `0` source code — is unit-testable without
/// disturbing the process-wide cache that the admit points read.
fn encode_reading(reading: Option<MemoryReading>) -> (u64, u64, u8) {
    match reading {
        Some(reading) => {
            let enforced = if reading.limit_is_enforced {
                ENFORCED_LIMIT_BIT
            } else {
                0
            };
            (
                reading.used_bytes,
                reading.limit_bytes.unwrap_or(u64::MAX),
                reading.source.to_code() | enforced,
            )
        }
        None => (0, u64::MAX, 0),
    }
}

/// Inverse of [`encode_reading`].
fn decode_reading(used: u64, limit: u64, source: u8) -> Option<MemoryReading> {
    Some(MemoryReading {
        used_bytes: used,
        limit_bytes: (limit != u64::MAX).then_some(limit),
        limit_is_enforced: source & ENFORCED_LIMIT_BIT != 0,
        source: MemorySource::from_code(source & !ENFORCED_LIMIT_BIT)?,
    })
}

fn load_cached() -> Option<MemoryReading> {
    decode_reading(
        CACHE_USED.load(Ordering::Relaxed),
        CACHE_LIMIT.load(Ordering::Relaxed),
        CACHE_SOURCE.load(Ordering::Relaxed),
    )
}

fn store_cached(reading: Option<MemoryReading>, at_micros: u64) {
    let (used, limit, source) = encode_reading(reading);
    CACHE_USED.store(used, Ordering::Relaxed);
    CACHE_LIMIT.store(limit, Ordering::Relaxed);
    CACHE_SOURCE.store(source, Ordering::Relaxed);
    CACHE_AT_MICROS.store(at_micros, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------------------
// Platform readers
// ---------------------------------------------------------------------------------------

/// cgroup v2 wins over v1 because a hybrid host mounts both but charges the process to
/// whichever hierarchy actually holds it, and v2 is the one systemd uses for `MemoryMax` on
/// every current distribution. Only when neither controller answers does the reading fall
/// back to whole-process RSS against whole-machine RAM, which reports the truth about the
/// machine but says nothing about a container's ceiling.
#[cfg(target_os = "linux")]
fn read_memory() -> Option<MemoryReading> {
    // `or_else`, not a list: v1's files must not be read when v2 already answered.
    let charged = cgroup::read_v2()
        .map(|reading| (reading, MemorySource::CgroupV2))
        .or_else(|| cgroup::read_v1().map(|reading| (reading, MemorySource::CgroupV1)));
    if let Some((reading, source)) = charged {
        let (limit_bytes, limit_is_enforced) = effective_limit(reading.limit_bytes, cgroup::read_mem_total());
        return Some(MemoryReading {
            used_bytes: reading.used_bytes,
            limit_bytes,
            limit_is_enforced,
            source,
        });
    }
    Some(MemoryReading {
        used_bytes: cgroup::read_statm_rss()?,
        limit_bytes: cgroup::read_mem_total(),
        limit_is_enforced: false,
        source: MemorySource::ProcSelf,
    })
}

/// Choose between a cgroup limit and the machine's RAM.
///
/// A cgroup that sets no `memory.max` — the default for a plain `docker run`, and for every
/// process on a desktop outside a resource-controlled unit — must still end up with *some*
/// ceiling, or the whole point of an auto budget is lost on the most common container
/// topology. A cgroup limit above total RAM (`MemoryMax=100G` on a 16 GiB host) is likewise
/// not the binding constraint and must not be treated as enforced, because the enforced
/// branch budgets a much larger fraction.
#[cfg(any(target_os = "linux", test))]
fn effective_limit(cgroup_limit: Option<u64>, mem_total: Option<u64>) -> (Option<u64>, bool) {
    match (cgroup_limit, mem_total) {
        (Some(limit), Some(total)) if limit < total => (Some(limit), true),
        (Some(limit), None) => (Some(limit), true),
        (_, total) => (total, false),
    }
}

#[cfg(target_os = "macos")]
fn read_memory() -> Option<MemoryReading> {
    Some(MemoryReading {
        used_bytes: mac_phys_footprint()?,
        limit_bytes: mac_hw_memsize(),
        limit_is_enforced: false,
        source: MemorySource::MachTaskInfo,
    })
}

#[cfg(windows)]
fn read_memory() -> Option<MemoryReading> {
    Some(MemoryReading {
        used_bytes: windows_working_set()?,
        limit_bytes: windows_total_physical(),
        limit_is_enforced: false,
        source: MemorySource::WindowsProcess,
    })
}

/// Every other target (BSD, illumos, wasm, …). Deliberately unavailable rather than
/// approximated: a wrong ceiling throttles a healthy scan or fails to throttle a sick one,
/// and both are worse than admitting we cannot see.
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn read_memory() -> Option<MemoryReading> {
    None
}

/// Total physical RAM from the `hw.memsize` sysctl. macOS has no per-process memory limit to
/// discover — jetsam applies to app processes, not command-line tools — so the machine size
/// is the only ceiling there is.
#[cfg(target_os = "macos")]
fn mac_hw_memsize() -> Option<u64> {
    let mut value: u64 = 0;
    let mut len = std::mem::size_of::<u64>();
    let name = c"hw.memsize";
    // SAFETY: `sysctlbyname` writes at most `len` bytes into `value`, which is a `u64` and
    // exactly `len` bytes; `name` is a NUL-terminated literal; the two null pointers are the
    // documented "no new value" arguments.
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&raw mut value).cast::<libc::c_void>(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0 && value > 0).then_some(value)
}

#[cfg(target_os = "macos")]
fn mac_phys_footprint() -> Option<u64> {
    use std::mem;

    use mach2::kern_return::KERN_SUCCESS;
    use mach2::message::mach_msg_type_number_t;
    use mach2::task::task_info;
    use mach2::task_info::{TASK_VM_INFO, task_info_t, task_vm_info};
    use mach2::traps::mach_task_self;

    // SAFETY: `task_info` with `TASK_VM_INFO` fills a `task_vm_info` struct with up to `count`
    // 32-bit words, and `count` is derived from the struct's own size. `task_vm_info` is
    // `#[repr(C, packed(4))]`, so the field is read by value and never by reference.
    unsafe {
        let mut info = task_vm_info::default();
        let mut count = (mem::size_of::<task_vm_info>() / mem::size_of::<u32>()) as mach_msg_type_number_t;
        let kr = task_info(
            mach_task_self(),
            TASK_VM_INFO,
            (&mut info as *mut task_vm_info).cast::<i32>() as task_info_t,
            &mut count,
        );
        if kr == KERN_SUCCESS {
            let footprint = info.phys_footprint;
            Some(footprint)
        } else {
            None
        }
    }
}

/// Physical memory footprint of the current process, in bytes — the usage half of
/// [`memory_reading`], kept as a standalone entry point because
/// [`crate::backpressure::FootprintGate`] samples only the usage.
///
/// The name is Darwin's: on macOS this is `phys_footprint`, which counts compressed and
/// swapped-out anonymous pages and so does *not* shrink when the OS compresses idle memory
/// under pressure. That property is why it is the right signal for a ceiling — plain RSS
/// reads artificially low exactly when the process is heaviest — and the Linux and Windows
/// readings are chosen to match it: a cgroup working set likewise counts swapped-out anonymous
/// memory and excludes reclaimable cache. Returns `None` where nothing can be read, which
/// callers treat as "skip throttling" rather than as zero.
pub fn phys_footprint() -> Option<u64> {
    memory_reading().map(|reading| reading.used_bytes)
}

// ---------------------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------------------

/// `MEMORYSTATUSEX`. Laid out by hand rather than pulled from a crate: this is the only
/// Windows FFI in the tree and a `windows-sys` dependency for two calls is not worth it.
#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
struct MemoryStatusEx {
    length: u32,
    memory_load: u32,
    total_phys: u64,
    avail_phys: u64,
    total_page_file: u64,
    avail_page_file: u64,
    total_virtual: u64,
    avail_virtual: u64,
    avail_extended_virtual: u64,
}

/// `PROCESS_MEMORY_COUNTERS`. `SIZE_T` is pointer-sized, hence `usize`.
#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
struct ProcessMemoryCounters {
    cb: u32,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
    quota_peak_paged_pool_usage: usize,
    quota_paged_pool_usage: usize,
    quota_peak_non_paged_pool_usage: usize,
    quota_non_paged_pool_usage: usize,
    pagefile_usage: usize,
    peak_pagefile_usage: usize,
}

// `K32GetProcessMemoryInfo` rather than psapi's `GetProcessMemoryInfo`: it is exported from
// kernel32 on every supported Windows, so no extra import library is needed.
#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GlobalMemoryStatusEx(buffer: *mut MemoryStatusEx) -> i32;
    fn GetCurrentProcess() -> *mut core::ffi::c_void;
    fn K32GetProcessMemoryInfo(process: *mut core::ffi::c_void, counters: *mut ProcessMemoryCounters, cb: u32) -> i32;
}

#[cfg(windows)]
fn windows_working_set() -> Option<u64> {
    let mut counters = ProcessMemoryCounters {
        cb: std::mem::size_of::<ProcessMemoryCounters>() as u32,
        ..ProcessMemoryCounters::default()
    };
    // SAFETY: `counters` is a correctly sized `PROCESS_MEMORY_COUNTERS` whose `cb` declares
    // its own size, as the API requires; `GetCurrentProcess` returns a pseudo-handle that
    // needs no closing.
    let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    (ok != 0).then_some(counters.working_set_size as u64)
}

#[cfg(windows)]
fn windows_total_physical() -> Option<u64> {
    let mut status = MemoryStatusEx {
        length: std::mem::size_of::<MemoryStatusEx>() as u32,
        ..MemoryStatusEx::default()
    };
    // SAFETY: `status` is a correctly sized `MEMORYSTATUSEX` whose `length` declares its own
    // size, which is the API's sole precondition.
    let ok = unsafe { GlobalMemoryStatusEx(&mut status) };
    (ok != 0 && status.total_phys > 0).then_some(status.total_phys)
}

// ---------------------------------------------------------------------------------------
// RSS (reporting)
// ---------------------------------------------------------------------------------------

#[cfg(target_os = "macos")]
fn current_rss() -> Option<u64> {
    use std::mem;

    use mach2::kern_return::KERN_SUCCESS;
    use mach2::message::mach_msg_type_number_t;
    use mach2::task::task_info;
    use mach2::task_info::{MACH_TASK_BASIC_INFO, mach_task_basic_info, task_info_t};
    use mach2::traps::mach_task_self;

    // SAFETY: `task_info` with `MACH_TASK_BASIC_INFO` fills a `mach_task_basic_info` struct.
    unsafe {
        let mut info = mem::zeroed::<mach_task_basic_info>();
        let mut count = (mem::size_of::<mach_task_basic_info>() / mem::size_of::<u32>()) as mach_msg_type_number_t;
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
    cgroup::read_statm_rss()
}

#[cfg(windows)]
fn current_rss() -> Option<u64> {
    windows_working_set()
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn current_rss() -> Option<u64> {
    None
}

/// Peak RSS over the process lifetime. Monotonic by construction, which is what makes it the
/// right instrument for a memory-budget regression test: it cannot be missed by sampling at
/// the wrong moment the way a current-usage reading can.
#[cfg(unix)]
fn peak_rss() -> Option<u64> {
    use std::mem;
    // SAFETY: `getrusage` writes a full `rusage` into the zeroed buffer; we read `ru_maxrss` only.
    unsafe {
        let mut usage: libc::rusage = mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut usage) != 0 {
            return None;
        }
        let maxrss = usage.ru_maxrss.max(0) as u64;
        // Darwin reports bytes; every other Unix reports kilobytes.
        #[cfg(target_os = "macos")]
        let bytes = maxrss;
        #[cfg(not(target_os = "macos"))]
        let bytes = maxrss.saturating_mul(1024);
        Some(bytes)
    }
}

#[cfg(windows)]
fn peak_rss() -> Option<u64> {
    let mut counters = ProcessMemoryCounters {
        cb: std::mem::size_of::<ProcessMemoryCounters>() as u32,
        ..ProcessMemoryCounters::default()
    };
    // SAFETY: see `windows_working_set`.
    let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    (ok != 0).then_some(counters.peak_working_set_size as u64)
}

#[cfg(not(any(unix, windows)))]
fn peak_rss() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_codes_round_trip_and_reserve_zero() {
        for source in [
            MemorySource::CgroupV2,
            MemorySource::CgroupV1,
            MemorySource::ProcSelf,
            MemorySource::MachTaskInfo,
            MemorySource::WindowsProcess,
        ] {
            assert_ne!(source.to_code(), 0, "0 is the no-reading marker");
            assert_eq!(MemorySource::from_code(source.to_code()), Some(source));
        }
        assert_eq!(MemorySource::from_code(0), None);
        assert_eq!(MemorySource::from_code(200), None);
    }

    #[test]
    fn a_cgroup_limit_below_total_ram_is_the_enforced_one() {
        assert_eq!(effective_limit(Some(2 << 30), Some(64 << 30)), (Some(2 << 30), true));
    }

    #[test]
    fn a_cgroup_with_no_limit_still_gets_total_ram_as_an_advisory_ceiling() {
        // A plain `docker run` — cgroup v2 present, `memory.max` = `max`. Without this the
        // most common container topology would end up with no ceiling at all.
        assert_eq!(effective_limit(None, Some(64 << 30)), (Some(64 << 30), false));
    }

    #[test]
    fn a_cgroup_limit_above_total_ram_is_not_the_binding_constraint() {
        // `MemoryMax=100G` on a 16 GiB host: real, but not what the process will die against.
        assert_eq!(
            effective_limit(Some(100 << 30), Some(16 << 30)),
            (Some(16 << 30), false)
        );
        // Equal is the same case — nothing is gained by budgeting against the larger fraction.
        assert_eq!(effective_limit(Some(16 << 30), Some(16 << 30)), (Some(16 << 30), false));
    }

    #[test]
    fn a_cgroup_limit_stands_alone_when_meminfo_is_unreadable() {
        assert_eq!(effective_limit(Some(2 << 30), None), (Some(2 << 30), true));
        assert_eq!(effective_limit(None, None), (None, false));
    }

    #[test]
    fn utilisation_is_none_without_a_usable_limit() {
        let mut reading = MemoryReading {
            used_bytes: 512,
            limit_bytes: Some(1024),
            limit_is_enforced: true,
            source: MemorySource::CgroupV2,
        };
        assert_eq!(reading.utilisation(), Some(0.5));
        reading.limit_bytes = None;
        assert_eq!(reading.utilisation(), None);
        // A zero limit would otherwise divide by zero and report infinity.
        reading.limit_bytes = Some(0);
        assert_eq!(reading.utilisation(), None);
    }

    /// Exercises the cache encoding through the pure pair rather than the statics, so this
    /// test cannot race the other tests in this module against the process-wide cache.
    #[test]
    fn cache_encoding_round_trips_including_the_absent_limit() {
        for reading in [
            Some(MemoryReading {
                used_bytes: 4096,
                limit_bytes: None,
                limit_is_enforced: false,
                source: MemorySource::ProcSelf,
            }),
            Some(MemoryReading {
                used_bytes: 8192,
                limit_bytes: Some(1 << 30),
                limit_is_enforced: true,
                source: MemorySource::CgroupV2,
            }),
            Some(MemoryReading {
                used_bytes: 8192,
                limit_bytes: Some(1 << 30),
                limit_is_enforced: false,
                source: MemorySource::CgroupV2,
            }),
            None,
        ] {
            let (used, limit, source) = encode_reading(reading);
            assert_eq!(decode_reading(used, limit, source), reading);
        }
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    fn memory_reading_answers_on_a_supported_platform() {
        let reading = memory_reading_uncached().expect("a supported platform must report memory");
        assert!(
            reading.used_bytes > 0,
            "usage must be positive, got {}",
            reading.used_bytes
        );
        // Every supported platform reports at least the machine's RAM as a limit.
        let limit = reading.limit_bytes.expect("a supported platform must report a limit");
        assert!(limit > 0, "limit must be positive, got {limit}");
    }

    #[test]
    #[cfg(unix)]
    fn sample_reports_nonzero_rss_on_unix() {
        let s = sample();
        let current = s.current_bytes.expect("current RSS should be readable on unix");
        assert!(current > 0, "current RSS must be positive, got {current}");
        let peak = s.peak_bytes.expect("peak RSS should be readable on unix");
        assert!(peak > 0, "peak RSS must be positive, got {peak}");
        #[cfg(target_os = "macos")]
        assert!(
            peak >= current,
            "peak RSS ({peak}) should be >= current RSS ({current})"
        );
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    fn phys_footprint_reports_positive_bytes() {
        let footprint = phys_footprint().expect("phys_footprint should be readable here");
        assert!(footprint > 0, "phys_footprint must be positive, got {footprint}");
    }
}
