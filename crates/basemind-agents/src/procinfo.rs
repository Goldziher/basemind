//! What the kernel says about a process: its parent, group, session, start time and whether it
//! is a zombie, read without running a program (`/proc` on Linux, `proc_pidinfo` on macOS).
//!
//! A pid alone names whichever process holds it now; the pair of a pid and its start time names
//! one process for its whole life, so every identity this crate keeps is that pair. Elsewhere
//! (other Unixes, Windows) nothing is known, and every caller treats that as "cannot verify".

/// One process, as the kernel reported it at one moment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ProcInfo {
    pub pid: i32,
    pub ppid: i32,
    pub pgid: i32,
    pub sid: i32,
    /// When the process started, in a unit fixed per platform: comparable only with another
    /// start time read here.
    pub start: u64,
    /// Exited and not yet reaped: it can run nothing more. Only Linux reports one; macOS does
    /// not describe a zombie at all.
    pub zombie: bool,
}

/// One process for its whole life: its pid and start time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ProcId {
    pub pid: i32,
    pub start: u64,
}

impl ProcInfo {
    pub fn id(&self) -> ProcId {
        ProcId {
            pid: self.pid,
            start: self.start,
        }
    }
}

/// Whether this platform can report processes at all.
pub(crate) const SUPPORTED: bool = cfg!(any(target_os = "linux", target_os = "android", target_os = "macos"));

/// Process `pid` now, `None` when it does not exist or cannot be read.
pub(crate) fn info(pid: i32) -> Option<ProcInfo> {
    if pid <= 0 {
        return None;
    }
    platform::info(pid)
}

/// Whether `id` is still the same process and has not exited. Only the Unix reaper asks.
#[cfg(unix)]
pub(crate) fn running(id: ProcId) -> bool {
    info(id.pid).is_some_and(|now| now.start == id.start && !now.zombie)
}

/// Every process now. `None` when the table could not be read, or read empty: no decision
/// may rest on an empty listing, since this process at least is always in it.
pub(crate) fn all() -> Option<Vec<ProcInfo>> {
    // A process that exited between the listing and its read is simply gone.
    let procs: Vec<ProcInfo> = platform::pids()?.into_iter().filter_map(info).collect();
    (!procs.is_empty()).then_some(procs)
}

/// How many entries a listing into a buffer of `room` entries returned, when it is known to be
/// whole: a negative count is an error, and a full buffer may have cut the listing short.
#[cfg_attr(
    not(target_os = "macos"),
    allow(dead_code, reason = "only macOS lists into a buffer")
)]
fn whole(listed: i32, room: usize) -> Option<usize> {
    usize::try_from(listed).ok().filter(|listed| *listed < room)
}

/// What a process runs, as far as it can be read: its command name (as the kernel truncates it)
/// and the device and inode of its executable. Two processes run the same program when either
/// matches.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Image {
    pub comm: Option<Vec<u8>>,
    pub file: Option<(u64, u64)>,
}

impl Image {
    /// Whether `self` and `other` are the same program: the same executable file, or the same
    /// command name. A name alone suffices, so a copy under the same name matches, and so does
    /// the file alone, so a link under another name matches too.
    pub fn same_program(&self, other: &Self) -> bool {
        fn same<T: PartialEq>(a: Option<&T>, b: Option<&T>) -> bool {
            a.is_some() && a == b
        }
        same(self.file.as_ref(), other.file.as_ref()) || same(self.comm.as_ref(), other.comm.as_ref())
    }
}

/// What process `pid` runs, `None` when neither part can be read.
pub(crate) fn image(pid: i32) -> Option<Image> {
    if pid <= 0 {
        return None;
    }
    let image = platform::image(pid);
    (image.comm.is_some() || image.file.is_some()).then_some(image)
}

/// The device and inode of the file at `path`.
#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
fn file_id(path: &std::path::Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.dev(), meta.ino()))
}

/// The ancestors of a process, nearest first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Chain {
    pub procs: Vec<ProcInfo>,
    /// Whether the walk reached a process whose parent is 0 or itself (the kernel's or init).
    /// `false` when it stopped at a parent that could not be read, such as a process of another
    /// user, which macOS does not describe (`login`, a setuid program): nothing is known above it.
    pub complete: bool,
}

