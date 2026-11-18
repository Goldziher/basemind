//! Transport abstraction for the broker: front-ends accept links, links carry frames.
//!
//! The front-ends ([`UdsFrontend`](super::frontend_uds) over a Unix domain socket, the named pipe
//! on Windows) decode the same [`CommsRequest`] and emit the same [`CommsOut`], so the broker is
//! transport-agnostic.
//!
//! ## Frame codec
//!
//! The Unix-socket link frames with [`tokio_util::codec::LengthDelimitedCodec`] (a `u32`
//! big-endian length prefix) and a msgpack body.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::Arc;

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::daemon::{Broker, LinkGuard, Session};
use super::protocol::{CommsOut, CommsRequest};

/// Maximum accepted frame size on the wire. A defensive cap so a malformed or hostile length
/// prefix cannot drive an unbounded allocation. 16 MiB comfortably exceeds any realistic
/// message body while bounding worst-case memory.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// How long one reply may take to write before the link is dropped.
pub const LINK_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Whether `buf` holds one whole length-delimited frame (`u32` big-endian length, then the body).
/// A declared length past [`MAX_FRAME_BYTES`] is an error.
///
/// A link hands its buffer to `LengthDelimitedCodec` only once this is true, because the codec
/// reserves the declared length as soon as it reads the prefix: a peer could otherwise make the
/// daemon allocate 16 MiB per link before sending a byte of body.
pub(crate) fn holds_whole_frame(buf: &[u8]) -> std::io::Result<bool> {
    let Some(&[a, b, c, d]) = buf.get(..4) else {
        return Ok(false);
    };
    let len = u32::from_be_bytes([a, b, c, d]) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame length {len} exceeds {MAX_FRAME_BYTES}"),
        ));
    }
    Ok(buf.len() - 4 >= len)
}

/// Requests a link reads ahead while one runs. Past it, or past [`MAX_READ_AHEAD_BYTES`], the link
/// is not read until the queue drains, so a client that pipelines more than this and then hangs up
/// is noticed only when it is reached.
const MAX_PIPELINED: usize = 64;

/// The encoded size of the requests a link reads ahead while one runs. The request that crosses it
/// is still queued, so a link holds at most this plus one frame of read-ahead.
const MAX_READ_AHEAD_BYTES: usize = MAX_FRAME_BYTES;

/// Counts the bytes written to it: the encoded size of a request, without a buffer.
struct ByteCount(usize);

impl std::io::Write for ByteCount {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 += buf.len();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The requests a link read while another ran, bounded by count and by encoded size.
#[derive(Default)]
struct ReadAhead {
    queued: VecDeque<(CommsRequest, usize)>,
    bytes: usize,
}

impl ReadAhead {
    fn has_room(&self) -> bool {
        self.queued.len() < MAX_PIPELINED && self.bytes < MAX_READ_AHEAD_BYTES
    }

    fn push(&mut self, req: CommsRequest) {
        let mut size = ByteCount(0);
        // A request that came off the wire encodes again; a failure only leaves it uncounted.
        let _ = rmp_serde::encode::write_named(&mut size, &req);
        self.bytes += size.0;
        self.queued.push_back((req, size.0));
    }

    fn pop(&mut self) -> Option<CommsRequest> {
        let (req, size) = self.queued.pop_front()?;
        self.bytes -= size;
        Some(req)
    }
}

/// Peer credentials of a connected link, used to reject cross-user connections. On platforms
/// without a peer-cred mechanism the fields are `None` and the daemon falls back to filesystem
/// permissions (the socket is created mode 0600).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PeerCred {
    /// The connecting process's uid, when the platform exposes it.
    pub uid: Option<u32>,
    /// The connecting process's pid, when the platform exposes it.
    pub pid: Option<u32>,
}

/// A bidirectional message link to one client. Implementors carry [`CommsRequest`]s inbound
/// and [`CommsOut`] frames (responses + notifications) outbound.
pub trait CommsLink: Send {
    /// Receive the next request, or `Ok(None)` when the peer closed the link cleanly.
    fn recv(&mut self) -> impl Future<Output = std::io::Result<Option<CommsRequest>>> + Send;

    /// Send one frame to the peer.
    fn send(&mut self, out: CommsOut) -> impl Future<Output = std::io::Result<()>> + Send;

    /// The peer's credentials, captured at accept time.
    fn peer_cred(&self) -> PeerCred;
}

/// A front-end owns a listening endpoint and drives the accept loop, handing each accepted
/// link to the broker until `shutdown` fires.
pub trait CommsFrontend: Send {
    /// Run the accept loop, serving `broker`, until `shutdown` is signalled.
    fn serve(
        self: Box<Self>,
        broker: Arc<Broker>,
        shutdown: watch::Receiver<bool>,
    ) -> impl Future<Output = std::io::Result<()>> + Send;
}

