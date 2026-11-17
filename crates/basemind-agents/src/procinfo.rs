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

/// The ancestors of `pid`, nearest first, ending at a process whose parent is 0 or itself
/// (the kernel's or init), or at the child of a process of another user, which macOS does not
/// describe (`login`, `launchd`, a setuid program). `None` when a link vanished while it was
/// read, which callers treat as unknown ancestry, or the walk ran over a bound no real tree
/// reaches.
pub(crate) fn ancestors(pid: i32) -> Option<Vec<ProcInfo>> {
    const MAX_DEPTH: usize = 4096;
    let mut chain = Vec::new();
    let mut at = info(pid)?;
    loop {
        if at.ppid <= 0 || at.ppid == at.pid {
            return Some(chain);
        }
        let Some(parent) = info(at.ppid) else {
            return another_users(at.ppid).then_some(chain);
        };
        // A parent that started after its child is a reused pid: the real parent is gone.
        if parent.start > at.start {
            return None;
        }
        chain.push(parent);
        if chain.len() > MAX_DEPTH {
            return None;
        }
        at = parent;
    }
}

/// Whether `pid` exists although it could not be read: a process of another user. Signal 0
/// reports `EPERM` for most of them, and succeeds for one in the caller's own POSIX session
/// (macOS lets a session's processes signal its leader, `login`, with SIGCONT).
fn another_users(pid: i32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: signal 0 only checks that the process exists and may be signalled.
        let status = unsafe { libc::kill(pid, 0) };
        status == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
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
}

#[cfg(target_os = "macos")]
mod platform {
    use super::ProcInfo;

    pub fn info(pid: i32) -> Option<ProcInfo> {
        // SAFETY: proc_bsdinfo is plain data, valid all-zero, and proc_pidinfo writes at most
        // the size it is given into it.
        let (written, bsd) = unsafe {
            let mut bsd: libc::proc_bsdinfo = std::mem::zeroed();
            let size = libc::c_int::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
            let written = libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut bsd).cast(), size);
            (written, bsd)
        };
        if usize::try_from(written).ok()? != std::mem::size_of::<libc::proc_bsdinfo>() {
            return None;
        }
        // SAFETY: getsid only reads the process table.
        let sid = unsafe { libc::getsid(pid) };
        if sid < 0 {
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
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
mod platform {
    use super::ProcInfo;

    pub fn info(_pid: i32) -> Option<ProcInfo> {
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
        let chain = ancestors(me.pid).unwrap();
        assert_eq!(chain.first().map(|parent| parent.pid), Some(me.ppid));
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

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_stat_line_with_parentheses_in_the_name_parses() {
        let stat = "42 (a) b (c)) S 1 42 42 0 -1 4194560 0 0 0 0 0 0 0 0 20 0 1 0 777 0 0";
        let parsed = platform::parse_stat(42, stat).unwrap();
        assert_eq!((parsed.ppid, parsed.pgid, parsed.sid, parsed.start), (1, 42, 42, 777));
        assert!(!parsed.zombie);
    }
}