/// The ancestors of `from`, a process as the caller read it: the walk starts from that reading,
/// never from a fresh read of its pid, which may name another process by now. `None` when a
/// link vanished or was reused while it was read, or the walk ran over a bound no real tree
/// reaches.
pub(crate) fn ancestors(from: &ProcInfo) -> Option<Chain> {
    const MAX_DEPTH: usize = 4096;
    let mut procs = Vec::new();
    let mut at = *from;
    loop {
        if at.ppid <= 0 || at.ppid == at.pid {
            return Some(Chain { procs, complete: true });
        }
        let Some(parent) = info(at.ppid) else {
            return Some(Chain { procs, complete: false });
        };
        // A parent that started after its child is a reused pid: the real parent is gone.
        if parent.start > at.start {
            return None;
        }
        procs.push(parent);
        if procs.len() > MAX_DEPTH {
            return None;
        }
        at = parent;
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod platform {
    use super::ProcInfo;

    pub fn info(pid: i32) -> Option<ProcInfo> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        parse_stat(pid, &stat)
    }

    /// The fields of `/proc/<pid>/stat` after the command name, which may itself hold spaces
    /// and parentheses, so it is skipped up to its last `)`. Numbered as in `proc(5)`: state
    /// is field 3, ppid 4, pgrp 5, session 6 and starttime 22.
    pub(super) fn parse_stat(pid: i32, stat: &str) -> Option<ProcInfo> {
        let rest = &stat[stat.rfind(')')? + 1..];
        let fields: Vec<&str> = rest.split_whitespace().collect();
        let field = |n: usize| fields.get(n - 3).copied();
        Some(ProcInfo {
            pid,
            ppid: field(4)?.parse().ok()?,
            pgid: field(5)?.parse().ok()?,
            sid: field(6)?.parse().ok()?,
            start: field(22)?.parse().ok()?,
            zombie: matches!(field(3)?, "Z" | "X"),
        })
    }

    /// The command name is readable for any process; the executable only for one this
    /// process may trace (`PTRACE_MODE_READ`), so not for a non-dumpable one.
    pub fn image(pid: i32) -> super::Image {
        let comm = std::fs::read(format!("/proc/{pid}/comm")).ok().map(|mut comm| {
            if comm.last() == Some(&b'\n') {
                comm.pop();
            }
            comm
        });
        super::Image {
            comm,
            file: super::file_id(std::path::Path::new(&format!("/proc/{pid}/exe"))),
        }
    }

    pub fn pids() -> Option<Vec<i32>> {
        let entries = std::fs::read_dir("/proc").ok()?;
        Some(
            entries
                .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
                .collect(),
        )
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::os::unix::ffi::OsStringExt;

    use super::ProcInfo;

    /// The process's BSD info, `None` when it cannot be read.
    fn bsd_info(pid: i32) -> Option<libc::proc_bsdinfo> {
        // SAFETY: proc_bsdinfo is plain data, valid all-zero, and proc_pidinfo writes at most
        // the size it is given into it.
        let (written, bsd) = unsafe {
            let mut bsd: libc::proc_bsdinfo = std::mem::zeroed();
            let size = libc::c_int::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
            let written = libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut bsd).cast(), size);
            (written, bsd)
        };
        (usize::try_from(written).ok()? == std::mem::size_of::<libc::proc_bsdinfo>()).then_some(bsd)
    }

    pub fn info(pid: i32) -> Option<ProcInfo> {
        let bsd = bsd_info(pid)?;
        // SAFETY: getsid only reads the process table.
        let sid = unsafe { libc::getsid(pid) };
        if sid < 0 {
            return None;
        }
        // The session id is read apart from the rest: unless the same process still holds the
        // pid afterwards, it may be another process's.
        let again = bsd_info(pid)?;
        if (again.pbi_start_tvsec, again.pbi_start_tvusec) != (bsd.pbi_start_tvsec, bsd.pbi_start_tvusec) {
            return None;
        }
        Some(ProcInfo {
            pid,
            ppid: i32::try_from(bsd.pbi_ppid).ok()?,
            pgid: i32::try_from(bsd.pbi_pgid).ok()?,
            sid,
            start: bsd
                .pbi_start_tvsec
                .checked_mul(1_000_000)?
                .checked_add(bsd.pbi_start_tvusec)?,
            zombie: bsd.pbi_status == libc::SZOMB,
        })
    }

    pub fn image(pid: i32) -> super::Image {
        let comm = bsd_info(pid).map(|bsd| {
            bsd.pbi_comm
                .iter()
                .take_while(|byte| **byte != 0)
                .map(|byte| byte.to_ne_bytes()[0])
                .collect()
        });
        let mut path = vec![0_u8; usize::try_from(libc::PROC_PIDPATHINFO_MAXSIZE).unwrap_or(4096)];
        let room = u32::try_from(path.len()).unwrap_or(0);
        // SAFETY: the buffer holds `room` bytes and proc_pidpath writes at most that many.
        let written = unsafe { libc::proc_pidpath(pid, path.as_mut_ptr().cast(), room) };
        let file = usize::try_from(written)
            .ok()
            .filter(|written| *written > 0)
            .and_then(|written| {
                path.truncate(written);
                let path = std::path::PathBuf::from(std::ffi::OsString::from_vec(path));
                super::file_id(&path)
            });
        super::Image { comm, file }
    }

    pub fn pids() -> Option<Vec<i32>> {
        // SAFETY: with a null buffer proc_listallpids returns the number of processes.
        let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        let mut room = usize::try_from(count).ok()? + 64;
        // A buffer the listing filled may have cut it short: retry larger, then give up, since
        // a partial table would pass for a complete one.
        for _ in 0..4 {
            let mut pids = vec![0_i32; room];
            let bytes = libc::c_int::try_from(room * std::mem::size_of::<i32>()).ok()?;
            // SAFETY: the buffer holds `room` pids, `bytes` bytes, and the call writes at most that.
            let listed = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
            if let Some(listed) = super::whole(listed, room) {
                pids.truncate(listed);
                return Some(pids);
            }
            room = room.checked_mul(2)?;
        }
        None
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
mod platform {
    use super::ProcInfo;

    pub fn info(_pid: i32) -> Option<ProcInfo> {
        None
    }

    pub fn image(_pid: i32) -> super::Image {
        super::Image::default()
    }

    pub fn pids() -> Option<Vec<i32>> {
        None
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "android", target_os = "macos")))]
mod tests {
    use super::*;

