//! Linux cgroup and procfs memory readers backing [`crate::sysres`].
//!
//! Carved out of `sysres.rs` so neither module approaches the 1000-line cap: the platform
//! trivia here (three hierarchies, a sentinel that means "no limit", a page-cache term that
//! has to be subtracted before the number means anything) needs more prose than code.
//!
//! The split into two layers is deliberate and is what makes this testable at all. **No unit
//! test can create a cgroup**, and CI does not run under a `MemoryMax`, so:
//!
//! - every format is decoded by a **pure function over `&str`**, exercised against real
//!   captured file contents including the malformed and sentinel cases;
//! - the **directory-shaped** logic — which cgroup directory answers, how far up the
//!   hierarchy the effective limit is searched, what happens when the path in
//!   `/proc/self/cgroup` does not exist inside a container's mount namespace — lives in
//!   `*_at` functions parameterised on the mount point, so it can be driven against a
//!   fixture tree in a tempdir on any OS, including a macOS dev machine where none of this
//!   runs for real.
//!
//! Only the zero-argument wrappers at the bottom touch the real `/proc` and `/sys`, and they
//! are the only `#[cfg(target_os = "linux")]` items in the file.

use std::path::{Path, PathBuf};

/// Mount point of the unified (v2) hierarchy, and the parent of the v1 controller
/// directories, on every mainstream Linux distribution.
#[cfg(target_os = "linux")]
pub(crate) const CGROUP_MOUNT: &str = "/sys/fs/cgroup";

/// Any v1 limit at or above this is "no limit".
///
/// cgroup v1 has no `max` keyword: an unlimited `memory.limit_in_bytes` is written as
/// `PAGE_COUNTER_MAX * PAGE_SIZE`, which materialises as `9223372036854775807` (`LONG_MAX`),
/// `9223372036854771712` (`LONG_MAX` truncated to a 4 KiB page) or `18446744073709551615`
/// depending on kernel and page size. Rather than enumerate the sentinels, treat anything
/// above 4 EiB as absent — no machine or container has that much RAM, so a real limit can
/// never be mistaken for the sentinel. Reporting the sentinel as a ceiling would be the worst
/// possible failure: an "auto" footprint budget derived from `LONG_MAX` is no budget at all.
pub(crate) const V1_NO_LIMIT_FLOOR: u64 = 1 << 62;

/// A memory reading taken from one cgroup, already reduced to a working set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CgroupReading {
    /// Charged memory minus reclaimable page cache — see [`working_set`].
    pub used_bytes: u64,
    /// The effective ceiling, or `None` when nothing in the hierarchy imposes one.
    pub limit_bytes: Option<u64>,
}

// ---------------------------------------------------------------------------------------
// Pure parsers
// ---------------------------------------------------------------------------------------

/// Decode a single-value cgroup file (`memory.current`, `memory.usage_in_bytes`, …).
pub(crate) fn parse_scalar(raw: &str) -> Option<u64> {
    raw.trim().parse::<u64>().ok()
}

/// Decode `memory.max` / `memory.high`. cgroup v2 spells "no limit" as the literal `max`,
/// which is not a number and must not be treated as a parse failure — it is a successful read
/// of "unlimited". `Some(None)` is that case; `None` means the file was unreadable garbage.
pub(crate) fn parse_memory_max(raw: &str) -> Option<Option<u64>> {
    let trimmed = raw.trim();
    if trimmed == "max" {
        return Some(None);
    }
    trimmed.parse::<u64>().ok().map(Some)
}

/// Decode `memory.limit_in_bytes` (cgroup v1), mapping the no-limit sentinel to `None`.
/// Same `Some(None)` convention as [`parse_memory_max`].
pub(crate) fn parse_v1_limit(raw: &str) -> Option<Option<u64>> {
    let value = parse_scalar(raw)?;
    if value >= V1_NO_LIMIT_FLOOR {
        Some(None)
    } else {
        Some(Some(value))
    }
}

/// Look up one `key value` line in a `memory.stat` body.
pub(crate) fn parse_memory_stat_field(raw: &str, key: &str) -> Option<u64> {
    raw.lines().find_map(|line| {
        let mut parts = line.split_ascii_whitespace();
        (parts.next()? == key).then(|| parts.next()?.parse::<u64>().ok())?
    })
}

