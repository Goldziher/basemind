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
    /// The process on the other end of the connected Unix-domain socket `socket`: `SO_PEERCRED`
    /// on Linux, `LOCAL_PEERPID` on macOS. Call it on the accepted connection before reading
    /// from it: the kernel reports the process that connected, and the start time read here
    /// pins that process only while it is still the one holding the pid.
    ///
    /// # Errors
    ///
    /// The socket's error; [`std::io::ErrorKind::PermissionDenied`] for a peer running as
    /// another user; [`std::io::ErrorKind::Unsupported`] on a platform whose kernel does not
    /// report the peer's process, or whose process table this crate cannot read; and
    /// [`std::io::ErrorKind::NotFound`] when the peer is already gone.
    #[cfg(unix)]
    pub fn of_socket(socket: std::os::fd::BorrowedFd<'_>) -> std::io::Result<Self> {
        let pid = socket_peer(socket)?;
        Self::of_process(pid)
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

/// The pid of the peer of `socket`, refusing a peer of another user.
#[cfg(unix)]
fn socket_peer(socket: std::os::fd::BorrowedFd<'_>) -> std::io::Result<i32> {
    use std::os::fd::AsRawFd;
    let fd = socket.as_raw_fd();
    // SAFETY: geteuid has no preconditions.
    let euid = unsafe { libc::geteuid() };
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
        let mut len = libc::socklen_t::try_from(std::mem::size_of::<libc::ucred>()).map_err(|_| unsupported())?;
        // SAFETY: getsockopt writes at most `len` bytes into `cred`, a plain struct.
        let status = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&raw mut cred).cast(),
                &mut len,
            )
        };
        if status != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if cred.uid != euid {
            return Err(other_user());
        }
        if cred.pid <= 0 {
            return Err(unsupported());
        }
        Ok(cred.pid)
    }
    #[cfg(target_os = "macos")]
    {
        let (mut uid, mut gid) = (0, 0);
        // SAFETY: getpeereid writes the two ids it is given.
        if unsafe { libc::getpeereid(fd, &mut uid, &mut gid) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if uid != euid {
            return Err(other_user());
        }
        let mut pid: libc::pid_t = 0;
        let mut len = libc::socklen_t::try_from(std::mem::size_of::<libc::pid_t>()).map_err(|_| unsupported())?;
        // SAFETY: getsockopt writes at most `len` bytes into `pid`.
        let status = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                (&raw mut pid).cast(),
                &mut len,
            )
        };
        if status != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if pid <= 0 {
            return Err(unsupported());
        }
        Ok(pid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
    {
        let _ = (fd, euid);
        Err(unsupported())
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

    #[test]
    fn a_socket_that_is_not_connected_has_no_peer() {
        let dir = tempfile::tempdir().unwrap();
        let listener = std::os::unix::net::UnixListener::bind(dir.path().join("socket")).unwrap();
        assert!(Peer::of_socket(listener.as_fd()).is_err());
    }
}
