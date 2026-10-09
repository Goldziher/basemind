//! `CommsClient`: the public client contract used by `basemind serve`, the CLI, and hooks.
//!
//! A thin async wrapper over a [`CommsLink`](super::transport::CommsLink) to the broker. The
//! client owns the request/response correlation: the broker answers requests in order on the
//! link, and notifications are surfaced separately so a caller can drain them. A later
//! component (the MCP/CLI tool surface) proxies straight to these methods, so the signatures
//! here are the stable contract.

use std::path::{Path, PathBuf};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(unix)]
use tokio::net::UnixStream as PlatformStream;
#[cfg(windows)]
use tokio::net::windows::named_pipe::NamedPipeClient as PlatformStream;
use tokio_util::bytes::{Bytes, BytesMut};
use tokio_util::codec::{Decoder, Encoder, LengthDelimitedCodec};
use tokio_util::sync::CancellationToken;

use super::cursor::Cursor;
use super::ids::{AgentId, ThreadId};
use super::model::{AgentCard, AgentRecord, Thread};
use super::protocol::{
    AgentsStatusReport, CleanupReport, CommsNotification, CommsOut, CommsRequest, CommsResponse, PROTO_VER, SeqMeta,
    StatusReport,
};
use super::singleton::{self, CommsPaths};
use super::transport::MAX_FRAME_BYTES;
use super::workspace_pool::AccessedWorkspace;

/// Outcome of a daemon-side [`CommsClient::rescan`]: the scan counts plus wall-clock time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RescanReport {
    /// Files considered by the scan.
    pub scanned: usize,
    /// Files whose index entries were written or refreshed.
    pub updated: usize,
    /// Documents re-extracted and re-embedded. Separate from `updated`, which is code-map only.
    pub docs_indexed: usize,
    /// Files pruned because they no longer exist.
    pub removed: usize,
    /// Wall-clock scan time in milliseconds.
    pub elapsed_ms: u64,
}

const READ_CHUNK: usize = 8 * 1024;

/// How long the Windows named-pipe dial retries a busy pipe before giving up. Bounds the spin
/// while the server mints its next instance during a client hand-off.
#[cfg(windows)]
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Env var overriding how long a coordination request waits for the broker's reply, in seconds.
pub const REQUEST_TIMEOUT_ENV: &str = "BASEMIND_COMMS_REQUEST_TIMEOUT_SECS";
/// Env var overriding how long the connect + `Hello` handshake may take, in seconds.
pub const HANDSHAKE_TIMEOUT_ENV: &str = "BASEMIND_COMMS_HANDSHAKE_TIMEOUT_SECS";
const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 10;
/// Generous because `Hello` is the first store-dependent request: a cold daemon answers it only once
/// its store has finished opening, which on a large store takes tens of seconds.
const DEFAULT_HANDSHAKE_TIMEOUT_SECS: u64 = 30;

/// A positive whole-second duration from `var`, else `default_secs`.
fn timeout_from_env(var: &str, default_secs: u64) -> std::time::Duration {
    let secs = std::env::var(var)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .unwrap_or(default_secs);
    std::time::Duration::from_secs(secs)
}

/// How long `req` may wait for its reply. Forwarded work (scans, embeds, memory, git-history and
/// index reads) legitimately runs for minutes and is unbounded here; everything else is a quick
/// broker round trip, so silence past the timeout means the broker is wedged, not busy.
fn request_timeout(req: &CommsRequest) -> Option<std::time::Duration> {
    match req {
        CommsRequest::Rescan { .. }
        | CommsRequest::Cleanup { .. }
        | CommsRequest::GitHistory { .. }
        | CommsRequest::ResolvedRefs { .. }
        | CommsRequest::IndexRead { .. }
        | CommsRequest::CodeSearchLanes { .. } => None,
        #[cfg(feature = "memory")]
        CommsRequest::Memory { .. } | CommsRequest::Governance { .. } => None,
        _ => Some(timeout_from_env(REQUEST_TIMEOUT_ENV, DEFAULT_REQUEST_TIMEOUT_SECS)),
    }
}

/// Cap on [`CommsClient::pending_notifications`]; the oldest is dropped once it is reached.
///
/// The queue is opportunistic, and for most clients nothing ever drains it: the only consumers are
/// [`CommsClient::poll_notification`] (reached solely by the timeout-bounded `wait` path) and
/// [`CommsClient::next_notification`], which the crate never calls. A client that only issues
/// ordinary requests — post, history, inbox, thread_list, i.e. nearly all MCP `agents` traffic —
/// therefore buffers every notification the broker pushes it and frees none of them until the link
/// dies and [`CommsClient::reconnect`] clears it. A long session on a busy thread grows it without
/// limit.
///
/// Dropping the oldest loses nothing durable: the broker is the authoritative record and `history` /
/// `inbox` read from it by request, so a discarded push costs at most an early wake-up, while the
/// unbounded queue costs resident memory for the whole session.
const PENDING_NOTIFICATION_CAP: usize = 1_024;

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
    /// The broker accepted the connection but did not answer in time. Retryable: the request may
    /// or may not have been applied, so re-read before repeating a mutation.
    #[error("broker unresponsive: no reply to {what} within {secs}s (retryable)")]
    Unresponsive {
        /// What was being waited on (a request method or the handshake).
        what: &'static str,
        /// The elapsed bound, in seconds.
        secs: u64,
    },
    /// The caller cancelled a long-poll; the client should be dropped, not reused.
    #[error("wait cancelled")]
    Cancelled,
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
    stream: PlatformStream,
    codec: LengthDelimitedCodec,
    read_buf: BytesMut,
    agent: AgentId,
    /// Notifications received while waiting for a response are queued here so the caller can
    /// drain them via [`CommsClient::next_notification`]. Bounded at [`PENDING_NOTIFICATION_CAP`],
    /// oldest-first, because most clients never drain it — see that constant.
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
    /// Id of the next correlated request ([`CommsRequest::Call`]) on this link.
    next_id: u64,
    /// True while a request frame is being written. A request future dropped in that window leaves
    /// a half-written frame on the socket, so the link can no longer be framed correctly and the
    /// next request must reconnect instead of reusing it.
    write_incomplete: bool,
    /// Explicit reply timeout for every request, overriding the per-method default and the env var.
    request_timeout_override: Option<std::time::Duration>,
}

