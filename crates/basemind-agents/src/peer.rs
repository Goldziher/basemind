//! The process on the other end of a connection, as the kernel reports it: the one fact about a
//! caller that a launched agent cannot choose.

use crate::procinfo::{self, ProcId};

/// A process the kernel vouched for: the peer of a Unix-domain socket, or this process.
///
/// It is the pid the kernel reported, bound to that process's start time when it was read, so a
/// later reuse of the pid by another process is told apart. There is no constructor from a bare
/// pid: a pid a client sends is whatever it wants it to be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Peer {
    pub(crate) id: ProcId,
}

impl Peer {
    /// The process on the other end of the connected Unix-domain socket `socket`, pinned to the
    /// process that connected: on macOS by the pid version in its audit token
    /// (`LOCAL_PEERTOKEN`), on Linux 6.5 and later by a pidfd (`SO_PEERPIDFD`). On an older
    /// Linux kernel only the pid is known (`SO_PEERCRED`), and the start time read here pins
    /// the process that holds it then: a pid freed and reused since the connect would be taken
    /// for the peer. Call it on the accepted connection before reading from it.
    ///
    /// # Errors
    ///
    /// The socket's error; [`std::io::ErrorKind::PermissionDenied`] for a peer running as
    /// another user; [`std::io::ErrorKind::Unsupported`] on a platform whose kernel does not
    /// report the peer's process, or whose process table this crate cannot read; and
    /// [`std::io::ErrorKind::NotFound`] when the peer is already gone.
    #[cfg(unix)]
    pub fn of_socket(socket: std::os::fd::BorrowedFd<'_>) -> std::io::Result<Self> {
        if !procinfo::SUPPORTED {
            return Err(unsupported());
        }
        socket_peer(socket).map(|id| Self { id })
    }

    /// This process, for an embedding that calls the supervisor from its own code (its own user
    /// interface), never on behalf of a connection.
    ///
    /// # Errors
    ///
    /// [`std::io::ErrorKind::Unsupported`] where this crate cannot read the process table.
    pub fn this_process() -> std::io::Result<Self> {
        let pid = i32::try_from(std::process::id()).map_err(|_| unsupported())?;
        Self::of_process(pid)
    }

    /// The peer's process id.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.id.pid.unsigned_abs()
    }

    fn of_process(pid: i32) -> std::io::Result<Self> {
        if !procinfo::SUPPORTED {
            return Err(unsupported());
        }
        let info = procinfo::info(pid)
            .filter(|info| !info.zombie)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "the peer process is gone"))?;
        Ok(Self { id: info.id() })
    }

    /// The process `pid` as it is now, standing in for what the kernel would report for a
    /// connection it made.
    #[cfg(all(test, unix))]
    pub(crate) fn of_pid(pid: u32) -> Self {
        Self::of_process(i32::try_from(pid).unwrap()).unwrap()
    }
}

fn unsupported() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "this platform does not report a connection's peer process",
    )
}

/// The peer of `socket`, pinned to the process that connected, refusing a peer of another user.
#[cfg(unix)]
fn socket_peer(socket: std::os::fd::BorrowedFd<'_>) -> std::io::Result<ProcId> {
    use std::os::fd::AsRawFd;
    let fd = socket.as_raw_fd();
    // SAFETY: geteuid has no preconditions.
    let euid = unsafe { libc::geteuid() };
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
        getsockopt(fd, libc::SOL_SOCKET, libc::SO_PEERCRED, &mut cred)?;
        if cred.uid != euid {
            return Err(other_user());
        }
        if cred.pid <= 0 {
            return Err(unsupported());
        }
        linux::pinned(fd, cred.pid)
    }
    #[cfg(target_os = "macos")]
    {
        // An audit token: val[1] the effective uid, val[5] the pid, val[7] the pid version,
        // which no later process holding the pid shares.
        let mut token = [0_u32; 8];
        getsockopt(fd, libc::SOL_LOCAL, libc::LOCAL_PEERTOKEN, &mut token)?;
        if token[1] != euid {
            return Err(other_user());
        }
        let pid = i32::try_from(token[5]).map_err(|_| unsupported())?;
        let version = i32::from_ne_bytes(token[7].to_ne_bytes());
        // The start time belongs to the peer only if the pid still has the peer's version
        // after it was read.
        let peer = live(pid)?;
        if macos::pid_version(pid) != Some(version) || live(pid)?.start != peer.start {
            return Err(gone());
        }
        Ok(peer.id())
    }
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
    {
        let _ = (fd, euid);
        Err(unsupported())
    }
}

/// Reads the socket option `name` into `value`, which it must fill exactly.
#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
fn getsockopt<T>(fd: i32, level: i32, name: i32, value: &mut T) -> std::io::Result<()> {
    let size = std::mem::size_of::<T>();
    let mut len = libc::socklen_t::try_from(size).map_err(|_| unsupported())?;
    // SAFETY: getsockopt writes at most `len` bytes into `value`, a plain value of that size.
    let status = unsafe { libc::getsockopt(fd, level, name, std::ptr::from_mut(value).cast(), &mut len) };
    if status != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if usize::try_from(len).ok() != Some(size) {
        return Err(unsupported());
    }
    Ok(())
}

/// Process `pid` as it is now, when it runs.
#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
fn live(pid: i32) -> std::io::Result<procinfo::ProcInfo> {
    procinfo::info(pid).filter(|info| !info.zombie).ok_or_else(gone)
}

