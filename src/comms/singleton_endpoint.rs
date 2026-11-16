//! One-shot blocking exchanges with a daemon's endpoint: what the liveness probe, the status read
//! and the stop request are built on.
//!
//! Every exchange is bounded, and none is skipped because of an earlier one. On Unix each read and
//! write takes a timeout. A Windows pipe opened as a file takes none, so there the exchange is
//! done with tokio's pipe client under a timeout, on a short-lived thread with a runtime of its
//! own (the callers are synchronous, and some run inside a runtime). Giving up drops the client,
//! which cancels the read: nothing outlives the call, and a daemon that accepted one exchange and
//! never answered it is asked again by the next.
//!
//! What could not be asked is not silence. A probe says [`Liveness::Unknown`] when the exchange
//! never started, and a caller must neither signal nor spawn on that. A pipe whose every instance
//! is serving another client refuses the open with `ERROR_PIPE_BUSY`: that is a daemon at work,
//! so the open is retried for a bounded time and a pipe still busy at the end counts as present.

use std::path::Path;
use std::time::Duration;

#[cfg(any(unix, windows))]
use super::super::protocol::CommsRequest;
use super::super::protocol::{CommsOut, CommsResponse};

/// How long one `Ping` waits for its answer.
#[cfg(any(unix, windows))]
const PING_TIMEOUT: Duration = Duration::from_millis(500);

/// What a `Ping` established about the daemon at an endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Liveness {
    /// It answered, or its pipe is there with every instance busy serving other clients.
    Answers,
    /// It was asked and did not answer in time, or nothing is listening.
    Silent,
    /// It could not be asked: the exchange never started. That says nothing about the daemon, so
    /// no signal and no spawn may follow from it.
    Unknown,
}

/// Send one framed request `body` and decode the one framed reply, giving up after `timeout`:
/// per read and write on Unix, per step of the exchange on Windows. `None` covers a daemon that
/// did not answer and one that could not be asked; a caller that has to tell them apart pings
/// ([`probe_once`]).
pub(super) fn exchange(socket_path: &Path, body: Vec<u8>, timeout: Duration) -> Option<CommsResponse> {
    #[cfg(windows)]
    {
        pipe::hosted(async {
            let pipe::Opened::Open(mut client) = pipe::open(socket_path, timeout / 2).await else {
                return None;
            };
            let reply = tokio::time::timeout(timeout, pipe::converse(&mut client, &body)).await;
            decode_reply(&reply.ok()??)
        })
        .flatten()
    }
    #[cfg(unix)]
    {
        use std::io::{Read as _, Write as _};

        let mut stream = std::os::unix::net::UnixStream::connect(socket_path).ok()?;
        let _ = stream.set_read_timeout(Some(timeout));
        let _ = stream.set_write_timeout(Some(timeout));
        stream.write_all(&frame(&body)?).ok()?;
        let mut prefix = [0u8; 4];
        stream.read_exact(&mut prefix).ok()?;
        let mut reply = vec![0u8; reply_len(prefix)?];
        stream.read_exact(&mut reply).ok()?;
        decode_reply(&reply)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (socket_path, body, timeout);
        None
    }
}

/// `body` behind its length prefix, as the daemon's front-end frames a request.
#[cfg(any(unix, windows))]
fn frame(body: &[u8]) -> Option<Vec<u8>> {
    let len = u32::try_from(body.len()).ok()?;
    let mut framed = Vec::with_capacity(body.len() + 4);
    framed.extend_from_slice(&len.to_be_bytes());
    framed.extend_from_slice(body);
    Some(framed)
}

/// The length a reply's prefix announces, when it is within the 64 KiB a one-shot reply may take.
#[cfg(any(unix, windows))]
fn reply_len(prefix: [u8; 4]) -> Option<usize> {
    const MAX_REPLY_BYTES: usize = 64 * 1024;
    let len = u32::from_be_bytes(prefix) as usize;
    (len <= MAX_REPLY_BYTES).then_some(len)
}