impl CommsClient {
    /// Preview or apply the configured retention policy.
    #[allow(clippy::too_many_arguments)]
    pub async fn cleanup_agents(
        &mut self,
        apply: bool,
        message_ttl_secs: u64,
        thread_idle_ttl_secs: u64,
        thread_retention_ttl_secs: u64,
        agent_ttl_secs: u64,
        claim_ttl_secs: u64,
    ) -> Result<CleanupReport, CommsClientError> {
        match self
            .request(CommsRequest::Cleanup {
                apply,
                message_ttl_secs,
                thread_idle_ttl_secs,
                thread_retention_ttl_secs,
                agent_ttl_secs,
                claim_ttl_secs,
            })
            .await?
        {
            CommsResponse::Cleanup(report) => Ok(report),
            other => Err(self.shape_err(other, "cleanup_agents")),
        }
    }

    /// Report active/stale agent counts and the last maintenance time.
    pub async fn agents_status(&mut self, agent_ttl_secs: u64) -> Result<AgentsStatusReport, CommsClientError> {
        match self.request(CommsRequest::AgentsStatus { agent_ttl_secs }).await? {
            CommsResponse::AgentsStatus(report) => Ok(report),
            other => Err(self.shape_err(other, "agents_status")),
        }
    }
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
        let handshake_timeout = timeout_from_env(HANDSHAKE_TIMEOUT_ENV, DEFAULT_HANDSHAKE_TIMEOUT_SECS);
        Self::connect_bounded(paths, agent, remote, cwd, spawn, handshake_timeout).await
    }

    /// [`CommsClient::connect_with_respawn`] with an explicit bound on the dial + `Hello` handshake.
    async fn connect_bounded(
        paths: &CommsPaths,
        agent: AgentId,
        remote: Option<String>,
        cwd: Option<PathBuf>,
        spawn: impl Fn(&CommsPaths) -> std::io::Result<()> + Send + Sync + 'static,
        handshake_timeout: std::time::Duration,
    ) -> Result<Self, CommsClientError> {
        let (stream, codec) = bounded("connect", Some(handshake_timeout), Self::dial(paths)).await?;
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
            next_id: 1,
            write_incomplete: false,
            request_timeout_override: None,
        };
        bounded("handshake", Some(handshake_timeout), client.handshake()).await?;
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

    /// Dial the endpoint and build the framing codec. No handshake yet. The connect is
    /// platform-specific (Unix socket vs Windows named pipe); the codec is identical.
    async fn dial(paths: &CommsPaths) -> Result<(PlatformStream, LengthDelimitedCodec), CommsClientError> {
        let stream = Self::connect_stream(&paths.socket_path).await?;
        let mut codec = LengthDelimitedCodec::new();
        codec.set_max_frame_length(MAX_FRAME_BYTES);
        Ok((stream, codec))
    }

    /// Open the platform stream to the daemon endpoint.
    #[cfg(unix)]
    async fn connect_stream(socket_path: &Path) -> Result<PlatformStream, CommsClientError> {
        PlatformStream::connect(socket_path)
            .await
            .map_err(|source| daemon_unreachable_error(socket_path, source))
    }

    /// Open the named-pipe client to the daemon endpoint. A busy pipe (`ERROR_PIPE_BUSY`, 231)
    /// means the server is mid-`connect()` for another client; retry on a short cadence up to the
    /// connect timeout. Any other error (notably a missing pipe ⇒ no daemon) is surfaced through
    /// [`daemon_unreachable_error`].
    #[cfg(windows)]
    async fn connect_stream(socket_path: &Path) -> Result<PlatformStream, CommsClientError> {
        use tokio::net::windows::named_pipe::ClientOptions;

        /// `ERROR_PIPE_BUSY`: all pipe instances are busy; the server has not yet minted the next.
        const ERROR_PIPE_BUSY: i32 = 231;
        /// Poll cadence while a busy pipe spins up its next instance.
        const RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

        let deadline = std::time::Instant::now() + CONNECT_TIMEOUT;
        loop {
            match ClientOptions::new().open(socket_path) {
                Ok(client) => return Ok(client),
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                    if std::time::Instant::now() >= deadline {
                        return Err(daemon_unreachable_error(socket_path, e));
                    }
                    tokio::time::sleep(RETRY_INTERVAL).await;
                }
                Err(source) => return Err(daemon_unreachable_error(socket_path, source)),
            }
        }
    }

    /// Send the `Hello` and validate the `Welcome`, using this client's retained scope context.
    async fn handshake(&mut self) -> Result<(), CommsClientError> {
        let resp = self
            .send_and_await(
                CommsRequest::Hello {
                    agent: self.agent.clone(),
                    proto_ver: PROTO_VER,
                    remote: self.remote.clone(),
                    cwd: self.cwd.clone(),
                },
                None,
            )
            .await?;
        match resp {
            CommsResponse::Welcome { proto_ver, .. } if proto_ver == PROTO_VER => Ok(()),
            CommsResponse::Welcome { proto_ver, .. } => Err(CommsClientError::ProtoSkew {
                daemon: proto_ver,
                client: PROTO_VER,
            }),
            CommsResponse::Error { code, message } => Err(CommsClientError::Broker { code, message }),
            _ => Err(CommsClientError::Unexpected { request: "hello" }),
        }
    }

    /// Re-establish the link after a broken/closed connection: ensure the daemon is alive
    /// (re-spawning it if the socket is gone), re-dial, and replay the `Hello` handshake. Any
    /// buffered notifications from the dead link are dropped — they belong to a connection that
    /// no longer exists.
    async fn reconnect(&mut self) -> Result<(), CommsClientError> {
        let spawn = &self.spawn;
        singleton::ensure_daemon_with(
            &self.paths,
            |socket| singleton::off_worker(|| singleton::probe_alive(socket)),
            |paths| spawn(paths),
        )
        .await?;
        let handshake_timeout = timeout_from_env(HANDSHAKE_TIMEOUT_ENV, DEFAULT_HANDSHAKE_TIMEOUT_SECS);
        let (stream, codec) = bounded("connect", Some(handshake_timeout), Self::dial(&self.paths)).await?;
        self.stream = stream;
        self.codec = codec;
        self.read_buf.clear();
        self.pending_notifications.clear();
        self.write_incomplete = false;
        bounded("handshake", Some(handshake_timeout), self.handshake()).await
    }

    /// Bound every request's wait for a reply to `timeout`, overriding the per-method default and
    /// [`REQUEST_TIMEOUT_ENV`].
    #[must_use]
    pub fn with_request_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.request_timeout_override = Some(timeout);
        self
    }

    /// The agent id this client authenticated as.
    pub fn agent(&self) -> &AgentId {
        &self.agent
    }

    /// Register or update this agent's card.
    pub async fn register_agent(&mut self, card: AgentCard) -> Result<(), CommsClientError> {
        self.expect_ok(CommsRequest::Register { card }, "register").await
    }

    /// List known agents, optionally restricted to members of one thread.
    pub async fn list_agents(&mut self, thread: Option<ThreadId>) -> Result<Vec<AgentRecord>, CommsClientError> {
        match self.request(CommsRequest::ListAgents { thread }).await? {
            CommsResponse::Agents(a) => Ok(a),
            other => Err(self.shape_err(other, "list_agents")),
        }
    }

    /// Start a thread addressed by at least two of subject / path / members. Returns the thread.
    pub async fn start_thread(
        &mut self,
        subject: Option<String>,
        path: Option<String>,
        members: Vec<AgentId>,
    ) -> Result<Thread, CommsClientError> {
        match self
            .request(CommsRequest::ThreadStart { subject, path, members })
            .await?
        {
            CommsResponse::Thread(t) => Ok(t),
            other => Err(self.shape_err(other, "start_thread")),
        }
    }

    /// List threads discoverable to this agent: member OR cwd matches the path glob OR the subject
    /// filter matches. Never all threads.
    pub async fn list_threads(
        &mut self,
        remote: Option<String>,
        cwd: Option<PathBuf>,
        subject_contains: Option<String>,
        include_archived: bool,
    ) -> Result<Vec<Thread>, CommsClientError> {
        match self
            .request(CommsRequest::ThreadList {
                remote,
                cwd,
                subject_contains,
                include_archived,
            })
            .await?
        {
            CommsResponse::Threads(t) => Ok(t),
            other => Err(self.shape_err(other, "list_threads")),
        }
    }

    /// Join a thread (durable membership; drives the inbox).
    pub async fn join_thread(&mut self, thread: ThreadId) -> Result<(), CommsClientError> {
        self.expect_ok(CommsRequest::ThreadJoin { thread }, "join_thread").await
    }

    /// Leave a thread.
    pub async fn leave_thread(&mut self, thread: ThreadId) -> Result<(), CommsClientError> {
        self.expect_ok(CommsRequest::ThreadLeave { thread }, "leave_thread")
            .await
    }

    /// List the members of a thread.
    pub async fn thread_members(&mut self, thread: ThreadId) -> Result<Vec<AgentId>, CommsClientError> {
        match self.request(CommsRequest::ThreadMembers { thread }).await? {
            CommsResponse::Members { members } => Ok(members),
            other => Err(self.shape_err(other, "thread_members")),
        }
    }

    /// Add a member to a thread (creator only).
    pub async fn add_member(&mut self, thread: ThreadId, member: AgentId) -> Result<(), CommsClientError> {
        self.expect_ok(CommsRequest::ThreadAddMember { thread, member }, "add_member")
            .await
    }

    /// Remove a member from a thread (creator only).
    pub async fn remove_member(&mut self, thread: ThreadId, member: AgentId) -> Result<(), CommsClientError> {
        self.expect_ok(CommsRequest::ThreadRemoveMember { thread, member }, "remove_member")
            .await
    }

    /// Archive a thread (creator only).
    pub async fn archive_thread(&mut self, thread: ThreadId) -> Result<(), CommsClientError> {
        self.expect_ok(CommsRequest::ThreadArchive { thread }, "archive_thread")
            .await
    }

    /// Post a message to a thread. Returns the new message id.
    pub async fn post_message(
        &mut self,
        thread: ThreadId,
        subject: String,
        body: Vec<u8>,
        tags: Vec<String>,
        reply_to: Option<String>,
    ) -> Result<String, CommsClientError> {
        match self
            .request(CommsRequest::ThreadPost {
                thread,
                subject,
                tags,
                reply_to,
                body,
            })
            .await?
        {
            CommsResponse::Posted { message_id } => Ok(message_id),
            other => Err(self.shape_err(other, "post_message")),
        }
    }

    /// Acknowledge inbox messages by advancing this agent's per-thread read cursors. Pass
    /// `message_ids` to ack specific messages (each resolved to its `(thread, seq)`), and/or a
    /// `(thread, to_seq)` pair to bulk-ack everything up to `to_seq` in that thread. Returns the
    /// count of acked ids and the `(thread, new_seq)` cursors that advanced.
    pub async fn ack_inbox(
        &mut self,
        message_ids: Vec<String>,
        thread: Option<ThreadId>,
        to_seq: Option<u64>,
    ) -> Result<(u32, Vec<(String, u64)>), CommsClientError> {
        match self
            .request(CommsRequest::AckInbox {
                message_ids,
                thread,
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

    /// Read a thread's history (front-matter only), oldest-first. `since_micros` is an absolute
    /// recency cutoff; `None` returns the full log.
    pub async fn read_history(
        &mut self,
        thread: ThreadId,
        cursor: Option<Cursor>,
        limit: u32,
        since_micros: Option<i64>,
    ) -> Result<(Vec<SeqMeta>, Option<Cursor>), CommsClientError> {
        match self
            .request(CommsRequest::ThreadHistory {
                thread,
                cursor,
                limit: Some(limit),
                since_micros,
            })
            .await?
        {
            CommsResponse::History { messages, next_cursor } => Ok((messages, next_cursor)),
            other => Err(self.shape_err(other, "read_history")),
        }
    }

    /// Fetch a single message body by id. `None` when the id is unknown.
    pub async fn get_body(&mut self, message_id: String) -> Result<Option<Vec<u8>>, CommsClientError> {
        match self.request(CommsRequest::GetBody { message_id }).await? {
            CommsResponse::Body { body } => Ok(body),
            other => Err(self.shape_err(other, "get_body")),
        }
    }

    /// Read this agent's inbox across subscribed rooms. Returns the page, the count of unread
    /// remaining after the page, and the next cursor.
    #[allow(clippy::type_complexity, clippy::too_many_arguments)]
    pub async fn read_inbox(
        &mut self,
        remote: Option<String>,
        cwd: Option<PathBuf>,
        cursor: Option<Cursor>,
        limit: u32,
        mark_read: bool,
        since_micros: Option<i64>,
    ) -> Result<(Vec<SeqMeta>, u32, Option<Cursor>), CommsClientError> {
        match self
            .request(CommsRequest::Inbox {
                remote,
                cwd,
                cursor,
                limit: Some(limit),
                mark_read,
                since_micros,
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

    /// Open a notification stream for a thread. Returns the subscription handle; subsequent
    /// [`CommsClient::next_notification`] calls surface posts to that thread.
    pub async fn subscribe(&mut self, thread: ThreadId) -> Result<u64, CommsClientError> {
        match self.request(CommsRequest::Subscribe { thread }).await? {
            CommsResponse::Subscribed { sub } => Ok(sub),
            other => Err(self.shape_err(other, "subscribe")),
        }
    }

    /// Open a notification stream for THIS agent's inbox: a push for every subsequent post to a
    /// thread the agent is already a member of (self-authored posts excluded), or — when `thread`
    /// is `Some` — restricted to that one thread. Passive: unlike [`CommsClient::subscribe`], it
    /// does NOT join anything. Backs [`CommsClient::wait_inbox`].
    pub async fn subscribe_inbox(&mut self, thread: Option<ThreadId>) -> Result<u64, CommsClientError> {
        match self.request(CommsRequest::SubscribeInbox { thread }).await? {
            CommsResponse::Subscribed { sub } => Ok(sub),
            other => Err(self.shape_err(other, "subscribe_inbox")),
        }
    }

    /// Cancel a notification stream opened by [`CommsClient::subscribe`] or
    /// [`CommsClient::subscribe_inbox`].
    pub async fn unsubscribe(&mut self, sub: u64) -> Result<(), CommsClientError> {
        self.expect_ok(CommsRequest::Unsubscribe { sub }, "unsubscribe").await
    }

    /// Long-poll the inbox: subscribe FIRST, then do one immediate non-blocking inbox read, then —
    /// only if that read came back empty — block on the notification stream up to `timeout`. The
    /// subscribe-first ordering is the race fix: a post landing between the caller's last read and
    /// this call is never lost, because the sink is already live before the immediate read runs.
    /// Unsubscribes on EVERY exit path (early return, wake, shutdown, timeout, or error) so a
    /// failed/aborted wait never leaks a sink.
    ///
    /// Returns `(timed_out, rows, unread, next_cursor)`:
    /// * a non-empty immediate read, or a wake from a post, yields `timed_out = false` with a
    ///   freshly re-read, consistent page (never marks read — see `inbox_ack` / `inbox_read`).
    /// * a daemon [`CommsNotification::Shutdown`] (the link is about to die) or the socket closing
    ///   yields `timed_out = true` with an empty page — the caller reconnects and retries.
    /// * the `timeout` elapsing with nothing new yields `timed_out = true`, carrying the unread
    ///   count observed by the immediate read.
    #[allow(clippy::too_many_arguments)]
    pub async fn wait_inbox(
        &mut self,
        remote: Option<String>,
        cwd: Option<PathBuf>,
        thread: Option<ThreadId>,
        since_micros: Option<i64>,
        cursor: Option<Cursor>,
        limit: u32,
        timeout: std::time::Duration,
    ) -> Result<(bool, Vec<SeqMeta>, u32, Option<Cursor>), CommsClientError> {
        self.wait_inbox_unseen(
            remote,
            cwd,
            thread,
            since_micros,
            cursor,
            limit,
            timeout,
            &CancellationToken::new(),
            |_| false,
        )
        .await
    }

    /// [`CommsClient::wait_inbox`] that skips rows the caller already `is_seen` and stops promptly
    /// when `cancel` fires.
    ///
    /// `is_seen` is what keeps a poll loop that never acks from spinning: an unread-but-already-
    /// reported backlog no longer satisfies the immediate read, so the call blocks for something
    /// genuinely new. On cancellation the sink is NOT unsubscribed over the wire (the caller is gone;
    /// the request would only queue behind a dead consumer): the caller drops this client, and the
    /// broker reaps the subscription when the link closes. Returns [`CommsClientError::Cancelled`].
    #[allow(clippy::too_many_arguments)]
    pub async fn wait_inbox_unseen(
        &mut self,
        remote: Option<String>,
        cwd: Option<PathBuf>,
        thread: Option<ThreadId>,
        since_micros: Option<i64>,
        cursor: Option<Cursor>,
        limit: u32,
        timeout: std::time::Duration,
        cancel: &CancellationToken,
        is_seen: impl Fn(&SeqMeta) -> bool,
    ) -> Result<(bool, Vec<SeqMeta>, u32, Option<Cursor>), CommsClientError> {
        let sub = tokio::select! {
            () = cancel.cancelled() => return Err(CommsClientError::Cancelled),
            sub = self.subscribe_inbox(thread) => sub?,
        };
        let outcome = self
            .wait_inbox_after_subscribe(remote, cwd, since_micros, cursor, limit, timeout, cancel, is_seen)
            .await;
        if !matches!(outcome, Err(CommsClientError::Cancelled)) {
            let _ = self.unsubscribe(sub).await;
        }
        outcome
    }

    /// The body of [`CommsClient::wait_inbox_unseen`] once the sink is live: check, then block, and
    /// re-check on every wake until something unseen shows up or `timeout` elapses.
    #[allow(clippy::too_many_arguments)]
    async fn wait_inbox_after_subscribe(
        &mut self,
        remote: Option<String>,
        cwd: Option<PathBuf>,
        since_micros: Option<i64>,
        cursor: Option<Cursor>,
        limit: u32,
        timeout: std::time::Duration,
        cancel: &CancellationToken,
        is_seen: impl Fn(&SeqMeta) -> bool,
    ) -> Result<(bool, Vec<SeqMeta>, u32, Option<Cursor>), CommsClientError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let (rows, unread, next) = tokio::select! {
                () = cancel.cancelled() => return Err(CommsClientError::Cancelled),
                read = self.read_inbox(remote.clone(), cwd.clone(), cursor.clone(), limit, false, since_micros) => read?,
            };
            let fresh: Vec<SeqMeta> = rows.into_iter().filter(|row| !is_seen(row)).collect();
            if !fresh.is_empty() {
                return Ok((false, fresh, unread, next));
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Ok((true, Vec::new(), unread, None));
            }
            let note = tokio::select! {
                () = cancel.cancelled() => return Err(CommsClientError::Cancelled),
                note = tokio::time::timeout(remaining, self.poll_notification()) => note,
            };
            match note {
                Ok(Ok(Some(CommsNotification::Message(_)))) => continue,
                // Discovery metadata is consumed directly through `subscribe_inbox` plus
                // `poll_notification`. Keep the legacy message-only wait shape exhaustive without
                // misreporting a live wake as a timeout.
                Ok(Ok(Some(CommsNotification::ThreadDiscovered(_)))) => return Ok((false, Vec::new(), unread, None)),
                Ok(Ok(Some(CommsNotification::Shutdown))) | Ok(Ok(None)) => return Ok((true, Vec::new(), 0, None)),
                Ok(Err(err)) => return Err(err),
                Err(_elapsed) => return Ok((true, Vec::new(), unread, None)),
            }
        }
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
    pub async fn poll_notification(&mut self) -> Result<Option<CommsNotification>, CommsClientError> {
        if let Some(n) = self.pending_notifications.pop_front() {
            return Ok(Some(n));
        }
        loop {
            match self.read_frame().await? {
                Some(CommsOut::Notification(n)) => return Ok(Some(n)),
                Some(CommsOut::Response(_) | CommsOut::Reply { .. }) => continue,
                None => return Ok(None),
            }
        }
    }

    /// List the workspaces the daemon currently holds hot (drives the `basemind statusline` CLI).
    pub async fn accessed_paths(&mut self) -> Result<Vec<AccessedWorkspace>, CommsClientError> {
        match self.request(CommsRequest::AccessedPaths).await? {
            CommsResponse::Accessed { workspaces } => Ok(workspaces),
            other => Err(self.shape_err(other, "accessed_paths")),
        }
    }

    /// List every registered workspace in the daemon's machine registry (git + plain). Read-only.
    pub async fn list_workspaces(&mut self) -> Result<Vec<crate::registry::WorkspaceRecord>, CommsClientError> {
        match self.request(CommsRequest::WorkspacesList).await? {
            CommsResponse::Workspaces { workspaces } => Ok(workspaces),
            other => Err(self.shape_err(other, "list_workspaces")),
        }
    }

    /// List the worktrees of a registered repo by id. An unknown repo id returns an empty list.
    pub async fn list_worktrees(
        &mut self,
        repo_id: String,
    ) -> Result<Vec<crate::registry::WorktreeRecord>, CommsClientError> {
        match self.request(CommsRequest::WorktreesList { repo_id }).await? {
            CommsResponse::Worktrees { worktrees } => Ok(worktrees),
            other => Err(self.shape_err(other, "list_worktrees")),
        }
    }

    /// List the local branches of a registered repo by id. An unknown repo id returns an empty list.
    pub async fn list_branches(
        &mut self,
        repo_id: String,
    ) -> Result<Vec<crate::registry::BranchRecord>, CommsClientError> {
        match self.request(CommsRequest::BranchesList { repo_id }).await? {
            CommsResponse::Branches { branches } => Ok(branches),
            other => Err(self.shape_err(other, "list_branches")),
        }
    }

    /// Advisory-claim a worktree for `claimant`. Returns `true` when the claim is now held by
    /// `claimant` (freshly taken or already theirs), `false` when another claimant holds it or the
    /// worktree is unknown.
    pub async fn claim_worktree(
        &mut self,
        repo_id: String,
        name: String,
        claimant: String,
    ) -> Result<bool, CommsClientError> {
        match self
            .request(CommsRequest::WorktreeClaim {
                repo_id,
                name,
                claimant,
            })
            .await?
        {
            CommsResponse::ClaimOutcome { held } => Ok(held),
            other => Err(self.shape_err(other, "claim_worktree")),
        }
    }

    /// Release an advisory worktree claim held by `claimant`. Returns `true` when a claim by
    /// `claimant` was cleared, `false` otherwise.
    pub async fn release_worktree(
        &mut self,
        repo_id: String,
        name: String,
        claimant: String,
    ) -> Result<bool, CommsClientError> {
        match self
            .request(CommsRequest::WorktreeRelease {
                repo_id,
                name,
                claimant,
            })
            .await?
        {
            CommsResponse::ClaimOutcome { held } => Ok(held),
            other => Err(self.shape_err(other, "release_worktree")),
        }
    }

    async fn expect_ok(&mut self, req: CommsRequest, label: &'static str) -> Result<(), CommsClientError> {
        match self.request(req).await? {
            CommsResponse::Ok => Ok(()),
            other => Err(self.shape_err(other, label)),
        }
    }

    pub(super) fn shape_err(&self, resp: CommsResponse, request: &'static str) -> CommsClientError {
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
    /// coordination message — not corruption — which is an accepted trade-off for making `thread_post`
    /// survive the daemon dying at all. (A client-supplied idempotency key would close it; deferred.)
    ///
    /// Cancel safety: every request is a correlated [`CommsRequest::Call`] and only the reply
    /// echoing its id is accepted. If a previous request future was dropped after its frame was
    /// written (a timeout, an aborted tool call), its reply is still in flight; it is recognised by
    /// its stale id and discarded rather than mistaken for this request's answer. If the drop
    /// landed mid-write the link is unusable, so the next request reconnects first.
    pub(super) async fn request(&mut self, req: CommsRequest) -> Result<CommsResponse, CommsClientError> {
        if self.write_incomplete {
            self.reconnect().await?;
        }
        let timeout = self.request_timeout_override.or_else(|| request_timeout(&req));
        let what = req.method();
        match bounded(what, timeout, self.send_correlated(req.clone())).await {
            Ok(resp) => Ok(resp),
            Err(err) if is_connection_lost(&err) => {
                self.reconnect().await?;
                bounded(what, timeout, self.send_correlated(req)).await
            }
            Err(err) => Err(err),
        }
    }

    /// Wrap `req` in a [`CommsRequest::Call`] with a fresh id and await the matching reply.
    async fn send_correlated(&mut self, req: CommsRequest) -> Result<CommsResponse, CommsClientError> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.send_and_await(
            CommsRequest::Call {
                id,
                request: Box::new(req),
            },
            Some(id),
        )
        .await
    }

    /// Write the frame and read until the response for `expect_id` arrives (or, for a bare request
    /// with `None`, the first uncorrelated response), buffering notifications and discarding stale
    /// replies to abandoned requests. No reconnect — the single-shot retry lives in
    /// [`CommsClient::request`].
    async fn send_and_await(
        &mut self,
        req: CommsRequest,
        expect_id: Option<u64>,
    ) -> Result<CommsResponse, CommsClientError> {
        self.write_request(&req).await?;
        loop {
            match self.read_frame().await? {
                Some(CommsOut::Reply { id, response }) if Some(id) == expect_id => return Ok(response),
                Some(CommsOut::Response(response)) if expect_id.is_none() => return Ok(response),
                Some(CommsOut::Reply { id, .. }) => {
                    tracing::debug!(stale_id = id, expected = ?expect_id, "comms: discarding reply to an abandoned request");
                }
                Some(CommsOut::Response(_)) => {
                    tracing::debug!(expected = ?expect_id, "comms: discarding uncorrelated response");
                }
                Some(CommsOut::Notification(n)) => buffer_notification(&mut self.pending_notifications, n),
                None => return Err(CommsClientError::Closed),
            }
        }
    }

    async fn write_request(&mut self, req: &CommsRequest) -> Result<(), CommsClientError> {
        let body = rmp_serde::to_vec_named(req)?;
        let mut framed = BytesMut::new();
        self.codec.encode(Bytes::from(body), &mut framed)?;
        self.write_incomplete = true;
        self.stream.write_all(&framed).await?;
        self.stream.flush().await?;
        self.write_incomplete = false;
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

/// Await `fut`, failing with [`CommsClientError::Unresponsive`] if `limit` elapses first.
async fn bounded<T>(
    what: &'static str,
    limit: Option<std::time::Duration>,
    fut: impl std::future::Future<Output = Result<T, CommsClientError>>,
) -> Result<T, CommsClientError> {
    let Some(limit) = limit else {
        return fut.await;
    };
    tokio::time::timeout(limit, fut)
        .await
        .unwrap_or(Err(CommsClientError::Unresponsive {
            what,
            secs: limit.as_secs(),
        }))
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

    /// Nothing drains `pending_notifications` for a client that only issues ordinary requests, so
    /// the buffer itself has to be the bound: past the cap the queue must stay at the cap and shed
    /// its OLDEST entries, never grow with the broker's push volume.
    #[test]
    fn buffering_notifications_past_the_cap_holds_at_the_cap() {
        let mut queue = std::collections::VecDeque::new();

        for _ in 0..PENDING_NOTIFICATION_CAP {
            buffer_notification(&mut queue, CommsNotification::Shutdown);
        }
        assert_eq!(
            queue.len(),
            PENDING_NOTIFICATION_CAP,
            "the cap is reached but not exceeded"
        );

        for _ in 0..(PENDING_NOTIFICATION_CAP * 3) {
            buffer_notification(&mut queue, CommsNotification::Shutdown);
        }
        assert_eq!(
            queue.len(),
            PENDING_NOTIFICATION_CAP,
            "four times the cap of pushes must still leave exactly the cap resident"
        );
    }

    /// The eviction has to be oldest-first: the newest notifications are the ones a `wait` caller
    /// would act on, so shedding those instead would make the bound cost correctness. The single
    /// `Message` is enqueued FIRST, so it is the one entry the cap must drop.
    #[test]
    fn buffering_evicts_the_oldest_notification_first() {
        let mut queue = std::collections::VecDeque::new();

        buffer_notification(&mut queue, CommsNotification::Message(test_message_meta()));
        for _ in 0..(PENDING_NOTIFICATION_CAP - 1) {
            buffer_notification(&mut queue, CommsNotification::Shutdown);
        }
        assert!(
            matches!(queue.front(), Some(CommsNotification::Message(_))),
            "precondition: the queue is exactly full and the Message is still the oldest entry"
        );

        buffer_notification(&mut queue, CommsNotification::Shutdown);

        assert_eq!(queue.len(), PENDING_NOTIFICATION_CAP, "the cap still holds");
        assert!(
            matches!(queue.front(), Some(CommsNotification::Shutdown)),
            "the oldest entry (the Message) must be the one dropped"
        );
        assert!(
            !queue.iter().any(|n| matches!(n, CommsNotification::Message(_))),
            "the evicted entry must be gone from the queue entirely"
        );
    }

    /// A scripted broker on a Unix socket: answers `Hello` bare, then each correlated `Call` in
    /// order, sleeping `delays[i]` before reply `i`. Returns the paths to dial.
    fn scripted_broker(
        delays: Vec<std::time::Duration>,
    ) -> (CommsPaths, tokio::task::JoinHandle<()>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("bm-client-{}-{}", std::process::id(), rand_suffix()));
        std::fs::create_dir_all(&dir).expect("dir");
        let socket_path = dir.join("c.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).expect("bind");
        let paths = CommsPaths {
            comms_dir: dir.clone(),
            socket_path,
        };
        let handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut codec = LengthDelimitedCodec::new();
            let mut buf = BytesMut::new();
            let mut served = 0usize;
            loop {
                let frame = loop {
                    if let Some(f) = codec.decode(&mut buf).expect("decode") {
                        break f;
                    }
                    if stream.read_buf(&mut buf).await.expect("read") == 0 {
                        return;
                    }
                };
                let req: CommsRequest = rmp_serde::from_slice(&frame).expect("req");
                let out = match req {
                    CommsRequest::Hello { .. } => CommsOut::Response(CommsResponse::Welcome {
                        proto_ver: PROTO_VER,
                        daemon_version: "test".to_string(),
                    }),
                    CommsRequest::Call { id, request } => {
                        if let Some(d) = delays.get(served) {
                            tokio::time::sleep(*d).await;
                        }
                        served += 1;
                        let response = match *request {
                            CommsRequest::ListAgents { .. } => CommsResponse::Agents(Vec::new()),
                            CommsRequest::AccessedPaths => CommsResponse::Accessed { workspaces: Vec::new() },
                            other => panic!("unscripted request {other:?}"),
                        };
                        CommsOut::Reply { id, response }
                    }
                    other => panic!("bare request {other:?}"),
                };
                let body = rmp_serde::to_vec_named(&out).expect("encode");
                let mut framed = BytesMut::new();
                codec.encode(Bytes::from(body), &mut framed).expect("frame");
                stream.write_all(&framed).await.expect("write");
            }
        });
        (paths, handle, dir)
    }

    fn rand_suffix() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    }

    /// The reported failure: a request abandoned after its frame was written (the delivery-notice
    /// probe's 200 ms timeout) leaves its reply on the socket. The next request must still get ITS
    /// answer, not the stale frame ("unexpected response shape").
    #[tokio::test]
    async fn dropped_request_does_not_poison_the_next_response() {
        let (paths, server, dir) = scripted_broker(vec![std::time::Duration::from_millis(200)]);
        let agent = AgentId::parse("agent-1".to_string()).expect("agent");
        let mut client = CommsClient::connect_with_respawn(&paths, agent, None, None, |_| Ok(()))
            .await
            .expect("connect");

        let abandoned = tokio::time::timeout(std::time::Duration::from_millis(30), client.list_agents(None)).await;
        assert!(abandoned.is_err(), "the first request must be cut off mid-flight");

        let workspaces = client
            .accessed_paths()
            .await
            .expect("second request gets its own reply");
        assert!(workspaces.is_empty());

        server.abort();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A broker that stops answering must surface a retryable `broker unresponsive` error within the
    /// bound rather than hanging the caller forever, and the client must still work afterwards.
    #[tokio::test]
    async fn silent_broker_yields_a_retryable_unresponsive_error() {
        let (paths, server, dir) = scripted_broker(vec![std::time::Duration::from_secs(30)]);
        let agent = AgentId::parse("agent-1".to_string()).expect("agent");
        let mut client = CommsClient::connect_with_respawn(&paths, agent, None, None, |_| Ok(()))
            .await
            .expect("connect")
            .with_request_timeout(std::time::Duration::from_millis(100));

        let started = std::time::Instant::now();
        let err = client.list_agents(None).await.expect_err("no reply must time out");
        assert!(
            matches!(
                err,
                CommsClientError::Unresponsive {
                    what: "list_agents",
                    ..
                }
            ),
            "{err:?}"
        );
        assert!(err.to_string().contains("broker unresponsive"), "{err}");
        assert!(started.elapsed() < std::time::Duration::from_secs(5));

        server.abort();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A daemon that accepts the connection but never answers `Hello` must fail the connect inside
    /// the handshake bound.
    #[tokio::test]
    async fn handshake_against_a_mute_listener_times_out() {
        let dir = std::env::temp_dir().join(format!("bm-mute-{}-{}", std::process::id(), rand_suffix()));
        std::fs::create_dir_all(&dir).expect("dir");
        let socket_path = dir.join("m.sock");
        let _listener = tokio::net::UnixListener::bind(&socket_path).expect("bind");
        let paths = CommsPaths {
            comms_dir: dir.clone(),
            socket_path,
        };
        let agent = AgentId::parse("agent-1".to_string()).expect("agent");
        let Err(err) = CommsClient::connect_bounded(
            &paths,
            agent,
            None,
            None,
            |_| Ok(()),
            std::time::Duration::from_millis(100),
        )
        .await
        else {
            panic!("a mute listener must time out");
        };
        assert!(
            matches!(err, CommsClientError::Unresponsive { what: "handshake", .. }),
            "{err:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    fn test_message_meta() -> crate::comms::model::MessageMeta {
        crate::comms::model::MessageMeta {
            id: "m1".to_string(),
            thread: ThreadId::parse("t1".to_string()).expect("valid thread id"),
            from: AgentId::parse("agent-1".to_string()).expect("valid agent id"),
            ts_micros: 0,
            subject: "s".to_string(),
            tags: Vec::new(),
            reply_to: None,
            body_len: 0,
            body_sha: String::new(),
        }
    }
}

/// Buffer a notification seen while awaiting a response, holding `queue` to
/// [`PENDING_NOTIFICATION_CAP`] by evicting the oldest first. A free function rather than a method
/// so the bound can be tested without a live broker socket.
fn buffer_notification(queue: &mut std::collections::VecDeque<CommsNotification>, n: CommsNotification) {
    while queue.len() >= PENDING_NOTIFICATION_CAP {
        queue.pop_front();
    }
    queue.push_back(n);
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
        if key.starts_with("path:") { None } else { Some(key) }
    });
    (remote, Some(cwd.to_path_buf()))
}
