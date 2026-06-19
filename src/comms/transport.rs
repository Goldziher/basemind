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

use std::future::Future;
use std::sync::Arc;

use tokio::sync::watch;

use super::daemon::Broker;
use super::protocol::{CommsOut, CommsRequest};

/// Maximum accepted frame size on the wire. A defensive cap so a malformed or hostile length
/// prefix cannot drive an unbounded allocation. 16 MiB comfortably exceeds any realistic
/// message body while bounding worst-case memory.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

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