/// Drive one socket-style link to completion: pump requests through the broker, write each
/// response back, and drain the broker's per-link notification sink onto the same link.
///
/// Shared by the Unix-socket and Windows named-pipe front-ends — both frame [`CommsRequest`] /
/// [`CommsOut`] identically over an `AsyncRead + AsyncWrite` stream, so the only per-transport
/// difference is the concrete [`CommsLink`].
///
/// `guard` is the link's refcount with the idle reaper, and the caller must have taken it in the
/// accept loop *before* spawning this task — see [`Broker::register_link`]. Holding it here for the
/// whole session, and dropping it only when the loop exits, is what tells the reaper this daemon has
/// someone to serve even when the client is silent for minutes (a forwarded scan or git-history
/// build). Deregistration is the guard's `Drop`, so it survives any early `break` — and a panic.
pub(crate) async fn serve_link<L: CommsLink>(broker: Arc<Broker>, mut link: L, guard: LinkGuard) {
    let _link_guard = guard;
    let mut session = Session::default();
    let mut ahead = ReadAhead::default();
    // ~keep The read side ended. A client that half-closed its write side still reads, so what it
    // ~keep queued is served; the link ends once that is done or a reply cannot be sent.
    let mut hung_up = false;
    loop {
        let req = match ahead.pop() {
            Some(req) => req,
            None if hung_up => break,
            None => match link.recv().await {
                Ok(Some(req)) => req,
                Ok(None) | Err(_) => break,
            },
        };
        // ~keep A waiting inbox read must know when its client hung up, so the link is read while a
        // ~keep request runs, pipelined requests included: an end of stream cancels `gone` for the
        // ~keep request running then, and each pipelined request waits its turn, up to a bound.
        let gone = CancellationToken::new();
        let out = {
            let handled = broker.handle_framed(req, &mut session, &gone);
            tokio::pin!(handled);
            loop {
                tokio::select! {
                    out = &mut handled => break out,
                    inbound = link.recv(), if ahead.has_room() && !hung_up => match inbound {
                        Ok(Some(next)) => ahead.push(next),
                        Ok(None) | Err(_) => {
                            hung_up = true;
                            gone.cancel();
                        }
                    },
                }
            }
        };
        // A client that stopped reading would park this task, its link guard and the drain on a
        // full socket buffer for good. ~keep
        if !matches!(
            tokio::time::timeout(LINK_WRITE_TIMEOUT, link.send(out)).await,
            Ok(Ok(()))
        ) {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comms::v2::api::{PostBody, PostTarget, ThreadId};

    fn post(body_bytes: usize) -> CommsRequest {
        CommsRequest::AgentPost {
            sub: None,
            body: PostBody {
                target: PostTarget::Thread(ThreadId::parse("t-abcdef").expect("thread")),
                body: "x".repeat(body_bytes),
            },
        }
    }

    /// A link whose client stopped reading must end once a reply cannot be written in time,
    /// releasing its guard so the daemon can idle out and drain.
    #[tokio::test(start_paused = true)]
    async fn a_reply_that_cannot_be_written_ends_the_link() {
        struct Stuck(Option<CommsRequest>);
        impl CommsLink for Stuck {
            async fn recv(&mut self) -> std::io::Result<Option<CommsRequest>> {
                match self.0.take() {
                    Some(req) => Ok(Some(req)),
                    None => std::future::pending().await,
                }
            }
            async fn send(&mut self, _out: CommsOut) -> std::io::Result<()> {
                std::future::pending().await
            }
            fn peer_cred(&self) -> PeerCred {
                PeerCred::default()
            }
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let broker = Arc::new(Broker::new(crate::comms::agents_hub::AgentHub::open(dir.path())));
        let guard = broker.register_link();
        tokio::time::timeout(
            LINK_WRITE_TIMEOUT * 2,
            serve_link(Arc::clone(&broker), Stuck(Some(CommsRequest::Ping)), guard),
        )
        .await
        .expect("the link ends when its reply cannot be written");
    }

    #[test]
    fn a_frame_is_whole_only_once_its_body_has_arrived() {
        let mut buf = 5u32.to_be_bytes().to_vec();
        assert!(!holds_whole_frame(&buf[..2]).expect("short prefix"));
        assert!(!holds_whole_frame(&buf).expect("no body yet"));
        buf.extend_from_slice(b"hello");
        assert!(holds_whole_frame(&buf).expect("whole"));
        let huge = u32::try_from(MAX_FRAME_BYTES + 1).expect("fits").to_be_bytes();
        assert!(
            holds_whole_frame(&huge).is_err(),
            "an over-long length is refused at once"
        );
    }

    /// The link must not let the codec reserve a declared length before the body arrives.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_declared_length_is_not_allocated_before_its_body_arrives() {
        use tokio::io::AsyncWriteExt;

        let (mut client, server) = tokio::net::UnixStream::pair().expect("pair");
        let mut link = crate::comms::frontend_uds::UdsLink::for_test(server);
        let declared = u32::try_from(MAX_FRAME_BYTES).expect("fits");
        client.write_all(&declared.to_be_bytes()).await.expect("prefix");
        client.write_all(&[0u8; 16]).await.expect("a little body");
        let pending = tokio::time::timeout(std::time::Duration::from_millis(200), link.recv()).await;
        assert!(pending.is_err(), "the frame is not whole yet");
        assert!(
            link.buffered_capacity() < 1024 * 1024,
            "{} bytes reserved for a frame whose body has not arrived",
            link.buffered_capacity()
        );
    }

    #[test]
    fn read_ahead_is_bounded_by_encoded_size() {
        let mut ahead = ReadAhead::default();
        ahead.push(post(MAX_FRAME_BYTES / 2));
        assert!(ahead.has_room(), "half a frame queued");
        ahead.push(post(MAX_FRAME_BYTES / 2));
        assert!(!ahead.has_room(), "a frame's worth queued: {} bytes", ahead.bytes);
        assert!(ahead.pop().is_some());
        assert!(ahead.has_room(), "room again once one is served");
        assert!(ahead.pop().is_some());
        assert_eq!(ahead.bytes, 0);
    }

    #[test]
    fn read_ahead_is_bounded_by_count() {
        let mut ahead = ReadAhead::default();
        for _ in 0..MAX_PIPELINED {
            assert!(ahead.has_room());
            ahead.push(CommsRequest::Ping);
        }
        assert!(!ahead.has_room());
    }
}