/// Reduce a cgroup's charged bytes to a working set by discarding reclaimable page cache.
///
/// `memory.current` (and v1's `memory.usage_in_bytes`) includes the file cache the kernel
/// will drop for free under pressure. A process that has merely *read* a large repository
/// therefore reads as near its limit while owning almost none of it, which would make an
/// auto-derived footprint ceiling fire constantly on a scan that is behaving perfectly.
/// kubelet defines container working set the same way — charged minus `inactive_file` — and
/// this number is what an OOM decision actually turns on, so it is the one to gate against.
pub(crate) fn working_set(charged: u64, inactive_file: Option<u64>) -> u64 {
    charged.saturating_sub(inactive_file.unwrap_or(0))
}

/// Extract the unified-hierarchy path from `/proc/self/cgroup`.
///
/// The v2 entry is the line with controller-set `0` and an empty controller name — `0::/path`
/// — and is present even on a hybrid host that also lists numbered v1 controllers. The path is
/// relative to the v2 mount despite its leading slash.
pub(crate) fn parse_v2_rel_path(proc_self_cgroup: &str) -> Option<&str> {
    proc_self_cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(str::trim_end)
}

/// Extract the path of the v1 `memory` controller from `/proc/self/cgroup`.
///
/// v1 lines are `hierarchy-id:controller[,controller…]:path`, and one hierarchy can carry
/// several co-mounted controllers (`4:cpu,memory:/foo`), so the middle field is matched
/// per-comma rather than compared whole.
pub(crate) fn parse_v1_memory_rel_path(proc_self_cgroup: &str) -> Option<&str> {
    proc_self_cgroup.lines().find_map(|line| {
        let mut parts = line.splitn(3, ':');
        let _id = parts.next()?;
        let controllers = parts.next()?;
        let path = parts.next()?;
        controllers
            .split(',')
            .any(|controller| controller == "memory")
            .then_some(path.trim_end())
    })
}

/// Decode one `Key:  value kB` line of `/proc/meminfo` into bytes. Every size in that file is
/// in kibibytes regardless of the unit suffix's absence on a few exotic fields.
pub(crate) fn parse_meminfo_field(raw: &str, key: &str) -> Option<u64> {
    raw.lines()
        .find_map(|line| {
            let (name, rest) = line.split_once(':')?;
            (name == key).then(|| rest.split_ascii_whitespace().next()?.parse::<u64>().ok())?
        })
        .map(|kib| kib.saturating_mul(1024))
}

/// Decode the resident-pages field of `/proc/self/statm` (the second of seven).
pub(crate) fn parse_statm_resident_pages(raw: &str) -> Option<u64> {
    raw.split_ascii_whitespace().nth(1)?.parse::<u64>().ok()
}

/// Build the directory chain for a cgroup path, **leaf first**, ending at the mount point.
///
/// The chain exists because the effective limit is not necessarily the leaf's. systemd puts a
/// unit's `MemoryMax` on the slice while the process lives in a nested scope below it, so a
/// leaf-only read reports `max` for a process that is in fact capped — exactly the blindness
/// that let #62 be OOM-killed without ever knowing its own ceiling.
///
/// `..` segments are dropped rather than resolved so a hostile or malformed `/proc/self/cgroup`
/// can never walk the reader outside the mount.
pub(crate) fn cgroup_dir_chain(mount: &Path, rel: &str) -> Vec<PathBuf> {
    let mut chain = vec![mount.to_path_buf()];
    let mut current = mount.to_path_buf();
    for segment in rel.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            continue;
        }
        current = current.join(segment);
        chain.push(current.clone());
    }
    chain.reverse();
    chain
}

// ---------------------------------------------------------------------------------------
// Mount-parameterised I/O
// ---------------------------------------------------------------------------------------

/// Read a file and decode it, treating any I/O error as "this file does not answer".
fn read_and_parse<T>(dir: &Path, file: &str, decode: impl FnOnce(&str) -> Option<T>) -> Option<T> {
    let raw = std::fs::read_to_string(dir.join(file)).ok()?;
    decode(&raw)
}

