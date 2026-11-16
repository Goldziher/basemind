//! Transport abstraction for the broker: front-ends accept links, links carry frames.
//!
//! Two implementations live alongside this module: [`UdsFrontend`](super::frontend_uds) over a
//! Unix domain socket (the production IPC path) and
//! [`InProcFrontend`](super::frontend_inproc) over tokio mpsc channels (for same-process
//! embedding and tests). Both decode the same [`CommsRequest`] and emit the same
//! [`CommsOut`], so the broker is transport-agnostic.
//!
//! ## Frame codec
//!
//! The Unix-socket link frames with [`tokio_util::codec::LengthDelimitedCodec`] (a `u32`
//! big-endian length prefix) and a msgpack body. The in-process link skips framing entirely
//! and moves owned values across channels.

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

/// Requests a link reads ahead while one runs. Past it the link is not read until the queue drains,
/// so a client that pipelines more than this and then hangs up is noticed only when it is reached.
const MAX_PIPELINED: usize = 64;

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
    let mut queued: VecDeque<CommsRequest> = VecDeque::new();
    loop {
        let req = match queued.pop_front() {
            Some(req) => req,
            None => match link.recv().await {
                Ok(Some(req)) => req,
                Ok(None) | Err(_) => break,
            },
        };
        // ~keep A waiting inbox read must know when its client hung up, so the link is read while a
        // ~keep request runs, pipelined requests included: an end of stream cancels `gone`, and each
        // ~keep pipelined request waits its turn in the queue, up to a bound.
        let gone = CancellationToken::new();
        let mut hung_up = false;
        let out = {
            let handled = broker.handle_framed(req, &mut session, &gone);
            tokio::pin!(handled);
            loop {
                tokio::select! {
                    out = &mut handled => break out,
                    inbound = link.recv(), if queued.len() < MAX_PIPELINED && !hung_up => match inbound {
                        Ok(Some(next)) => queued.push_back(next),
                        Ok(None) | Err(_) => {
                            hung_up = true;
                            gone.cancel();
                        }
                    },
                }
            }
        };
        if hung_up || link.send(out).await.is_err() {
            break;
        }
    }
}
