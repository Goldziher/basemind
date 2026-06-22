//! `CommsClient`: the public client contract used by `basemind serve`, the CLI, and hooks.
//!
//! A thin async wrapper over a [`CommsLink`](super::transport::CommsLink) to the broker. The
//! client owns the request/response correlation: the broker answers requests in order on the
//! link, and notifications are surfaced separately so a caller can drain them. A later
//! component (the MCP/CLI tool surface) proxies straight to these methods, so the signatures
//! here are the stable contract.

use std::path::{Path, PathBuf};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio_util::bytes::{Bytes, BytesMut};
use tokio_util::codec::{Decoder, Encoder, LengthDelimitedCodec};

use super::cursor::Cursor;
use super::ids::{AgentId, RoomId};
use super::model::{AgentCard, AgentRecord, Room, RoomScope};
use super::protocol::{
    CommsNotification, CommsOut, CommsRequest, CommsResponse, PROTO_VER, SeqMeta, StatusReport,
};
use super::singleton::{self, CommsPaths};
use super::transport::MAX_FRAME_BYTES;

const READ_CHUNK: usize = 8 * 1024;

/// Strategy for (re)spawning the daemon when a reconnect finds the socket dead. Defaults to the
/// production [`singleton::spawn_detached_daemon`]; tests inject a closure that launches the real
/// `basemind` binary against an isolated comms dir (the test binary has no `comms daemon` verb).
type SpawnFn = Box<dyn Fn(&CommsPaths) -> std::io::Result<()> + Send + Sync>;

