//! Windows named-pipe front-end + link — the production local IPC path on Windows.
//!
//! The mirror image of [`frontend_uds`](super::frontend_uds): frames ride a
//! [`LengthDelimitedCodec`](tokio_util::codec::LengthDelimitedCodec) (`u32` big-endian length
//! prefix) carrying a msgpack [`CommsRequest`](super::protocol::CommsRequest) /
//! [`CommsOut`](super::protocol::CommsOut) body over a Windows
//! `NamedPipeServer`, which is
//! `AsyncRead + AsyncWrite`. The framing, max-frame cap, and broker pump loop
//! (`serve_link`) are byte-for-byte identical to the Unix path.
//!
//! ## Access control
//!
//! Windows has no `SO_PEERCRED` analogue cheap enough to read inline, so `NamedPipeLink`
//! reports an empty `PeerCred`. The pipe is user-scoped by name
//! (`\\.\pipe\basemind-comms-<USERNAME>`, see `singleton::comms_socket_path`) and the default
//! pipe DACL grants access to the creating user, so cross-user connections are refused by the
//! OS at connect time rather than by a uid comparison in the accept loop.

#[cfg(windows)]
mod imp {
    use std::ffi::OsString;
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
    use tokio::sync::watch;
    use tokio_util::bytes::{Bytes, BytesMut};
    use tokio_util::codec::{Decoder, Encoder, LengthDelimitedCodec};

    use crate::comms::daemon::Broker;
    use crate::comms::protocol::{CommsOut, CommsRequest};
    use crate::comms::transport::{CommsFrontend, CommsLink, MAX_FRAME_BYTES, PeerCred, serve_link};

    /// Read chunk size pulled from the pipe per `read_buf` call.
    const READ_CHUNK: usize = 8 * 1024;

    /// A framed named-pipe link to one client.
    ///
    /// Drives [`LengthDelimitedCodec`] directly via its [`Decoder`] / [`Encoder`] impls over an
    /// in-memory [`BytesMut`] read buffer pumped by tokio's `AsyncReadExt` / `AsyncWriteExt`
    /// (the `io-util` feature) — identical to the Unix [`UdsLink`](super::super::frontend_uds).
    pub struct NamedPipeLink {
        server: NamedPipeServer,
        codec: LengthDelimitedCodec,
        read_buf: BytesMut,
    }

    impl NamedPipeLink {
        /// Wrap a connected [`NamedPipeServer`] instance (one whose `connect()` has resolved).
        pub fn new(server: NamedPipeServer) -> Self {
            let mut codec = LengthDelimitedCodec::new();
            codec.set_max_frame_length(MAX_FRAME_BYTES);
            Self {
                server,
                codec,
                read_buf: BytesMut::with_capacity(READ_CHUNK),
            }
        }
    }

    impl CommsLink for NamedPipeLink {
        async fn recv(&mut self) -> std::io::Result<Option<CommsRequest>> {
            loop {
                if let Some(frame) = self.codec.decode(&mut self.read_buf)? {
                    let req = rmp_serde::from_slice(&frame)
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
                    return Ok(Some(req));
                }
                let n = self.server.read_buf(&mut self.read_buf).await?;
                if n == 0 {
                    if self.read_buf.is_empty() {
                        return Ok(None);
                    }
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "peer closed mid-frame",
                    ));
                }
            }
        }

        async fn send(&mut self, out: CommsOut) -> std::io::Result<()> {
            let body = rmp_serde::to_vec_named(&out)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
            let mut framed = BytesMut::new();
            self.codec.encode(Bytes::from(body), &mut framed)?;
            self.server.write_all(&framed).await?;
            self.server.flush().await
        }

        fn peer_cred(&self) -> PeerCred {
            PeerCred::default()
        }
    }

    /// The named-pipe front-end: owns the first server instance (created as the singleton lock)
    /// and runs the connect/accept loop, minting the next instance before serving each client so
    /// no client is refused during the hand-off.
    pub struct NamedPipeFrontend {
        first: NamedPipeServer,
        pipe_name: OsString,
    }

    impl NamedPipeFrontend {
        /// Wrap the already-created first pipe instance. The creation of that first instance with
        /// `first_pipe_instance(true)` IS the singleton lock (see `singleton::bind_listener`), so
        /// this constructor takes the server instance rather than a name to avoid a TOCTOU window.
        pub fn from_first_instance(first: NamedPipeServer, pipe_name: OsString) -> Self {
            Self { first, pipe_name }
        }
    }

    impl CommsFrontend for NamedPipeFrontend {
        async fn serve(
            self: Box<Self>,
            broker: Arc<Broker>,
            mut shutdown: watch::Receiver<bool>,
        ) -> std::io::Result<()> {
            broker.mark_active().await;
            let mut server = self.first;
            loop {
                tokio::select! {
                    conn = server.connect() => {
                        if let Err(e) = conn {
                            tracing::warn!(error = %e, "comms: pipe connect failed");
                            server = ServerOptions::new().create(&self.pipe_name)?;
                            continue;
                        }
                        let connected = server;
                        server = ServerOptions::new().create(&self.pipe_name)?;
                        // Counted at accept time, before the spawn — same ordering as the UDS
                        // front-end, and for the same reason (see `Broker::register_link`).
                        let guard = broker.register_link();
                        tokio::spawn(serve_link(broker.clone(), NamedPipeLink::new(connected), guard));
                    }
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() {
                            break;
                        }
                    }
                }
            }
            // Finish the links we already accepted rather than tearing them mid-request. There is no
            // path to unlink here — a named pipe has no filesystem entry to remove; dropping the
            // listener instance is what stops new clients from connecting.
            broker.drain_links(crate::comms::daemon::DRAIN_GRACE).await;
            Ok(())
        }
    }
}

#[cfg(windows)]
pub use imp::{NamedPipeFrontend, NamedPipeLink};
