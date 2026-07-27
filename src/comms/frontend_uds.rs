//! Unix-domain-socket front-end + link — the production local IPC path.
//!
//! Frames ride a [`LengthDelimitedCodec`](tokio_util::codec::LengthDelimitedCodec) (`u32`
//! big-endian length prefix) carrying a msgpack
//! [`CommsRequest`](super::protocol::CommsRequest) / [`CommsOut`](super::protocol::CommsOut)
//! body. At accept time the link captures the peer's credentials and the daemon rejects any
//! connection
//! whose uid differs from its own — defence in depth on top of the socket's mode-0600
//! permissions.
//!
//! ## Peer credentials without `libc`
//!
//! basemind does not depend on `libc`, so this module declares the two C entry points it needs
//! itself (`getuid`, `getsockopt`). They are part of the platform libc that is always linked
//! on Unix, so the `extern "C"` declarations resolve at link time. Each call site carries a
//! `// SAFETY:` note. On non-Unix targets the front-end is unavailable; Windows production IPC
//! lives in [`frontend_named_pipe`](super::frontend_named_pipe).

#[cfg(unix)]
mod imp {
    use std::os::fd::AsRawFd;
    use std::path::PathBuf;
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{UnixListener, UnixStream};
    use tokio::sync::watch;
    use tokio_util::bytes::{Bytes, BytesMut};
    use tokio_util::codec::{Decoder, Encoder, LengthDelimitedCodec};

    use crate::comms::daemon::Broker;
    use crate::comms::protocol::{CommsOut, CommsRequest};
    use crate::comms::transport::{CommsFrontend, CommsLink, MAX_FRAME_BYTES, PeerCred, serve_link};

    /// Read chunk size pulled from the socket per `read_buf` call.
    const READ_CHUNK: usize = 8 * 1024;

    /// A framed Unix-socket link to one client.
    ///
    /// We drive [`LengthDelimitedCodec`] directly via its [`Decoder`] / [`Encoder`] impls over
    /// an in-memory [`BytesMut`] read buffer, pumped by tokio's `AsyncReadExt` / `AsyncWriteExt`
    /// (the `io-util` feature). This honours the length-delimited framing contract without
    /// pulling the `futures` Stream/Sink layer (not in the `comms` feature set).
    pub struct UdsLink {
        stream: UnixStream,
        codec: LengthDelimitedCodec,
        read_buf: BytesMut,
        peer: PeerCred,
    }

    impl UdsLink {
        fn new(stream: UnixStream, peer: PeerCred) -> Self {
            let mut codec = LengthDelimitedCodec::new();
            codec.set_max_frame_length(MAX_FRAME_BYTES);
            Self {
                stream,
                codec,
                read_buf: BytesMut::with_capacity(READ_CHUNK),
                peer,
            }
        }
    }

    impl CommsLink for UdsLink {
        async fn recv(&mut self) -> std::io::Result<Option<CommsRequest>> {
            loop {
                if let Some(frame) = self.codec.decode(&mut self.read_buf)? {
                    let req = rmp_serde::from_slice(&frame)
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
                    return Ok(Some(req));
                }
                let n = self.stream.read_buf(&mut self.read_buf).await?;
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
            self.stream.write_all(&framed).await?;
            self.stream.flush().await
        }

        fn peer_cred(&self) -> PeerCred {
            self.peer
        }
    }

    /// The Unix-socket front-end: binds (or adopts) a listener and runs the accept loop.
    pub struct UdsFrontend {
        listener: UnixListener,
        socket_path: PathBuf,
    }

    impl UdsFrontend {
        /// Wrap an already-bound listener. The bind itself is the singleton lock (see
        /// `singleton::bind_listener`), so this constructor takes the listener rather than a
        /// path to avoid a TOCTOU window.
        pub fn from_listener(listener: UnixListener, socket_path: PathBuf) -> Self {
            Self { listener, socket_path }
        }
    }

