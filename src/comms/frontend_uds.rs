//! Unix-domain-socket front-end + link — the production local IPC path.
//!
//! Frames ride a [`LengthDelimitedCodec`](tokio_util::codec::LengthDelimitedCodec) (`u32`
//! big-endian length prefix) carrying a msgpack [`CommsRequest`] / [`CommsOut`] body. At
//! accept time the link captures the peer's credentials and the daemon rejects any connection
//! whose uid differs from its own — defence in depth on top of the socket's mode-0600
//! permissions.
//!
//! ## Peer credentials without `libc`
//!
//! basemind does not depend on `libc`, so this module declares the two C entry points it needs
//! itself (`getuid`, `getsockopt`). They are part of the platform libc that is always linked
//! on Unix, so the `extern "C"` declarations resolve at link time. Each call site carries a
//! `// SAFETY:` note. On non-Unix targets the front-end is unavailable (see the `#[cfg]`
//! stubs); the singleton path still resolves a Windows pipe name for a future named-pipe
//! front-end (TODO), but this iteration ships the Unix path.

#[cfg(unix)]
mod imp {
    use std::os::fd::AsRawFd;
    use std::path::PathBuf;
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{UnixListener, UnixStream};
    use tokio::sync::{mpsc, watch};
    use tokio_util::bytes::{Bytes, BytesMut};
    use tokio_util::codec::{Decoder, Encoder, LengthDelimitedCodec};

    use crate::comms::daemon::{Broker, Session};
    use crate::comms::protocol::{CommsOut, CommsRequest};
    use crate::comms::transport::{CommsFrontend, CommsLink, MAX_FRAME_BYTES, PeerCred};

    /// Notification fan-out buffer per link. Mirrors the in-process depth.
    const CHANNEL_DEPTH: usize = 256;
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
                // Try to decode a complete frame from whatever is already buffered.
                if let Some(frame) = self.codec.decode(&mut self.read_buf)? {
                    let req = rmp_serde::from_slice(&frame).map_err(|e| {
                        std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
                    })?;
                    return Ok(Some(req));
                }
                // Need more bytes from the socket.
                let n = self.stream.read_buf(&mut self.read_buf).await?;
                if n == 0 {
                    // EOF: a clean close has an empty buffer; a partial frame is an error.
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
            Self {
                listener,
                socket_path,
            }
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
                        let broker = broker.clone();
                        tokio::spawn(serve_uds_link(broker, UdsLink::new(stream, peer)));
                    }
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() {
                            break;
                        }
                    }
                }
            }
            // Draining: unlink the socket so a future daemon can rebind cleanly.
            let _ = std::fs::remove_file(&self.socket_path);
            Ok(())
        }
    }

    /// Drive one Unix-socket link: pump requests through the broker, write responses, and
    /// drain the broker's notification sink for this link onto the same socket.
    async fn serve_uds_link(broker: Arc<Broker>, mut link: UdsLink) {
        let (link_tx, mut link_rx) = mpsc::channel::<CommsOut>(CHANNEL_DEPTH);
        let mut session = Session::default();
        loop {
            tokio::select! {
                inbound = link.recv() => {
                    match inbound {
                        Ok(Some(req)) => {
                            let resp = broker.handle(req, &mut session, &link_tx).await;
                            if link.send(CommsOut::Response(resp)).await.is_err() {
                                break;
                            }
                        }
                        Ok(None) | Err(_) => break,
                    }
                }
                note = link_rx.recv() => {
                    match note {
                        Some(out) => {
                            if link.send(out).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    }
                }
            }
        }
    }

    /// Read the peer's credentials from a connected stream. Best-effort: returns an empty
    /// [`PeerCred`] when the platform call fails, in which case the daemon relies on the
    /// socket's filesystem permissions.
    fn peer_cred_of(stream: &UnixStream) -> PeerCred {
        super::peer_cred_from_fd(stream.as_raw_fd())
    }
}

#[cfg(unix)]
pub use imp::{UdsFrontend, UdsLink};

// ─── peer-cred + uid via self-declared C entry points (no `libc` dep) ─────────────────────

/// The daemon's own real user id. Used to reject cross-user socket connections.
#[cfg(unix)]
pub fn daemon_uid() -> u32 {
    // SAFETY: `getuid()` takes no arguments, reads no caller memory, never fails, and returns
    // the calling process's real user id as a `uid_t` (32-bit on Linux/macOS). It is one of
    // the always-safe POSIX calls.
    unsafe { getuid() }
}

/// On non-Unix targets there is no uid; report a fixed value so callers compile. The Windows
/// named-pipe front-end (TODO) will use a different access-control mechanism.
#[cfg(not(unix))]
pub fn daemon_uid() -> u32 {
    0
}

#[cfg(unix)]
unsafe extern "C" {
    /// POSIX `getuid(2)`.
    fn getuid() -> u32;

    /// POSIX `getsockopt(2)`. Used to read peer credentials.
    fn getsockopt(
        sockfd: i32,
        level: i32,
        optname: i32,
        optval: *mut core::ffi::c_void,
        optlen: *mut u32,
    ) -> i32;
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
        // struct ucred { pid_t pid; uid_t uid; gid_t gid; } — three 32-bit fields.
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
        // this call. `optval`/`optlen` point at a correctly-sized, properly-aligned `Ucred`
        // and `u32` on the stack; `getsockopt` writes at most `len` bytes into `cred`. The
        // return code is checked before the out-params are read.
        let rc = unsafe {
            getsockopt(
                fd,
                SOL_SOCKET,
                SO_PEERCRED,
                (&mut cred as *mut Ucred).cast(),
                &mut len,
            )
        };
        if rc == 0 {
            return crate::comms::transport::PeerCred {
                uid: Some(cred.uid),
                pid: Some(cred.pid as u32),
            };
        }
    }
    #[cfg(target_os = "macos")]
    {
        // struct xucred { u_int cr_version; uid_t cr_uid; short cr_ngroups; uid_t cr_groups[16]; }
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
        // correctly-sized stack `Xucred`/`u32`, and the result is checked before reading.
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
