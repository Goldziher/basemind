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

    /// Whether [`Self::of_socket`] pins a peer to the process that connected on this host: on
    /// macOS always, on Linux when the kernel gives a pidfd for a socket's peer (6.5 and
    /// later). Where it does not, a pid freed and reused between the connect and
    /// [`Self::of_socket`] would be taken for the peer; an engine can refuse callers then.
    #[must_use]
    pub fn pins_connections() -> bool {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            linux::supported()
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            cfg!(target_os = "macos")
        }
    }

    /// The embedding's own process: the peer of a request the embedding makes itself, such as
    /// the lead's tool calls over its own stdio in the in-process topology, or its user
    /// interface. Never the peer of a connection: build that with [`Self::of_socket`] on the
    /// accepted socket. A caller with this peer is whatever this process is registered as, so a
    /// request handler that used it for a connection would hand that role to whoever connected.
    ///
    /// # Errors
    ///
    /// [`std::io::ErrorKind::Unsupported`] where this crate cannot read the process table.
    pub fn embedding() -> std::io::Result<Self> {
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
    // Only a connected socket has a peer: Linux reports the process that called listen() for a
    // listening socket's credentials, which would pass this process off as its own peer.
    // SAFETY: sockaddr_storage is plain data, valid all-zero; getpeername writes at most `len`
    // bytes into it.
    let connected = unsafe {
        let mut address: libc::sockaddr_storage = std::mem::zeroed();
        let mut len = libc::socklen_t::try_from(std::mem::size_of::<libc::sockaddr_storage>()).unwrap_or(0);
        libc::getpeername(fd, (&raw mut address).cast(), &mut len) == 0
    };
    if !connected {
        return Err(std::io::Error::last_os_error());
    }
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
fn getsockopt<T: Copy>(fd: i32, level: i32, name: i32, value: &mut T) -> std::io::Result<()> {
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
        let Some(option) = SO_PEERPIDFD.filter(|_| supported()) else {
            // A kernel before 6.5: only the pid is known.
            return Ok(live(pid)?.id());
        };
        let mut pidfd: libc::c_int = -1;
        match super::getsockopt(fd, libc::SOL_SOCKET, option, &mut pidfd) {
            Ok(()) => {}
            // The kernel pins peers, so a peer it cannot give a pidfd for has exited (some
            // kernels report that as EINVAL): it is never taken by its pid alone.
            Err(error) if matches!(error.raw_os_error(), Some(libc::ESRCH | libc::EINVAL)) => return Err(gone()),
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

    /// Whether this kernel gives a pidfd for a socket's peer, probed once on a socket pair whose
    /// peer is this process.
    pub fn supported() -> bool {
        static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *SUPPORTED.get_or_init(|| {
            let Some(option) = SO_PEERPIDFD else {
                return false;
            };
            let Ok((ours, _theirs)) = std::os::unix::net::UnixStream::pair() else {
                return false;
            };
            let mut pidfd: libc::c_int = -1;
            let fd = std::os::fd::AsRawFd::as_raw_fd(&ours);
            if super::getsockopt(fd, libc::SOL_SOCKET, option, &mut pidfd).is_err() || pidfd < 0 {
                return false;
            }
            // SAFETY: the kernel just returned this descriptor; it is closed here.
            drop(unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(pidfd) });
            true
        })
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
        assert_eq!(peer, Peer::embedding().unwrap());
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
    fn a_peer_that_exited_before_it_was_read_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agents.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        // Connects and exits at once, before the connection is accepted.
        let status = std::process::Command::new("perl")
            .arg("-MIO::Socket::UNIX")
            .arg("-e")
            .arg(format!("IO::Socket::UNIX->new(Peer => q{{{}}}) or die", path.display()))
            .status()
            .expect("perl runs the connecting client");
        assert!(status.success());
        let (connection, _) = listener.accept().unwrap();
        // Refused however the kernel reports it: the peer is gone (Linux), or the socket no
        // longer describes it (macOS forgets a peer that closed its end).
        let read = Peer::of_socket(connection.as_fd());
        assert!(read.is_err(), "an exited peer was taken by its pid: {read:?}");
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_kernel_from_6_5_pins_peers_with_a_pidfd() {
        let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap();
        let mut parts = release
            .trim()
            .split(['.', '-'])
            .map(|part| part.parse::<u32>().unwrap_or(0));
        let version = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
        let generic = cfg!(any(
            target_arch = "x86_64",
            target_arch = "x86",
            target_arch = "aarch64",
            target_arch = "arm",
            target_arch = "riscv64"
        ));
        if version >= (6, 5) && generic {
            assert!(
                Peer::pins_connections(),
                "kernel {release} gave no pidfd for a socket's peer"
            );
        } else {
            eprintln!("kernel {release}: SO_PEERPIDFD not expected");
        }
    }

    #[test]
    fn a_socket_that_is_not_connected_has_no_peer() {
        let dir = tempfile::tempdir().unwrap();
        let listener = std::os::unix::net::UnixListener::bind(dir.path().join("socket")).unwrap();
        assert!(Peer::of_socket(listener.as_fd()).is_err());
    }
}
