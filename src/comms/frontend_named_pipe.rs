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
    use std::pin::Pin;
    use std::sync::Arc;
    use std::task::{Context, Poll};

    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
    use tokio::sync::watch;
    use tokio_util::bytes::{Bytes, BytesMut};
    use tokio_util::codec::{Decoder, Encoder, LengthDelimitedCodec};

    use crate::comms::daemon::Broker;
    use crate::comms::protocol::{CommsOut, CommsRequest};
    use crate::comms::transport::{CommsFrontend, CommsLink, MAX_FRAME_BYTES, PeerCred, serve_link};

    /// Read chunk size pulled from the pipe per `read_buf` call.
    const READ_CHUNK: usize = 8 * 1024;

    /// An `AsyncRead` + `AsyncWrite` stream that replays a consumed PREFIX before delegating to the
    /// inner stream.
    ///
    /// Windows named pipes have no non-destructive peek like Unix `MSG_PEEK`, so the accept loop
    /// discriminates a relay connection from a legacy comms link by *consuming* the first byte. This
    /// wrapper hands that byte back: the relay handshake ([`Broker::serve_relay_connection`]) then
    /// reads the full [`RELAY_MAGIC`](crate::comms::relay::RELAY_MAGIC) as if nothing had been taken.
    struct PrefixReader<S> {
        prefix: Vec<u8>,
        pos: usize,
        inner: S,
    }

    impl<S> PrefixReader<S> {
        fn new(prefix: Vec<u8>, inner: S) -> Self {
            Self { prefix, pos: 0, inner }
        }
    }

    impl<S: AsyncRead + Unpin> AsyncRead for PrefixReader<S> {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            if self.pos < self.prefix.len() {
                let remaining = &self.prefix[self.pos..];
                let take = remaining.len().min(buf.remaining());
                buf.put_slice(&remaining[..take]);
                self.pos += take;
                return Poll::Ready(Ok(()));
            }
            Pin::new(&mut self.inner).poll_read(cx, buf)
        }
    }

    impl<S: AsyncWrite + Unpin> AsyncWrite for PrefixReader<S> {
        fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
            Pin::new(&mut self.inner).poll_write(cx, buf)
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.inner).poll_flush(cx)
        }

        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.inner).poll_shutdown(cx)
        }
    }

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

        /// Wrap a connected pipe whose leading `prefix` byte(s) were already consumed by the accept
        /// loop's relay/legacy routing. Seeding the frame buffer replays them so the codec still sees
        /// the whole length-delimited frame.
        pub fn with_prefix(server: NamedPipeServer, prefix: &[u8]) -> Self {
            let mut link = Self::new(server);
            link.read_buf.extend_from_slice(prefix);
            link
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
                        let guard = broker.register_link();
                        let broker = broker.clone();
                        // ~keep Route a RELAY (rmcp) connection apart from a legacy comms link. Windows
                        // ~keep named pipes have no MSG_PEEK, so consume the first byte and replay it: a
                        // ~keep relay client writes RELAY_MAGIC (first byte 0x42), disjoint from a legacy
                        // ~keep length-delimited frame's first byte (0x00 for any body < 16 MiB). A relay
                        // ~keep session is hosted by the broker; everything else is a legacy comms link.
                        tokio::spawn(async move {
                            let mut connected = connected;
                            let first = match connected.read_u8().await {
                                Ok(byte) => byte,
                                // ~keep A connection that yields no first byte (EOF/error) has nothing to
                                // ~keep serve on either path, so drop it. The Unix peek instead routes such
                                // ~keep a connection to the legacy path, which then closes on its own EOF —
                                // ~keep same end state, one fewer hop.
                                Err(error) => {
                                    tracing::warn!(error = %error, "comms: pipe first-byte read failed");
                                    return;
                                }
                            };
                            if first == crate::comms::relay::RELAY_MAGIC[0] {
                                broker
                                    .serve_relay_connection(PrefixReader::new(vec![first], connected), guard)
                                    .await;
                            } else {
                                serve_link(broker, NamedPipeLink::with_prefix(connected, &[first]), guard).await;
                            }
                        });
                    }
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() {
                            break;
                        }
                    }
                }
            }
            broker.drain_links(crate::comms::daemon::DRAIN_GRACE).await;
            Ok(())
        }
    }
}

#[cfg(windows)]
pub use imp::{NamedPipeFrontend, NamedPipeLink};
