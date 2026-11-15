//! One-shot blocking exchanges with a daemon's endpoint: what the liveness probe, the status read
//! and the stop request are built on.
//!
//! On Unix every read and write takes a timeout. A Windows pipe opened as a file takes none, so
//! there an exchange runs on a thread of its own under [`bounded`], and the caller gives up on it
//! at the limit.

use std::path::Path;
use std::time::Duration;

use super::super::protocol::{CommsOut, CommsResponse};

/// Send one framed request `body` and decode the one framed reply, giving up after `timeout`:
/// per read and write on Unix, for the whole exchange on Windows.
pub(super) fn exchange(socket_path: &Path, body: Vec<u8>, timeout: Duration) -> Option<CommsResponse> {
    #[cfg(windows)]
    {
        let endpoint = socket_path.to_path_buf();
        bounded(timeout, move || {
            let stream = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&endpoint)
                .ok()?;
            converse(stream, &body)
        })
    }
    #[cfg(unix)]
    {
        let stream = std::os::unix::net::UnixStream::connect(socket_path).ok()?;
        let _ = stream.set_read_timeout(Some(timeout));
        let _ = stream.set_write_timeout(Some(timeout));
        converse(stream, &body)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (socket_path, body, timeout);
        None
    }
}

/// Write the framed request and read the framed reply. Bounds the reply to 64 KiB.
#[cfg(any(unix, windows))]
fn converse(mut stream: impl std::io::Read + std::io::Write, body: &[u8]) -> Option<CommsResponse> {
    let len = u32::try_from(body.len()).ok()?;
    stream.write_all(&len.to_be_bytes()).ok()?;
    stream.write_all(body).ok()?;
    let mut prefix = [0u8; 4];
    stream.read_exact(&mut prefix).ok()?;
    let rlen = u32::from_be_bytes(prefix) as usize;
    if rlen > 64 * 1024 {
        return None;
    }
    let mut buf = vec![0u8; rlen];
    stream.read_exact(&mut buf).ok()?;
    match rmp_serde::from_slice::<CommsOut>(&buf).ok()? {
        CommsOut::Response(resp) | CommsOut::Reply { response: resp, .. } => Some(resp),
        CommsOut::Notification(_) => None,
    }
}

/// Run blocking `work` on a thread of its own and wait at most `limit` for what it returns. For
/// I/O that takes no timeout: past the limit the caller moves on with `None`, and the thread ends
/// whenever its call returns.
#[cfg(any(windows, test))]
pub(super) fn bounded<T: Send + 'static>(
    limit: Duration,
    work: impl FnOnce() -> Option<T> + Send + 'static,
) -> Option<T> {
    let (done, result) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("comms-endpoint".to_string())
        .spawn(move || {
            let _ = done.send(work());
        })
        .ok()?;
    result.recv_timeout(limit).ok().flatten()
}

/// Whether anything accepts a connection at the endpoint: a daemon that is serving, or one that
/// has stopped answering but not yet closed its listener. Sends nothing.
pub(crate) fn endpoint_accepts(socket_path: &Path) -> bool {
    #[cfg(unix)]
    {
        std::os::unix::net::UnixStream::connect(socket_path).is_ok()
    }
    #[cfg(windows)]
    {
        /// Every instance of the pipe is serving a client, which takes a server.
        const ERROR_PIPE_BUSY: i32 = 231;
        match std::fs::OpenOptions::new().read(true).write(true).open(socket_path) {
            Ok(_) => true,
            Err(error) => error.raw_os_error() == Some(ERROR_PIPE_BUSY),
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = socket_path;
        false
    }
}

/// One connect+ping attempt against the Unix socket. See [`probe_alive`](super::probe_alive) for
/// the retrying wrapper.
#[cfg(unix)]
pub(super) fn probe_once(socket_path: &Path) -> bool {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    let Ok(mut stream) = UnixStream::connect(socket_path) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));

    let body = match rmp_serde::to_vec_named(&super::super::protocol::CommsRequest::Ping) {
        Ok(b) => b,
        Err(_) => return false,
    };
    let len = match u32::try_from(body.len()) {
        Ok(l) => l,
        Err(_) => return false,
    };
    if stream.write_all(&len.to_be_bytes()).is_err() || stream.write_all(&body).is_err() {
        return false;
    }
    let mut prefix = [0u8; 4];
    stream.read_exact(&mut prefix).is_ok()
}

/// One connect+ping attempt against the Windows named pipe. See [`probe_alive`](super::probe_alive)
/// for the retrying wrapper. A `\\.\pipe\...` path opens as an ordinary [`std::fs::File`], so the
/// SAME framed `Ping` the Unix probe sends goes over it (u32-be length prefix + msgpack), and any
/// 4-byte response prefix read back means alive. A transient busy/not-found at open time means
/// not alive (the caller treats that as "reclaimable"). The read has no timeout of its own, so
/// the attempt runs under [`bounded`]: a daemon that accepts and never answers is not alive.
#[cfg(windows)]
pub(super) fn probe_once(socket_path: &Path) -> bool {
    use std::io::{Read, Write};

    const PING_TIMEOUT: Duration = Duration::from_millis(500);
    let endpoint = socket_path.to_path_buf();
    bounded(PING_TIMEOUT, move || {
        let mut stream = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&endpoint)
            .ok()?;
        let body = rmp_serde::to_vec_named(&super::super::protocol::CommsRequest::Ping).ok()?;
        let len = u32::try_from(body.len()).ok()?;
        stream.write_all(&len.to_be_bytes()).ok()?;
        stream.write_all(&body).ok()?;
        let mut prefix = [0u8; 4];
        stream.read_exact(&mut prefix).ok()
    })
    .is_some()
}
