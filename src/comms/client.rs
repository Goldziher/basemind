//! `CommsClient`: the public client contract used by `basemind serve`, the CLI, and hooks.
//!
//! A thin async wrapper over a [`CommsLink`](super::transport::CommsLink) to the broker. The
//! client owns the request/response correlation: the broker answers requests in order on the
//! link, and notifications are surfaced separately so a caller can drain them. A later
//! component (the MCP/CLI tool surface) proxies straight to these methods, so the signatures
//! here are the stable contract.

use std::path::{Path, PathBuf};

use super::protocol::{CommsOut, CommsRequest, CommsResponse, PROTO_VER, StatusReport};
use super::singleton::{self, CommsPaths};
use super::transport::MAX_FRAME_BYTES;
use super::v2::api::{CommsError, MAX_WAIT_SECS, Principal};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(unix)]
use tokio::net::UnixStream as PlatformStream;
#[cfg(windows)]
use tokio::net::windows::named_pipe::NamedPipeClient as PlatformStream;
use tokio_util::bytes::{Bytes, BytesMut};
use tokio_util::codec::{Decoder, Encoder, LengthDelimitedCodec};

/// Outcome of a daemon-side [`CommsClient::rescan`]: the scan counts plus wall-clock time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RescanReport {
    /// Files considered by the scan.
    pub scanned: usize,
    /// Files whose index entries were written or refreshed.
    pub updated: usize,
    /// Documents re-extracted and re-embedded. Separate from `updated`, which is code-map only.
    pub docs_indexed: usize,
    /// Documents chunked by the linear fallback because they exceeded the `[documents]` chunk cutovers.
    /// `0` from a daemon that predates the field.
    pub docs_degraded: usize,
    /// Subset of `docs_degraded` re-read as `text/plain` after a structured-extraction failure. `0`
    /// from a daemon that predates the field.
    pub docs_text_fallback: usize,
    /// Documents abandoned because extraction overran `[documents] extraction_timeout_secs`. `0` from a
    /// daemon that predates the field.
    pub doc_timeouts: usize,
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
/// What an agent request may wait beyond the request bound for the daemon's agent store to open.
pub(crate) const AGENT_STORE_OPEN_ALLOWANCE: std::time::Duration = std::time::Duration::from_secs(60);
/// Generous because a cold client may first have to spawn the daemon and wait for its socket.
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

/// Whether a request may be sent again after the connection broke mid-request: not an agent
/// request that writes, which carries no replay key from a CLI or hook caller.
fn retry_safe(req: &CommsRequest) -> bool {
    match req {
        CommsRequest::AgentPost { .. } | CommsRequest::AgentThreadUpdate { .. } | CommsRequest::AgentCleanup { .. } => {
            false
        }
        CommsRequest::AgentInbox { body, .. } => body.kind().is_ok_and(|kind| !kind.writes()),
        _ => true,
    }
}

/// How long `req` may wait for its reply. Forwarded work (scans, embeds, memory, git-history and
/// index reads) legitimately runs for minutes and is unbounded here; everything else is a quick
/// broker round trip, so silence past the timeout means the broker is wedged, not busy.
fn request_timeout(req: &CommsRequest) -> Option<std::time::Duration> {
    match req {
        // ~keep The daemon opens its agent store on a thread of its own, and a request made while it
        // ~keep opens waits for it: creating the store can take tens of seconds on a contended disk.
        CommsRequest::AgentInbox { wait_secs, .. } => Some(
            timeout_from_env(REQUEST_TIMEOUT_ENV, DEFAULT_REQUEST_TIMEOUT_SECS)
                + AGENT_STORE_OPEN_ALLOWANCE
                + std::time::Duration::from_secs(u64::from((*wait_secs).min(MAX_WAIT_SECS))),
        ),
        CommsRequest::AgentPost { .. }
        | CommsRequest::AgentThreads { .. }
        | CommsRequest::AgentThreadUpdate { .. }
        | CommsRequest::AgentCleanup { .. } => {
            Some(timeout_from_env(REQUEST_TIMEOUT_ENV, DEFAULT_REQUEST_TIMEOUT_SECS) + AGENT_STORE_OPEN_ALLOWANCE)
        }
        CommsRequest::Rescan { .. }
        | CommsRequest::GitHistory { .. }
        | CommsRequest::ResolvedRefs { .. }
        | CommsRequest::IndexRead { .. }
        | CommsRequest::CodeSearchLanes { .. } => None,
        #[cfg(feature = "memory")]
        CommsRequest::Memory { .. } => None,
        _ => Some(timeout_from_env(REQUEST_TIMEOUT_ENV, DEFAULT_REQUEST_TIMEOUT_SECS)),
    }
}

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
    /// The daemon refused an agent request.
    #[error("{0}")]
    Agent(CommsError),
    /// The daemon's protocol version differs from this build's.
    #[error("protocol skew: daemon speaks {daemon}, client speaks {client}")]
    ProtoSkew {
        /// The daemon's protocol version.
        daemon: u32,
        /// This client's protocol version.
        client: u32,
    },
}

