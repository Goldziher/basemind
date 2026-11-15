//! One-shot blocking exchanges with a daemon's endpoint: what the liveness probe, the status read
//! and the stop request are built on.
//!
//! On Unix every read and write takes a timeout. A Windows pipe opened as a file takes none, so
//! there an exchange runs on a thread of its own under [`bounded`], and the caller gives up on it
//! at the limit. Two things follow that this module keeps in hand:
//!
//! - A pipe whose every instance is serving another client refuses the open with
//!   `ERROR_PIPE_BUSY`. That is a daemon at work, the opposite of a silent one: the open is
//!   retried for a bounded time, and a pipe still busy at the end counts as present.
//! - A call given up on leaves its thread blocked in the read, with the pipe open, until the
//!   daemon answers, closes the pipe or dies. No new exchange is started with an endpoint while
//!   one given up on is still blocked there, so a daemon that accepts and never answers holds one
//!   thread and one pipe handle of this process, however often it is probed. (Callers that were
//!   already waiting on the same endpoint at that moment may each leave one more.)

use std::path::Path;
use std::time::Duration;

use super::super::protocol::{CommsOut, CommsResponse};

/// Send one framed request `body` and decode the one framed reply, giving up after `timeout`:
/// per read and write on Unix, for the whole exchange on Windows.
pub(super) fn exchange(socket_path: &Path, body: Vec<u8>, timeout: Duration) -> Option<CommsResponse> {
    #[cfg(windows)]
    {
        let endpoint = socket_path.to_path_buf();
        bounded(socket_path, timeout, move || {
            let Pipe::Open(stream) = open_pipe(&endpoint, timeout / 2) else {
                return None;
            };
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

/// Endpoints with a [`bounded`] call that was given up on and is still blocked, and how many.
#[cfg(any(windows, test))]
static ABANDONED: std::sync::Mutex<std::collections::BTreeMap<std::path::PathBuf, usize>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

/// Run blocking `work` against `endpoint` on a thread of its own and wait at most `limit` for
/// what it returns. For I/O that takes no timeout: past the limit the caller moves on with `None`,
/// and the thread ends whenever its call returns. While a call given up on is still blocked,
/// further calls for the same endpoint return `None` at once without starting `work`, which is
/// what bounds the threads and handles one unanswering endpoint can hold.
#[cfg(any(windows, test))]
pub(super) fn bounded<T: Send + 'static>(
    endpoint: &Path,
    limit: Duration,
    work: impl FnOnce() -> Option<T> + Send + 'static,
) -> Option<T> {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, PoisonError};

    let abandoned = || ABANDONED.lock().unwrap_or_else(PoisonError::into_inner);
    if abandoned().contains_key(endpoint) {
        return None;
    }
    // Both flags are read and written only under the `ABANDONED` lock, which is what lets the
    // thread and the caller agree on who saw the other first.
    let finished = Arc::new(AtomicBool::new(false));
    let given_up = Arc::new(AtomicBool::new(false));
    let (done, result) = std::sync::mpsc::channel();
    let (thread_finished, thread_given_up, key) = (finished.clone(), given_up.clone(), endpoint.to_path_buf());
    std::thread::Builder::new()
        .name("comms-endpoint".to_string())
        .spawn(move || {
            let outcome = work();
            {
                let mut abandoned = ABANDONED.lock().unwrap_or_else(PoisonError::into_inner);
                thread_finished.store(true, Ordering::Relaxed);
                if thread_given_up.load(Ordering::Relaxed)
                    && let Some(count) = abandoned.get_mut(&key)
                {
                    *count -= 1;
                    if *count == 0 {
                        abandoned.remove(&key);
                    }
                }
            }
            let _ = done.send(outcome);
        })
        .ok()?;
    if let Ok(outcome) = result.recv_timeout(limit) {
        return outcome;
    }
    {
        let mut abandoned = abandoned();
        if !finished.load(Ordering::Relaxed) {
            given_up.store(true, Ordering::Relaxed);
            *abandoned.entry(endpoint.to_path_buf()).or_insert(0) += 1;
            return None;
        }
    }
    // The work returned as the limit ran out; its result is on its way.
    result.recv().ok().flatten()
}

/// What opening the daemon's pipe found.
#[cfg(windows)]
enum Pipe {
    /// A free instance, connected.
    Open(std::fs::File),
    /// The pipe exists and every instance of it stayed busy for the whole wait: a daemon is
    /// there, serving other clients.
    Busy,
    /// No such pipe, or it cannot be opened.
    Absent,
}

/// Open the daemon's pipe, retrying for up to `wait` while every instance is busy.
#[cfg(windows)]
fn open_pipe(socket_path: &Path, wait: Duration) -> Pipe {
    /// Every instance of the pipe is serving a client, which takes a server.
    const ERROR_PIPE_BUSY: i32 = 231;
    const RETRY_INTERVAL: Duration = Duration::from_millis(50);

    let deadline = std::time::Instant::now() + wait;
    loop {
        match std::fs::OpenOptions::new().read(true).write(true).open(socket_path) {
            Ok(pipe) => return Pipe::Open(pipe),
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                if std::time::Instant::now() >= deadline {
                    return Pipe::Busy;
                }
                std::thread::sleep(RETRY_INTERVAL);
            }
            Err(_) => return Pipe::Absent,
        }
    }
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
        !matches!(open_pipe(socket_path, Duration::ZERO), Pipe::Absent)
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
/// 4-byte response prefix read back means alive. The read has no timeout of its own, so the
/// attempt runs under [`bounded`]: a daemon that accepts and never answers is not alive. A pipe
/// that is not there is not alive either. One whose every instance stays busy is: something is
/// serving those clients.
#[cfg(windows)]
pub(super) fn probe_once(socket_path: &Path) -> bool {
    use std::io::{Read, Write};

    const PING_TIMEOUT: Duration = Duration::from_millis(500);
    let endpoint = socket_path.to_path_buf();
    bounded(socket_path, PING_TIMEOUT, move || {
        let mut stream = match open_pipe(&endpoint, PING_TIMEOUT / 2) {
            Pipe::Open(stream) => stream,
            Pipe::Busy => return Some(()),
            Pipe::Absent => return None,
        };
        let body = rmp_serde::to_vec_named(&super::super::protocol::CommsRequest::Ping).ok()?;
        let len = u32::try_from(body.len()).ok()?;
        stream.write_all(&len.to_be_bytes()).ok()?;
        stream.write_all(&body).ok()?;
        let mut prefix = [0u8; 4];
        stream.read_exact(&mut prefix).ok()
    })
    .is_some()
}