    impl CommsFrontend for UdsFrontend {
        async fn serve(
            self: Box<Self>,
            broker: Arc<Broker>,
            mut shutdown: watch::Receiver<bool>,
        ) -> std::io::Result<()> {
            broker.mark_active().await;
            let my_uid = super::daemon_uid();
            loop {
                tokio::select! {
                    accepted = self.listener.accept() => {
                        let (stream, _addr) = match accepted {
                            Ok(pair) => pair,
                            Err(e) => {
                                tracing::warn!(error = %e, "comms: accept failed");
                                continue;
                            }
                        };
                        let peer = peer_cred_of(&stream);
                        if let Some(uid) = peer.uid && uid != my_uid {
                            tracing::warn!(
                                peer_uid = uid,
                                daemon_uid = my_uid,
                                "comms: rejecting cross-user connection"
                            );
                            continue;
                        }
                        let guard = broker.register_link();
                        let broker = broker.clone();
                        // ~keep Peek the first byte INSIDE the spawned task, never in the accept loop.
                        // ~keep `peek_first_byte` awaits the client's first byte; doing it inline here
                        // ~keep would park the whole accept loop on one slow/silent connection, leaving
                        // ~keep every other client unaccepted in the backlog — including ensure_daemon's
                        // ~keep readiness probe, whose timeout then reports a healthy daemon as unreachable
                        // ~keep (the SpawnTimeout flake). The Windows named-pipe front-end reads its first
                        // ~keep byte in-task for the same reason.
                        //
                        // ~keep Route a RELAY (rmcp) connection apart from a legacy comms link. A relay
                        // ~keep client writes RELAY_MAGIC first, whose first byte (0x42) is disjoint from a
                        // ~keep legacy length-delimited frame's first byte (0x00 for any body < 16 MiB), so
                        // ~keep peeking one byte discriminates without consuming it. A relay session is
                        // ~keep hosted by the broker (shared read stack + rmcp router); everything else is a
                        // ~keep legacy comms link, byte-for-byte the existing path.
                        tokio::spawn(async move {
                            let is_relay =
                                peek_first_byte(&stream).await == Some(crate::comms::relay::RELAY_MAGIC[0]);
                            if is_relay {
                                broker.serve_relay_connection(stream, guard).await;
                            } else {
                                serve_link(broker, UdsLink::new(stream, peer), guard).await;
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
            let _ = std::fs::remove_file(&self.socket_path);
            broker.drain_links(crate::comms::daemon::DRAIN_GRACE).await;
            Ok(())
        }
    }

    /// Read the peer's credentials from a connected stream. Best-effort: returns an empty
    /// [`PeerCred`] when the platform call fails, in which case the daemon relies on the
    /// socket's filesystem permissions.
    fn peer_cred_of(stream: &UnixStream) -> PeerCred {
        super::peer_cred_from_fd(stream.as_raw_fd())
    }

    /// Peek the first byte of a freshly accepted stream WITHOUT consuming it (via `MSG_PEEK`), so the
    /// accept loop can route a relay connection (first byte [`RELAY_MAGIC`](crate::comms::relay::RELAY_MAGIC)`[0]`
    /// = `0x42`) apart from a legacy comms link (a length-delimited frame under 16 MiB starts `0x00`).
    /// Returns `None` on EOF or any error, which the caller treats as "not a relay" (the legacy path,
    /// which then fails the frame decode or serves normally).
    async fn peek_first_byte(stream: &UnixStream) -> Option<u8> {
        use tokio::io::Interest;
        loop {
            stream.readable().await.ok()?;
            let mut byte = 0u8;
            let peeked = stream.try_io(Interest::READABLE, || {
                // SAFETY: `stream`'s fd is a live connected socket for the duration of this call;
                // ~keep `byte` is a valid 1-byte writable buffer; `MSG_PEEK` leaves the datum queued so
                // ~keep the subsequent real read still sees it. `recv` returns the byte count or -1.
                let n = unsafe {
                    super::recv(
                        stream.as_raw_fd(),
                        std::ptr::from_mut(&mut byte).cast(),
                        1,
                        super::MSG_PEEK,
                    )
                };
                if n < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(n)
                }
            });
            match peeked {
                Ok(0) => return None,
                Ok(_) => return Some(byte),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(_) => return None,
            }
        }
    }
}

#[cfg(unix)]
pub use imp::{UdsFrontend, UdsLink};

/// The daemon's own real user id. Used to reject cross-user socket connections.
#[cfg(unix)]
pub fn daemon_uid() -> u32 {
    // SAFETY: `getuid()` takes no arguments, reads no caller memory, never fails, and returns
    unsafe { getuid() }
}

/// On non-Unix targets there is no uid; report a fixed value so shared callers compile.
/// Windows access control is enforced by the named-pipe endpoint rather than a uid check.
#[cfg(not(unix))]
pub fn daemon_uid() -> u32 {
    0
}

/// POSIX `MSG_PEEK` flag for [`recv`]: return data from the socket queue without consuming it.
/// `0x2` on both Linux and macOS.
#[cfg(unix)]
const MSG_PEEK: i32 = 0x2;

#[cfg(unix)]
unsafe extern "C" {
    /// POSIX `getuid(2)`.
    fn getuid() -> u32;

    /// POSIX `getsockopt(2)`. Used to read peer credentials.
    fn getsockopt(sockfd: i32, level: i32, optname: i32, optval: *mut core::ffi::c_void, optlen: *mut u32) -> i32;

    /// POSIX `recv(2)`. Used with [`MSG_PEEK`] to sniff a connection's first byte without consuming
    /// it, so the accept loop can route a relay (rmcp) connection apart from a legacy comms link.
    fn recv(sockfd: i32, buf: *mut core::ffi::c_void, len: usize, flags: i32) -> isize;
}

/// Read peer credentials from a raw socket fd.
///
/// On Linux we use `SO_PEERCRED` (struct `ucred { pid, uid, gid }`); on macOS we use
/// `LOCAL_PEERCRED` (`struct xucred`) for the uid and fall back to no pid. On any failure we
/// return an empty [`PeerCred`] and let filesystem permissions guard the socket.
#[cfg(unix)]
pub(crate) fn peer_cred_from_fd(fd: i32) -> crate::comms::transport::PeerCred {
    #[cfg(target_os = "linux")]
    {
        const SOL_SOCKET: i32 = 1;
        const SO_PEERCRED: i32 = 17;
        #[repr(C)]
        #[derive(Default, Clone, Copy)]
        struct Ucred {
            pid: i32,
            uid: u32,
            gid: u32,
        }
        let mut cred = Ucred::default();
        let mut len = core::mem::size_of::<Ucred>() as u32;
        // SAFETY: `fd` is a live connected socket fd owned by the caller for the duration of
        let rc = unsafe { getsockopt(fd, SOL_SOCKET, SO_PEERCRED, (&mut cred as *mut Ucred).cast(), &mut len) };
        if rc == 0 {
            return crate::comms::transport::PeerCred {
                uid: Some(cred.uid),
                pid: Some(cred.pid as u32),
            };
        }
    }
    #[cfg(target_os = "macos")]
    {
        const SOL_LOCAL: i32 = 0;
        const LOCAL_PEERCRED: i32 = 0x001;
        #[repr(C)]
        struct Xucred {
            cr_version: u32,
            cr_uid: u32,
            cr_ngroups: i16,
            cr_groups: [u32; 16],
        }
        let mut cred = Xucred {
            cr_version: 0,
            cr_uid: u32::MAX,
            cr_ngroups: 0,
            cr_groups: [0; 16],
        };
        let mut len = core::mem::size_of::<Xucred>() as u32;
        // SAFETY: as in the Linux branch — `fd` is live for the call, the out-params point at a
        let rc = unsafe {
            getsockopt(
                fd,
                SOL_LOCAL,
                LOCAL_PEERCRED,
                (&mut cred as *mut Xucred).cast(),
                &mut len,
            )
        };
        if rc == 0 {
            return crate::comms::transport::PeerCred {
                uid: Some(cred.cr_uid),
                pid: None,
            };
        }
    }
    crate::comms::transport::PeerCred::default()
}

#[cfg(all(test, unix))]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{UnixListener, UnixStream};
    use tokio::sync::watch;

    use super::UdsFrontend;
    use crate::comms::daemon::Broker;
    use crate::comms::protocol::{CommsOut, CommsRequest, CommsResponse};
    use crate::comms::store::CommsStore;
    use crate::comms::transport::CommsFrontend;

    async fn send_req(stream: &mut UnixStream, req: &CommsRequest) {
        let body = rmp_serde::to_vec_named(req).expect("encode");
        let len = u32::try_from(body.len()).expect("len fits");
        stream.write_all(&len.to_be_bytes()).await.expect("write len");
        stream.write_all(&body).await.expect("write body");
        stream.flush().await.expect("flush");
    }

    async fn read_resp(stream: &mut UnixStream) -> CommsOut {
        let mut prefix = [0u8; 4];
        stream.read_exact(&mut prefix).await.expect("read len");
        let len = u32::from_be_bytes(prefix) as usize;
        let mut buf = vec![0u8; len];
        stream.read_exact(&mut buf).await.expect("read body");
        rmp_serde::from_slice(&buf).expect("decode")
    }

    /// The accept loop must never park on a freshly-accepted client's first byte: one client that
    /// connects and then stays silent must NOT stall service to every other client. Regression for
    /// the comms-daemon readiness flake — an inline `peek_first_byte` in the accept loop let a
    /// single slow/silent connection freeze new accepts, so `ensure_daemon`'s probe timed out
    /// against a perfectly healthy daemon and surfaced `SpawnTimeout`.
    #[tokio::test]
    async fn a_silent_client_does_not_stall_the_accept_loop() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(CommsStore::open(dir.path()).expect("store"));
        let broker = Arc::new(Broker::new(store));

        let socket = dir.path().join("accept.sock");
        let listener = UnixListener::bind(&socket).expect("bind");
        let frontend = UdsFrontend::from_listener(listener, socket.clone());
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        tokio::spawn(Box::new(frontend).serve(broker, shutdown_rx));

        // Client A connects and then says NOTHING, holding the connection open. Under the bug this
        // parks the accept loop in `peek_first_byte`, leaving everyone after it unaccepted.
        let _silent = UnixStream::connect(&socket).await.expect("connect A");
        // Give the accept loop time to accept A and (buggily) park on its first-byte peek.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Client B must still be served promptly.
        let mut b = UnixStream::connect(&socket).await.expect("connect B");
        send_req(&mut b, &CommsRequest::Ping).await;
        let resp = tokio::time::timeout(Duration::from_secs(3), read_resp(&mut b))
            .await
            .expect("B must be served even while A is silent — otherwise the accept loop is stalled");
        assert_eq!(
            resp,
            CommsOut::Response(CommsResponse::Pong),
            "B's Ping must be answered with Pong"
        );
    }
}