/// Errors surfaced by the client.
#[derive(Debug, thiserror::Error)]
pub enum CommsClientError {
    /// An io / transport failure.
    #[error("comms transport error: {0}")]
    Io(#[from] std::io::Error),
    /// msgpack encode failure.
    #[error("encode error: {0}")]
    Encode(#[from] rmp_serde::encode::Error),
    /// msgpack decode failure.
    #[error("decode error: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
    /// Singleton bring-up failed.
    #[error(transparent)]
    Singleton(#[from] super::singleton::SingletonError),
    /// The link closed before a response arrived.
    #[error("connection closed before a response was received")]
    Closed,
    /// The broker returned an error response.
    #[error("broker error [{code}]: {message}")]
    Broker {
        /// Stable error token from the broker.
        code: String,
        /// Human-readable detail.
        message: String,
    },
    /// The broker returned a response of the wrong shape for the request.
    #[error("unexpected response shape for {request}")]
    Unexpected {
        /// The request whose reply was malformed.
        request: &'static str,
    },
    /// The daemon's protocol version differs from this build's.
    #[error("protocol skew: daemon speaks {daemon}, client speaks {client}")]
    ProtoSkew {
        /// The daemon's protocol version.
        daemon: u32,
        /// This client's protocol version.
        client: u32,
    },
}

/// A connected, said-hello client to the comms broker.
pub struct CommsClient {
    stream: UnixStream,
    codec: LengthDelimitedCodec,
    read_buf: BytesMut,
    agent: AgentId,
    /// Notifications received while waiting for a response are queued here so the caller can
    /// drain them via [`CommsClient::next_notification`].
    pending_notifications: std::collections::VecDeque<CommsNotification>,
    /// Connection context retained so the client can transparently re-establish the link (and
    /// re-spawn the daemon) after the daemon dies mid-session.
    paths: CommsPaths,
    /// Scope context replayed on the `Hello` of a reconnect.
    remote: Option<String>,
    /// Working directory replayed on the `Hello` of a reconnect.
    cwd: Option<PathBuf>,
    /// Respawn strategy used by [`CommsClient::reconnect`] when the socket is dead.
    spawn: SpawnFn,
}

impl CommsClient {
    /// Connect to an already-running daemon at `paths` and complete the `Hello` handshake.
    /// Use [`CommsClient::ensure_and_connect`] to spawn the daemon first when needed.
    ///
    /// The returned client re-spawns the daemon via [`singleton::spawn_detached_daemon`] when a
    /// reconnect finds the socket dead. Use [`CommsClient::connect_with_respawn`] to inject a
    /// different spawn strategy.
    pub async fn connect(
        paths: &CommsPaths,
        agent: AgentId,
        remote: Option<String>,
        cwd: Option<PathBuf>,
    ) -> Result<Self, CommsClientError> {
        Self::connect_with_respawn(paths, agent, remote, cwd, |paths| {
            singleton::spawn_detached_daemon(paths)
        })
        .await
    }

    /// Connect like [`CommsClient::connect`], but inject the daemon respawn strategy used by the
    /// transparent reconnect path. The production [`CommsClient::connect`] supplies
    /// [`singleton::spawn_detached_daemon`]; tests inject a closure that launches the real
    /// `basemind` binary so the reconnect can resurrect an isolated daemon.
    pub async fn connect_with_respawn(
        paths: &CommsPaths,
        agent: AgentId,
        remote: Option<String>,
        cwd: Option<PathBuf>,
        spawn: impl Fn(&CommsPaths) -> std::io::Result<()> + Send + Sync + 'static,
    ) -> Result<Self, CommsClientError> {
        let (stream, codec) = Self::dial(paths).await?;
        let mut client = Self {
            stream,
            codec,
            read_buf: BytesMut::with_capacity(READ_CHUNK),
            agent,
            pending_notifications: std::collections::VecDeque::new(),
            paths: paths.clone(),
            remote,
            cwd,
            spawn: Box::new(spawn),
        };
        client.handshake().await?;
        Ok(client)
    }

    /// Resolve the per-user paths, ensure a daemon is running (spawning it if needed), then
    /// connect + handshake. The one-call entry point for serve / CLI / hooks.
    pub async fn ensure_and_connect(
        agent: AgentId,
        remote: Option<String>,
        cwd: Option<PathBuf>,
    ) -> Result<Self, CommsClientError> {
        let paths = singleton::resolve_paths()?;
        singleton::ensure_daemon(&paths).await?;
        Self::connect(&paths, agent, remote, cwd).await
    }

    /// Dial the socket and build the framing codec. No handshake yet.
    async fn dial(
        paths: &CommsPaths,
    ) -> Result<(UnixStream, LengthDelimitedCodec), CommsClientError> {
        let stream = UnixStream::connect(&paths.socket_path)
            .await
            .map_err(|source| daemon_unreachable_error(&paths.socket_path, source))?;
        let mut codec = LengthDelimitedCodec::new();
        codec.set_max_frame_length(MAX_FRAME_BYTES);
        Ok((stream, codec))
    }

    /// Send the `Hello` and validate the `Welcome`, using this client's retained scope context.
    async fn handshake(&mut self) -> Result<(), CommsClientError> {
        let resp = self
            .send_and_await(CommsRequest::Hello {
                agent: self.agent.clone(),
                proto_ver: PROTO_VER,
                remote: self.remote.clone(),
                cwd: self.cwd.clone(),
            })
            .await?;
        match resp {
            CommsResponse::Welcome { proto_ver, .. } if proto_ver == PROTO_VER => Ok(()),
            CommsResponse::Welcome { proto_ver, .. } => Err(CommsClientError::ProtoSkew {
                daemon: proto_ver,
                client: PROTO_VER,
            }),
            CommsResponse::Error { code, message } => {
                Err(CommsClientError::Broker { code, message })
            }
            _ => Err(CommsClientError::Unexpected { request: "hello" }),
        }
    }

    /// Re-establish the link after a broken/closed connection: ensure the daemon is alive
    /// (re-spawning it if the socket is gone), re-dial, and replay the `Hello` handshake. Any
    /// buffered notifications from the dead link are dropped — they belong to a connection that
    /// no longer exists.
    async fn reconnect(&mut self) -> Result<(), CommsClientError> {
        // `spawn` is a borrow of `self`, but `ensure_daemon_with` only needs it as `FnOnce`;
        // borrow it through a closure so we do not move it out of `self`.
        let spawn = &self.spawn;
        singleton::ensure_daemon_with(&self.paths, singleton::probe_alive, |paths| spawn(paths))
            .await?;
        let (stream, codec) = Self::dial(&self.paths).await?;
        self.stream = stream;
        self.codec = codec;
        self.read_buf.clear();
        self.pending_notifications.clear();
        self.handshake().await
    }

    /// The agent id this client authenticated as.
    pub fn agent(&self) -> &AgentId {
        &self.agent
    }

    // ─── public API (the proxied contract) ────────────────────────────────────────────────

    /// Register or update this agent's card.
    pub async fn register_agent(&mut self, card: AgentCard) -> Result<(), CommsClientError> {
        self.expect_ok(CommsRequest::Register { card }, "register")
            .await
    }

    /// List known agents, optionally restricted to subscribers of one room.
    pub async fn list_agents(
        &mut self,
        room: Option<RoomId>,
    ) -> Result<Vec<AgentRecord>, CommsClientError> {
        match self.request(CommsRequest::ListAgents { room }).await? {
            CommsResponse::Agents(a) => Ok(a),
            other => Err(self.shape_err(other, "list_agents")),
        }
    }

    /// Create (and register) a room with an explicit scope.
    pub async fn create_room(
        &mut self,
        room: RoomId,
        scope: RoomScope,
        title: Option<String>,
    ) -> Result<Room, CommsClientError> {
        match self
            .request(CommsRequest::CreateRoom { room, scope, title })
            .await?
        {
            CommsResponse::Room(r) => Ok(r),
            other => Err(self.shape_err(other, "create_room")),
        }
    }

    /// List rooms whose scope matches the supplied chain (remote + cwd).
    pub async fn list_rooms(
        &mut self,
        remote: Option<String>,
        cwd: Option<PathBuf>,
    ) -> Result<Vec<Room>, CommsClientError> {
        match self
            .request(CommsRequest::ListRooms { remote, cwd })
            .await?
        {
            CommsResponse::Rooms(r) => Ok(r),
            other => Err(self.shape_err(other, "list_rooms")),
        }
    }

    /// Subscribe this agent to a room (durable membership; drives the inbox).
    pub async fn join_room(&mut self, room: RoomId) -> Result<(), CommsClientError> {
        self.expect_ok(CommsRequest::Join { room }, "join_room")
            .await
    }

    /// Unsubscribe this agent from a room.
    pub async fn leave_room(&mut self, room: RoomId) -> Result<(), CommsClientError> {
        self.expect_ok(CommsRequest::Leave { room }, "leave_room")
            .await
    }

    /// Post a message to a room. Returns the new message id. `scope` carries optional glob / path
    /// patterns describing where the message applies (empty when unscoped).
    #[allow(clippy::too_many_arguments)]
    pub async fn post_message(
        &mut self,
        room: RoomId,
        subject: String,
        body: Vec<u8>,
        tags: Vec<String>,
        reply_to: Option<String>,
        scope: Vec<String>,
    ) -> Result<String, CommsClientError> {
        match self
            .request(CommsRequest::Post {
                room,
                subject,
                tags,
                reply_to,
                scope,
                body,
            })
            .await?
        {
            CommsResponse::Posted { message_id } => Ok(message_id),
            other => Err(self.shape_err(other, "post_message")),
        }
    }

    /// Acknowledge inbox messages by advancing this agent's per-room read cursors. Pass
    /// `message_ids` to ack specific messages (each resolved to its `(room, seq)`), and/or a
    /// `(room, to_seq)` pair to bulk-ack everything up to `to_seq` in that room. Returns the count
    /// of acked ids and the `(room, new_seq)` cursors that advanced. Never deletes from the shared
    /// log and never affects another agent.
    pub async fn ack_inbox(
        &mut self,
        message_ids: Vec<String>,
        room: Option<RoomId>,
        to_seq: Option<u64>,
    ) -> Result<(u32, Vec<(String, u64)>), CommsClientError> {
        match self
            .request(CommsRequest::AckInbox {
                message_ids,
                room,
                to_seq,
            })
            .await?
        {
            CommsResponse::Acked {
                acked,
                cursors_advanced,
            } => Ok((acked, cursors_advanced)),
            other => Err(self.shape_err(other, "ack_inbox")),
        }
    }

    /// Read a room's history (front-matter only), oldest-first. Returns the page plus the next
    /// cursor when more remain.
    pub async fn read_history(
        &mut self,
        room: RoomId,
        cursor: Option<Cursor>,
        limit: u32,
    ) -> Result<(Vec<SeqMeta>, Option<Cursor>), CommsClientError> {
        match self
            .request(CommsRequest::History {
                room,
                cursor,
                limit: Some(limit),
            })
            .await?
        {
            CommsResponse::History {
                messages,
                next_cursor,
            } => Ok((messages, next_cursor)),
            other => Err(self.shape_err(other, "read_history")),
        }
    }

    /// Fetch a single message body by id. `None` when the id is unknown.
    pub async fn get_body(
        &mut self,
        message_id: String,
    ) -> Result<Option<Vec<u8>>, CommsClientError> {
        match self.request(CommsRequest::GetBody { message_id }).await? {
            CommsResponse::Body { body } => Ok(body),
            other => Err(self.shape_err(other, "get_body")),
        }
    }

    /// Read this agent's inbox across subscribed rooms. Returns the page, the count of unread
    /// remaining after the page, and the next cursor.
    #[allow(clippy::type_complexity)]
    pub async fn read_inbox(
        &mut self,
        remote: Option<String>,
        cwd: Option<PathBuf>,
        cursor: Option<Cursor>,
        limit: u32,
        mark_read: bool,
    ) -> Result<(Vec<SeqMeta>, u32, Option<Cursor>), CommsClientError> {
        match self
            .request(CommsRequest::Inbox {
                remote,
                cwd,
                cursor,
                limit: Some(limit),
                mark_read,
            })
            .await?
        {
            CommsResponse::Inbox {
                messages,
                unread,
                next_cursor,
            } => Ok((messages, unread, next_cursor)),
            other => Err(self.shape_err(other, "read_inbox")),
        }
    }

    /// Open a notification stream for a room. Returns the subscription handle; subsequent
    /// [`CommsClient::next_notification`] calls surface posts to that room.
    pub async fn subscribe(&mut self, room: RoomId) -> Result<u64, CommsClientError> {
        match self.request(CommsRequest::Subscribe { room }).await? {
            CommsResponse::Subscribed { sub } => Ok(sub),
            other => Err(self.shape_err(other, "subscribe")),
        }
    }

    /// Cancel a notification stream.
    pub async fn unsubscribe(&mut self, sub: u64) -> Result<(), CommsClientError> {
        self.expect_ok(CommsRequest::Unsubscribe { sub }, "unsubscribe")
            .await
    }

    /// Ask the daemon for its status snapshot.
    pub async fn status(&mut self) -> Result<StatusReport, CommsClientError> {
        match self.request(CommsRequest::Status).await? {
            CommsResponse::Status(s) => Ok(s),
            other => Err(self.shape_err(other, "status")),
        }
    }

    /// Ask the daemon to drain and stop.
    pub async fn stop(&mut self) -> Result<(), CommsClientError> {
        self.expect_ok(CommsRequest::Stop, "stop").await
    }

    /// Drain the next buffered notification, if any was received while awaiting a response.
    /// Does not block on the socket — call [`CommsClient::poll_notification`] to read one
    /// directly off the wire.
    pub fn next_notification(&mut self) -> Option<CommsNotification> {
        self.pending_notifications.pop_front()
    }

    /// Await the next notification directly from the socket (after draining any buffered ones).
    pub async fn poll_notification(
        &mut self,
    ) -> Result<Option<CommsNotification>, CommsClientError> {
        if let Some(n) = self.pending_notifications.pop_front() {
            return Ok(Some(n));
        }
        loop {
            match self.read_frame().await? {
                Some(CommsOut::Notification(n)) => return Ok(Some(n)),
                Some(CommsOut::Response(_)) => continue, // unsolicited response — ignore
                None => return Ok(None),
            }
        }
    }

    // ─── transport plumbing ───────────────────────────────────────────────────────────────

    async fn expect_ok(
        &mut self,
        req: CommsRequest,
        label: &'static str,
    ) -> Result<(), CommsClientError> {
        match self.request(req).await? {
            CommsResponse::Ok => Ok(()),
            other => Err(self.shape_err(other, label)),
        }
    }

    fn shape_err(&self, resp: CommsResponse, request: &'static str) -> CommsClientError {
        match resp {
            CommsResponse::Error { code, message } => CommsClientError::Broker { code, message },
            _ => CommsClientError::Unexpected { request },
        }
    }

    /// Send a request and await its direct response, transparently recovering from a dead daemon.
    ///
    /// On the first attempt, a broken/closed connection (`BrokenPipe` / `ConnectionReset` /
    /// unexpected EOF / a clean close before any reply) triggers exactly ONE reconnect — which
    /// re-spawns the daemon if its socket is gone — followed by a single retry. A second failure
    /// (or any non-connection error) is surfaced. This single-shot bound rules out an infinite
    /// reconnect loop against a daemon that keeps dying.
    ///
    /// Replay safety: the retry only fires when the connection broke, and most requests are
    /// trivially replayable — history / inbox / status / get_body are pure reads, and ack only
    /// advances a monotonic per-agent cursor idempotently. The dominant failure this fixes is a
    /// dead/stale daemon: the WRITE fails before any daemon sees the request, so the post-reconnect
    /// replay is the *first* delivery, not a duplicate.
    ///
    /// The one residual window is a `Post` (or other mutation) that the old daemon committed to the
    /// shared, persistent Fjall log and *then* crashed before its reply reached us: because the
    /// reconnected daemon reads that same log, the replay would append a SECOND copy. This window
    /// is narrow (a crash between store-commit and socket-write) and the worst case is a duplicate
    /// coordination message — not corruption — which is an accepted trade-off for making `room_post`
    /// survive the daemon dying at all. (A client-supplied idempotency key would close it; deferred.)
    async fn request(&mut self, req: CommsRequest) -> Result<CommsResponse, CommsClientError> {
        match self.send_and_await(req.clone()).await {
            Ok(resp) => Ok(resp),
            Err(err) if is_connection_lost(&err) => {
                // The link to the broker is gone. Re-spawn the daemon if its socket is dead,
                // re-dial, replay the `Hello`, then retry the request exactly once. A second
                // failure (connection or otherwise) is surfaced — this single-shot bound rules
                // out an infinite reconnect loop against a daemon that keeps dying.
                self.reconnect().await?;
                self.send_and_await(req).await
            }
            Err(err) => Err(err),
        }
    }

    /// Write the request and read frames until the direct response arrives, buffering any
    /// notifications seen in the meantime. No reconnect — the single-shot retry lives in
    /// [`CommsClient::request`].
    async fn send_and_await(
        &mut self,
        req: CommsRequest,
    ) -> Result<CommsResponse, CommsClientError> {
        self.write_request(&req).await?;
        loop {
            match self.read_frame().await? {
                Some(CommsOut::Response(resp)) => return Ok(resp),
                Some(CommsOut::Notification(n)) => self.pending_notifications.push_back(n),
                None => return Err(CommsClientError::Closed),
            }
        }
    }

    async fn write_request(&mut self, req: &CommsRequest) -> Result<(), CommsClientError> {
        let body = rmp_serde::to_vec_named(req)?;
        let mut framed = BytesMut::new();
        self.codec.encode(Bytes::from(body), &mut framed)?;
        self.stream.write_all(&framed).await?;
        self.stream.flush().await?;
        Ok(())
    }

    async fn read_frame(&mut self) -> Result<Option<CommsOut>, CommsClientError> {
        loop {
            if let Some(frame) = self.codec.decode(&mut self.read_buf)? {
                let out: CommsOut = rmp_serde::from_slice(&frame)?;
                return Ok(Some(out));
            }
            let n = self.stream.read_buf(&mut self.read_buf).await?;
            if n == 0 {
                if self.read_buf.is_empty() {
                    return Ok(None);
                }
                return Err(CommsClientError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "broker closed mid-frame",
                )));
            }
        }
    }
}

/// Classify an error as "the link to the broker is gone" — the only class the single-shot
/// reconnect+retry fires on. Covers the kernel signals for a dead peer (`BrokenPipe`,
/// `ConnectionReset`, `ConnectionAborted`, `NotConnected`), an unexpected mid-frame EOF, and the
/// clean-close [`CommsClientError::Closed`] (the daemon dropped the link before replying).
fn is_connection_lost(err: &CommsClientError) -> bool {
    match err {
        CommsClientError::Closed => true,
        CommsClientError::Io(io) => matches!(
            io.kind(),
            std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::NotConnected
                | std::io::ErrorKind::UnexpectedEof
        ),
        _ => false,
    }
}

#[cfg(all(test, feature = "comms", unix))]
mod tests {
    use super::*;