/// Read the unified hierarchy rooted at `mount` for the cgroup named by `proc_self_cgroup`.
///
/// The usage comes from the innermost directory that actually exists: inside a container's
/// cgroup namespace `/proc/self/cgroup` frequently reports the *host* path (`0::/system.slice/
/// docker-….scope`) while the mount is the namespaced root, so a strict join finds nothing and
/// the mount itself is the correct answer. The limit is the minimum over that directory and
/// every ancestor up to the mount, which is what the kernel enforces.
pub(crate) fn read_v2_at(mount: &Path, proc_self_cgroup: &str) -> Option<CgroupReading> {
    let rel = parse_v2_rel_path(proc_self_cgroup)?;
    let chain = cgroup_dir_chain(mount, rel);
    let start = chain.iter().position(|dir| dir.join("memory.current").is_file())?;
    let usage_dir = &chain[start];

    let charged = read_and_parse(usage_dir, "memory.current", parse_scalar)?;
    let inactive_file = read_and_parse(usage_dir, "memory.stat", |raw| {
        parse_memory_stat_field(raw, "inactive_file")
    });

    let limit_bytes = chain[start..]
        .iter()
        .filter_map(|dir| read_and_parse(dir, "memory.max", parse_memory_max))
        .flatten()
        .min();

    Some(CgroupReading {
        used_bytes: working_set(charged, inactive_file),
        limit_bytes,
    })
}

/// Read the legacy hierarchy. The v1 `memory` controller is mounted one level below the
/// cgroup mount (`/sys/fs/cgroup/memory`), and v1 exposes the hierarchical roll-up directly as
/// `total_inactive_file`, so no ancestor walk is needed for the working set — but the limit
/// still needs one, for the same nested-unit reason as v2.
pub(crate) fn read_v1_at(mount: &Path, proc_self_cgroup: &str) -> Option<CgroupReading> {
    let controller_root = mount.join("memory");
    let rel = parse_v1_memory_rel_path(proc_self_cgroup).unwrap_or("/");
    let chain = cgroup_dir_chain(&controller_root, rel);
    let start = chain
        .iter()
        .position(|dir| dir.join("memory.usage_in_bytes").is_file())?;
    let usage_dir = &chain[start];

    let charged = read_and_parse(usage_dir, "memory.usage_in_bytes", parse_scalar)?;
    let inactive_file = read_and_parse(usage_dir, "memory.stat", |raw| {
        parse_memory_stat_field(raw, "total_inactive_file").or_else(|| parse_memory_stat_field(raw, "inactive_file"))
    });

    let limit_bytes = chain[start..]
        .iter()
        .filter_map(|dir| read_and_parse(dir, "memory.limit_in_bytes", parse_v1_limit))
        .flatten()
        .min();

    Some(CgroupReading {
        used_bytes: working_set(charged, inactive_file),
        limit_bytes,
    })
}

// ---------------------------------------------------------------------------------------
// Real-filesystem entry points
// ---------------------------------------------------------------------------------------

/// Bytes in one page, for turning `/proc/self/statm` page counts into bytes.
#[cfg(target_os = "linux")]
pub(crate) fn page_size() -> u64 {
    // SAFETY: `sysconf` is a pure query with no memory effects; a non-positive result means the
    // value is indeterminate on this system, for which 4 KiB is the only sane guess.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page > 0 { page as u64 } else { 4096 }
}

/// The process's own cgroup membership, or an empty string when procfs is unavailable.
#[cfg(target_os = "linux")]
fn proc_self_cgroup() -> String {
    std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default()
}

/// cgroup v2 reading for the running process.
#[cfg(target_os = "linux")]
pub(crate) fn read_v2() -> Option<CgroupReading> {
    read_v2_at(Path::new(CGROUP_MOUNT), &proc_self_cgroup())
}

/// cgroup v1 reading for the running process.
#[cfg(target_os = "linux")]
pub(crate) fn read_v1() -> Option<CgroupReading> {
    read_v1_at(Path::new(CGROUP_MOUNT), &proc_self_cgroup())
}

/// Resident set size of the running process from `/proc/self/statm`.
#[cfg(target_os = "linux")]
pub(crate) fn read_statm_rss() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    Some(parse_statm_resident_pages(&statm)?.saturating_mul(page_size()))
}