/// Who a link speaks for, sent in its `Hello` (ADR-0021). A link that only manages the daemon or
/// forwards index work names nobody; one that makes agent requests names the principal the
/// identity resolver chose and the canonical root it chose it for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClientHello {
    /// The principal the link's agent requests act as.
    pub principal: Option<Principal>,
    /// The canonical checkout root.
    pub root: Option<PathBuf>,
    /// The agent harness (`BASEMIND_HOST`).
    pub host: Option<String>,
}

impl ClientHello {
    /// A link that makes no agent requests.
    pub fn anonymous() -> Self {
        Self::default()
    }

    /// A link that acts as `principal` in the checkout at `root`.
    pub fn agent(principal: Principal, root: PathBuf, host: Option<String>) -> Self {
        Self {
            principal: Some(principal),
            root: Some(root),
            host,
        }
    }
}

/// A connected, said-hello client to the comms broker.
pub struct CommsClient {
    stream: PlatformStream,
    codec: LengthDelimitedCodec,
    read_buf: BytesMut,
    /// Connection context retained so the client can transparently re-establish the link (and
    /// re-spawn the daemon) after the daemon dies mid-session.
    paths: CommsPaths,
    /// What the `Hello` of every (re)connect says.
    hello: ClientHello,
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
    /// Connect to an already-running daemon at `paths` and complete the `Hello` handshake.
    /// Use [`CommsClient::ensure_and_connect`] to spawn the daemon first when needed.
    ///
    /// The returned client re-spawns the daemon via [`singleton::spawn_detached_daemon`] when a
    /// reconnect finds the socket dead. Use [`CommsClient::connect_with_respawn`] to inject a
    /// different spawn strategy.
    pub async fn connect(paths: &CommsPaths, hello: ClientHello) -> Result<Self, CommsClientError> {
        Self::connect_with_respawn(paths, hello, singleton::spawn_detached_daemon).await
    }

    /// Connect like [`CommsClient::connect`], but inject the daemon respawn strategy used by the
    /// transparent reconnect path. The production [`CommsClient::connect`] supplies
    /// [`singleton::spawn_detached_daemon`]; tests inject a closure that launches the real
    /// `basemind` binary so the reconnect can resurrect an isolated daemon.
    pub async fn connect_with_respawn(
        paths: &CommsPaths,
        hello: ClientHello,
        spawn: impl Fn(&CommsPaths) -> std::io::Result<()> + Send + Sync + 'static,
    ) -> Result<Self, CommsClientError> {
        let handshake_timeout = timeout_from_env(HANDSHAKE_TIMEOUT_ENV, DEFAULT_HANDSHAKE_TIMEOUT_SECS);
        Self::connect_bounded(paths, hello, spawn, handshake_timeout).await
    }

    /// [`CommsClient::connect_with_respawn`] with an explicit bound on the dial + `Hello` handshake.
    async fn connect_bounded(
        paths: &CommsPaths,
        hello: ClientHello,
        spawn: impl Fn(&CommsPaths) -> std::io::Result<()> + Send + Sync + 'static,
        handshake_timeout: std::time::Duration,
    ) -> Result<Self, CommsClientError> {
        let (stream, codec) = bounded("connect", Some(handshake_timeout), Self::dial(paths)).await?;
        let mut client = Self {
            stream,
            codec,
            read_buf: BytesMut::with_capacity(READ_CHUNK),
            paths: paths.clone(),
            hello,
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
    pub async fn ensure_and_connect(hello: ClientHello) -> Result<Self, CommsClientError> {
        let paths = singleton::resolve_paths()?;
        singleton::ensure_daemon(&paths).await?;
        Self::connect(&paths, hello).await
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
                    proto_ver: PROTO_VER,
                    principal: self.hello.principal.clone(),
                    root: self.hello.root.clone(),
                    host: self.hello.host.clone(),
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
    /// (re-spawning it if the socket is gone), re-dial, and replay the `Hello` handshake.
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

    /// What this client's `Hello` says.
    pub fn hello(&self) -> &ClientHello {
        &self.hello
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
    /// Replay safety: the retry only fires when the connection broke, and only for a request that
    /// changes nothing or that the daemon makes idempotent itself (the index and status requests).
    /// The dominant failure this fixes is a dead or stale daemon: the write fails before any daemon
    /// sees the request, so the retry is the first delivery.
    ///
    /// An agent request that writes (a post, a thread change, a read that marks or announces, a
    /// cleanup) is retried only when its frame could not be written: a CLI or hook call carries no
    /// replay key, so a daemon that read it, committed it and died before replying would act on it
    /// twice. Its caller sees the error.
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
            // ~keep A frame whose write failed never reached a daemon that could act on it: a
            // ~keep partial frame does not decode. So even a write is sent again then.
            Err(err) if is_connection_lost(&err) && (retry_safe(&req) || self.write_incomplete) => {
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
    /// with `None`, the first uncorrelated response), skipping notifications and discarding stale
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
                Some(CommsOut::Notification(_)) => {}
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

#[path = "client_ops.rs"]
mod ops;

#[cfg(all(test, feature = "comms", unix))]
#[path = "client_tests.rs"]
mod tests;

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