#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
fn gone() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::NotFound, "the peer process is gone")
}

#[cfg(target_os = "macos")]
mod macos {
    /// `struct proc_uniqidentifierinfo` of XNU's `sys/proc_info_private.h`.
    #[repr(C)]
    struct UniqIdentifierInfo {
        uuid: [u8; 16],
        unique_id: u64,
        parent_unique_id: u64,
        id_version: i32,
        orig_ppid_version: i32,
        reserved: [u64; 2],
    }

    /// `PROC_PIDUNIQIDENTIFIERINFO`.
    const UNIQ_IDENTIFIER_INFO: libc::c_int = 17;

    /// The pid version of process `pid` now: the value its audit token carries.
    pub fn pid_version(pid: i32) -> Option<i32> {
        let size = libc::c_int::try_from(std::mem::size_of::<UniqIdentifierInfo>()).ok()?;
        // SAFETY: the struct is plain data, valid all-zero, and proc_pidinfo writes at most
        // `size` bytes into it.
        let (written, info) = unsafe {
            let mut info: UniqIdentifierInfo = std::mem::zeroed();
            let written = libc::proc_pidinfo(pid, UNIQ_IDENTIFIER_INFO, 0, (&raw mut info).cast(), size);
            (written, info)
        };
        (written == size).then_some(info.id_version)
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod linux {
    use super::{ProcId, gone, live};

    /// `SO_PEERPIDFD` (Linux 6.5), where its value is the generic one.
    #[cfg(any(
        target_arch = "x86_64",
        target_arch = "x86",
        target_arch = "aarch64",
        target_arch = "arm",
        target_arch = "riscv64"
    ))]
    const SO_PEERPIDFD: Option<libc::c_int> = Some(77);
    #[cfg(not(any(
        target_arch = "x86_64",
        target_arch = "x86",
        target_arch = "aarch64",
        target_arch = "arm",
        target_arch = "riscv64"
    )))]
    const SO_PEERPIDFD: Option<libc::c_int> = None;

    /// Peer `pid` of socket `fd`, pinned through a pidfd of the process that connected where
    /// the kernel gives one: while that process lives no other can hold its pid, so the start
    /// time read before the pidfd is checked belongs to it.
    pub fn pinned(fd: i32, pid: i32) -> std::io::Result<ProcId> {
        let Some(option) = SO_PEERPIDFD else {
            return Ok(live(pid)?.id());
        };
        let mut pidfd: libc::c_int = -1;
        match super::getsockopt(fd, libc::SOL_SOCKET, option, &mut pidfd) {
            Ok(()) => {}
            // A kernel before 6.5: only the pid is known.
            Err(error) if matches!(error.raw_os_error(), Some(libc::ENOPROTOOPT | libc::EINVAL)) => {
                return Ok(live(pid)?.id());
            }
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => return Err(gone()),
            Err(error) => return Err(error),
        }
        // SAFETY: the kernel just returned this descriptor, owned here from now on.
        let pidfd = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(pidfd) };
        let raw = std::os::fd::AsRawFd::as_raw_fd(&pidfd);
        let peer = live(pid)?;
        if pidfd_pid(raw) != Some(pid) {
            return Err(gone());
        }
        // SAFETY: pidfd_send_signal takes the pidfd, signal 0 (a check only) and a null siginfo.
        let alive = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                raw,
                0,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        } == 0;
        if !alive {
            return Err(gone());
        }
        Ok(peer.id())
    }

    /// The pid a pidfd refers to, from `/proc/self/fdinfo`; `None` once it has exited.
    fn pidfd_pid(fd: i32) -> Option<i32> {
        let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{fd}")).ok()?;
        let pid: i32 = info
            .lines()
            .find_map(|line| line.strip_prefix("Pid:"))?
            .trim()
            .parse()
            .ok()?;
        (pid > 0).then_some(pid)
    }
}

#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
fn other_user() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "the peer process runs as another user",
    )
}

#[cfg(all(test, any(target_os = "linux", target_os = "android", target_os = "macos")))]
mod tests {
    use super::*;
    use std::os::fd::AsFd;

    #[test]
    fn the_peer_of_a_socket_is_the_process_that_holds_its_other_end() {
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let peer = Peer::of_socket(ours.as_fd()).unwrap();
        assert_eq!(peer.pid(), std::process::id());
        assert_eq!(peer, Peer::this_process().unwrap());
        drop(theirs);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_audit_token_carries_the_pid_version_this_crate_reads() {
        use std::os::fd::AsRawFd;
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut token = [0_u32; 8];
        getsockopt(ours.as_raw_fd(), libc::SOL_LOCAL, libc::LOCAL_PEERTOKEN, &mut token).unwrap();
        let me = i32::try_from(std::process::id()).unwrap();
        assert_eq!(i32::try_from(token[5]).ok(), Some(me));
        let version = macos::pid_version(me).unwrap();
        assert_ne!(version, 0);
        assert_eq!(i32::from_ne_bytes(token[7].to_ne_bytes()), version);
        drop(theirs);
    }

    #[test]
    fn a_socket_that_is_not_connected_has_no_peer() {
        let dir = tempfile::tempdir().unwrap();
        let listener = std::os::unix::net::UnixListener::bind(dir.path().join("socket")).unwrap();
        assert!(Peer::of_socket(listener.as_fd()).is_err());
    }
}