/// Total physical RAM from `/proc/meminfo`.
#[cfg(target_os = "linux")]
pub(crate) fn read_mem_total() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    parse_meminfo_field(&meminfo, "MemTotal")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim `/proc/self/cgroup` from a systemd user session on a v2-only host.
    const V2_ONLY: &str = "0::/user.slice/user-1000.slice/session-3.scope\n";

    /// Verbatim `/proc/self/cgroup` from a hybrid host: numbered v1 controllers plus the v2 line.
    const HYBRID: &str = "\
11:devices:/user.slice
4:cpu,cpuacct,memory:/user.slice/user-1000.slice
1:name=systemd:/user.slice/user-1000.slice/session-2.scope
0::/user.slice/user-1000.slice/session-2.scope
";

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::create_dir_all(dir).expect("create fixture dir");
        std::fs::write(dir.join(name), body).expect("write fixture file");
    }

    #[test]
    fn parses_the_v2_line_from_a_v2_only_host() {
        assert_eq!(
            parse_v2_rel_path(V2_ONLY),
            Some("/user.slice/user-1000.slice/session-3.scope")
        );
    }

    #[test]
    fn parses_the_v2_line_out_of_a_hybrid_hierarchy() {
        assert_eq!(
            parse_v2_rel_path(HYBRID),
            Some("/user.slice/user-1000.slice/session-2.scope")
        );
    }

    #[test]
    fn v2_root_cgroup_parses_as_a_bare_slash() {
        assert_eq!(parse_v2_rel_path("0::/\n"), Some("/"));
    }

    #[test]
    fn a_v1_only_proc_file_has_no_v2_line() {
        assert_eq!(parse_v2_rel_path("4:memory:/foo\n"), None);
        assert_eq!(parse_v2_rel_path(""), None);
        assert_eq!(parse_v2_rel_path("garbage"), None);
    }

    #[test]
    fn finds_the_memory_controller_among_co_mounted_controllers() {
        assert_eq!(parse_v1_memory_rel_path(HYBRID), Some("/user.slice/user-1000.slice"));
        assert_eq!(parse_v1_memory_rel_path("4:memory:/docker/abc\n"), Some("/docker/abc"));
        assert_eq!(parse_v1_memory_rel_path("4:cpu,cpuacct:/x\n"), None);
        assert_eq!(parse_v1_memory_rel_path(V2_ONLY), None);
    }

    #[test]
    fn memory_max_reads_the_max_keyword_as_no_limit() {
        assert_eq!(parse_memory_max("max\n"), Some(None));
        assert_eq!(parse_memory_max("2147483648\n"), Some(Some(2_147_483_648)));
        assert_eq!(parse_memory_max(""), None);
        assert_eq!(parse_memory_max("not-a-number\n"), None);
    }

    #[test]
    fn v1_sentinels_are_no_limit_not_a_ceiling() {
        // The three shapes seen in the wild: LONG_MAX, LONG_MAX truncated to a 4 KiB page, U64_MAX.
        assert_eq!(parse_v1_limit("9223372036854775807\n"), Some(None));
        assert_eq!(parse_v1_limit("9223372036854771712\n"), Some(None));
        assert_eq!(parse_v1_limit("18446744073709551615\n"), Some(None));
        assert_eq!(parse_v1_limit("2147483648\n"), Some(Some(2_147_483_648)));
        assert_eq!(parse_v1_limit("junk"), None);
    }

    #[test]
    fn memory_stat_lookup_ignores_prefix_collisions() {
        let stat = "anon 1000\ninactive_file 4096\ntotal_inactive_file 8192\nfile 99\n";
        assert_eq!(parse_memory_stat_field(stat, "inactive_file"), Some(4096));
        assert_eq!(parse_memory_stat_field(stat, "total_inactive_file"), Some(8192));
        assert_eq!(parse_memory_stat_field(stat, "missing"), None);
        assert_eq!(parse_memory_stat_field("inactive_file\n", "inactive_file"), None);
    }

    #[test]
    fn working_set_subtracts_reclaimable_cache_and_never_underflows() {
        assert_eq!(working_set(1000, Some(400)), 600);
        assert_eq!(working_set(1000, None), 1000);
        assert_eq!(working_set(100, Some(4000)), 0);
    }

    #[test]
    fn meminfo_reports_kibibyte_fields_in_bytes() {
        let meminfo = "MemTotal:       16316848 kB\nMemFree:         1000 kB\nSwapTotal: 0 kB\n";
        assert_eq!(parse_meminfo_field(meminfo, "MemTotal"), Some(16_316_848 * 1024));
        assert_eq!(parse_meminfo_field(meminfo, "SwapTotal"), Some(0));
        assert_eq!(parse_meminfo_field(meminfo, "Nope"), None);
        assert_eq!(parse_meminfo_field("MemTotal: kB\n", "MemTotal"), None);
    }

    #[test]
    fn statm_resident_is_the_second_field() {
        assert_eq!(parse_statm_resident_pages("101234 4567 890 12 0 3456 0\n"), Some(4567));
        assert_eq!(parse_statm_resident_pages("101234\n"), None);
        assert_eq!(parse_statm_resident_pages(""), None);
    }

    #[test]
    fn dir_chain_runs_leaf_first_up_to_the_mount() {
        let chain = cgroup_dir_chain(Path::new("/sys/fs/cgroup"), "/a/b");
        assert_eq!(
            chain,
            vec![
                PathBuf::from("/sys/fs/cgroup/a/b"),
                PathBuf::from("/sys/fs/cgroup/a"),
                PathBuf::from("/sys/fs/cgroup"),
            ]
        );
    }

    #[test]
    fn dir_chain_never_escapes_the_mount() {
        assert_eq!(
            cgroup_dir_chain(Path::new("/sys/fs/cgroup"), "/../../etc"),
            vec![PathBuf::from("/sys/fs/cgroup/etc"), PathBuf::from("/sys/fs/cgroup")]
        );
        assert_eq!(
            cgroup_dir_chain(Path::new("/sys/fs/cgroup"), "/"),
            vec![PathBuf::from("/sys/fs/cgroup")]
        );
    }

    #[test]
    fn v2_reads_the_leaf_and_takes_the_ancestor_limit() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mount = tmp.path();
        let leaf = mount.join("user.slice/session.scope");
        // The scope itself is uncapped; the slice above it carries the MemoryMax. A leaf-only
        // reader would report "no limit" for a process that is very much capped.
        write(&mount.join("user.slice"), "memory.max", "1073741824\n");
        write(&leaf, "memory.max", "max\n");
        write(&leaf, "memory.current", "536870912\n");
        write(&leaf, "memory.stat", "anon 100\ninactive_file 268435456\n");

        let reading = read_v2_at(mount, "0::/user.slice/session.scope\n").expect("v2 reading");
        assert_eq!(reading.limit_bytes, Some(1_073_741_824));
        assert_eq!(reading.used_bytes, 536_870_912 - 268_435_456);
    }

    #[test]
    fn v2_falls_back_to_the_mount_when_the_reported_path_is_not_present() {
        // A container's cgroup namespace: /proc/self/cgroup names the host path, but the
        // namespaced mount root is where the files actually are.
        let tmp = tempfile::tempdir().expect("tempdir");
        let mount = tmp.path();
        write(mount, "memory.current", "104857600\n");
        write(mount, "memory.max", "209715200\n");

        let reading = read_v2_at(mount, "0::/system.slice/docker-deadbeef.scope\n").expect("v2 reading");
        assert_eq!(reading.limit_bytes, Some(209_715_200));
        // No memory.stat at all: the working set degrades to the charged figure, not to zero.
        assert_eq!(reading.used_bytes, 104_857_600);
    }

    #[test]
    fn v2_reports_no_limit_when_every_level_says_max() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write(tmp.path(), "memory.current", "1000\n");
        write(tmp.path(), "memory.max", "max\n");
        let reading = read_v2_at(tmp.path(), "0::/\n").expect("v2 reading");
        assert_eq!(reading.limit_bytes, None);
        assert_eq!(reading.used_bytes, 1000);
    }

    #[test]
    fn v2_declines_when_the_hierarchy_is_absent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert_eq!(read_v2_at(tmp.path(), "0::/\n"), None);
        // A v1-only host has no `0::` line at all.
        assert_eq!(read_v2_at(tmp.path(), "4:memory:/\n"), None);
    }

    #[test]
    fn v1_reads_the_memory_controller_subtree() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let leaf = tmp.path().join("memory/docker/abc");
        write(&leaf, "memory.usage_in_bytes", "800000\n");
        write(&leaf, "memory.limit_in_bytes", "2000000\n");
        write(&leaf, "memory.stat", "cache 1\ntotal_inactive_file 300000\n");

        let reading = read_v1_at(tmp.path(), "4:memory:/docker/abc\n").expect("v1 reading");
        assert_eq!(reading.limit_bytes, Some(2_000_000));
        assert_eq!(reading.used_bytes, 500_000);
    }

    #[test]
    fn v1_no_limit_sentinel_survives_the_full_read_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("memory");
        write(&root, "memory.usage_in_bytes", "4096\n");
        write(&root, "memory.limit_in_bytes", "9223372036854771712\n");
        let reading = read_v1_at(tmp.path(), "4:memory:/\n").expect("v1 reading");
        assert_eq!(reading.limit_bytes, None);
        assert_eq!(reading.used_bytes, 4096);
    }

    #[test]
    fn v1_declines_when_the_controller_is_not_mounted() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert_eq!(read_v1_at(tmp.path(), "4:memory:/\n"), None);
    }
}
