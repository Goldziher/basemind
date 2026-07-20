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

use super::cursor::Cursor;
use super::ids::{AgentId, ThreadId};
use super::model::{AgentCard, AgentRecord, Thread};
use super::protocol::{CommsNotification, CommsOut, CommsRequest, CommsResponse, PROTO_VER, SeqMeta, StatusReport};
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
    stream: PlatformStream,
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
        singleton::ensure_daemon_with(&self.paths, singleton::probe_alive, |paths| spawn(paths)).await?;
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
        let sub = self.subscribe_inbox(thread).await?;
        let outcome = self
            .wait_inbox_after_subscribe(remote, cwd, since_micros, cursor, limit, timeout)
            .await;
        let _ = self.unsubscribe(sub).await;
        outcome
    }

    /// The body of [`CommsClient::wait_inbox`] once the sink is live: immediate check, then block.
    /// Split out so the caller can wrap it in a single unsubscribe-on-every-exit point.
    #[allow(clippy::too_many_arguments)]
    async fn wait_inbox_after_subscribe(
        &mut self,
        remote: Option<String>,
        cwd: Option<PathBuf>,
        since_micros: Option<i64>,
        cursor: Option<Cursor>,
        limit: u32,
        timeout: std::time::Duration,
    ) -> Result<(bool, Vec<SeqMeta>, u32, Option<Cursor>), CommsClientError> {
        let (rows, unread, next) = self
            .read_inbox(remote.clone(), cwd.clone(), cursor.clone(), limit, false, since_micros)
            .await?;
        if !rows.is_empty() {
            return Ok((false, rows, unread, next));
        }

        match tokio::time::timeout(timeout, self.poll_notification()).await {
            Ok(Ok(Some(CommsNotification::Message(_)))) => {
                let (rows, unread, next) = self.read_inbox(remote, cwd, cursor, limit, false, since_micros).await?;
                Ok((false, rows, unread, next))
            }
            Ok(Ok(Some(CommsNotification::Shutdown))) | Ok(Ok(None)) => Ok((true, Vec::new(), 0, None)),
            Ok(Err(err)) => Err(err),
            Err(_elapsed) => Ok((true, Vec::new(), unread, None)),
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
                Some(CommsOut::Response(_)) => continue,
                None => return Ok(None),
            }
        }
    }

    /// Ask the daemon (the machine's sole fjall writer) to scan or rescan a workspace. Front-ends
    /// forward their writes here so concurrent read-only sessions never contend for the index lock.
    /// A non-empty `paths` (with `full == false`) drives an incremental rescan; otherwise the whole
    /// working tree is scanned. `embed` requests an
    /// [`EmbedMode::Inline`](crate::scanner::EmbedMode::Inline) vector-fill pass (documents + code
    /// chunks); `false` is the fast `Deferred` code-map pass. Idempotent, so the transparent
    /// reconnect-and-retry is replay-safe.
    pub async fn rescan(
        &mut self,
        root: PathBuf,
        paths: Option<Vec<PathBuf>>,
        full: bool,
        embed: bool,
    ) -> Result<RescanReport, CommsClientError> {
        match self
            .request(CommsRequest::Rescan {
                root,
                paths,
                full,
                embed,
            })
            .await?
        {
            CommsResponse::Rescanned {
                scanned,
                updated,
                removed,
                elapsed_ms,
            } => Ok(RescanReport {
                scanned,
                updated,
                removed,
                elapsed_ms,
            }),
            other => Err(self.shape_err(other, "rescan")),
        }
    }

    /// Forward a CORE memory operation to the daemon (the sole fjall writer). A `daemon_writer`
    /// serve resolves the namespace + scope and ships the op here; the vector (LanceDB) half stays
    /// serve-side. Idempotent for get/list/delete; `put` is a preserving RMW, so a replayed retry is
    /// safe.
    #[cfg(feature = "memory")]
    pub async fn memory_op(
        &mut self,
        root: PathBuf,
        scope: String,
        op: crate::comms::memory_proto::MemoryOp,
    ) -> Result<crate::comms::memory_proto::MemoryOutcome, CommsClientError> {
        match self.request(CommsRequest::Memory { root, scope, op }).await? {
            CommsResponse::Memory(outcome) => Ok(outcome),
            other => Err(self.shape_err(other, "memory_op")),
        }
    }

    /// Forward a PROPOSAL governance operation to the daemon (the sole fjall writer). A
    /// `daemon_writer` serve does the git-log mining / audit verdict / LanceDB embed on its side and
    /// ships only the fjall reads/writes here. Idempotent for list/get; reject + promote are
    /// terminal writes and mine-apply is tombstone-guarded, so a replayed retry is safe.
    #[cfg(feature = "memory")]
    pub async fn governance_op(
        &mut self,
        root: PathBuf,
        scope: String,
        op: crate::comms::proposals_proto::GovernanceOp,
    ) -> Result<crate::comms::proposals_proto::GovernanceOutcome, CommsClientError> {
        match self.request(CommsRequest::Governance { root, scope, op }).await? {
            CommsResponse::Governance(outcome) => Ok(outcome),
            other => Err(self.shape_err(other, "governance_op")),
        }
    }

    /// Forward a precise resolved-reference read to the daemon (the sole fjall writer, holding the
    /// cross-file `refs_by_def` / `refs_by_path` index a `daemon_writer` serve cannot see). Backs
    /// the precise cross-file `find_callers` / `goto_definition` path. A pure read, so the
    /// transparent reconnect-and-retry is replay-safe.
    pub async fn resolved_refs(
        &mut self,
        root: PathBuf,
        query: crate::comms::resolved_proto::ResolvedRefQuery,
    ) -> Result<crate::comms::resolved_proto::ResolvedRefResult, CommsClientError> {
        match self.request(CommsRequest::ResolvedRefs { root, query }).await? {
            CommsResponse::ResolvedRefs(result) => Ok(result),
            other => Err(self.shape_err(other, "resolved_refs")),
        }
    }

    /// Forward a git-history operation to the daemon — the sole holder of `git-history.fjall/`
    /// (fjall's directory lock is exclusive, so no front-end may open it). Backs both the index
    /// BUILD a `daemon_writer` serve requests at startup instead of running it in-process, and the
    /// history reads its `recent_changes` / `commits_touching` / `hot_files` / `search_git_history`
    /// tools would otherwise have to degrade to a live walk.
    ///
    /// Every op is idempotent — the reads are pure and `Sync` is freshness-checked (a replayed sync
    /// against an up-to-date index is a no-op) — so the transparent reconnect-and-retry is safe.
    pub async fn git_history(
        &mut self,
        root: PathBuf,
        op: crate::git_history::proto::GitHistoryOp,
    ) -> Result<crate::git_history::proto::GitHistoryReply, CommsClientError> {
        match self.request(CommsRequest::GitHistory { root, op }).await? {
            CommsResponse::GitHistory(reply) => Ok(reply),
            other => Err(self.shape_err(other, "git_history")),
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
    /// coordination message — not corruption — which is an accepted trade-off for making `thread_post`
    /// survive the daemon dying at all. (A client-supplied idempotency key would close it; deferred.)
    async fn request(&mut self, req: CommsRequest) -> Result<CommsResponse, CommsClientError> {
        match self.send_and_await(req.clone()).await {
            Ok(resp) => Ok(resp),
            Err(err) if is_connection_lost(&err) => {
                self.reconnect().await?;
                self.send_and_await(req).await
            }
            Err(err) => Err(err),
        }
    }

    /// Write the request and read frames until the direct response arrives, buffering any
    /// notifications seen in the meantime. No reconnect — the single-shot retry lives in
    /// [`CommsClient::request`].
    async fn send_and_await(&mut self, req: CommsRequest) -> Result<CommsResponse, CommsClientError> {
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
        if key.starts_with("path:") { None } else { Some(key) }
    });
    (remote, Some(cwd.to_path_buf()))
}