    /// Dialing a socket path that no daemon is listening on must surface an actionable error
    /// naming that the comms daemon is not running and how to start it — not a bare OS string.
    #[tokio::test]
    async fn dial_missing_socket_reports_daemon_not_running_with_start_hint() {
        let dir = std::env::temp_dir().join(format!("basemind-comms-test-{}", std::process::id()));
        let paths = CommsPaths {
            comms_dir: dir.clone(),
            socket_path: dir.join("definitely-absent.sock"),
        };

        let err = match CommsClient::dial(&paths).await {
            Ok(_) => panic!("dialing an absent socket must fail"),
            Err(err) => err,
        };
        let msg = err.to_string();

        assert!(
            msg.contains("comms daemon is not running"),
            "error should name that the daemon is not running, got: {msg}"
        );
        assert!(
            msg.contains("basemind comms start"),
            "error should name the start command, got: {msg}"
        );
        assert!(
            !msg.starts_with("comms transport error: No such file or directory"),
            "error must not be the bare OS string, got: {msg}"
        );
    }
}

/// Map a `UnixStream::connect` failure into an actionable error. A missing socket file
/// (`NotFound`) or a refused connection (`ConnectionRefused`) means no daemon is listening, so we
/// wrap it with a message naming that the comms daemon is not running and the start command
/// (`basemind comms start`). Any other connect error keeps its original `io::Error` context.
fn daemon_unreachable_error(socket_path: &Path, source: std::io::Error) -> CommsClientError {
    match source.kind() {
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {
            CommsClientError::Io(std::io::Error::new(
                source.kind(),
                format!(
                    "comms daemon is not running (no socket at {}); start it with \
                     `basemind comms start`",
                    socket_path.display()
                ),
            ))
        }
        _ => CommsClientError::Io(source),
    }
}

/// Resolve the agent's scope context (remote + cwd) for a `Hello` from the current directory.
/// Convenience for the CLI / hook callers that just want "whatever repo I'm in".
pub fn scope_context_for(cwd: &Path) -> (Option<String>, Option<PathBuf>) {
    let repo = crate::git::Repo::discover(cwd).ok();
    let remote = repo.as_ref().and_then(|r| {
        let key = crate::git::scope_key(r);
        if key.starts_with("path:") {
            None
        } else {
            Some(key)
        }
    });
    (remote, Some(cwd.to_path_buf()))
}