#[cfg(any(unix, windows))]
fn decode_reply(reply: &[u8]) -> Option<CommsResponse> {
    match rmp_serde::from_slice::<CommsOut>(reply).ok()? {
        CommsOut::Response(resp) | CommsOut::Reply { response: resp, .. } => Some(resp),
        CommsOut::Notification(_) => None,
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
        match std::fs::OpenOptions::new().read(true).write(true).open(socket_path) {
            Ok(_) => true,
            Err(error) => error.raw_os_error() == Some(pipe::ERROR_PIPE_BUSY),
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = socket_path;
        false
    }
}

/// One connect+ping attempt: the framed `Ping` every front-end answers, where any 4-byte reply
/// prefix read back means the daemon answers. See [`probe_liveness`](super::probe_liveness) for
/// the retrying wrapper.
#[cfg(any(unix, windows))]
pub(super) fn probe_once(socket_path: &Path) -> Liveness {
    let Some(ping) = rmp_serde::to_vec_named(&CommsRequest::Ping)
        .ok()
        .and_then(|body| frame(&body))
    else {
        return Liveness::Unknown;
    };
    #[cfg(unix)]
    {
        use std::io::{Read as _, Write as _};

        let Ok(mut stream) = std::os::unix::net::UnixStream::connect(socket_path) else {
            return Liveness::Silent;
        };
        let _ = stream.set_read_timeout(Some(PING_TIMEOUT));
        let _ = stream.set_write_timeout(Some(PING_TIMEOUT));
        let mut prefix = [0u8; 4];
        if stream.write_all(&ping).is_ok() && stream.read_exact(&mut prefix).is_ok() {
            Liveness::Answers
        } else {
            Liveness::Silent
        }
    }
    #[cfg(windows)]
    {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        pipe::hosted(async {
            let mut client = match pipe::open(socket_path, PING_TIMEOUT / 2).await {
                pipe::Opened::Open(client) => client,
                pipe::Opened::Busy => return Liveness::Answers,
                pipe::Opened::Absent => return Liveness::Silent,
            };
            let answered = tokio::time::timeout(PING_TIMEOUT, async {
                client.write_all(&ping).await.ok()?;
                let mut prefix = [0u8; 4];
                client.read_exact(&mut prefix).await.ok()
            })
            .await;
            match answered {
                Ok(Some(_)) => Liveness::Answers,
                Ok(None) | Err(_) => Liveness::Silent,
            }
        })
        .unwrap_or(Liveness::Unknown)
    }
}

/// No endpoint exists on this platform, so nothing can be asked.
#[cfg(not(any(unix, windows)))]
pub(super) fn probe_once(_socket_path: &Path) -> Liveness {
    Liveness::Unknown
}

/// The Windows side: a named pipe driven by tokio, so that a read can be given up on.
#[cfg(windows)]
mod pipe {
    use std::future::Future;
    use std::path::Path;
    use std::time::Duration;

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};

    /// Every instance of the pipe is serving a client, which takes a server.
    pub(super) const ERROR_PIPE_BUSY: i32 = 231;

    /// What opening the daemon's pipe found.
    pub(super) enum Opened {
        /// A free instance, connected.
        Open(NamedPipeClient),
        /// The pipe exists and every instance of it stayed busy for the whole wait: a daemon is
        /// there, serving other clients.
        Busy,
        /// No such pipe, or it cannot be opened.
        Absent,
    }

    /// Open the daemon's pipe, retrying for up to `wait` while every instance is busy.
    pub(super) async fn open(socket_path: &Path, wait: Duration) -> Opened {
        const RETRY_INTERVAL: Duration = Duration::from_millis(50);

        let deadline = std::time::Instant::now() + wait;
        loop {
            match ClientOptions::new().open(socket_path) {
                Ok(client) => return Opened::Open(client),
                Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                    if std::time::Instant::now() >= deadline {
                        return Opened::Busy;
                    }
                    tokio::time::sleep(RETRY_INTERVAL).await;
                }
                Err(_) => return Opened::Absent,
            }
        }
    }

    /// Write the framed request and read the framed reply's body. The caller bounds it.
    pub(super) async fn converse(client: &mut NamedPipeClient, body: &[u8]) -> Option<Vec<u8>> {
        client.write_all(&super::frame(body)?).await.ok()?;
        let mut prefix = [0u8; 4];
        client.read_exact(&mut prefix).await.ok()?;
        let mut reply = vec![0u8; super::reply_len(prefix)?];
        client.read_exact(&mut reply).await.ok()?;
        Some(reply)
    }

    /// Run `work` to its end on a thread of its own with a runtime of its own, and wait for it.
    /// `work` must bound every wait it makes; the thread then always ends with the call. `None`
    /// when the thread or its runtime could not be started, so that nothing was asked.
    pub(super) fn hosted<T: Send>(work: impl Future<Output = T> + Send) -> Option<T> {
        std::thread::scope(|scope| {
            std::thread::Builder::new()
                .name("comms-endpoint".to_string())
                .spawn_scoped(scope, || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .ok()?;
                    Some(runtime.block_on(work))
                })
                .ok()?
                .join()
                .ok()?
        })
    }
}

#[cfg(all(test, windows))]
#[path = "singleton_endpoint_windows_tests.rs"]
mod windows_tests;
