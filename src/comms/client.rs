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
}

impl CommsClient {
    /// Connect to an already-running daemon at `paths` and complete the `Hello` handshake.
    /// Use [`CommsClient::ensure_and_connect`] to spawn the daemon first when needed.
    pub async fn connect(
        paths: &CommsPaths,
        agent: AgentId,
        remote: Option<String>,
        cwd: Option<PathBuf>,
    ) -> Result<Self, CommsClientError> {
        let stream = UnixStream::connect(&paths.socket_path).await?;
        let mut codec = LengthDelimitedCodec::new();
        codec.set_max_frame_length(MAX_FRAME_BYTES);
        let mut client = Self {
            stream,
            codec,
            read_buf: BytesMut::with_capacity(READ_CHUNK),
            agent: agent.clone(),
            pending_notifications: std::collections::VecDeque::new(),
        };
        let resp = client
            .request(CommsRequest::Hello {
                agent,
                proto_ver: PROTO_VER,
                remote,
                cwd,
            })
            .await?;
        match resp {
            CommsResponse::Welcome { proto_ver, .. } if proto_ver == PROTO_VER => Ok(client),
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

    /// Send a request and read frames until the direct response arrives, buffering any
    /// notifications seen in the meantime.
    async fn request(&mut self, req: CommsRequest) -> Result<CommsResponse, CommsClientError> {
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