    #[test]
    fn this_process_and_its_parent_are_known() {
        let me = info(i32::try_from(std::process::id()).unwrap()).unwrap();
        assert_eq!(u32::try_from(me.pid).ok(), Some(std::process::id()));
        assert_eq!(u32::try_from(me.ppid).ok(), Some(std::os::unix::process::parent_id()));
        assert!(!me.zombie);
        assert!(running(me.id()));
        assert!(!running(ProcId {
            start: me.start + 1,
            ..me.id()
        }));
        assert!(all().unwrap().iter().any(|proc| proc.id() == me.id()));
        let chain = ancestors(&me).unwrap();
        assert_eq!(chain.procs.first().map(|parent| parent.pid), Some(me.ppid));
        assert!(info(0).is_none() && info(-1).is_none());
    }

    #[test]
    fn an_exited_child_reads_as_a_zombie_or_not_at_all_before_it_is_reaped() {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let pid = i32::try_from(child.id()).unwrap();
        let first = info(pid).unwrap();
        assert_eq!(u32::try_from(first.ppid).ok(), Some(std::process::id()));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        // Linux describes a zombie; macOS describes none.
        while !info(pid).is_none_or(|now| now.zombie) {
            assert!(std::time::Instant::now() < deadline, "the child never exited");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        child.wait().unwrap();
    }

    #[test]
    fn a_listing_that_filled_its_buffer_is_not_taken_as_whole() {
        assert_eq!(whole(10, 11), Some(10));
        assert_eq!(whole(11, 11), None, "a full buffer was taken for the whole table");
        assert_eq!(whole(-1, 11), None);
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_stat_line_with_parentheses_in_the_name_parses() {
        let stat = "42 (a) b (c)) S 1 42 42 0 -1 4194560 0 0 0 0 0 0 0 0 20 0 1 0 777 0 0";
        let parsed = platform::parse_stat(42, stat).unwrap();
        assert_eq!((parsed.ppid, parsed.pgid, parsed.sid, parsed.start), (1, 42, 42, 777));
        assert!(!parsed.zombie);
    }
}
